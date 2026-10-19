//! Platform-neutral half of the audio path (PLAN.md section 4): PCM from any loopback
//! capture -> stereo -> 48 kHz -> 20 ms Opus frames -> numbered UDP packets.
//!
//! Opus comes from `unsafe-libopus` (libopus 1.3.1 translated to Rust), so no C toolchain
//! or CMake is needed and the same code builds for Windows, Android test tools and later
//! macOS/Linux senders.

use anyhow::{bail, Result};
use unsafe_libopus as opus;
use unsafe_libopus::varargs::{VarArg, VarArgs};

use crate::protocol::{
    AudioHeader, AUDIO_CHANNELS, AUDIO_FRAME_MS, AUDIO_FRAME_SAMPLES, AUDIO_HEADER_LEN, AUDIO_MAX_PAYLOAD,
    AUDIO_SAMPLE_RATE,
};

/// Interleaved stereo samples in one 20 ms frame.
pub const FRAME_LEN: usize = AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS;

/// Opus encoder for 48 kHz stereo, application Audio, VBR. No DTX: libopus 1.3.1 only
/// does DTX in SILK (voice) mode; silence is handled by `AudioEncoder` instead.
pub struct OpusEncoder {
    st: *mut opus::OpusEncoder,
}

// The encoder state is plain memory owned by this struct.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    /// `complexity` 0..=10; low values keep the sender's CPU use small.
    pub fn new(bitrate_bps: i32, complexity: i32) -> Result<Self> {
        let mut err = 0;
        let st = unsafe {
            opus::opus_encoder_create(AUDIO_SAMPLE_RATE as i32, AUDIO_CHANNELS as i32, opus::OPUS_APPLICATION_AUDIO, &mut err)
        };
        if st.is_null() || err != opus::OPUS_OK {
            bail!("opus_encoder_create failed ({err})");
        }
        let enc = Self { st };
        enc.ctl(opus::OPUS_SET_BITRATE_REQUEST, bitrate_bps)?;
        enc.ctl(opus::OPUS_SET_COMPLEXITY_REQUEST, complexity.clamp(0, 10))?;
        enc.ctl(opus::OPUS_SET_VBR_REQUEST, 1)?;
        Ok(enc)
    }

    fn ctl(&self, request: i32, value: i32) -> Result<()> {
        let r = unsafe { opus::opus_encoder_ctl_impl(self.st, request, VarArgs::new(vec![VarArg::I32(value)])) };
        if r != opus::OPUS_OK {
            bail!("opus ctl {request} = {value} failed ({r})");
        }
        Ok(())
    }

    /// Encoder delay in samples at 48 kHz (the receiver's OpusHead pre-skip).
    pub fn lookahead(&self) -> i32 {
        let mut v = 0;
        unsafe { opus::opus_encoder_ctl_impl(self.st, opus::OPUS_GET_LOOKAHEAD_REQUEST, VarArgs::new(vec![VarArg::I32Out(&mut v)])) };
        v
    }

    /// Encodes one 20 ms frame (`FRAME_LEN` interleaved samples) into `out`; returns its length.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        assert_eq!(pcm.len(), FRAME_LEN);
        let max = out.len().min(AUDIO_MAX_PAYLOAD) as i32;
        let n = unsafe { opus::opus_encode_float(self.st, pcm.as_ptr(), AUDIO_FRAME_SAMPLES as i32, out.as_mut_ptr(), max) };
        if n < 0 {
            bail!("opus_encode_float failed ({n})");
        }
        Ok(n as usize)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { opus::opus_encoder_destroy(self.st) };
    }
}

/// Opus decoder for 48 kHz stereo (tests and a future desktop receiver).
pub struct OpusDecoder {
    st: *mut opus::OpusDecoder,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        let st = unsafe { opus::opus_decoder_create(AUDIO_SAMPLE_RATE as i32, AUDIO_CHANNELS as i32, &mut err) };
        if st.is_null() || err != opus::OPUS_OK {
            bail!("opus_decoder_create failed ({err})");
        }
        Ok(Self { st })
    }

    /// Decodes one packet into interleaved stereo; returns samples per channel.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        let frames = (out.len() / AUDIO_CHANNELS) as i32;
        let n = unsafe { opus::opus_decode_float(self.st, packet.as_ptr(), packet.len() as i32, out.as_mut_ptr(), frames, 0) };
        if n < 0 {
            bail!("opus_decode_float failed ({n})");
        }
        Ok(n as usize)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { opus::opus_decoder_destroy(self.st) };
    }
}

/// Appends `input` (interleaved, `channels` per frame) to `out` as interleaved stereo.
/// Mono is duplicated; for 3+ channels (5.1/7.1) the centre is mixed into both sides at
/// -3 dB and surround channels are dropped (good enough for a classroom TV).
pub fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => input.iter().for_each(|&s| out.extend_from_slice(&[s, s])),
        2 => out.extend_from_slice(input),
        n => {
            for f in input.chunks_exact(n) {
                let c = f[2] * std::f32::consts::FRAC_1_SQRT_2;
                out.extend_from_slice(&[(f[0] + c).clamp(-1.0, 1.0), (f[1] + c).clamp(-1.0, 1.0)]);
            }
        }
    }
}

/// Streaming linear-interpolation resampler for interleaved stereo. Shared-mode WASAPI
/// is almost always 48 kHz already (then this is a copy); 44.1 kHz devices get resampled.
/// Linear interpolation is not hi-fi but is cheap and fine for speech/video sound.
pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame, in input frames, relative to `prev` (index -1).
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, pos: 0.0, prev: [0.0; 2] }
    }

    pub fn is_passthrough(&self) -> bool {
        self.step == 1.0
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        let frame = |i: isize| -> [f32; 2] {
            if i < 0 {
                self.prev
            } else {
                [input[i as usize * 2], input[i as usize * 2 + 1]]
            }
        };
        // Interpolate between frame floor(pos) and the next one, both inside [-1, n-1].
        while self.pos < (n - 1) as f64 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let (a, b) = (frame(i as isize), frame(i as isize + 1));
            out.extend_from_slice(&[a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = frame(n as isize - 1);
    }
}

/// Numbers Opus frames into audio packets (header + payload).
#[derive(Default)]
pub struct AudioPacketizer {
    seq: u32,
    frames: u32,
}

impl AudioPacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Continues numbering at `seq` (audio switched back on in the same cast), so the
    /// receiver does not see a jump backwards.
    pub fn starting_at(seq: u32) -> Self {
        Self { seq, frames: seq }
    }

    pub fn packet(&mut self, opus: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0u8; AUDIO_HEADER_LEN + opus.len()];
        AudioHeader { seq: self.seq, pts_ms: self.frames.wrapping_mul(AUDIO_FRAME_MS) }.write(&mut pkt);
        pkt[AUDIO_HEADER_LEN..].copy_from_slice(opus);
        self.seq = self.seq.wrapping_add(1);
        self.frames = self.frames.wrapping_add(1);
        pkt
    }
}

/// Collects arbitrary-sized 48 kHz stereo chunks and hands out encoded 20 ms packets.
pub struct AudioEncoder {
    opus: OpusEncoder,
    packetizer: AudioPacketizer,
    pending: Vec<f32>,
    scratch: Vec<u8>,
    silent_frames: u64,
}

impl AudioEncoder {
    pub fn new(bitrate_bps: i32, complexity: i32) -> Result<Self> {
        Self::starting_at(bitrate_bps, complexity, 0)
    }

    /// Like `new`, with packet numbers continuing at `seq`.
    pub fn starting_at(bitrate_bps: i32, complexity: i32, seq: u32) -> Result<Self> {
        Ok(Self {
            opus: OpusEncoder::new(bitrate_bps, complexity)?,
            packetizer: AudioPacketizer::starting_at(seq),
            pending: Vec::with_capacity(FRAME_LEN * 4),
            scratch: vec![0u8; AUDIO_MAX_PAYLOAD],
            silent_frames: 0,
        })
    }

    pub fn lookahead(&self) -> i32 {
        self.opus.lookahead()
    }

    /// Samples waiting for a full frame (interleaved).
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Drops buffered samples (after a gap, so stale sound is not played late).
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Adds samples and calls `send` with every complete packet.
    pub fn push(&mut self, pcm: &[f32], mut send: impl FnMut(Vec<u8>)) -> Result<()> {
        self.pending.extend_from_slice(pcm);
        let mut used = 0;
        while self.pending.len() - used >= FRAME_LEN {
            let frame = &self.pending[used..used + FRAME_LEN];
            // Digital silence (nothing playing): an empty payload, see `push_silence`.
            let n = if is_silent(frame) { 0 } else { self.opus.encode(frame, &mut self.scratch)? };
            self.silent_frames += (n == 0) as u64;
            send(self.packetizer.packet(&self.scratch[..n]));
            used += FRAME_LEN;
        }
        self.pending.drain(..used);
        Ok(())
    }

    /// Sends one 20 ms silence packet: header only, no Opus payload. The receiver plays
    /// 20 ms of zeros for it without decoding. Packets keep flowing while nothing plays
    /// (WASAPI loopback then delivers zeros or nothing at all), so the receiver keeps its
    /// clock and buffer, and silence costs 10 B / 20 ms = 4 kbps instead of ~96 kbps
    /// (Opus CELT does not shrink silent frames).
    pub fn push_silence(&mut self, mut send: impl FnMut(Vec<u8>)) {
        self.pending.clear();
        self.silent_frames += 1;
        send(self.packetizer.packet(&[]));
    }

    /// Frames sent as silence packets so far.
    pub fn silent_frames(&self) -> u64 {
        self.silent_frames
    }
}

/// True if every sample is (almost) exactly zero, i.e. nothing is playing.
fn is_silent(frame: &[f32]) -> bool {
    frame.iter().all(|s| s.abs() < 1.0 / 65536.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, rate: u32, hz: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 0.5;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn stereo_downmix() {
        let mut out = Vec::new();
        to_stereo(&[0.1, 0.2], 1, &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2]);
        out.clear();
        // 5.1: FL FR C LFE SL SR
        to_stereo(&[0.5, -0.5, 0.0, 1.0, 1.0, 1.0], 6, &mut out);
        assert_eq!(out, [0.5, -0.5]);
    }

    #[test]
    fn resampler_lengths_and_continuity() {
        let mut r = Resampler::new(44_100, 48_000);
        let input = tone(44_100, 44_100, 440.0);
        let mut out = Vec::new();
        // Feed in uneven chunks like a capture callback would.
        for chunk in input.chunks(441 * 2 + 6) {
            r.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_001).contains(&frames), "{frames}");
        // A 440 Hz sine at 48 kHz moves at most ~0.029 per sample; no jumps at chunk edges.
        let max_step = out.chunks(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.04, "{max_step}");

        let mut same = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        same.process(&[1.0, 2.0], &mut out);
        assert_eq!(out, [1.0, 2.0]);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut enc = AudioEncoder::new(96_000, 3).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let input = tone(48_000, 48_000, 1000.0);
        let mut packets = Vec::new();
        for chunk in input.chunks(480 * 2) {
            enc.push(chunk, |p| packets.push(p)).unwrap();
        }
        assert_eq!(packets.len(), 50);
        assert_eq!(enc.pending(), 0);
        let h = AudioHeader::parse(&packets[49]).unwrap();
        assert_eq!((h.seq, h.pts_ms), (49, 980));
        let avg = packets.iter().map(|p| p.len() - AUDIO_HEADER_LEN).sum::<usize>() / packets.len();
        assert!((150..400).contains(&avg), "avg payload {avg} B at 96 kbps");

        let mut pcm = vec![0f32; FRAME_LEN];
        let mut energy = 0.0;
        for p in &packets[10..] {
            assert_eq!(dec.decode(&p[AUDIO_HEADER_LEN..], &mut pcm).unwrap(), AUDIO_FRAME_SAMPLES);
            energy += pcm.iter().map(|s| s * s).sum::<f32>();
        }
        let rms = (energy / (40 * FRAME_LEN) as f32).sqrt();
        assert!((0.3..0.42).contains(&rms), "rms {rms} (sine 0.5 -> 0.354)");

        // Silence (pushed zeros or explicit) is header-only and still numbered.
        let mut silent = Vec::new();
        enc.push(&[0.0; FRAME_LEN], |p| silent.push(p)).unwrap();
        enc.push_silence(|p| silent.push(p));
        assert!(silent.iter().all(|p| p.len() == AUDIO_HEADER_LEN));
        assert_eq!(AudioHeader::parse(&silent[1]).unwrap().pts_ms, 51 * 20);
        assert_eq!(enc.silent_frames(), 2);
        // Hard-coded as OPUS_PRE_SKIP in the Android receiver (Protocol.kt).
        assert_eq!(enc.lookahead(), 312);
    }
}
