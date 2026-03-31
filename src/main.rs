//! # lnxcast
//!
//! Miracast / RTSP streaming daemon for Linux.
//!
//! ## Usage
//!
//!   ./lnxcast                     # auto-detect P2P interface
//!   LNXCAST_IFACE=p2p-dev-wlan0 ./lnxcast
//!   RUST_LOG=debug ./lnxcast
//!
//! ## Do NOT run with sudo
//!
//! The XDG Desktop Portal (screen capture) runs inside your Wayland session
//! as your normal user.  Running with `sudo` switches to a different D-Bus
//! environment where the portal is not present.  Always run lnxcast as the
//! desktop user.  wpa_supplicant is accessible via the system D-Bus without
//! elevated privileges on all major distributions.

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use capture::{CaptureBackend, CaptureOptions, ScreenCapturer};
use media::{MediaPipeline, PipelineConfig};
use net::{DeviceEvent, DeviceScanner, NetError, WpaSupplicantScanner};
use server::{RtspServer, ServerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ─────────────────────────────────────────────────────────────
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("lnxcast starting");

    // ── Sudo guard ───────────────────────────────────────────────────────────
    // Detect if running as root and warn loudly – the portal will not be
    // available and the P2P scan does not need elevated privileges either.
    if unsafe { libc::geteuid() } == 0 {
        anyhow::bail!(
            "lnxcast must NOT be run with sudo / as root.\n\
             The XDG Desktop Portal for screen capture runs in your Wayland \
             session as your normal user and is not accessible to root.\n\
             wpa_supplicant is reachable via the system D-Bus without elevated \
             privileges.  Run: ./lnxcast"
        );
    }

    // ── 1. Detect capture back-end ──────────────────────────────────────────
    let backend = CaptureBackend::detect().await;
    if !backend.is_available() {
        anyhow::bail!(
            "No screen-capture back-end available.\n\
             Ensure xdg-desktop-portal (and a compositor-specific back-end \
             such as xdg-desktop-portal-gnome or xdg-desktop-portal-wlr) \
             is running inside your Wayland session.\n\
             Hint: check with: systemctl --user status xdg-desktop-portal"
        );
    }
    info!("Capture backend: {backend:?}");

    // ── 2. Acquire PipeWire stream ───────────────────────────────────────────
    let capturer = ScreenCapturer::new(CaptureOptions::default());
    // `stream` must stay alive for the whole pipeline lifetime – it owns the
    // OwnedFd that keeps the portal PipeWire connection open.
    let stream = capturer
        .start()
        .await
        .context("Failed to open PipeWire capture stream")?;

    let node_id = stream.node_id();
    let pw_fd   = stream.as_raw_fd();
    info!(node_id, pw_fd, "PipeWire stream ready");

    // ── 3. Build GStreamer pipeline ──────────────────────────────────────────
    let pipeline_cfg = PipelineConfig {
        pipewire_fd: pw_fd,
        pipewire_node_id: node_id,
        bitrate_kbps: 4_000,
        framerate: 30,
        ..Default::default()
    };

    let media_pipeline = MediaPipeline::build(&pipeline_cfg)
        .context("Failed to build GStreamer pipeline")?;

    info!(encoder = %media_pipeline.encoder(), "Media pipeline ready");
    media_pipeline.play().context("Failed to start pipeline")?;

    // ── 4. Start P2P scanner in background ──────────────────────────────────
    // Auto-detect the p2p-dev-* interface, or use LNXCAST_IFACE env var.
    let p2p_iface = if let Ok(iface) = std::env::var("LNXCAST_IFACE") {
        info!(iface = %iface, "Using P2P interface from LNXCAST_IFACE");
        iface
    } else {
        match WpaSupplicantScanner::detect_p2p_interface().await {
            Ok(iface) => iface,
            Err(NetError::WpaNotRunning) => {
                warn!(
                    "wpa_supplicant is not running – P2P discovery disabled.\n\
                     Start it with: sudo systemctl start wpa_supplicant"
                );
                // Continue without P2P scanning; RTSP still works.
                run_server_only(stream, media_pipeline).await?;
                return Ok(());
            }
            Err(NetError::NoP2pInterface { ref tried }) => {
                warn!(
                    ?tried,
                    "No p2p-dev-* interface found in wpa_supplicant.\n\
                     Enable P2P on your adapter:\n\
                       sudo iw dev <wlan> interface add p2p-dev-<wlan> type __p2p_device\n\
                     Or set LNXCAST_IFACE=p2p-dev-<wlan> manually."
                );
                run_server_only(stream, media_pipeline).await?;
                return Ok(());
            }
            Err(e) => {
                warn!("P2P interface detection failed: {e} – continuing without P2P");
                run_server_only(stream, media_pipeline).await?;
                return Ok(());
            }
        }
    };

    let (event_tx, mut event_rx) = mpsc::channel::<DeviceEvent>(64);
    let scanner = WpaSupplicantScanner::new(p2p_iface);

    tokio::spawn(async move {
        if let Err(e) = scanner.scan(event_tx).await {
            error!("P2P scanner stopped: {e}");
        }
    });

    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            match event {
                DeviceEvent::Discovered(dev) => info!(
                    name = %dev.device_name,
                    addr = %dev.device_addr,
                    wfd  = dev.wfd_capable,
                    "P2P sink discovered"
                ),
                DeviceEvent::Lost { object_path } => info!(%object_path, "P2P sink lost"),
                DeviceEvent::ScanComplete => info!("P2P scan sweep complete"),
                DeviceEvent::Error(e) => error!("P2P error: {e}"),
            }
        }
    });

    // ── 5. Launch RTSP server ────────────────────────────────────────────────
    run_server_only(stream, media_pipeline).await
}

/// Run only the RTSP server (no P2P scanning).
/// `_stream` is kept alive here so the PipeWire fd is not closed.
async fn run_server_only(
    _stream: capture::CaptureStream,
    media_pipeline: MediaPipeline,
) -> Result<()> {
    let server_cfg = ServerConfig::default(); // port 7236, /live
    let server = RtspServer::new(server_cfg).context("Failed to create RTSP server")?;

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("SIGINT received – shutting down");
        let _ = stop_tx.send(());
    });

    server
        .run_with_shutdown(stop_rx)
        .await
        .context("RTSP server error")?;

    media_pipeline.stop().ok();
    info!("lnxcast stopped");
    Ok(())
}