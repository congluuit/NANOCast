//! Audio path: UDP AUDIO_PORT (one 20 ms Opus frame per packet, `protocol::AudioHeader`)
//! -> `nanocast_core::audio::OpusDecoder` -> small jitter buffer -> cpal output.
//!
//! The buffer holds ~40 ms before playback starts (and again after an underrun) and is
//! trimmed back when it grows past ~120 ms, so sound never drifts behind the picture.
//! An empty payload is 20 ms of silence (sender's digital-silence packets).

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use nanocast_core::audio::{OpusDecoder, Resampler, FRAME_LEN};
use nanocast_core::protocol::{AudioHeader, AUDIO_FRAME_SAMPLES, AUDIO_HEADER_LEN, AUDIO_SAMPLE_RATE};

use crate::stats::Stats;

/// Start (or restart after an underrun) once this much is buffered.
const PREFILL_MS: usize = 40;
/// Drop the oldest audio beyond this, to keep latency bounded.
const MAX_MS: usize = 120;
/// Gaps up to this many packets are filled with silence to keep timing; larger ones are skipped.
const MAX_FILL_PACKETS: u32 = 3;

/// Interleaved stereo samples at the device rate, shared with the cpal callback.
struct Jitter {
    buf: VecDeque<f32>,
    playing: bool,
    prefill: usize,
    max: usize,
    underruns: u64,
}

impl Jitter {
    fn new(rate: u32) -> Self {
        let per_ms = rate as usize * 2 / 1000;
        Self { buf: VecDeque::new(), playing: false, prefill: PREFILL_MS * per_ms, max: MAX_MS * per_ms, underruns: 0 }
    }

    fn push(&mut self, stereo: &[f32]) {
        self.buf.extend(stereo);
        if self.buf.len() > self.max {
            let excess = self.buf.len() - self.prefill;
            self.buf.drain(..excess - excess % 2);
        }
    }

    /// Fills `out` (interleaved, `channels` per frame) from the buffer, zeros when empty.
    fn pull(&mut self, out: &mut [f32], channels: usize) {
        if !self.playing && self.buf.len() >= self.prefill {
            self.playing = true;
        }
        for frame in out.chunks_exact_mut(channels) {
            let (l, r) = if self.playing && self.buf.len() >= 2 {
                (self.buf.pop_front().unwrap(), self.buf.pop_front().unwrap())
            } else {
                if self.playing {
                    self.playing = false;
                    self.underruns += 1;
                }
                (0.0, 0.0)
            };
            match channels {
                1 => frame[0] = (l + r) * 0.5,
                _ => {
                    frame[0] = l;
                    frame[1] = r;
                    frame[2..].fill(0.0);
                }
            }
        }
    }
}

struct Output {
    _stream: cpal::Stream,
    jitter: Arc<Mutex<Jitter>>,
    resampler: Resampler,
}

fn open_output() -> Result<Output> {
    let host = cpal::default_host();
    let device = host.default_output_device().context("no audio output device")?;
    let supported = device.default_output_config().context("audio output config")?;
    let format = supported.sample_format();
    let mut config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let jitter = Arc::new(Mutex::new(Jitter::new(rate)));
    // Ask for a 10 ms device buffer; fall back to the default if the device refuses.
    let mut stream = None;
    for size in [cpal::BufferSize::Fixed(rate / 100), cpal::BufferSize::Default] {
        config.buffer_size = size;
        match build_stream(&device, &config, format, channels, jitter.clone()) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => log::debug!("audio buffer {size:?}: {e}"),
        }
    }
    let stream = stream.context("opening audio output")?;
    stream.play().context("starting audio output")?;
    log::info!("audio out: {rate} Hz, {channels} ch, {format:?}, buffer {:?}", config.buffer_size);
    Ok(Output { _stream: stream, jitter, resampler: Resampler::new(AUDIO_SAMPLE_RATE, rate) })
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    jitter: Arc<Mutex<Jitter>>,
) -> Result<cpal::Stream> {
    let err = |e| log::warn!("audio output: {e}");
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            *config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| jitter.lock().unwrap().pull(out, channels),
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => {
            let mut tmp = Vec::new();
            device.build_output_stream(
                *config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    tmp.resize(out.len(), 0.0);
                    jitter.lock().unwrap().pull(&mut tmp, channels);
                    for (o, s) in out.iter_mut().zip(&tmp) {
                        *o = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err,
                None,
            )?
        }
        f => bail!("unsupported output sample format {f:?}"),
    };
    Ok(stream)
}

/// Receives and plays audio until the process exits. The output device is opened on the
/// first packet (so a receiver without sound hardware still shows video).
pub fn run(bind: SocketAddr, stats: Arc<Stats>) -> Result<()> {
    let sock = UdpSocket::bind(bind).with_context(|| format!("binding audio UDP {bind}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    log::info!("audio listening on UDP {bind}");
    let mut dec = OpusDecoder::new()?;
    let mut out: Option<Output> = None;
    let mut out_failed = false;
    let mut buf = [0u8; 2048];
    let mut pcm = vec![0f32; FRAME_LEN];
    let mut resampled = Vec::with_capacity(FRAME_LEN * 2);
    let silence = vec![0f32; FRAME_LEN];
    let mut last_seq: Option<u32> = None;
    loop {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                last_seq = None; // stream paused: next packet starts fresh
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e.into()),
        };
        let Some(h) = AudioHeader::parse(&buf[..n]) else { continue };
        stats.audio_packets.fetch_add(1, Relaxed);
        let mut fill = 0;
        if let Some(prev) = last_seq {
            let gap = h.seq.wrapping_sub(prev);
            if gap == 0 || gap > u32::MAX / 2 {
                continue; // duplicate or late
            }
            if gap > 1 {
                stats.audio_lost.fetch_add((gap - 1) as u64, Relaxed);
                fill = (gap - 1).min(MAX_FILL_PACKETS);
            }
        }
        last_seq = Some(h.seq);

        if out.is_none() && !out_failed {
            match open_output() {
                Ok(o) => out = Some(o),
                Err(e) => {
                    log::warn!("no audio output, dropping sound: {e:#}");
                    out_failed = true;
                }
            }
        }
        let Some(o) = out.as_mut() else { continue };

        let payload = &buf[AUDIO_HEADER_LEN..n];
        let samples = if payload.is_empty() {
            pcm.fill(0.0);
            AUDIO_FRAME_SAMPLES
        } else {
            match dec.decode(payload, &mut pcm) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("opus: {e}");
                    continue;
                }
            }
        };
        resampled.clear();
        for _ in 0..fill {
            o.resampler.process(&silence, &mut resampled);
        }
        o.resampler.process(&pcm[..samples * 2], &mut resampled);
        let mut j = o.jitter.lock().unwrap();
        j.push(&resampled);
        stats.audio_underruns.store(j.underruns, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_prefill_underrun_and_trim() {
        let mut j = Jitter::new(48_000); // 96 samples per ms
        let mut out = vec![1.0f32; 2 * 480]; // 10 ms stereo
        j.push(&vec![0.5; 96 * 20]);
        j.pull(&mut out, 2);
        assert!(out.iter().all(|&s| s == 0.0), "below prefill: silence");
        j.push(&vec![0.5; 96 * 20]);
        j.pull(&mut out, 2);
        assert!(out.iter().all(|&s| s == 0.5));
        // Drain past the end: underrun, then silence until refilled.
        let mut big = vec![0.0f32; 2 * 48_000];
        j.pull(&mut big, 2);
        assert_eq!(j.underruns, 1);
        assert!(!j.playing);
        // Too much buffered: trimmed back to the prefill level.
        j.push(&vec![0.1; 96 * 200]);
        assert_eq!(j.buf.len(), 96 * PREFILL_MS);
        // Mono and 6-channel devices.
        let mut mono = vec![0.0f32; 4];
        j.pull(&mut mono, 1);
        assert!(mono.iter().all(|&s| (s - 0.1).abs() < 1e-6));
        let mut six = vec![9.0f32; 12];
        j.pull(&mut six, 6);
        assert_eq!(&six[..6], &[0.1, 0.1, 0.0, 0.0, 0.0, 0.0]);
    }
}
