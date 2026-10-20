//! System sound for Google Cast: WASAPI loopback (cpal) -> stereo 48 kHz f32, handed out
//! in fixed blocks on the wall clock (1024 samples for AAC, 960 for Opus).
//!
//! The clock thread, not the sound card, decides when a block is due: the stream timeline
//! never stalls when nothing plays (loopback then delivers nothing) and sound / picture
//! share one clock. Missing samples are filled with silence, extra ones (device clock
//! drift) are dropped once more than `MAX_QUEUE_MS` is waiting. Muted = silence blocks.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use log::{info, warn};
use nanocast_core::audio::{to_stereo, Resampler};

pub const RATE: u32 = 48_000;
const MAX_QUEUE_MS: usize = 120;

/// One block: interleaved stereo, and its position in samples since `t0`.
pub type OnBlock = Box<dyn FnMut(&[f32], u64) -> Result<()>>;
/// Builds the block handler on the sound thread (COM objects such as the AAC encoder must
/// live on the thread that uses them).
pub type MakeOnBlock = Box<dyn FnOnce() -> Result<OnBlock> + Send>;

pub struct SoundClock {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SoundClock {
    /// Starts at the next block boundary after now; blocks are numbered from `t0`.
    pub fn start(t0: Instant, block: usize, enabled: Arc<AtomicBool>, make: MakeOnBlock) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, error) = (stop.clone(), error.clone());
            std::thread::Builder::new()
                .name("cast-sound".into())
                .spawn(move || {
                    let mut on_block = match make() {
                        Ok(f) => f,
                        Err(e) => {
                            warn!("sound encoder: {e:#}");
                            *error.lock().unwrap() = Some(format!("{e:#}"));
                            return;
                        }
                    };
                    let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
                    // The device may be missing: then the track carries silence.
                    let stream = match open_loopback(queue.clone()) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            warn!("sound capture unavailable: {e:#}");
                            *error.lock().unwrap() = Some(format!("{e:#}"));
                            None
                        }
                    };
                    let start = t0.elapsed().as_secs_f64() * RATE as f64;
                    let mut pos = ((start as u64) / block as u64 + 1) * block as u64;
                    let mut pcm = vec![0f32; block * 2];
                    let max_queue = RATE as usize * 2 * MAX_QUEUE_MS / 1000;
                    while !stop.load(Ordering::Relaxed) {
                        let now = (t0.elapsed().as_secs_f64() * RATE as f64) as u64;
                        if now < pos + block as u64 {
                            std::thread::sleep(Duration::from_millis(4));
                            continue;
                        }
                        {
                            let mut q = queue.lock().unwrap();
                            if q.len() > max_queue {
                                let extra = q.len() - max_queue / 2;
                                q.drain(..extra & !1);
                            }
                            let take = q.len().min(block * 2);
                            for (i, s) in q.drain(..take).enumerate() {
                                pcm[i] = s;
                            }
                            pcm[take..].fill(0.0);
                        }
                        if !enabled.load(Ordering::Relaxed) || stream.is_none() {
                            pcm.fill(0.0);
                        }
                        if let Err(e) = on_block(&pcm, pos) {
                            warn!("sound encoder: {e:#}");
                            *error.lock().unwrap() = Some(format!("{e:#}"));
                            break;
                        }
                        pos += block as u64;
                        // Far behind (thread starved): jump ahead rather than burst.
                        if now > pos + RATE as u64 / 2 {
                            pos = now / block as u64 * block as u64;
                        }
                    }
                    drop(stream);
                })
                .expect("spawn sound thread")
        };
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for SoundClock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open_loopback(queue: Arc<Mutex<VecDeque<f32>>>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| anyhow!("không có thiết bị phát âm thanh"))?;
    let supported = device.default_output_config().context("output device format")?;
    let format = supported.sample_format();
    let config = supported.config();
    let (rate, channels) = (config.sample_rate, config.channels as usize);
    info!("cast sound loopback: {rate} Hz {channels} ch {format}");
    let mut resampler = Resampler::new(rate, RATE);
    let mut stereo = Vec::new();
    let convert = move |input: &[f32], out: &mut Vec<f32>| {
        stereo.clear();
        to_stereo(input, channels, &mut stereo);
        resampler.process(&stereo, out);
    };
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, queue, convert),
        SampleFormat::I16 => build::<i16>(&device, &config, queue, convert),
        SampleFormat::I32 => build::<i32>(&device, &config, queue, convert),
        SampleFormat::U16 => build::<u16>(&device, &config, queue, convert),
        other => bail!("unsupported loopback sample format {other}"),
    }
    .context("open WASAPI loopback")?;
    stream.play().context("start WASAPI loopback")?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: Arc<Mutex<VecDeque<f32>>>,
    mut convert: impl FnMut(&[f32], &mut Vec<f32>) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let mut float = Vec::new();
    let mut out = Vec::new();
    let stream = device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            float.clear();
            float.extend(data.iter().map(|s| s.to_sample::<f32>()));
            out.clear();
            convert(&float, &mut out);
            queue.lock().unwrap().extend(out.iter().copied());
        },
        |e: cpal::Error| warn!("sound stream error: {e}"),
        Some(Duration::from_secs(2)),
    )?;
    Ok(stream)
}
