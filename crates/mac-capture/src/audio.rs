//! System sound to the TV: ScreenCaptureKit audio (`capturesAudio`, macOS 13+, the same
//! SCStream as the picture) -> f32 stereo 48 kHz -> Opus 20 ms (`nanocast_core::audio`)
//! -> UDP AUDIO_PORT, same packets as the Windows sender (magic 0x4E41 | seq | pts_ms).
//!
//! Split in two like `nanocast_win::audio`:
//! - `AudioTap` lives inside the capture stream. Its callback converts each sample buffer
//!   and forwards it only while an `AudioSender` is connected; otherwise it returns at once.
//!   SCK keeps tapping the mix while the cast runs (cheap), so switching sound on or off
//!   mid-cast needs no stream reconfiguration.
//! - `AudioSender` owns the encoder thread (Opus, silence packets on a 20 ms clock while
//!   nothing plays, exactly like Windows).

use std::net::{SocketAddr, UdpSocket};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use log::{info, warn};
use nanocast_core::audio::{to_stereo, AudioEncoder, Resampler};
use nanocast_core::protocol::{AUDIO_BITRATE, AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM, AudioBuffer,
    AudioStreamBasicDescription,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, CMAudioFormatDescriptionGetStreamBasicDescription,
    CMBlockBuffer, CMSampleBuffer,
};

/// Low complexity keeps the encoder at a fraction of one core (same as Windows).
pub const OPUS_COMPLEXITY: i32 = 3;
/// No capture data for this long = nothing is playing: switch to silence packets.
const SILENCE_AFTER: Duration = Duration::from_millis(60);
/// Samples queued between the capture callback and the encoder beyond this are dropped.
const MAX_PENDING_MS: usize = 100;
/// SCK delivers planar float, one buffer per channel; 8 covers any layout.
const MAX_BUFFERS: usize = 8;

/// Counters shared with the session (stats) and the GUI (same fields as Windows).
#[derive(Debug, Default)]
pub struct AudioCounters {
    pub bytes: AtomicU64,
    pub packets: AtomicU64,
    pub silent_packets: AtomicU64,
    /// Capture is open and delivering (or waiting for) sound.
    pub running: AtomicBool,
    pub error: Mutex<Option<String>>,
    /// e.g. "ScreenCaptureKit system audio 48000 Hz 2 ch f32 planar".
    pub device: Mutex<Option<String>>,
}

#[derive(Default)]
struct TapState {
    /// Input rate the resampler was built for.
    rate: u32,
    resampler: Option<Resampler>,
    interleaved: Vec<f32>,
    stereo: Vec<f32>,
    warned: bool,
}

/// The capture-side end of the audio path; see the module docs.
#[derive(Default)]
pub struct AudioTap {
    tx: Mutex<Option<SyncSender<Vec<f32>>>>,
    state: Mutex<TapState>,
    /// Format of the first buffer seen, for `AudioCounters::device`.
    format: Mutex<Option<String>>,
}

/// `AudioBufferList` with room for `MAX_BUFFERS` buffers (the C struct is variable-length).
#[repr(C)]
struct BufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; MAX_BUFFERS],
}

impl AudioTap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn connect(&self, tx: SyncSender<Vec<f32>>) {
        *self.tx.lock().unwrap() = Some(tx);
    }

    fn disconnect(&self) {
        self.tx.lock().unwrap().take();
    }

    fn format(&self) -> Option<String> {
        self.format.lock().unwrap().clone()
    }

    /// Called on the SCK audio queue for every audio sample buffer.
    pub(crate) fn push(&self, sample: &CMSampleBuffer) {
        let Some(tx) = self.tx.lock().unwrap().clone() else { return };
        let mut st = self.state.lock().unwrap();
        match unsafe { convert(sample, &mut st, &self.format) } {
            Ok(Some(chunk)) => {
                // Full queue = encoder thread stalled; drop rather than block SCK.
                let _ = tx.try_send(chunk);
            }
            Ok(None) => {}
            Err(e) if !st.warned => {
                warn!("audio buffer skipped: {e:#}");
                st.warned = true;
            }
            Err(_) => {}
        }
    }
}

/// One CMSampleBuffer of LPCM float -> interleaved stereo at 48 kHz.
unsafe fn convert(sample: &CMSampleBuffer, st: &mut TapState, format: &Mutex<Option<String>>) -> Result<Option<Vec<f32>>> {
    if !sample.is_valid() || !sample.data_is_ready() {
        return Ok(None);
    }
    let frames = sample.num_samples().max(0) as usize;
    if frames == 0 {
        return Ok(None);
    }
    let desc = sample.format_description().context("no format description")?;
    let asbd: &AudioStreamBasicDescription =
        CMAudioFormatDescriptionGetStreamBasicDescription(&desc).as_ref().context("not an audio format")?;
    if asbd.mFormatID != kAudioFormatLinearPCM || asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 {
        bail!("unexpected audio format id {:#x} flags {:#x} bits {}", asbd.mFormatID, asbd.mFormatFlags, asbd.mBitsPerChannel);
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let rate = asbd.mSampleRate.round() as u32;
    {
        let mut f = format.lock().unwrap();
        if f.is_none() {
            *f = Some(format!(
                "ScreenCaptureKit system audio {rate} Hz {channels} ch f32{}",
                if planar { " planar" } else { "" }
            ));
        }
    }

    let mut list: BufferList = std::mem::zeroed();
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = sample.audio_buffer_list_with_retained_block_buffer(
        std::ptr::null_mut(),
        (&mut list as *mut BufferList).cast(),
        std::mem::size_of::<BufferList>(),
        None,
        None,
        kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
        &mut block,
    );
    if status != 0 {
        bail!("CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer failed ({status})");
    }
    // Keeps the sample memory alive while we read it; released at the end of scope.
    let _block = NonNull::new(block).map(|b| CFRetained::from_raw(b));
    let buffers = &list.buffers[..(list.number_buffers as usize).min(MAX_BUFFERS)];

    st.interleaved.clear();
    st.interleaved.reserve(frames * channels);
    if planar {
        let planes: Vec<&[f32]> = buffers
            .iter()
            .map(|b| {
                let n = (b.mDataByteSize as usize / 4).min(frames);
                if b.mData.is_null() { &[][..] } else { std::slice::from_raw_parts(b.mData as *const f32, n) }
            })
            .collect();
        if planes.len() < channels || planes.iter().any(|p| p.len() < frames) {
            bail!("short planar audio buffer ({} planes for {channels} ch)", planes.len());
        }
        for i in 0..frames {
            st.interleaved.extend(planes[..channels].iter().map(|p| p[i]));
        }
    } else {
        let b = buffers.first().context("empty AudioBufferList")?;
        let n = (b.mDataByteSize as usize / 4).min(frames * channels);
        if !b.mData.is_null() {
            st.interleaved.extend_from_slice(std::slice::from_raw_parts(b.mData as *const f32, n));
        }
    }

    st.stereo.clear();
    to_stereo(&st.interleaved, channels, &mut st.stereo);
    if st.resampler.is_none() || st.rate != rate {
        st.rate = rate;
        st.resampler = Some(Resampler::new(rate, AUDIO_SAMPLE_RATE));
    }
    let mut out = Vec::with_capacity(st.stereo.len() + 16);
    let TapState { resampler, stereo, .. } = st;
    resampler.as_mut().unwrap().process(stereo, &mut out);
    Ok(Some(out))
}

pub struct AudioSender {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSender {
    /// Starts encoding what `tap` captures and sending it to `addr` (the receiver's
    /// AUDIO_PORT). Failures end up in `counters.error`.
    pub fn start(addr: SocketAddr, tap: Arc<AudioTap>, counters: Arc<AudioCounters>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    *counters.error.lock().unwrap() = None;
                    if let Err(e) = run(addr, &tap, &stop, &counters) {
                        warn!("audio: {e:#}");
                        *counters.error.lock().unwrap() = Some(format!("{e:#}"));
                    }
                    tap.disconnect();
                    counters.running.store(false, Ordering::Relaxed);
                })
                .expect("spawn audio thread")
        };
        Self { stop, thread: Some(thread) }
    }

    /// Records why audio cannot run (e.g. macOS 12) without starting a thread.
    pub fn unavailable(reason: &str, counters: &AudioCounters) {
        *counters.error.lock().unwrap() = Some(reason.into());
        counters.running.store(false, Ordering::Relaxed);
    }

    /// Stops without waiting for the thread (it exits within ~10 ms).
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

fn run(addr: SocketAddr, tap: &AudioTap, stop: &AtomicBool, counters: &AudioCounters) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("audio UDP socket")?;
    sock.connect(addr).with_context(|| format!("audio UDP to {addr}"))?;

    let (tx, rx) = sync_channel::<Vec<f32>>(32);
    tap.connect(tx);
    counters.running.store(true, Ordering::Relaxed);
    info!("audio (ScreenCaptureKit) -> {addr}");

    let mut encoder = AudioEncoder::new(AUDIO_BITRATE, OPUS_COMPLEXITY)?;
    let frame = Duration::from_millis(AUDIO_FRAME_MS as u64);
    let max_pending = AUDIO_SAMPLE_RATE as usize * 2 * MAX_PENDING_MS / 1000;
    let mut send_err_logged = false;
    let mut send = |pkt: Vec<u8>| {
        match sock.send(&pkt) {
            Ok(_) => {}
            // Receiver not listening yet (ICMP port unreachable): keep going.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => {}
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
            Err(RecvTimeoutError::Disconnected) => bail!("audio tap closed"),
        }
        if counters.device.lock().unwrap().is_none() {
            if let Some(f) = tap.format() {
                info!("audio format: {f}");
                *counters.device.lock().unwrap() = Some(f);
            }
        }
        // Nothing captured lately (SCK sends no buffers while nothing plays): keep the
        // stream alive with silence packets on a 20 ms clock.
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
    counters.silent_packets.store(encoder.silent_frames(), Ordering::Relaxed);
    tap.disconnect();
    info!("audio stopped");
    Ok(())
}
