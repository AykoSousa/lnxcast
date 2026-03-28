//! # lnxcast
//!
//! Miracast / RTSP streaming daemon for Linux.
//!
//! Orchestrates the four sub-crates:
//!  - `net`     – P2P device discovery
//!  - `capture` – screen capture via PipeWire portal
//!  - `media`   – GStreamer H.264 pipeline
//!  - `lnxcast-server`  – RTSP server on port 7236
//!
//! Usage:
//!   lnxcast [--iface <wlan>] [--bitrate <kbps>] [--port <port>]

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};

use capture::{CaptureBackend, CaptureOptions, ScreenCapturer};
use media::{MediaPipeline, PipelineConfig};
use net::{DeviceEvent, DeviceScanner, WpaSupplicantScanner};
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

    // ── 1. Detect capture back-end ──────────────────────────────────────────
    let backend = CaptureBackend::detect().await;
    if !backend.is_available() {
        anyhow::bail!(
            "No screen-capture back-end available. \
             Ensure xdg-desktop-portal is running on a Wayland session."
        );
    }
    info!("Capture backend: {backend:?}");

    // ── 2. Acquire PipeWire stream ───────────────────────────────────────────
    let capturer = ScreenCapturer::new(CaptureOptions::default());
    // `stream` must stay alive until the pipeline is stopped: it owns the
    // OwnedFd that keeps the PipeWire portal connection open.
    let stream = capturer
        .start()
        .await
        .context("Failed to open PipeWire capture stream")?;

    let node_id = stream.node_id();
    let pw_fd   = stream.as_raw_fd();
    info!(node_id, pw_fd, "PipeWire stream ready");

    // ── 3. Build GStreamer pipeline ──────────────────────────────────────────
    let pipeline_cfg = PipelineConfig {
        pipewire_fd: pw_fd,      // portal fd – required for node lookup
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
    let (event_tx, mut event_rx) = mpsc::channel::<DeviceEvent>(64);
    let scanner = WpaSupplicantScanner::new("wlx0cc65541a4b7");

    tokio::spawn(async move {
        if let Err(e) = scanner.scan(event_tx).await {
            error!("P2P scanner error: {e}");
        }
    });

    // Log discovered devices in background.
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            match event {
                DeviceEvent::Discovered(dev) => {
                    info!(
                        name  = %dev.device_name,
                        addr  = %dev.device_addr,
                        wfd   = dev.wfd_capable,
                        "P2P sink discovered"
                    );
                }
                DeviceEvent::Lost { object_path } => {
                    info!(%object_path, "P2P sink lost");
                }
                DeviceEvent::ScanComplete => info!("P2P scan sweep complete"),
                DeviceEvent::Error(e) => error!("P2P error: {e}"),
            }
        }
    });

    // ── 5. Launch RTSP server ────────────────────────────────────────────────
    let server_cfg = ServerConfig::default(); // port 7236 / /live
    let server = RtspServer::new(server_cfg).context("Failed to create RTSP server")?;

    // Install Ctrl-C handler.
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