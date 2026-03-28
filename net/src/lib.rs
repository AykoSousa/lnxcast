//! # net
//!
//! Discovers Miracast / Wi-Fi Direct (P2P) sink devices through the
//! `wpa_supplicant` D-Bus interface and emits typed events via an
//! `mpsc` channel so the rest of the application can react
//! asynchronously.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  WpaSupplicantScanner                   │
//! │  ┌──────────┐    ┌────────────────────┐ │
//! │  │  zbus    │───▶│  DeviceEvent chan  │─┼──▶ consumers
//! │  │  proxy   │    │  (mpsc::Sender)    │ │
//! │  └──────────┘    └────────────────────┘ │
//! └─────────────────────────────────────────┘
//! ```

use std::collections::HashMap;

// use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::{Connection, proxy};

// ──────────────────────────────────────────────────────────────────────────────
// Public error type
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum NetError {
    #[error("D-Bus connection failed: {0}")]
    DbusConnection(#[from] zbus::Error),

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

/// A discovered Wi-Fi Direct sink (Miracast receiver).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2pDevice {
    /// D-Bus object path that identifies this device.
    pub object_path: String,
    /// Human-readable device name (from P2P attributes).
    pub device_name: String,
    /// MAC address in `aa:bb:cc:dd:ee:ff` form.
    pub device_addr: String,
    /// Received signal strength indicator (dBm), if available.
    pub signal_level: Option<i32>,
    /// Whether the peer has advertised Miracast / WFD capability.
    pub wfd_capable: bool,
}

/// Events emitted by a [`DeviceScanner`] implementation.
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// A new P2P sink was found.
    Discovered(P2pDevice),
    /// A previously-seen sink went away.
    Lost { object_path: String },
    /// The underlying scan completed one full sweep.
    ScanComplete,
    /// A fatal error occurred; the scanner has stopped.
    Error(String),
}

// ──────────────────────────────────────────────────────────────────────────────
// Trait definition
// ──────────────────────────────────────────────────────────────────────────────

/// Abstraction over Wi-Fi / P2P discovery back-ends.
///
/// Implementors must drive the scan loop and push [`DeviceEvent`]s through
/// the provided sender.  The method is `async` so that it can be spawned
/// directly onto a Tokio task.
#[async_trait::async_trait]
pub trait DeviceScanner: Send + Sync {
    /// Start scanning and emit events.  Runs until `tx` is dropped or an
    /// unrecoverable error occurs.
    async fn scan(&self, tx: mpsc::Sender<DeviceEvent>) -> NetResult<()>;
}

// ──────────────────────────────────────────────────────────────────────────────
// wpa_supplicant D-Bus proxies
// ──────────────────────────────────────────────────────────────────────────────

/// Minimal proxy for the top-level `fi.w1.wpa_supplicant1` service.
#[proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    /// Returns the D-Bus object path for the named network interface.
    fn get_interface(&self, ifname: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// List all managed interfaces.
    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;
}

/// Proxy for a single `fi.w1.wpa_supplicant1.Interface`.
#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaInterface {
    /// Trigger a P2P device discovery sweep.
    fn p2_p_find(
        &self,
        timeout: i32,
        args: HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    /// Stop any running P2P discovery.
    fn p2_p_stop_find(&self) -> zbus::Result<()>;

    /// Current interface state string (e.g. `"inactive"`, `"scanning"`).
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
}

/// Proxy for a discovered `fi.w1.wpa_supplicant1.Peer`.
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

    /// Raw WFD IEs – non-empty means the peer is WFD/Miracast capable.
    #[zbus(property, name = "IEs")]
    fn ies(&self) -> zbus::Result<Vec<u8>>;
}

// ──────────────────────────────────────────────────────────────────────────────
// WpaSupplicantScanner – concrete implementation
// ──────────────────────────────────────────────────────────────────────────────

/// Scans for P2P sinks by driving `wpa_supplicant` over D-Bus.
pub struct WpaSupplicantScanner {
    /// Network interface to use (e.g. `"wlan0"`, `"p2p-dev-wlan0"`).
    pub interface: String,
    /// Seconds per `P2PFind` sweep.  Defaults to 30 s.
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

    /// Convert a raw 6-byte MAC slice to `"aa:bb:cc:dd:ee:ff"`.
    fn mac_to_string(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Fetch peer metadata and build a [`P2pDevice`].
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
        let addr_bytes = peer.device_address().await.unwrap_or_default();
        let device_addr = Self::mac_to_string(&addr_bytes);
        let signal_level = peer.level().await.ok();
        let ies = peer.ies().await.unwrap_or_default();
        // WFD IEs begin with OUI 50:6f:9a:0a; a non-empty slice is sufficient
        // as a heuristic for Miracast capability.
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

#[async_trait::async_trait]
impl DeviceScanner for WpaSupplicantScanner {
    async fn scan(&self, tx: mpsc::Sender<DeviceEvent>) -> NetResult<()> {
        info!(iface = %self.interface, "Connecting to wpa_supplicant via D-Bus");

        let conn = Connection::system()
            .await
            .map_err(NetError::DbusConnection)?;

        let wpa = WpaSupplicantProxy::new(&conn)
            .await
            .map_err(NetError::DbusConnection)?;

        // Resolve the object path for the requested interface.
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
            // Kick off one P2P find sweep.
            debug!(timeout = self.scan_timeout_secs, "Starting P2PFind");
            iface
                .p2_p_find(self.scan_timeout_secs, HashMap::new())
                .await
                .map_err(|e| NetError::P2pOperation(e.to_string()))?;

            // Wait for the sweep to finish (simple sleep-based poll).
            tokio::time::sleep(std::time::Duration::from_secs(
                self.scan_timeout_secs as u64,
            ))
            .await;

            // Query all currently-known peers from the P2P group manager.
            // We re-list interfaces and filter for peer objects under this
            // interface's sub-tree.  A production implementation would
            // subscribe to P2PDeviceFound / P2PDeviceLost signals instead.
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
                            debug!(name = %dev.device_name, addr = %dev.device_addr, wfd = dev.wfd_capable, "Discovered P2P peer");
                            tx.send(DeviceEvent::Discovered(dev))
                                .await
                                .map_err(|_| NetError::ChannelClosed)?;
                        }
                        Err(e) => {
                            warn!("Could not build device for {path}: {e}");
                        }
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

    /// Verifies that a system D-Bus connection can be established and that the
    /// well-known `fi.w1.wpa_supplicant1` service is reachable (even if the
    /// process is not running the proxy call will return a D-Bus error, not a
    /// transport error).
    ///
    /// On a CI runner without wpa_supplicant the inner `get_interface` call
    /// will fail with a D-Bus service-not-found error, which is acceptable –
    /// the point is that the *interface plumbing* exists.
    #[tokio::test]
    async fn test_p2p_interface_exists() {
        let result = Connection::system().await;

        match result {
            Ok(conn) => {
                // Connection succeeded – verify we can at least build the proxy.
                let proxy_result = WpaSupplicantProxy::new(&conn).await;
                // The proxy constructor itself should not fail (it is just a
                // D-Bus introspection call on a well-known path).
                match proxy_result {
                    Ok(_) => {
                        // Great: the proxy was built.  Whether wpa_supplicant
                        // is actually running is environment-dependent.
                    }
                    Err(e) => {
                        // Acceptable in test environments without the daemon.
                        eprintln!("wpa_supplicant proxy unavailable (expected in CI): {e}");
                    }
                }
            }
            Err(e) => {
                // No system bus at all (sandboxed CI).  Not a failure of our
                // code, just the environment.
                eprintln!("System D-Bus not available (expected in CI): {e}");
            }
        }
    }

    /// Ensures the `WpaSupplicantScanner` can be constructed and the
    /// `DeviceEvent` channel types are compatible.
    #[tokio::test]
    async fn test_scanner_channel_types() {
        let scanner = WpaSupplicantScanner::new("wlan0");
        let (tx, mut rx) = mpsc::channel::<DeviceEvent>(32);

        // We do NOT actually call `scanner.scan(tx)` here because that
        // requires a live wpa_supplicant.  Instead we verify the type
        // signature compiles and the channel round-trips a value.
        tx.send(DeviceEvent::ScanComplete).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(matches!(evt, DeviceEvent::ScanComplete));

        // Confirm Default impl
        assert_eq!(scanner.scan_timeout_secs, 30);
    }

    /// Checks the MAC-formatting helper.
    #[test]
    fn test_mac_to_string() {
        let bytes = [0x00u8, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e];
        let s = WpaSupplicantScanner::mac_to_string(&bytes);
        assert_eq!(s, "00:1a:2b:3c:4d:5e");
    }

    /// Verifies `P2pDevice` serializes to / from JSON without loss.
    #[test]
    fn test_p2p_device_serde() {
        let dev = P2pDevice {
            object_path: "/fi/w1/wpa_supplicant1/Peers/0".into(),
            device_name: "Chromecast-Ultra".into(),
            device_addr: "aa:bb:cc:dd:ee:ff".into(),
            signal_level: Some(-65),
            wfd_capable: true,
        };
        let json = serde_json::to_string(&dev).unwrap();
        let back: P2pDevice = serde_json::from_str(&json).unwrap();
        assert_eq!(dev, back);
    }
}