//! System sound to the TV: WASAPI loopback of the default output device (cpal) ->
//! stereo 48 kHz -> Opus 20 ms (`nanocast_core::audio`) -> UDP AUDIO_PORT.
//!
//! An `AudioSender` owns one thread, which owns the cpal stream. Dropping (or
//! `stop_detached`) ends the thread and releases the WASAPI client, so audio off costs
//! no CPU. Starting never blocks the caller: the device is opened on the audio thread,
//! so toggling audio during a cast cannot stall the video loop.
//!
//! Silence: when nothing plays, WASAPI loopback delivers zeros or no callbacks at all.
//! The thread then keeps sending one header-only "silence" packet every 20 ms (4 kbps)
//! so the receiver's clock and buffer stay steady.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use log::{info, warn};
use nanocast_core::audio::{to_stereo, AudioEncoder, Resampler};
use nanocast_core::protocol::{AUDIO_BITRATE, AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE};

/// Low complexity keeps the encoder at a fraction of one core (see docs/benchmark.md).
pub const OPUS_COMPLEXITY: i32 = 3;
/// No capture data for this long = nothing is playing: switch to silence packets.
const SILENCE_AFTER: Duration = Duration::from_millis(60);
/// Samples queued between the capture callback and the encoder beyond this are dropped
/// (the encoder fell behind; better a click than growing delay).
const MAX_PENDING_MS: usize = 100;

/// Counters shared with the session (stats) and the GUI.
#[derive(Debug, Default)]
pub struct AudioCounters {
    pub bytes: AtomicU64,
    pub packets: AtomicU64,
    pub silent_packets: AtomicU64,
    /// Capture is open and delivering (or waiting for) sound.
    pub running: AtomicBool,
    pub error: Mutex<Option<String>>,
    /// e.g. "Speakers (Realtek) 48000 Hz 2 ch f32".
    pub device: Mutex<Option<String>>,
}

pub struct AudioSender {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSender {
    /// Starts capturing and sending to `addr` (the receiver's AUDIO_PORT) in the background.
    /// Failures (no output device...) end up in `counters.error`.
    pub fn start(addr: SocketAddr, counters: Arc<AudioCounters>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    *counters.error.lock().unwrap() = None;
                    if let Err(e) = run(addr, &stop, &counters) {
                        warn!("audio: {e:#}");
                        *counters.error.lock().unwrap() = Some(format!("{e:#}"));
                    }
                    counters.running.store(false, Ordering::Relaxed);
                })
                .expect("spawn audio thread")
        };
        Self { stop, thread: Some(thread) }
    }

    /// Stops without waiting for the thread (it exits within ~20 ms and releases the
    /// device on its own). Used when the user switches audio off mid-cast.
    pub fn stop_detached(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take();
    }
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(addr: SocketAddr, stop: &AtomicBool, counters: &AudioCounters) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("audio UDP socket")?;
    sock.connect(addr).with_context(|| format!("audio UDP to {addr}"))?;

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| anyhow!("no default audio output device"))?;
    // Shared-mode loopback must use the device mix format; resample afterwards if needed.
    let supported = device.default_output_config().context("output device format")?;
    let format = supported.sample_format();
    let config = supported.config();
    let (rate, channels) = (config.sample_rate, config.channels as usize);
    let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into());
    let desc = format!("{name} {rate} Hz {channels} ch {format}");
    info!("audio loopback: {desc} -> {addr}");
    *counters.device.lock().unwrap() = Some(desc);

    // Callback -> this thread. Each chunk is one WASAPI period (~10 ms), stereo 48 kHz.
    let (tx, rx) = sync_channel::<Vec<f32>>(32);
    let mut resampler = Resampler::new(rate, AUDIO_SAMPLE_RATE);
    let mut stereo = Vec::new();
    let convert = move |mono_or_more: &[f32], out: &mut Vec<f32>| {
        stereo.clear();
        to_stereo(mono_or_more, channels, &mut stereo);
        resampler.process(&stereo, out);
    };
    let err_fn = |e: cpal::Error| warn!("audio stream error: {e}");
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, tx, convert, err_fn),
        SampleFormat::I16 => build::<i16>(&device, &config, tx, convert, err_fn),
        SampleFormat::I32 => build::<i32>(&device, &config, tx, convert, err_fn),
        SampleFormat::U16 => build::<u16>(&device, &config, tx, convert, err_fn),
        other => bail!("unsupported loopback sample format {other}"),
    }
    .context("open WASAPI loopback")?;
    stream.play().context("start WASAPI loopback")?;
    counters.running.store(true, Ordering::Relaxed);

    // Numbering continues across off/on within a cast (`packets` is per cast).
    let first_seq = counters.packets.load(Ordering::Relaxed) as u32;
    let mut encoder = AudioEncoder::starting_at(AUDIO_BITRATE, OPUS_COMPLEXITY, first_seq)?;
    let frame = Duration::from_millis(AUDIO_FRAME_MS as u64);
    let max_pending = AUDIO_SAMPLE_RATE as usize * 2 * MAX_PENDING_MS / 1000;
    let mut send_err_logged = false;
    let mut send = |pkt: Vec<u8>| {
        match sock.send(&pkt) {
            Ok(_) => {}
            // Receiver not listening yet (ICMP port unreachable): keep going.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => {
                if !send_err_logged {
                    warn!("audio send: {e}");
                    send_err_logged = true;
                }
            }
        }
        counters.packets.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
    };

    let started = Instant::now();
    let mut last_data = Instant::now();
    let mut next_silence = Instant::now() + SILENCE_AFTER;
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(chunk) => {
                last_data = Instant::now();
                if encoder.pending() > max_pending {
                    warn!("audio encoder behind; dropping {} ms", encoder.pending() * 1000 / 2 / AUDIO_SAMPLE_RATE as usize);
                    encoder.clear_pending();
                }
                encoder.push(&chunk, &mut send)?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("audio stream closed"),
        }
        // Nothing captured lately: keep the stream alive with silence packets on a 20 ms clock.
        let now = Instant::now();
        if now.duration_since(last_data) >= SILENCE_AFTER {
            if next_silence < last_data + SILENCE_AFTER {
                next_silence = last_data + SILENCE_AFTER;
            }
            while next_silence <= now {
                encoder.push_silence(&mut send);
                next_silence += frame;
            }
        }
    }
    counters.silent_packets.fetch_add(encoder.silent_frames(), Ordering::Relaxed);
    drop(stream);
    let secs = started.elapsed().as_secs_f64();
    info!(
        "audio loopback stopped after {secs:.1} s: {} packets ({} silent), encoder thread CPU {:.2} % of one core",
        counters.packets.load(Ordering::Relaxed),
        encoder.silent_frames(),
        thread_cpu_secs() / secs * 100.0
    );
    Ok(())
}

/// CPU time (user + kernel) used so far by the calling thread.
fn thread_cpu_secs() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    match unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } {
        Ok(()) => ft(k) + ft(u),
        Err(_) => 0.0,
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    mut convert: impl FnMut(&[f32], &mut Vec<f32>) + Send + 'static,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let mut float = Vec::new();
    let stream = device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            float.clear();
            float.extend(data.iter().map(|s| s.to_sample::<f32>()));
            let mut out = Vec::with_capacity(float.len() + 16);
            convert(&float, &mut out);
            // Full queue = encoder thread stalled; drop rather than block the audio engine.
            let _ = tx.try_send(out);
        },
        err_fn,
        Some(Duration::from_secs(2)),
    )?;
    Ok(stream)
}
