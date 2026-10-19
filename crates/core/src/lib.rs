//! NANOCast core: wire protocol, control channel, adaptive bitrate logic, H.264 access-unit helpers, Opus audio packets,
//! LAN discovery, and the
//! capture/encoder traits that platform crates implement.

pub mod adapt;
pub mod au;
pub mod audio;
pub mod control;
pub mod control_link;
pub mod discovery;
pub mod protocol;

/// One captured video frame, already in the encoder input format.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// Capture timestamp in microseconds (monotonic).
    pub pts_us: u64,
    /// NV12 pixel data (CPU path). The GPU path will carry a texture handle instead.
    pub nv12: Vec<u8>,
}

/// Encoded access unit (one frame) ready to packetize.
pub struct EncodedFrame {
    pub pts_us: u64,
    pub keyframe: bool,
    /// H.264 Annex B byte stream.
    pub data: Vec<u8>,
}

pub trait Capturer {
    fn next_frame(&mut self) -> anyhow::Result<Option<VideoFrame>>;
}

pub trait Encoder {
    fn encode(&mut self, frame: &VideoFrame) -> anyhow::Result<Vec<EncodedFrame>>;
    fn request_keyframe(&mut self);
}
