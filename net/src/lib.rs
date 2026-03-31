//! # lnxcast-net
//!
//! Discovers Miracast / Wi-Fi Direct (P2P) sink devices through the
//! `wpa_supplicant` D-Bus interface and emits typed events via an
//! `mpsc` channel.
//!
//! ## P2P interface naming
//!
//! wpa_supplicant exposes P2P on a *dedicated virtual interface* whose
//! name is `p2p-dev-<parent>`.  Given adapter `wlx0cc65541a4b7` the P2P
//! interface is `p2p-dev-wlx0cc65541a4b7`.
//!
//! [`WpaSupplicantScanner::detect_p2p_interface`] discovers this name
//! automatically without requiring elevated D-Bus privileges:
//!
//!  1. Read `/proc/net/wireless` to list kernel-known Wi-Fi interfaces.
//!  2. For each, call `wpa_supplicant GetInterface("p2p-dev-<name>")` —
//!     this call is allowed for regular users unlike reading `Interfaces`.
//!  3. Return the first one that succeeds.
//!
//! ## Permissions
//!
//! Run lnxcast as the **normal desktop user**, never with `sudo`.
//! wpa_supplicant's `GetInterface` method is accessible without elevated
//! privileges on all major distributions.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::{proxy, Connection};

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum NetError {
    #[error("D-Bus connection failed: {0}")]
    DbusConnection(#[from] zbus::Error),

    #[error("wpa_supplicant not running or not reachable on system D-Bus")]
    WpaNotRunning,

    #[error("No P2P-capable interface found (tried: {tried:?})")]
    NoP2pInterface { tried: Vec<String> },

    #[error("wpa_supplicant interface not found: {0}")]
    InterfaceNotFound(String),

    #[error("P2P operation failed: {0}")]
    P2pOperation(String),

    #[error("Channel send error")]
    ChannelClosed,

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type NetResult<T> = Result<T, NetError>;

// ──────────────────────────────────────────────────────────────────────────────
// Domain types
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
    Error(String),
}

// ──────────────────────────────────────────────────────────────────────────────
// Trait
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait DeviceScanner: Send + Sync {
    async fn scan(&self, tx: mpsc::Sender<DeviceEvent>) -> NetResult<()>;
}

// ──────────────────────────────────────────────────────────────────────────────
// D-Bus proxies
// ──────────────────────────────────────────────────────────────────────────────

#[proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    /// This call is permitted for regular users (no polkit required).
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
// Interface discovery helpers (no elevated privileges required)
// ──────────────────────────────────────────────────────────────────────────────

/// Return all Wi-Fi interface names visible to the kernel.
///
/// Reads `/proc/net/wireless` which is world-readable on all Linux systems.
/// Falls back to parsing `ip link` output if the file is unavailable.
fn wifi_interfaces_from_proc() -> Vec<String> {
    // /proc/net/wireless format:
    //   Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
    //    face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
    //   wlan0: 0000   70.  -40.  -256.       0      0      0      0      0        0
    let Ok(content) = std::fs::read_to_string("/proc/net/wireless") else {
        return wifi_interfaces_from_ip_link();
    };

    content
        .lines()
        .skip(2) // header lines
        .filter_map(|line| {
            let name = line.split(':').next()?.trim().to_owned();
            if name.is_empty() { None } else { Some(name) }
        })
        .collect()
}

/// Fallback: parse `ip link show` for interfaces of type `ether` or `wifi`.
fn wifi_interfaces_from_ip_link() -> Vec<String> {
    let Ok(out) = std::process::Command::new("ip")
        .args(["link", "show"])
        .output()
    else {
        return Vec::new();
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut names = Vec::new();
    for line in stdout.lines() {
        // Lines like: "2: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> ..."
        if let Some(rest) = line.trim().strip_prefix(|c: char| c.is_ascii_digit()) {
            let rest = rest.trim_start_matches(": ").trim_start_matches(|c: char| c.is_ascii_digit());
            if let Some(name) = rest.split(':').next().map(str::trim) {
                // Heuristic: Wi-Fi adapters typically start with wl
                if name.starts_with("wl") {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names
}

// ──────────────────────────────────────────────────────────────────────────────
// WpaSupplicantScanner
// ──────────────────────────────────────────────────────────────────────────────

pub struct WpaSupplicantScanner {
    pub interface: String,
    pub scan_timeout_secs: i32,
}

impl Default for WpaSupplicantScanner {
    fn default() -> Self {
        Self {
            interface: "p2p-dev-wlan0".to_owned(),
            scan_timeout_secs: 30,
        }
    }
}

impl WpaSupplicantScanner {
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            ..Default::default()
        }
    }

    /// Discover the P2P device interface name without elevated privileges.
    ///
    /// Strategy:
    /// 1. Enumerate Wi-Fi adapters from `/proc/net/wireless`.
    /// 2. For each adapter `wlX`, call `GetInterface("p2p-dev-wlX")` on
    ///    wpa_supplicant — this call is permitted for regular users.
    /// 3. Return the first name that wpa_supplicant confirms it manages.
    ///
    /// If wpa_supplicant is not reachable at all → `Err(WpaNotRunning)`.
    /// If no P2P interface is confirmed → `Err(NoP2pInterface)`.
    pub async fn detect_p2p_interface() -> NetResult<String> {
        // Connect to the system bus first to verify wpa_supplicant is up.
        let conn = Connection::system().await.map_err(|e| {
            debug!("system D-Bus unavailable: {e}");
            NetError::WpaNotRunning
        })?;

        let wpa = WpaSupplicantProxy::new(&conn).await.map_err(|e| {
            debug!("wpa_supplicant proxy failed: {e}");
            NetError::WpaNotRunning
        })?;

        // Verify the service is responding with a cheap no-op call.
        // GetInterface with a dummy name will return an error from the daemon
        // (not a transport error), which proves it is running.
        let daemon_alive = wpa.get_interface("__probe__").await;
        match daemon_alive {
            Err(zbus::Error::MethodError(..)) => {
                // Expected: daemon replied "interface not found" → it IS running.
            }
            Err(e) => {
                // Transport / service-not-found error → daemon not running.
                debug!("wpa_supplicant probe failed: {e}");
                return Err(NetError::WpaNotRunning);
            }
            Ok(_) => {
                // Unlikely but harmless – "__probe__" was somehow found.
            }
        }

        // Enumerate Wi-Fi adapters from proc and try p2p-dev-<name> for each.
        let wifi_ifaces = wifi_interfaces_from_proc();
        debug!(ifaces = ?wifi_ifaces, "Wi-Fi interfaces from /proc/net/wireless");

        let mut tried: Vec<String> = Vec::new();

        for parent in &wifi_ifaces {
            let candidate = format!("p2p-dev-{parent}");
            tried.push(candidate.clone());

            match wpa.get_interface(&candidate).await {
                Ok(_path) => {
                    info!(iface = %candidate, "Auto-detected P2P interface");
                    return Ok(candidate);
                }
                Err(zbus::Error::MethodError(name, ..))
                    if name.as_str().contains("InterfaceUnknown") =>
                {
                    debug!(candidate = %candidate, "Not managed by wpa_supplicant, trying next");
                }
                Err(e) => {
                    debug!(candidate = %candidate, "GetInterface error: {e}");
                }
            }
        }

        // Last resort: try the bare Wi-Fi name itself (some configs use it).
        for parent in &wifi_ifaces {
            tried.push(parent.clone());
            if let Ok(_) = wpa.get_interface(parent).await {
                warn!(
                    iface = %parent,
                    "wpa_supplicant manages the Wi-Fi interface directly (no p2p-dev-*). \
                     P2P discovery requires the p2p-dev-* interface to be created."
                );
                // Return the derived name anyway; the scan() call will report
                // InterfaceNotFound if it truly does not exist.
                let derived = format!("p2p-dev-{parent}");
                return Ok(derived);
            }
        }

        Err(NetError::NoP2pInterface { tried })
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

        let device_name = peer.device_name().await.unwrap_or_else(|_| "Unknown".into());
        let addr_bytes  = peer.device_address().await.unwrap_or_default();
        let device_addr = Self::mac_to_string(&addr_bytes);
        let signal_level = peer.level().await.ok();
        let ies = peer.ies().await.unwrap_or_default();
        let wfd_capable = !ies.is_empty();

        Ok(P2pDevice {
            object_path: path.to_string(),
            device_name,
            device_addr,
            signal_level,
            wfd_capable,
        })
    }
}

#[async_trait]
impl DeviceScanner for WpaSupplicantScanner {
    async fn scan(&self, tx: mpsc::Sender<DeviceEvent>) -> NetResult<()> {
        info!(iface = %self.interface, "Connecting to wpa_supplicant via D-Bus");

        let conn = Connection::system()
            .await
            .map_err(|_| NetError::WpaNotRunning)?;

        let wpa = WpaSupplicantProxy::new(&conn)
            .await
            .map_err(|_| NetError::WpaNotRunning)?;

        let iface_path = wpa
            .get_interface(&self.interface)
            .await
            .map_err(|_| NetError::InterfaceNotFound(self.interface.clone()))?;

        info!(path = %iface_path, "Obtained wpa_supplicant interface");

        let iface = WpaInterfaceProxy::builder(&conn)
            .path(iface_path)
            .map_err(NetError::DbusConnection)?
            .build()
            .await
            .map_err(NetError::DbusConnection)?;

        loop {
            debug!(timeout = self.scan_timeout_secs, "Starting P2PFind");
            iface
                .p2_p_find(self.scan_timeout_secs, HashMap::new())
                .await
                .map_err(|e| NetError::P2pOperation(e.to_string()))?;

            tokio::time::sleep(std::time::Duration::from_secs(
                self.scan_timeout_secs as u64,
            ))
            .await;

            let object_manager = zbus::fdo::ObjectManagerProxy::builder(&conn)
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
                    match Self::build_device(&conn, &path.as_ref()).await {
                        Ok(dev) => {
                            debug!(
                                name = %dev.device_name,
                                addr = %dev.device_addr,
                                wfd  = dev.wfd_capable,
                                "Discovered P2P peer"
                            );
                            tx.send(DeviceEvent::Discovered(dev))
                                .await
                                .map_err(|_| NetError::ChannelClosed)?;
                        }
                        Err(e) => warn!("Could not build device for {path}: {e}"),
                    }
                }
            }

            tx.send(DeviceEvent::ScanComplete)
                .await
                .map_err(|_| NetError::ChannelClosed)?;

            info!("Scan sweep complete, restarting");
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wifi_interfaces_from_proc() {
        // Just verifies the function doesn't panic; list may be empty in CI.
        let ifaces = wifi_interfaces_from_proc();
        println!("Wi-Fi interfaces: {ifaces:?}");
        for name in &ifaces {
            assert!(!name.is_empty());
            assert!(!name.contains(':'));
        }
    }

    #[tokio::test]
    async fn test_p2p_interface_exists() {
        match WpaSupplicantScanner::detect_p2p_interface().await {
            Ok(name) => {
                println!("P2P interface detected: {name}");
                assert!(!name.is_empty());
            }
            Err(NetError::WpaNotRunning) => {
                eprintln!("wpa_supplicant not running (expected in CI)");
            }
            Err(NetError::NoP2pInterface { tried }) => {
                eprintln!("No P2P interface among: {tried:?}");
            }
            Err(e) => panic!("Unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn test_scanner_channel_types() {
        let scanner = WpaSupplicantScanner::new("p2p-dev-wlan0");
        let (tx, mut rx) = mpsc::channel::<DeviceEvent>(32);
        tx.send(DeviceEvent::ScanComplete).await.unwrap();
        assert!(matches!(rx.recv().await.unwrap(), DeviceEvent::ScanComplete));
        assert_eq!(scanner.scan_timeout_secs, 30);
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
}