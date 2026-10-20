//! Google Cast (Chromecast built-in) fallback for TVs where nothing can be installed:
//! the emergency path when the HDMI cable is broken. Higher latency than the NANOCast
//! receiver, but works on every Android TV / Google TV / Chromecast dongle out of the box.
//!
//! This file fixes the public API the GUI codes against; the implementation lives in the
//! submodules (docs/google-cast.md):
//! - `discovery`: mDNS `_googlecast._tcp`;
//! - `proto` + `channel`: Cast v2 (TLS 8009, protobuf `CastMessage`, JSON namespaces);
//! - `mirror`: path B, Cast Streaming like Chrome's mirroring (default);
//! - `fmp4` + `http`: path A, live fMP4 served to the Default Media Receiver (fallback);
//! - Windows only: `video` (nanocast-win capture + H.264), `sound` (WASAPI loopback),
//!   `aac` (Media Foundation AAC), `winsession` (wiring).
//!
//! Compared with the cast built into Chrome, NANOCast sends 1080p at 60 fps and a higher
//! bitrate from the hardware encoder, with no B-frames and paced packets.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::Result;

pub mod channel;
pub mod discovery;
pub mod fmp4;
pub mod http;
pub mod mirror;
pub mod proto;

#[cfg(windows)]
mod aac;
#[cfg(windows)]
mod sound;
#[cfg(windows)]
mod video;
#[cfg(windows)]
mod winsession;

/// Path A on this PC only (no TV): the live fMP4 stream for a browser, for testing.
#[cfg(windows)]
pub use winsession::Preview;

/// A Google Cast receiver found on the LAN (`_googlecast._tcp.local.`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastDevice {
    /// Friendly name from TXT `fn` (e.g. "Phong hoc 12").
    pub name: String,
    /// Model from TXT `md` (e.g. "Chromecast", "BRAVIA 4K").
    pub model: String,
    pub addr: Ipv4Addr,
    /// Cast channel port (TLS), normally 8009.
    pub port: u16,
    /// Cast receiver id from TXT `id`.
    pub id: String,
}

/// What to send and how.
#[derive(Clone, Debug)]
pub struct CastConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub audio: bool,
}

impl Default for CastConfig {
    fn default() -> Self {
        // Smoother and sharper than Chrome's built-in cast (1080p30, ~5 Mbps).
        Self { width: 1920, height: 1080, fps: 60, bitrate_bps: 12_000_000, audio: true }
    }
}

/// Which Cast path is in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastMode {
    /// Cast Streaming (the protocol Chrome uses for tab/desktop mirroring): ~0.1–0.4 s.
    Mirroring,
    /// Default Media Receiver playing a live fMP4/HLS stream served by this PC: ~1–3 s.
    MediaReceiver,
}

#[derive(Clone, Debug, Default)]
pub struct CastStats {
    pub fps: f32,
    pub mbps: f32,
    /// Estimated end-to-end delay in milliseconds, when the path can tell.
    pub latency_ms: Option<u32>,
    pub connected: bool,
    pub mode: Option<CastMode>,
    pub message: String,
}

/// Browses the LAN for Google Cast receivers for `timeout`.
pub fn discover(timeout: Duration) -> Result<Vec<CastDevice>> {
    discovery::discover(timeout)
}

/// A running cast to one Google Cast device. Encoded frames come from the platform
/// pipeline (nanocast-win / -mac / -linux); this crate owns transport and receiver control.
pub struct CastSession {
    device: CastDevice,
    #[cfg(windows)]
    inner: winsession::Session,
}

impl CastSession {
    /// Connects, launches the receiver app and starts streaming the given source.
    /// `source` is an opaque description the platform crate understands (monitor index or
    /// window handle), passed through unchanged.
    ///
    /// Blocks while connecting (a few seconds): tries Cast Streaming mirroring first and
    /// falls back to the Default Media Receiver (`NANOCAST_CAST_MODE=mirroring|media`
    /// forces one). Errors are in Vietnamese, meant for the user.
    pub fn start(device: CastDevice, config: CastConfig, source: SourceSpec) -> Result<Self> {
        #[cfg(windows)]
        {
            let inner = winsession::Session::start(&device, config, source)?;
            Ok(Self { device, inner })
        }
        #[cfg(not(windows))]
        {
            let _ = (config, source);
            anyhow::bail!("Cast tới Chromecast hiện chỉ có trên Windows ({})", device.name)
        }
    }

    pub fn device(&self) -> &CastDevice {
        &self.device
    }

    /// Sound on / off. The sound track keeps running (silence while off) so the TV's
    /// player never stalls; a cast started with `audio: false` has no sound track at all on
    /// the media path.
    pub fn set_audio(&mut self, on: bool) {
        #[cfg(windows)]
        self.inner.set_audio(on);
        #[cfg(not(windows))]
        let _ = on;
    }

    pub fn stats(&self) -> CastStats {
        #[cfg(windows)]
        return self.inner.stats();
        #[cfg(not(windows))]
        CastStats::default()
    }

    /// Stops streaming and closes the receiver app on the TV.
    pub fn stop(self) {
        #[cfg(windows)]
        {
            let mut s = self;
            s.inner.stop();
        }
    }
}

/// What to capture, in platform-neutral terms (the platform crate resolves it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSpec {
    /// Monitor by index, 0 = primary; n >= 1 = the one-based index of
    /// `nanocast_win::sources::list_monitors` (`MonitorInfo::index`).
    Monitor(usize),
    /// Window by its native handle (HWND on Windows, CGWindowID on macOS, XID on Linux).
    Window(u64),
}
