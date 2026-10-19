//! H.264 decoding: a `VideoDecoder` trait (so a hardware decoder can be dropped in later),
//! the OpenH264 software implementation, and the decoder thread with its input queue.
//!
//! Why OpenH264 first: FFmpeg (`ffmpeg-next`) needs FFmpeg dev libraries plus libclang for
//! bindgen at build time, which this dev machine (and most classroom PCs) do not have.
//! OpenH264 is built from bundled C++ source by `cc`, needs nothing installed, and decodes
//! 1080p30 on one core of an old PC. Hardware decode (d3d11va/dxva2 on Windows,
//! vaapi/v4l2m2m on Linux/Raspberry Pi) is the planned second implementation of the trait.

use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use nanocast_core::au::is_keyframe;

use crate::assembler::Frame;
use crate::stats::Stats;

/// One decoded picture, planar YUV 4:2:0 (I420), BT.709 limited range from our sender.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub stride_y: u32,
    pub stride_uv: u32,
    /// When the frame's last packet arrived (for glass-to-glass style measurements).
    pub complete_at: Instant,
}

pub trait VideoDecoder: Send {
    fn name(&self) -> &str;
    /// Decodes one access unit (Annex B). `Ok(None)` = no picture out of this one (yet).
    fn decode(&mut self, au: &[u8], complete_at: Instant) -> Result<Option<Picture>>;
}

pub struct OpenH264Decoder {
    dec: openh264::decoder::Decoder,
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self { dec: openh264::decoder::Decoder::new().map_err(|e| anyhow!("openh264: {e}"))? })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn name(&self) -> &str {
        "openh264 (software)"
    }

    fn decode(&mut self, au: &[u8], complete_at: Instant) -> Result<Option<Picture>> {
        use openh264::formats::YUVSource;
        let Some(yuv) = self.dec.decode(au).map_err(|e| anyhow!("openh264 decode: {e}"))? else { return Ok(None) };
        let (w, h) = yuv.dimensions();
        let (sy, su, _) = yuv.strides();
        let ch = h.div_ceil(2);
        Ok(Some(Picture {
            width: w as u32,
            height: h as u32,
            y: yuv.y()[..sy * h].to_vec(),
            u: yuv.u()[..su * ch].to_vec(),
            v: yuv.v()[..su * ch].to_vec(),
            stride_y: sy as u32,
            stride_uv: su as u32,
            complete_at,
        }))
    }
}

/// Frames waiting for the decoder: 250 ms at 60 fps. More means the decoder is too slow for
/// the stream; latency would only grow, so the backlog is dropped and an IDR requested.
pub const MAX_PENDING: usize = 15;

/// Input queue between the network thread and the decoder thread (logic of `VideoDecoder.kt`
/// `submit`): frames are held back until the first keyframe, and when the decoder falls
/// `MAX_PENDING` frames behind the backlog is dropped and decoding resumes at the next IDR
/// rather than letting latency grow.
pub struct FrameQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    pending: VecDeque<Frame>,
    need_keyframe: bool,
    started: bool,
    closed: bool,
}

impl Default for FrameQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameQueue {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(QueueState { pending: VecDeque::new(), need_keyframe: true, started: false, closed: false }),
            ready: Condvar::new(),
        }
    }

    /// Network thread. `on_need_keyframe(reason)` is called (rate-limited by the control
    /// channel) whenever frames are discarded while waiting for an IDR.
    pub fn submit(&self, frame: Frame, stats: &Stats, mut on_need_keyframe: impl FnMut(&'static str)) {
        let mut s = self.state.lock().unwrap();
        let key = is_keyframe(&frame.data);
        if s.pending.len() >= MAX_PENDING {
            stats.dropped_frames.fetch_add(s.pending.len() as u64, Relaxed);
            s.pending.clear();
            s.need_keyframe = true;
        }
        if s.need_keyframe {
            if !key {
                stats.dropped_frames.fetch_add(1, Relaxed);
                on_need_keyframe(if s.started { "decoder_backlog" } else { "decoder_start" });
                return;
            }
            s.need_keyframe = false;
            s.started = true;
        }
        s.pending.push_back(frame);
        self.ready.notify_one();
    }

    /// Starts over at the next keyframe (new stream, decoder error).
    pub fn reset(&self) {
        let mut s = self.state.lock().unwrap();
        s.pending.clear();
        s.need_keyframe = true;
        s.started = false;
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    /// Decoder thread: blocks for the next frame; `None` once closed.
    pub fn next(&self) -> Option<Frame> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.closed {
                return None;
            }
            if let Some(f) = s.pending.pop_front() {
                return Some(f);
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().pending.len()
    }
}

/// Latest decoded picture, replaced by each new one: the renderer always shows the newest
/// picture and never queues old ones.
pub type LatestPicture = Arc<Mutex<Option<Picture>>>;

/// Runs the decoder until the queue is closed. `on_picture` wakes the renderer.
pub fn run_decoder(
    mut dec: Box<dyn VideoDecoder>,
    queue: Arc<FrameQueue>,
    latest: LatestPicture,
    stats: Arc<Stats>,
    mut on_error: impl FnMut(&'static str),
    on_picture: impl Fn(),
) {
    log::info!("decoder: {}", dec.name());
    let mut consecutive_errors = 0u32;
    while let Some(frame) = queue.next() {
        let t = Instant::now();
        let result = dec.decode(&frame.data, frame.complete_at);
        stats.on_decode_call(t.elapsed().as_micros() as u64);
        match result {
            Ok(Some(pic)) => {
                consecutive_errors = 0;
                stats.on_decoded(frame.complete_at.elapsed().as_micros() as u64);
                *latest.lock().unwrap() = Some(pic);
                on_picture();
            }
            Ok(None) => consecutive_errors = 0,
            Err(e) => {
                // A frame after a lost one references a missing picture: OpenH264 conceals it
                // and may still report an error. Ask for an IDR; after many in a row, restart.
                stats.decoder_errors.fetch_add(1, Relaxed);
                consecutive_errors += 1;
                log::debug!("frame {}: {e}", frame.frame_id);
                on_error("decoder_start");
                if consecutive_errors >= 30 {
                    log::warn!("decoder keeps failing ({e}); restarting at the next keyframe");
                    match OpenH264Decoder::new() {
                        Ok(d) => dec = Box::new(d),
                        Err(e) => log::error!("decoder restart failed: {e}"),
                    }
                    queue.reset();
                    consecutive_errors = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: i64, key: bool) -> Frame {
        let nal = if key { 0x65 } else { 0x41 };
        Frame { frame_id: id, data: vec![0, 0, 0, 1, nal, 0x88], complete_at: Instant::now() }
    }

    #[test]
    fn waits_for_keyframe_then_flushes_backlog() {
        let (q, stats) = (FrameQueue::new(), Stats::new());
        let mut reasons = Vec::new();
        q.submit(frame(0, false), &stats, |r| reasons.push(r));
        assert_eq!((q.len(), reasons.as_slice()), (0, &["decoder_start"][..]));
        q.submit(frame(1, true), &stats, |r| reasons.push(r));
        for i in 2..(MAX_PENDING as i64 + 1) {
            q.submit(frame(i, false), &stats, |r| reasons.push(r));
        }
        assert_eq!(q.len(), MAX_PENDING);
        // One more P-frame: backlog dropped, waiting for an IDR again.
        q.submit(frame(99, false), &stats, |r| reasons.push(r));
        assert_eq!(q.len(), 0);
        assert_eq!(reasons.last(), Some(&"decoder_backlog"));
        assert_eq!(stats.dropped_frames.load(Relaxed), 1 + MAX_PENDING as u64 + 1);
        q.submit(frame(100, true), &stats, |r| reasons.push(r));
        assert_eq!(q.next().unwrap().frame_id, 100);
        q.close();
        assert!(q.next().is_none());
    }

    /// Encodes a few frames with OpenH264 and decodes them back through the trait.
    #[test]
    fn openh264_roundtrip() {
        use openh264::encoder::Encoder;
        use openh264::formats::YUVBuffer;
        let (w, h) = (320usize, 240usize);
        let mut enc = Encoder::new().unwrap();
        let mut dec = OpenH264Decoder::new().unwrap();
        let mut got = 0;
        for i in 0..5u8 {
            let mut yuv = vec![128u8; w * h * 3 / 2];
            yuv[..w * h].fill(16 + i * 40);
            let buf = YUVBuffer::from_vec(yuv, w, h);
            let bits = enc.encode(&buf).unwrap().to_vec();
            if let Some(p) = dec.decode(&bits, Instant::now()).unwrap() {
                assert_eq!((p.width, p.height), (w as u32, h as u32));
                assert!(p.stride_y as usize >= w && p.y.len() >= p.stride_y as usize * h);
                let luma = p.y[(h / 2) * p.stride_y as usize + w / 2] as i32;
                assert!((luma - (16 + i as i32 * 40)).abs() < 12, "frame {i}: luma {luma}");
                got += 1;
            }
        }
        assert_eq!(got, 5);
    }
}
