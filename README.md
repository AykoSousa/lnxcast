# lnxcast

> **Miracast / RTSP screen-casting daemon for Linux**, written in Rust 2024.

`lnxcast` captures your Wayland desktop via PipeWire, encodes it to H.264
(hardware via VA-API when possible, software x264 as fallback) and serves the
stream over RTSP on port **7236** so any Miracast-compatible sink (TV, dongle,
projector) can connect.

```
rtsp://<your-ip>:7236/live
```

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                         lnxcast                              │
│                                                              │
│  ┌─────────────┐   DeviceEvent   ┌──────────────────────┐   │
│  │     net     │ ──────mpsc────▶ │   main event loop    │   │
│  │ (P2P scan)  │                 │   (tokio runtime)    │   │
│  └─────────────┘                 └──────────┬───────────┘   │
│                                             │               │
│  ┌─────────────────┐   PipeWire FD          │               │
│  │     capture     │ ──────────────────────▶│               │
│  │ (XDG Portal)    │                        │               │
│  └─────────────────┘                        │               │
│                                             ▼               │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                     media                            │
│  │  pipewiresrc ▶ videoconvert ▶ [vaapih264enc|x264enc] │   │
│  │              ▶ h264parse ▶ rtph264pay ▶ appsink      │   │
│  └──────────────────────────────┬───────────────────────┘   │
│                                 │ GstPipeline               │
│  ┌──────────────────────────────▼───────────────────────┐   │
│  │                     server                       │   │
│  │   gst-rtsp-server  :7236/live                        │   │
│  └──────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────┘
```

---

## Workspace layout

```
lnxcast/
├── Cargo.toml           ← workspace root + binary crate
├── src/
│   └── main.rs          ← orchestrator binary
├── net/         ← P2P device discovery (wpa_supplicant / D-Bus)
│   └── src/lib.rs
├── capture/     ← Screen capture (ashpd / PipeWire portal)
│   └── src/lib.rs
├── media/       ← GStreamer pipeline builder
│   └── src/lib.rs
└── server/      ← RTSP server (gst-rtsp-server)
    └── src/lib.rs
```

---

## Prerequisites

### System packages (Debian / Ubuntu)

```bash
sudo apt install -y \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly \
  gstreamer1.0-rtsp \
  libgstrtspserver-1.0-dev \
  gstreamer1.0-vaapi \
  libpipewire-0.3-dev \
  xdg-desktop-portal \
  xdg-desktop-portal-gnome   # or -wlr / -kde
```

### System packages (Arch / Manjaro)

```bash
sudo pacman -S \
  gstreamer gst-plugins-base gst-plugins-good \
  gst-plugins-bad gst-plugins-ugly gst-rtsp-server \
  gstreamer-vaapi pipewire xdg-desktop-portal
```

### Rust toolchain

```bash
rustup toolchain install stable
rustup override set stable   # edition 2024 requires Rust ≥ 1.85
```

---

## Building

```bash
git clone https://github.com/lnxcast/lnxcast
cd lnxcast
cargo build --release
```

The binary lands at `target/release/lnxcast`.

---

## Running

```bash
# Basic (auto-detects encoder, uses p2p-dev-wlan0)
./target/release/lnxcast

# Override log level
RUST_LOG=debug ./target/release/lnxcast
```

Connect any RTSP client or Miracast receiver to:

```
rtsp://<your-ip>:7236/live
```

### Quick test with VLC

```bash
vlc rtsp://127.0.0.1:7236/live
```

---

## Crate reference

### `lnxcast-net`

| Symbol | Description |
|--------|-------------|
| `DeviceScanner` | Trait – async scan loop emitting `DeviceEvent` via `mpsc` |
| `WpaSupplicantScanner` | Concrete impl using `wpa_supplicant` D-Bus |
| `P2pDevice` | Discovered sink (name, MAC, RSSI, WFD capability) |
| `DeviceEvent` | `Discovered`, `Lost`, `ScanComplete`, `Error` |

### `lnxcast-capture`

| Symbol | Description |
|--------|-------------|
| `CaptureBackend` | `PipeWirePortal` or `Unavailable` |
| `ScreenCapturer` | Opens XDG portal session and returns `CaptureStream` |
| `CaptureStream` | Wraps PipeWire FD + node ID |
| `CaptureOptions` | Cursor inclusion, source type, restore token |

### `lnxcast-media`

| Symbol | Description |
|--------|-------------|
| `MediaPipeline` | Owns and drives the GStreamer pipeline |
| `PipelineConfig` | PipeWire node ID, bitrate, FPS, resolution, encoder override |
| `EncoderBackend` | `VaapiHardware` or `X264Software` |
| `detect_encoder()` | Runtime probe – prefers VA-API, falls back to x264 |

### `lnxcast-server`

| Symbol | Description |
|--------|-------------|
| `RtspServer` | Wraps `gst-rtsp-server`, exposes async `run()` / `run_with_shutdown()` |
| `ServerConfig` | Bind address, port (default 7236), mount path (default `/live`) |

---

## Tests

```bash
# All workspace tests
cargo test --workspace

# Single crate
cargo test -p lnxcast-net
cargo test -p lnxcast-capture
cargo test -p lnxcast-media
cargo test -p lnxcast-server
```

| Test | Crate | Notes |
|------|-------|-------|
| `test_p2p_interface_exists` | `lnxcast-net` | Async; tolerates missing `wpa_supplicant` in CI |
| `test_scanner_channel_types` | `lnxcast-net` | Validates `mpsc` channel type compatibility |
| `test_mac_to_string` | `lnxcast-net` | Pure unit test for MAC formatting |
| `test_p2p_device_serde` | `lnxcast-net` | JSON round-trip for `P2pDevice` |
| `test_capture_source_availability` | `lnxcast-capture` | Async; accepts both variants |
| `test_capture_options_defaults` | `lnxcast-capture` | Options struct validation |
| `test_source_type_conversion` | `lnxcast-capture` | `ashpd` type mapping |
| `test_start_fails_gracefully_without_portal` | `lnxcast-capture` | Error-path test |
| `test_gst_init_and_factory` | `lnxcast-media` | GStreamer availability probe |
| `test_pipeline_construction` | `lnxcast-media` | Full pipeline build (hardware or software) |
| `test_pipeline_software_encoder_override` | `lnxcast-media` | Forces `x264enc` path |
| `test_pipeline_hardware_encoder_override` | `lnxcast-media` | Forces `vaapih264enc` path |
| `test_encoder_backend_display` | `lnxcast-media` | `Display` impl sanity |
| `test_server_config_defaults` | `lnxcast-server` | Port 7236 + `/live` |
| `test_service_string` | `lnxcast-server` | Port → string conversion |
| `test_default_launch_pipeline_non_empty` | `lnxcast-server` | Pipeline string sanity |
| `test_custom_launch_pipeline` | `lnxcast-server` | Override respected |
| `test_rtsp_server_bind` | `lnxcast-server` | Async; builds RTSP server object |
| `test_rtsp_server_immediate_shutdown` | `lnxcast-server` | Async; graceful shutdown |

---

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | `info` | Log filter (e.g. `debug`, `lnxcast_media=trace`) |
| `GST_DEBUG` | _(unset)_ | GStreamer internal debug level (0–9) |
| `WAYLAND_DISPLAY` | _(set by compositor)_ | Required for portal |

---

## Encoder selection logic

```
Is `vaapih264enc` factory available?
        │
       YES ──▶ Use vaapih264enc (Intel/AMD/Nvidia VA-API)
        │
        NO ──▶ Is `x264enc` factory available?
                    │
                   YES ──▶ Use x264enc (software, zero-latency)
                    │
                    NO ──▶ Error: no H.264 encoder found
```

You can force a specific encoder via `PipelineConfig::encoder_override`.

---

## License

Dual-licensed under **MIT** or **Apache-2.0** at your option.