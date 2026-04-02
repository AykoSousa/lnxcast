//! # net
//!
//! Descoberta de dispositivos Miracast / Wi-Fi Direct via wpa_supplicant.
//!
//! ## Modelo de camadas
//!
//! ```text
//! Kernel / socket  →  /run/wpa_supplicant/*   (fonte da verdade)
//! D-Bus            →  GetInterface             (visão parcial, pode falhar)
//! D-Bus            →  ObjectManager            (visão global, mais confiável)
//! ```
//!
//! **Regra**: nunca assume que D-Bus reflete o estado real do sistema.
//! `GetInterface` é usada de forma *opcional* para habilitar P2PFind ativo.
//! `get_managed_objects()` é a fonte primária de peers descobertos.
//!
//! ## Fluxo de scan
//!
//! ```text
//! 1. Conecta ao system D-Bus
//! 2. Tenta GetInterface("p2p-dev-*")  ← opcional, não bloqueia
//!    ├─ Ok  → chama P2PFind ativo (melhor)
//!    └─ Err → modo passivo (escuta apenas)
//! 3. ObjectManager.get_managed_objects()  ← fonte primária de peers
//! 4. Filtra objetos com interface "fi.w1.wpa_supplicant1.Peer"
//! 5. Emite DeviceEvent para cada peer descoberto
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::{proxy, Connection};

// ──────────────────────────────────────────────────────────────────────────────
// Erros
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum NetError {
    #[error("D-Bus connection failed: {0}")]
    DbusConnection(#[from] zbus::Error),

    #[error("wpa_supplicant não está rodando ou inacessível no system D-Bus")]
    WpaNotRunning,

    #[error("Nenhuma interface P2P encontrada (tentadas: {tried:?})")]
    NoP2pInterface { tried: Vec<String> },

    #[error("Canal fechado pelo consumidor")]
    ChannelClosed,

    #[error("Erro interno: {0}")]
    Internal(String),
}

pub type NetResult<T> = Result<T, NetError>;

// ──────────────────────────────────────────────────────────────────────────────
// Tipos de domínio
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2pDevice {
    pub object_path: String,
    pub device_name: String,
    pub device_addr: String,
    pub signal_level: Option<i32>,
    pub wfd_capable: bool,
}

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    Discovered(P2pDevice),
    Lost { object_path: String },
    ScanComplete,
    /// Modo de operação atual do scanner (informativo).
    ScanMode(ScanMode),
    Error(String),
}

/// Modo de operação do scanner P2P.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// P2PFind ativo habilitado via D-Bus.
    Active,
    /// Escuta passiva via ObjectManager (GetInterface não disponível).
    Passive,
}

impl std::fmt::Display for ScanMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active  => write!(f, "ativo (P2PFind via D-Bus)"),
            Self::Passive => write!(f, "passivo (ObjectManager apenas)"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Trait
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait DeviceScanner: Send + Sync {
    async fn scan(&self, tx: mpsc::Sender<DeviceEvent>) -> NetResult<()>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Proxies D-Bus
// ──────────────────────────────────────────────────────────────────────────────

#[proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    /// Retorna o object path da interface — pode falhar em ambientes
    /// com NetworkManager. Sempre trate o erro como não-fatal.
    fn get_interface(&self, ifname: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaInterface {
    fn p2_p_find(
        &self,
        timeout: i32,
        args: HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    fn p2_p_stop_find(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "fi.w1.wpa_supplicant1.Peer",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaPeer {
    #[zbus(property, name = "DeviceName")]
    fn device_name(&self) -> zbus::Result<String>;

    #[zbus(property, name = "DeviceAddress")]
    fn device_address(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "Level")]
    fn level(&self) -> zbus::Result<i32>;

    #[zbus(property, name = "IEs")]
    fn ies(&self) -> zbus::Result<Vec<u8>>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Descoberta de interfaces Wi-Fi sem privilégios
// ──────────────────────────────────────────────────────────────────────────────

/// Lê `/proc/net/wireless` (world-readable) para listar adaptadores Wi-Fi.
fn wifi_interfaces_from_proc() -> Vec<String> {
    let Ok(content) = std::fs::read_to_string("/proc/net/wireless") else {
        return wifi_interfaces_from_sysfs();
    };
    content
        .lines()
        .skip(2)
        .filter_map(|line| {
            let name = line.split(':').next()?.trim().to_owned();
            if name.is_empty() { None } else { Some(name) }
        })
        .collect()
}

/// Fallback: lista interfaces wl* via /sys/class/net/.
fn wifi_interfaces_from_sysfs() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    dir.filter_map(|entry| {
        let name = entry.ok()?.file_name().to_string_lossy().into_owned();
        if name.starts_with("wl") { Some(name) } else { None }
    })
    .collect()
}

/// Lê sockets existentes em `/run/wpa_supplicant/` para validar
/// quais interfaces o daemon conhece no nível do sistema de arquivos.
fn wpa_socket_interfaces() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/run/wpa_supplicant") else {
        return Vec::new();
    };
    dir.filter_map(|entry| {
        let name = entry.ok()?.file_name().to_string_lossy().into_owned();
        // Filtra sockets de controle (não o socket global)
        if name != "global" && !name.ends_with(".pid") {
            Some(name)
        } else {
            None
        }
    })
    .collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// WpaSupplicantScanner
// ──────────────────────────────────────────────────────────────────────────────

pub struct WpaSupplicantScanner {
    /// Interface P2P preferida. Se vazia, auto-detecta.
    pub interface: Option<String>,
    /// Segundos por sweep de P2PFind.
    pub scan_timeout_secs: i32,
}

impl Default for WpaSupplicantScanner {
    fn default() -> Self {
        Self {
            interface: None,
            scan_timeout_secs: 30,
        }
    }
}

impl WpaSupplicantScanner {
    /// Cria scanner com interface explícita.
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: Some(interface.into()),
            ..Default::default()
        }
    }

    /// Cria scanner com auto-detecção de interface.
    pub fn auto() -> Self {
        Self::default()
    }

    /// Detecta a interface P2P sem privilégios elevados.
    ///
    /// Estratégia em ordem de prioridade:
    /// 1. Sockets em `/run/wpa_supplicant/` com prefixo `p2p-dev-`
    /// 2. Derivação a partir de `/proc/net/wireless` → `p2p-dev-<wlX>`
    /// 3. Validação via `GetInterface` D-Bus (opcional, não bloqueia)
    pub async fn detect_p2p_interface() -> NetResult<String> {
        // ── 1. Sockets de controle (mais confiável) ──────────────────────────
        let socket_ifaces = wpa_socket_interfaces();
        debug!(sockets = ?socket_ifaces, "Sockets em /run/wpa_supplicant/");

        if let Some(name) = socket_ifaces.iter().find(|n| n.starts_with("p2p-dev-")) {
            info!(iface = %name, "Interface P2P encontrada via socket");
            return Ok(name.clone());
        }

        // ── 2. Derivação via /proc/net/wireless ──────────────────────────────
        let wifi = wifi_interfaces_from_proc();
        debug!(wifi = ?wifi, "Adaptadores Wi-Fi de /proc/net/wireless");

        // Verifica se o socket derivado existe mesmo sem prefixo p2p-dev-*
        for parent in &wifi {
            let candidate = format!("p2p-dev-{parent}");
            if socket_ifaces.contains(&candidate) {
                info!(iface = %candidate, "Interface P2P derivada de socket existente");
                return Ok(candidate);
            }
        }

        // ── 3. Tentativa via D-Bus GetInterface (opcional) ───────────────────
        if let Ok(conn) = Connection::system().await {
            if let Ok(wpa) = WpaSupplicantProxy::new(&conn).await {
                // Verifica se daemon responde (sonda com nome inválido)
                let alive = matches!(
                    wpa.get_interface("__probe__").await,
                    Err(zbus::Error::MethodError(..)) | Ok(_)
                );

                if alive {
                    let mut tried = Vec::new();
                    for parent in &wifi {
                        let candidate = format!("p2p-dev-{parent}");
                        tried.push(candidate.clone());
                        if wpa.get_interface(&candidate).await.is_ok() {
                            info!(iface = %candidate, "Interface P2P confirmada via D-Bus");
                            return Ok(candidate);
                        }
                    }
                    // Tenta também os sockets sem p2p-dev-*
                    for name in &socket_ifaces {
                        let candidate = format!("p2p-dev-{name}");
                        tried.push(candidate.clone());
                        if wpa.get_interface(&candidate).await.is_ok() {
                            info!(iface = %candidate, "Interface P2P confirmada via D-Bus (socket)");
                            return Ok(candidate);
                        }
                    }
                    if !tried.is_empty() {
                        return Err(NetError::NoP2pInterface { tried });
                    }
                }
            }
        }

        // ── 4. Melhor palpite: primeiro socket não-p2p-dev + derivação ───────
        if let Some(parent) = socket_ifaces.iter().find(|n| !n.starts_with("p2p-dev-")) {
            let candidate = format!("p2p-dev-{parent}");
            warn!(
                iface = %candidate,
                "Usando interface derivada de socket (não confirmada via D-Bus)"
            );
            return Ok(candidate);
        }

        if let Some(parent) = wifi.first() {
            let candidate = format!("p2p-dev-{parent}");
            warn!(
                iface = %candidate,
                "Usando interface derivada de /proc/net/wireless (não confirmada)"
            );
            return Ok(candidate);
        }

        Err(NetError::NoP2pInterface { tried: socket_ifaces })
    }

    fn mac_to_string(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
    }

    async fn build_device(
        conn: &Connection,
        path: &zbus::zvariant::ObjectPath<'_>,
    ) -> NetResult<P2pDevice> {
        let peer = WpaPeerProxy::builder(conn)
            .path(path)
            .map_err(NetError::DbusConnection)?
            .build()
            .await
            .map_err(NetError::DbusConnection)?;

        let device_name  = peer.device_name().await.unwrap_or_else(|_| "Unknown".into());
        let addr_bytes   = peer.device_address().await.unwrap_or_default();
        let device_addr  = Self::mac_to_string(&addr_bytes);
        let signal_level = peer.level().await.ok();
        let ies          = peer.ies().await.unwrap_or_default();
        let wfd_capable  = !ies.is_empty();

        Ok(P2pDevice {
            object_path: path.to_string(),
            device_name,
            device_addr,
            signal_level,
            wfd_capable,
        })
    }

    /// Tenta obter o proxy da interface para P2PFind ativo.
    /// Retorna `None` sem erro se não for possível — o scan continua passivo.
    async fn try_get_interface_proxy<'c>(
        conn: &'c Connection,
        wpa: &WpaSupplicantProxy<'c>,
        iface_name: &str,
    ) -> Option<WpaInterfaceProxy<'c>> {
        let path = match wpa.get_interface(iface_name).await {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    iface = %iface_name,
                    "GetInterface falhou (NetworkManager pode estar gerenciando): {e}\n\
                     Continuando em modo passivo."
                );
                return None;
            }
        };

        WpaInterfaceProxy::builder(conn)
            .path(path)
            .ok()?
            .build()
            .await
            .ok()
    }

    /// Coleta peers atualmente conhecidos via ObjectManager.
    /// Esta é a fonte **primária** de peers — não depende de GetInterface.
    async fn collect_peers(
        conn: &Connection,
        tx: &mpsc::Sender<DeviceEvent>,
    ) -> NetResult<()> {
        let object_manager = zbus::fdo::ObjectManagerProxy::builder(conn)
            .destination("fi.w1.wpa_supplicant1")
            .map_err(NetError::DbusConnection)?
            .path("/fi/w1/wpa_supplicant1")
            .map_err(NetError::DbusConnection)?
            .build()
            .await
            .map_err(NetError::DbusConnection)?;

        let managed = object_manager
            .get_managed_objects()
            .await
            .map_err(|e| NetError::DbusConnection(e.into()))?;

        for (path, interfaces) in &managed {
            if interfaces.contains_key("fi.w1.wpa_supplicant1.Peer") {
                match Self::build_device(conn, &path.as_ref()).await {
                    Ok(dev) => {
                        debug!(
                            name = %dev.device_name,
                            addr = %dev.device_addr,
                            wfd  = dev.wfd_capable,
                            "Peer P2P descoberto"
                        );
                        tx.send(DeviceEvent::Discovered(dev))
                            .await
                            .map_err(|_| NetError::ChannelClosed)?;
                    }
                    Err(e) => warn!("Não foi possível construir device para {path}: {e}"),
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl DeviceScanner for WpaSupplicantScanner {
    async fn scan(&self, tx: mpsc::Sender<DeviceEvent>) -> NetResult<()> {
        let iface_name = match &self.interface {
            Some(name) => name.clone(),
            None => Self::detect_p2p_interface().await?,
        };

        info!(iface = %iface_name, "Iniciando scanner P2P");

        let conn = Connection::system()
            .await
            .map_err(|_| NetError::WpaNotRunning)?;

        let wpa = WpaSupplicantProxy::new(&conn)
            .await
            .map_err(|_| NetError::WpaNotRunning)?;

        // GetInterface é opcional — não bloqueia o scanner se falhar.
        let iface_proxy = Self::try_get_interface_proxy(&conn, &wpa, &iface_name).await;

        let mode = if iface_proxy.is_some() {
            ScanMode::Active
        } else {
            ScanMode::Passive
        };

        info!("Modo de scan: {mode}");
        tx.send(DeviceEvent::ScanMode(mode))
            .await
            .map_err(|_| NetError::ChannelClosed)?;

        loop {
            // ── Scan ativo (melhor) ──────────────────────────────────────────
            if let Some(ref iface) = iface_proxy {
                match iface.p2_p_find(self.scan_timeout_secs, HashMap::new()).await {
                    Ok(()) => {
                        debug!(timeout = self.scan_timeout_secs, "P2PFind iniciado");
                        tokio::time::sleep(std::time::Duration::from_secs(
                            self.scan_timeout_secs as u64,
                        ))
                        .await;
                    }
                    Err(e) => {
                        warn!("P2PFind falhou, retornando ao modo passivo: {e}");
                        // Não encerra — continua com coleta passiva.
                    }
                }
            } else {
                // ── Modo passivo: aguarda intervalo e coleta ─────────────────
                tokio::time::sleep(std::time::Duration::from_secs(
                    self.scan_timeout_secs as u64,
                ))
                .await;
            }

            // ── Coleta via ObjectManager (sempre, independente do modo) ──────
            Self::collect_peers(&conn, &tx).await?;

            tx.send(DeviceEvent::ScanComplete)
                .await
                .map_err(|_| NetError::ChannelClosed)?;

            info!(mode = %mode, "Sweep completo, reiniciando");
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Testes
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wifi_interfaces_from_proc() {
        let ifaces = wifi_interfaces_from_proc();
        println!("Wi-Fi interfaces: {ifaces:?}");
        for name in &ifaces {
            assert!(!name.is_empty());
            assert!(!name.contains(':'));
        }
    }

    #[test]
    fn test_wpa_socket_interfaces() {
        let sockets = wpa_socket_interfaces();
        println!("Sockets wpa_supplicant: {sockets:?}");
        // Não falha se o diretório não existe (CI headless)
        for s in &sockets {
            assert!(!s.is_empty());
            assert_ne!(s, "global");
        }
    }

    #[tokio::test]
    async fn test_p2p_interface_exists() {
        match WpaSupplicantScanner::detect_p2p_interface().await {
            Ok(name) => {
                println!("Interface P2P detectada: {name}");
                assert!(!name.is_empty());
            }
            Err(NetError::WpaNotRunning) => {
                eprintln!("wpa_supplicant não rodando (esperado em CI)");
            }
            Err(NetError::NoP2pInterface { tried }) => {
                eprintln!("Nenhuma interface P2P entre: {tried:?}");
            }
            Err(e) => panic!("Erro inesperado: {e}"),
        }
    }

    #[tokio::test]
    async fn test_scanner_channel_types() {
        let scanner = WpaSupplicantScanner::auto();
        let (tx, mut rx) = mpsc::channel::<DeviceEvent>(32);
        tx.send(DeviceEvent::ScanComplete).await.unwrap();
        assert!(matches!(rx.recv().await.unwrap(), DeviceEvent::ScanComplete));
        assert_eq!(scanner.scan_timeout_secs, 30);
        assert!(scanner.interface.is_none());
    }

    #[test]
    fn test_mac_to_string() {
        let bytes = [0x00u8, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e];
        assert_eq!(WpaSupplicantScanner::mac_to_string(&bytes), "00:1a:2b:3c:4d:5e");
    }

    #[test]
    fn test_p2p_device_serde() {
        let dev = P2pDevice {
            object_path: "/fi/w1/wpa_supplicant1/Peers/0".into(),
            device_name: "TestTV".into(),
            device_addr: "aa:bb:cc:dd:ee:ff".into(),
            signal_level: Some(-65),
            wfd_capable: true,
        };
        let json = serde_json::to_string(&dev).unwrap();
        let back: P2pDevice = serde_json::from_str(&json).unwrap();
        assert_eq!(dev, back);
    }

    #[test]
    fn test_scan_mode_display() {
        assert_eq!(ScanMode::Active.to_string(),  "ativo (P2PFind via D-Bus)");
        assert_eq!(ScanMode::Passive.to_string(), "passivo (ObjectManager apenas)");
    }
}