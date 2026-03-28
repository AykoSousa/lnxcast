//! # capture
//!
//! Obtains a PipeWire screen-capture stream through the XDG Desktop Portal
//! (`org.freedesktop.portal.ScreenCast`) using the [`ashpd`] crate.
//!
//! The public API returns a [`CaptureStream`] that wraps the raw PipeWire
//! file descriptor so downstream components (e.g. `media`) can plug
//! it directly into a GStreamer `pipewiresrc` element.
//!
//! ## Wayland / Portal requirement
//!
//! This module requires a running XDG portal back-end (e.g. `xdg-desktop-portal-gnome`
//! or `-wlr`).  On X11 or headless runners the [`CaptureBackend::detect`] call
//! will return [`CaptureBackend::Unavailable`] so callers can fall back
//! gracefully.
//!
//! ## Example
//!
//! ```no_run
//! use capture::{ScreenCapturer, CaptureOptions, CaptureBackend};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let backend = CaptureBackend::detect().await;
//!     println!("Backend: {backend:?}");
//!
//!     if backend.is_available() {
//!         let capturer = ScreenCapturer::new(CaptureOptions::default());
//!         let stream = capturer.start().await?;
//!         println!("PipeWire node id: {}", stream.node_id());
//!     }
//!     Ok(())
//! }
//! ```

use std::os::unix::io::{IntoRawFd, RawFd};

use ashpd::{
    desktop::screencast::{CursorMode, Screencast, SourceType},
    // WindowIdentifier,
};
use thiserror::Error;
use tracing::{debug, info};

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("XDG portal error: {0}")]
    Portal(#[from] ashpd::Error),

    #[error("Capture back-end unavailable: {0}")]
    Unavailable(String),

    #[error("PipeWire FD not yet available – call start() first")]
    NotStarted,

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type CaptureResult<T> = Result<T, CaptureError>;

// ──────────────────────────────────────────────────────────────────────────────
// Back-end detection
// ──────────────────────────────────────────────────────────────────────────────

/// Identifies the screen-capture back-end that is available at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureBackend {
    /// XDG portal is reachable – PipeWire capture is possible.
    PipeWirePortal,
    /// No supported back-end found (headless, X11 without portal, …).
    Unavailable,
}

impl CaptureBackend {
    /// Probe the runtime environment asynchronously.
    pub async fn detect() -> Self {
        // Attempt to open a portal session; if it fails we consider the
        // back-end unavailable.
        match Screencast::new().await {
            Ok(_) => {
                debug!("XDG ScreenCast portal is available");
                Self::PipeWirePortal
            }
            Err(e) => {
                debug!("XDG ScreenCast portal unavailable: {e}");
                Self::Unavailable
            }
        }
    }

    /// Returns `true` when capture is possible.
    #[inline]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::PipeWirePortal)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Capture options
// ──────────────────────────────────────────────────────────────────────────────

/// Configuration passed to [`ScreenCapturer`].
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Whether to capture the cursor alongside the desktop content.
    pub include_cursor: bool,
    /// Preferred source type; defaults to `Monitor`.
    pub source_type: PortalSourceType,
    /// Restore token from a previous session (optional).
    pub restore_token: Option<String>,
}

/// Thin wrapper so callers don't need to import `ashpd` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalSourceType {
    Monitor,
    Window,
    Virtual,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            include_cursor: false,
            source_type: PortalSourceType::Monitor,
            restore_token: None,
        }
    }
}

impl From<PortalSourceType> for SourceType {
    fn from(t: PortalSourceType) -> Self {
        match t {
            PortalSourceType::Monitor => SourceType::Monitor,
            PortalSourceType::Window => SourceType::Window,
            PortalSourceType::Virtual => SourceType::Virtual,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// CaptureStream – result of a successful portal session
// ──────────────────────────────────────────────────────────────────────────────

/// An active PipeWire screen-capture stream.
///
/// Holds the raw file descriptor and the PipeWire *node ID* that must be
/// passed to `pipewiresrc path=<node_id>` in GStreamer.
pub struct CaptureStream {
    /// Raw PipeWire FD.  Ownership transferred to caller on [`into_raw_fd`].
    fd: RawFd,
    /// PipeWire node id for this stream.
    node_id: u32,
}

impl CaptureStream {
    /// PipeWire node id (use as `pipewiresrc path=<node_id>`).
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Borrow the raw PipeWire file descriptor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Consume the stream and return the underlying file descriptor.
    /// The caller is responsible for closing it.
    pub fn into_raw_fd(self) -> RawFd {
        self.fd
    }
}

impl std::fmt::Debug for CaptureStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureStream")
            .field("fd", &self.fd)
            .field("node_id", &self.node_id)
            .finish()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ScreenCapturer
// ──────────────────────────────────────────────────────────────────────────────

/// Opens an XDG portal session and returns a [`CaptureStream`].
pub struct ScreenCapturer {
    options: CaptureOptions,
}

impl ScreenCapturer {
    pub fn new(options: CaptureOptions) -> Self {
        Self { options }
    }

    /// Negotiate a portal session and obtain the PipeWire FD.
    ///
    /// Displays the native screen-picker dialog to the user.
    pub async fn start(&self) -> CaptureResult<CaptureStream> {
        info!("Opening XDG ScreenCast portal session");

        let screencast = Screencast::new()
            .await
            .map_err(CaptureError::Portal)?;

        let session = screencast
            .create_session()
            .await
            .map_err(CaptureError::Portal)?;

        let cursor_mode = if self.options.include_cursor {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };

        screencast
            .select_sources(
                &session,
                cursor_mode,
                SourceType::Monitor | SourceType::Window,
                false, // multiple sources
                self.options.restore_token.as_deref(),
                ashpd::desktop::PersistMode::DoNot,
            )
            .await
            .map_err(CaptureError::Portal)?;

        let response = screencast
            .start(&session, None)
            .await
            .map_err(CaptureError::Portal)?
            .response()
            .map_err(|e| CaptureError::Internal(format!("portal start response: {e}")))?;

        // The portal returns one or more streams; we take the first.
        let stream_info = response
            .streams()
            .first()
            .ok_or_else(|| CaptureError::Internal("portal returned no streams".into()))?;

        let node_id = stream_info.pipe_wire_node_id();

        let pw_fd = screencast
            .open_pipe_wire_remote(&session)
            .await
            .map_err(CaptureError::Portal)?;

        let raw_fd = pw_fd.into_raw_fd();
        info!(node_id, raw_fd, "PipeWire stream acquired");

        Ok(CaptureStream {
            fd: raw_fd,
            node_id,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the capture back-end detection logic.
    ///
    /// On a headless CI runner this should return `Unavailable`; on a Wayland
    /// desktop it returns `PipeWirePortal`.  Either result is acceptable – we
    /// just confirm the function completes without panic.
    #[tokio::test]
    async fn test_capture_source_availability() {
        let backend = CaptureBackend::detect().await;
        // The result depends on the environment; we only assert it is a valid variant.
        assert!(
            backend == CaptureBackend::PipeWirePortal || backend == CaptureBackend::Unavailable,
            "unexpected backend variant"
        );
        println!("Detected capture backend: {backend:?}");
    }

    /// CaptureOptions default values are sensible.
    #[test]
    fn test_capture_options_defaults() {
        let opts = CaptureOptions::default();
        assert!(!opts.include_cursor);
        assert_eq!(opts.source_type, PortalSourceType::Monitor);
        assert!(opts.restore_token.is_none());
    }

    /// PortalSourceType converts correctly to the ashpd type.
    #[test]
    fn test_source_type_conversion() {
        assert_eq!(SourceType::from(PortalSourceType::Monitor), SourceType::Monitor);
        assert_eq!(SourceType::from(PortalSourceType::Window), SourceType::Window);
        assert_eq!(SourceType::from(PortalSourceType::Virtual), SourceType::Virtual);
    }

    /// Calling `start()` without a portal fails gracefully with a typed error
    /// rather than a panic.
    #[tokio::test]
    async fn test_start_fails_gracefully_without_portal() {
        let capturer = ScreenCapturer::new(CaptureOptions::default());
        let result = capturer.start().await;
        match result {
            // On a Wayland + portal desktop this might actually succeed.
            Ok(stream) => {
                println!("Unexpectedly got a stream (running on Wayland?): {stream:?}");
            }
            Err(CaptureError::Portal(_)) | Err(CaptureError::Unavailable(_)) => {
                // Expected in CI / headless environments.
            }
            Err(e) => {
                panic!("Unexpected error variant: {e}");
            }
        }
    }
}