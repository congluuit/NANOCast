//! macOS sender pipeline (PLAN.md Phase 3), the counterpart of `nanocast-win`:
//! ScreenCaptureKit (one display or one window, NV12 scaled on the GPU, paced by
//! `minimumFrameInterval`) -> VideoToolbox H.264 (real-time, no B-frames, AVCC converted
//! to Annex B) -> paced UDP (`nanocast_core::protocol::PacedSender`) + TCP control channel,
//! plus system sound from ScreenCaptureKit (macOS 13+) -> Opus -> UDP AUDIO_PORT.
//!
//! The module layout and public API mirror `nanocast-win` (`capture::Source`,
//! `sources::{list_monitors, list_windows}`, `session::{CastConfig, CastSession, ...}`,
//! `mf_encoder::hw_encoder_names`, `control_link::{ReceiverReport, TvStats}`), so the GUI
//! only needs `cfg(target_os)` to pick the crate. See docs/macos.md.
//!
//! Needs macOS 12.3+ (ScreenCaptureKit) and the Screen Recording permission. On every
//! other OS only the platform-neutral helpers below are compiled.

pub mod annexb;
pub mod convert;

#[cfg(target_os = "macos")]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod capture;
#[cfg(target_os = "macos")]
pub mod control_link;
#[cfg(target_os = "macos")]
pub mod session;
#[cfg(target_os = "macos")]
pub mod sources;
#[cfg(target_os = "macos")]
pub mod vt_encoder;

/// Same path as on Windows so the GUI code is shared (`mf_encoder::hw_encoder_names()`).
#[cfg(target_os = "macos")]
pub mod mf_encoder {
    pub use crate::vt_encoder::hw_encoder_names;
}
