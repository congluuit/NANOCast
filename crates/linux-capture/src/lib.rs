//! Linux sender pipeline (PLAN.md Phase 3), the counterpart of `nanocast-win`/`nanocast-mac`:
//!
//! - capture: xdg-desktop-portal ScreenCast + PipeWire (Wayland, and X11 desktops whose
//!   portal supports it; the system dialog picks the screen or window) or, on X11 sessions,
//!   MIT-SHM `GetImage` of one monitor or one window with the cursor drawn in (XFixes);
//!   paced to the target fps and converted to NV12 at a fixed canvas size on the CPU;
//! - encode: H.264 through the system FFmpeg (`h264_vaapi` Intel/AMD, `h264_nvenc`,
//!   `h264_v4l2m2m` Raspberry Pi 4, `libx264`) or OpenH264 built from source, low latency
//!   (no B-frames, CBR, forced IDR on `keyframe_request`), Annex B with SPS/PPS before every
//!   keyframe;
//! - send: paced UDP (`nanocast_core::protocol::PacedSender`) + TCP control channel;
//! - sound: the default sink's monitor through PipeWire (or PulseAudio) -> Opus -> UDP
//!   AUDIO_PORT.
//!
//! The module layout and public API mirror `nanocast-win` (`capture::Source`,
//! `sources::{list_monitors, list_windows}`, `session::{CastConfig, CastSession, ...}`,
//! `mf_encoder::hw_encoder_names`, `control_link::{ReceiverReport, TvStats}`, `audio`), so
//! the GUI only needs `cfg(target_os)` to pick the crate. See docs/linux.md.
//!
//! Native back ends are cargo features (`pipewire`, `ffmpeg`, `openh264`, `pulse`, all on by
//! default). On every other OS only the platform-neutral helpers below are compiled.

pub mod annexb;
pub mod convert;

#[cfg(target_os = "linux")]
pub mod audio;
#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod control_link;
#[cfg(target_os = "linux")]
pub mod encoder;
#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
mod ff_encoder;
#[cfg(all(target_os = "linux", feature = "openh264"))]
mod oh_encoder;
#[cfg(all(target_os = "linux", feature = "portal"))]
#[cfg_attr(not(feature = "pipewire"), allow(dead_code))]
mod portal;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pw_video;
#[cfg(target_os = "linux")]
pub mod session;
#[cfg(target_os = "linux")]
pub mod sources;
#[cfg(target_os = "linux")]
mod x11;

/// Same path as on Windows so the GUI code is shared (`mf_encoder::hw_encoder_names()`).
#[cfg(target_os = "linux")]
pub mod mf_encoder {
    pub use crate::encoder::hw_encoder_names;
}
