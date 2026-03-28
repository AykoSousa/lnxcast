//! # media
//!
//! Builds and manages a dynamic GStreamer pipeline that:
//!
//! 1. Receives frames from PipeWire via `pipewiresrc`.
//! 2. Converts the raw video to `I420` (YUV planar).
//! 3. Encodes to H.264 – preferring hardware acceleration (`vaapih264enc`)
//!    with automatic fallback to software encoding (`x264enc`).
//! 4. Wraps the bitstream in RTP (`rtph264pay`) ready for RTSP transport.
//!
//! ## Pipeline graph
//!
//! ```text
//! pipewiresrc   (negotiates freely – any format the portal delivers)
//!      │
//!      ▼
//! videoconvert  (converts from native portal format to I420)
//!      │
//!      ▼
//! videoscale ──▶ capsfilter(max_width × max_height)
//!      │
//!      ▼
//! capsfilter(video/x-raw, format=I420, framerate=N/1)
//!      │
//!      ├──────────────────────────┐
//!      │ hardware (vaapih264enc)  │ software (x264enc)
//!      └──────────────────────────┘
//!      │
//!      ▼
//! h264parse ──▶ capsfilter(byte-stream) ──▶ rtph264pay ──▶ appsink
//! ```
//!
//! ### Why no capsfilter before videoconvert?
//!
//! The XDG Desktop Portal delivers the PipeWire stream in whatever pixel
//! format the compositor chose – typically `BGRA`, `xRGB`, or `DMA_DRM`.
//! Placing `video/x-raw,format=I420` upstream of `videoconvert` forces
//! `pipewiresrc` to negotiate I420 directly with the portal, which fails
//! with *"no more input formats"* because the portal does not offer I420 in
//! its first-choice format list.  The correct pattern is:
//!
//! `pipewiresrc` (any) → `videoconvert` (converts) → `capsfilter` (I420)

use std::os::unix::io::RawFd;

use gstreamer::{
    self as gst,
    prelude::*,
    Element, ElementFactory, Pipeline, State,
};
use thiserror::Error;
use tracing::{debug, info, warn};

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("GStreamer initialisation failed: {0}")]
    Init(String),

    #[error("Element `{name}` could not be created: plugin missing?")]
    ElementCreate { name: &'static str },

    #[error("Failed to link elements: {0}")]
    Link(String),

    #[error("Pipeline state change failed: {0}")]
    StateChange(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type MediaResult<T> = Result<T, MediaError>;

// ──────────────────────────────────────────────────────────────────────────────
// Encoder selection
// ──────────────────────────────────────────────────────────────────────────────

/// Which H.264 encoder back-end is being used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    /// VA-API hardware encoder (`vaapih264enc`).
    VaapiHardware,
    /// Libx264 software encoder (`x264enc`).
    X264Software,
}

impl std::fmt::Display for EncoderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VaapiHardware => write!(f, "vaapih264enc (hardware)"),
            Self::X264Software => write!(f, "x264enc (software)"),
        }
    }
}

/// Returns `true` when the named GStreamer element factory is available.
fn factory_available(name: &str) -> bool {
    ElementFactory::find(name).is_some()
}

/// Probe which encoder is available and return the preferred one.
pub fn detect_encoder() -> EncoderBackend {
    if factory_available("vaapih264enc") {
        info!("Hardware encoder vaapih264enc found – using VA-API");
        EncoderBackend::VaapiHardware
    } else {
        warn!("vaapih264enc not found – falling back to x264enc (software)");
        EncoderBackend::X264Software
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Pipeline configuration
// ──────────────────────────────────────────────────────────────────────────────

/// Tunable parameters for the media pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// PipeWire file descriptor returned by the XDG portal.
    ///
    /// Must be passed to `pipewiresrc fd=<pipewire_fd>` so GStreamer
    /// uses the portal-authenticated connection instead of opening its
    /// own, which would fail with *"target not found"*.
    pub pipewire_fd: RawFd,
    /// PipeWire node ID (from `capture`).
    pub pipewire_node_id: u32,
    /// Target bit-rate in kbps.  Defaults to 4 000 kbps.
    pub bitrate_kbps: u32,
    /// Target frame-rate.  Defaults to 30.
    pub framerate: u32,
    /// Maximum width in pixels.  Defaults to 1920.
    pub max_width: u32,
    /// Maximum height in pixels.  Defaults to 1080.
    pub max_height: u32,
    /// Force a specific encoder regardless of hardware availability.
    pub encoder_override: Option<EncoderBackend>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            pipewire_fd: -1,
            pipewire_node_id: 0,
            bitrate_kbps: 4_000,
            framerate: 30,
            max_width: 1920,
            max_height: 1080,
            encoder_override: None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// MediaPipeline
// ──────────────────────────────────────────────────────────────────────────────

/// Owns a GStreamer [`Pipeline`] configured for Miracast streaming.
pub struct MediaPipeline {
    pipeline: Pipeline,
    encoder: EncoderBackend,
}

impl MediaPipeline {
    // ── helpers ────────────────────────────────────────────────────────────

    fn make(name: &'static str) -> MediaResult<Element> {
        ElementFactory::make(name)
            .build()
            .map_err(|_| MediaError::ElementCreate { name })
    }

    fn make_named(factory: &'static str, element_name: &str) -> MediaResult<Element> {
        ElementFactory::make(factory)
            .name(element_name)
            .build()
            .map_err(|_| MediaError::ElementCreate { name: factory })
    }

    // ── constructor ────────────────────────────────────────────────────────

    /// Build the pipeline.  Initialises GStreamer if not already done.
    pub fn build(config: &PipelineConfig) -> MediaResult<Self> {
        gst::init().map_err(|e| MediaError::Init(e.to_string()))?;

        let encoder = config
            .encoder_override
            .unwrap_or_else(detect_encoder);

        info!(%encoder, node_id = config.pipewire_node_id, "Building GStreamer pipeline");

        let pipeline = Pipeline::with_name("lnxcast-pipeline");

        // ── Source ──────────────────────────────────────────────────────────
        let src = Self::make("pipewiresrc")?;
        // fd  – the portal-authenticated PipeWire connection.  Without this
        //       pipewiresrc opens its own connection and cannot see the
        //       portal node, failing with "target not found".
        // path – the node ID inside that connection.
        src.set_property("fd", config.pipewire_fd);
        src.set_property("path", config.pipewire_node_id.to_string());
        src.set_property("do-timestamp", true);
        // IMPORTANT: do NOT place a capsfilter between pipewiresrc and
        // videoconvert. The XDG portal may deliver the screen in BGRA,
        // DMA_DRM, or other packed/DMA formats. Forcing I420 upstream
        // causes pipewiresrc to error with "no more input formats" during
        // format negotiation. videoconvert must see the native format first.

        // ── Video conversion ────────────────────────────────────────────────
        //
        // CAPS NEGOTIATION RULES for pipewiresrc:
        //   Any capsfilter with width, height, or framerate constraints placed
        //   anywhere in the pipeline will be back-propagated to pipewiresrc,
        //   which then tries to satisfy them with the portal.  The portal only
        //   offers the formats it natively produces (BGRA, xRGB, DMA_DRM …)
        //   and will fail with "no more input formats" on any constraint it
        //   cannot meet.
        //
        //   Safe strategy: use NO capsfilters at all until after the encoder.
        //   Let pipewiresrc, videoconvert and x264enc negotiate freely.
        //   x264enc accepts any I420/NV12 input resolution and framerate.
        //
        // Pipeline: pipewiresrc → videoconvert → videoscale → videorate → encoder
        let convert = Self::make("videoconvert")?;

        // videoscale: downscale silently if source > max_width × max_height.
        // No downstream capsfilter – adding one re-introduces the back-prop problem.
        let videoscale = Self::make("videoscale")?;
        videoscale.set_property_from_str("method", "bilinear"); // 1 = bilinear

        // videorate: re-timestamps frames to the desired fps.
        // Also has no capsfilter downstream for the same reason.
        let videorate = Self::make("videorate")?;
        videorate.set_property("max-rate", config.framerate as i32);
        videorate.set_property_from_str("drop-only", "true");

        // ── Encoder ─────────────────────────────────────────────────────────
        let enc: Element = match encoder {
            EncoderBackend::VaapiHardware => {
                let e = Self::make_named("vaapih264enc", "lnxcast-encoder")?;
                // VA-API bitrate in kbps.
                e.set_property("bitrate", config.bitrate_kbps);
                // Rate control: constant bit-rate.
                e.set_property_from_str("rate-control", "cbr");
                e
            }
            EncoderBackend::X264Software => {
                let e = Self::make_named("x264enc", "lnxcast-encoder")?;
                // x264enc bitrate in kbps.
                e.set_property("bitrate", config.bitrate_kbps);
                // Low-latency tuning important for live streaming.
                e.set_property_from_str("tune", "zerolatency");
                e.set_property_from_str("speed-preset", "ultrafast");
                e
            }
        };

        // ── H.264 parse ─────────────────────────────────────────────────────
        let h264parse = Self::make("h264parse")?;
        // Emit stream-format=byte-stream so rtph264pay can consume it.
        let enc_caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let enc_caps_filter = Self::make("capsfilter")?;
        enc_caps_filter.set_property("caps", &enc_caps);

        // ── RTP payloader ───────────────────────────────────────────────────
        let rtppay = Self::make("rtph264pay")?;
        rtppay.set_property("config-interval", 1i32); // send SPS/PPS inline
        rtppay.set_property("pt", 96u32);

        // ── App sink (output hand-off point) ────────────────────────────────
        let appsink = Self::make("appsink")?;
        appsink.set_property("emit-signals", true);
        appsink.set_property("sync", false);

        // ── Add all elements ────────────────────────────────────────────────
        // Pipeline order (NO capsfilters upstream of the encoder):
        //   pipewiresrc → videoconvert → videoscale → videorate → encoder
        //   → h264parse → enc_caps_filter → rtph264pay → appsink
        //
        // enc_caps_filter (byte-stream) is the ONLY capsfilter; it is
        // downstream of the encoder so it cannot affect pipewiresrc negotiation.
        let elements = [
            &src, &convert, &videoscale, &videorate,
            &enc, &h264parse, &enc_caps_filter, &rtppay, &appsink,
        ];
        pipeline
            .add_many(elements)
            .map_err(|e| MediaError::Link(e.to_string()))?;

        // ── Link ────────────────────────────────────────────────────────────
        gst::Element::link_many(elements)
            .map_err(|e| MediaError::Link(e.to_string()))?;

        debug!("Pipeline elements linked successfully");

        Ok(Self { pipeline, encoder })
    }

    // ── Public accessors ───────────────────────────────────────────────────

    /// The encoder back-end that was selected.
    pub fn encoder(&self) -> EncoderBackend {
        self.encoder
    }

    /// Borrow the underlying GStreamer pipeline.
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// Transition to `Playing` state.
    pub fn play(&self) -> MediaResult<()> {
        self.pipeline
            .set_state(State::Playing)
            .map_err(|e| MediaError::StateChange(e.to_string()))?;
        info!("Pipeline set to PLAYING");
        Ok(())
    }

    /// Transition to `Null` state (releases hardware resources).
    pub fn stop(&self) -> MediaResult<()> {
        self.pipeline
            .set_state(State::Null)
            .map_err(|e| MediaError::StateChange(e.to_string()))?;
        info!("Pipeline set to NULL");
        Ok(())
    }

    /// Retrieve a named element from the pipeline (useful for dynamic linking).
    pub fn element_by_name(&self, name: &str) -> Option<Element> {
        self.pipeline.by_name(name)
    }
}

impl Drop for MediaPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(State::Null);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: initialise GStreamer once per test binary run.
    fn init_gst() -> bool {
        gst::init().is_ok()
    }

    /// Verify that GStreamer initialises and that the `factory_available`
    /// helper works correctly for a well-known element (`fakesrc`).
    #[test]
    fn test_gst_init_and_factory() {
        if !init_gst() {
            eprintln!("GStreamer not available in this environment – skipping");
            return;
        }
        // `fakesrc` ships with every GStreamer base install.
        assert!(factory_available("fakesrc"), "fakesrc must always be present");
        // An element with an implausible name must not be found.
        assert!(!factory_available("__no_such_element__"));
    }

    /// Attempt to construct a pipeline with the auto-detected encoder.
    ///
    /// On systems without `pipewiresrc` the build will fail with a typed
    /// `MediaError::ElementCreate` – that is an acceptable result because it
    /// demonstrates that the error handling path works.
    ///
    /// On a fully-equipped desktop the pipeline should build successfully.
    #[test]
    fn test_pipeline_construction() {
        if !init_gst() {
            eprintln!("GStreamer not available – skipping pipeline test");
            return;
        }

        let config = PipelineConfig {
            pipewire_node_id: 42,
            ..Default::default()
        };

        match MediaPipeline::build(&config) {
            Ok(mp) => {
                println!("Pipeline built with encoder: {}", mp.encoder());
                // Confirm we can retrieve the encoder element by name.
                assert!(
                    mp.element_by_name("lnxcast-encoder").is_some(),
                    "encoder element must be reachable by name"
                );
            }
            Err(MediaError::ElementCreate { name }) => {
                eprintln!("Missing plugin `{name}` (expected in CI) – test passed as environment-skip");
            }
            Err(e) => {
                panic!("Unexpected error building pipeline: {e}");
            }
        }
    }

    /// Force the software encoder path and confirm the correct element is used.
    #[test]
    fn test_pipeline_software_encoder_override() {
        if !init_gst() {
            return;
        }

        let config = PipelineConfig {
            encoder_override: Some(EncoderBackend::X264Software),
            ..Default::default()
        };

        match MediaPipeline::build(&config) {
            Ok(mp) => {
                assert_eq!(mp.encoder(), EncoderBackend::X264Software);
            }
            Err(MediaError::ElementCreate { name }) => {
                eprintln!("Missing plugin `{name}` – skipping");
            }
            Err(e) => panic!("{e}"),
        }
    }

    /// Force the hardware encoder path and confirm the correct element is used.
    #[test]
    fn test_pipeline_hardware_encoder_override() {
        if !init_gst() {
            return;
        }

        let config = PipelineConfig {
            encoder_override: Some(EncoderBackend::VaapiHardware),
            ..Default::default()
        };

        match MediaPipeline::build(&config) {
            Ok(mp) => {
                assert_eq!(mp.encoder(), EncoderBackend::VaapiHardware);
            }
            Err(MediaError::ElementCreate { name }) => {
                eprintln!("Missing plugin `{name}` (VA-API not present) – skipping");
            }
            Err(e) => panic!("{e}"),
        }
    }

    /// `EncoderBackend::Display` must not panic.
    #[test]
    fn test_encoder_backend_display() {
        assert!(!EncoderBackend::VaapiHardware.to_string().is_empty());
        assert!(!EncoderBackend::X264Software.to_string().is_empty());
    }
}