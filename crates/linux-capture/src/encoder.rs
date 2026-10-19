//! H.264 encoding on Linux: one `Encoder` type over several back ends, tried in order.
//!
//! Choice (see docs/linux.md "Why FFmpeg"): VA-API through the `libva`/`cros-codecs` crates
//! would mean building SPS/PPS/slice parameter buffers by hand (cros-codecs targets
//! ChromeOS, its encoder API is still moving, and it has no V4L2 M2M path for the Raspberry
//! Pi). libavcodec already wraps every Linux encoder behind one low-latency API and is
//! packaged everywhere, so `ff_encoder.rs` calls it directly (`ffmpeg-sys-next`):
//!
//! | name           | hardware                         | limits used here         |
//! |----------------|----------------------------------|--------------------------|
//! | `h264_vaapi`   | Intel (iHD/i965), AMD (radeonsi) | 1080p60                  |
//! | `h264_nvenc`   | NVIDIA (proprietary driver)      | 1080p60                  |
//! | `h264_v4l2m2m` | Raspberry Pi 4 (bcm2835-codec)   | 1080p30                  |
//! | `libx264`      | software (FFmpeg built with x264)| 720p30                   |
//! | `openh264`     | software, built from source (BSD)| 720p30                   |
//!
//! All run without B-frames, constant bitrate, a long GOP (keyframes on request) and force
//! an IDR on `request_keyframe`. Output goes through `annexb::ParamSets` so every IDR
//! carries SPS/PPS. The Raspberry Pi 5 has no H.264 encoder: it uses the software ones.

// Built without any encoder feature only to type-check the rest of the crate.
#![cfg_attr(not(any(feature = "ffmpeg", feature = "openh264")), allow(unused_variables, unused_mut, unreachable_code))]

use std::time::Instant;

use anyhow::{bail, Result};
use log::{info, warn};

use nanocast_core::protocol::VideoCodec;

use crate::annexb::ParamSets;

#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Distance between periodic keyframes.
    pub gop_frames: u32,
}

pub struct EncodedFrame {
    /// Annex B, SPS/PPS before every IDR.
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub capture_instant: Instant,
    /// Submit -> packet out.
    pub encode_latency_us: u64,
}

/// One encoder packet before the parameter-set fix-up.
pub(crate) struct RawPacket {
    pub data: Vec<u8>,
    pub captured: Instant,
    pub submitted: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Vaapi,
    Nvenc,
    V4l2m2m,
    X264,
    OpenH264,
}

/// Largest picture and frame rate a back end is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_size: (u32, u32),
    pub max_fps: u32,
}

impl Kind {
    /// Default order: hardware first, then software.
    pub const ALL: [Kind; 5] = [Kind::Vaapi, Kind::Nvenc, Kind::V4l2m2m, Kind::X264, Kind::OpenH264];

    /// FFmpeg encoder name (or `openh264`); also the name shown in the GUI settings.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Vaapi => "h264_vaapi",
            Kind::Nvenc => "h264_nvenc",
            Kind::V4l2m2m => "h264_v4l2m2m",
            Kind::X264 => "libx264",
            Kind::OpenH264 => "openh264",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(name.trim()))
    }

    pub fn hardware(self) -> bool {
        !matches!(self, Kind::X264 | Kind::OpenH264)
    }

    /// PLAN.md: without a hardware encoder fall back to 720p30; the Pi 4 block tops out at 1080p30.
    pub fn limits(self) -> Limits {
        match self {
            Kind::Vaapi | Kind::Nvenc => Limits { max_size: (1920, 1080), max_fps: 60 },
            Kind::V4l2m2m => Limits { max_size: (1920, 1080), max_fps: 30 },
            Kind::X264 | Kind::OpenH264 => Limits { max_size: (1280, 720), max_fps: 30 },
        }
    }

    /// Compiled in, present in the system FFmpeg, and the device node exists.
    pub fn available(self) -> bool {
        match self {
            Kind::OpenH264 => cfg!(feature = "openh264"),
            _ => ffmpeg_has(self.name()) && device_present(self),
        }
    }
}

#[cfg(feature = "ffmpeg")]
fn ffmpeg_has(name: &str) -> bool {
    crate::ff_encoder::has_encoder(name)
}

#[cfg(not(feature = "ffmpeg"))]
fn ffmpeg_has(_name: &str) -> bool {
    false
}

fn any_path(prefix: &str, dir: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with(prefix)))
        .unwrap_or(false)
}

fn device_present(kind: Kind) -> bool {
    match kind {
        Kind::Vaapi => any_path("renderD", "/dev/dri"),
        Kind::Nvenc => std::path::Path::new("/dev/nvidiactl").exists(),
        Kind::V4l2m2m => any_path("video", "/dev"),
        Kind::X264 | Kind::OpenH264 => true,
    }
}

/// Encoders usable on this machine for `codec`, in the order they are tried (hardware
/// first). Same function as `nanocast_win::mf_encoder::hw_encoder_names` for the GUI
/// settings list. HEVC is not implemented on Linux yet: its list is empty.
pub fn hw_encoder_names(codec: VideoCodec) -> Result<Vec<String>> {
    if codec != VideoCodec::H264 {
        return Ok(Vec::new());
    }
    Ok(Kind::ALL.into_iter().filter(|k| k.available()).map(|k| k.name().to_string()).collect())
}

/// The encoders to try: `preferred` first if given, then the default order, minus `skip`.
pub fn candidates(preferred: Option<&str>, skip: &[String]) -> Vec<Kind> {
    let mut out: Vec<Kind> = preferred.and_then(Kind::from_name).into_iter().collect();
    if let Some(p) = preferred {
        if Kind::from_name(p).is_none() {
            warn!("unknown encoder \"{p}\"; using the default order");
        }
    }
    for k in Kind::ALL {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out.retain(|k| k.available() && !skip.iter().any(|s| s == k.name()));
    out
}

enum Backend {
    #[cfg(feature = "ffmpeg")]
    Ff(Box<crate::ff_encoder::FfEncoder>),
    #[cfg(feature = "openh264")]
    Oh(Box<crate::oh_encoder::OhEncoder>),
    /// Keeps the enum inhabited-looking when no encoder feature is enabled (never built).
    #[allow(dead_code)]
    None(std::convert::Infallible),
}

pub struct Encoder {
    backend: Backend,
    pub kind: Kind,
    pub name: String,
    pub hardware: bool,
    params: ParamSets,
    force_keyframe: bool,
}

impl Encoder {
    pub fn open(kind: Kind, cfg: &EncoderConfig) -> Result<Self> {
        let backend = match kind {
            #[cfg(feature = "openh264")]
            Kind::OpenH264 => Backend::Oh(Box::new(crate::oh_encoder::OhEncoder::open(cfg)?)),
            #[cfg(feature = "ffmpeg")]
            Kind::Vaapi | Kind::Nvenc | Kind::V4l2m2m | Kind::X264 => Backend::Ff(Box::new(crate::ff_encoder::FfEncoder::open(kind, cfg)?)),
            #[allow(unreachable_patterns)]
            _ => bail!("encoder {} is not compiled in (cargo features ffmpeg/openh264)", kind.name()),
        };
        let mut params = ParamSets::new();
        #[cfg(feature = "ffmpeg")]
        if let Backend::Ff(ff) = &backend {
            params.learn(&ff.extradata());
        }
        info!(
            "encoder {} opened: {}x{} @ {} fps, {} kbps CBR, GOP {}",
            kind.name(),
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_bps / 1000,
            cfg.gop_frames
        );
        Ok(Self { backend, kind, name: kind.name().to_string(), hardware: kind.hardware(), params, force_keyframe: false })
    }

    /// Tries `candidates` in order and keeps the first that opens.
    pub fn open_first_working(kinds: &[Kind], cfg: &EncoderConfig) -> Result<Self> {
        let mut errors = Vec::new();
        for &k in kinds {
            match Self::open(k, cfg) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    warn!("encoder {} unavailable: {e:#}", k.name());
                    errors.push(format!("{}: {e:#}", k.name()));
                }
            }
        }
        if errors.is_empty() {
            bail!("no H.264 encoder available (install FFmpeg with VA-API/x264, or build with the openh264 feature)");
        }
        bail!("no H.264 encoder could be opened ({})", errors.join("; "))
    }

    /// The next submitted frame becomes an IDR.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Encodes one NV12 frame of the configured size. `pts_us` must increase.
    pub fn submit(&mut self, nv12: &[u8], pts_us: i64, captured: Instant) -> Result<()> {
        let force = std::mem::take(&mut self.force_keyframe);
        match &mut self.backend {
            #[cfg(feature = "ffmpeg")]
            Backend::Ff(e) => e.submit(nv12, pts_us, captured, force),
            #[cfg(feature = "openh264")]
            Backend::Oh(e) => e.submit(nv12, pts_us, captured, force),
            Backend::None(n) => match *n {},
        }
    }

    /// Hands every finished frame to `sink`.
    pub fn poll_output(&mut self, mut sink: impl FnMut(EncodedFrame)) -> Result<()> {
        let mut raw: Vec<RawPacket> = Vec::new();
        match &mut self.backend {
            #[cfg(feature = "ffmpeg")]
            Backend::Ff(e) => e.poll(&mut raw)?,
            #[cfg(feature = "openh264")]
            Backend::Oh(e) => e.poll(&mut raw),
            Backend::None(n) => match *n {},
        }
        let now = Instant::now();
        for p in raw {
            if let Some((data, keyframe)) = self.params.fix(&p.data) {
                sink(EncodedFrame {
                    data,
                    keyframe,
                    capture_instant: p.captured,
                    encode_latency_us: now.duration_since(p.submitted).as_micros() as u64,
                });
            }
        }
        Ok(())
    }

    /// Changes the target bitrate of the running encoder when the back end supports it
    /// (libx264 and NVENC reconfigure between frames). `false` = reopen the encoder.
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> bool {
        match &mut self.backend {
            #[cfg(feature = "ffmpeg")]
            Backend::Ff(e) => e.set_bitrate(bitrate_bps),
            #[cfg(feature = "openh264")]
            Backend::Oh(_) => {
                let _ = bitrate_bps;
                false
            }
            Backend::None(n) => match *n {},
        }
    }

    /// Frames the encoder refused (queue full) since it was opened.
    pub fn dropped(&self) -> u64 {
        match &self.backend {
            #[cfg(feature = "ffmpeg")]
            Backend::Ff(e) => e.dropped,
            #[cfg(feature = "openh264")]
            Backend::Oh(_) => 0,
            Backend::None(n) => match *n {},
        }
    }
}
