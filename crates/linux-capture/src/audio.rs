//! System sound to the TV: the default output's monitor -> f32 stereo 48 kHz -> Opus 20 ms
//! (`nanocast_core::audio`) -> UDP AUDIO_PORT, same packets as the Windows sender
//! (magic 0x4E41 | seq | pts_ms, header-only packet = 20 ms of silence).
//!
//! Capture back ends, tried in order on the audio thread:
//! - PipeWire (feature `pipewire`): a capture stream with `stream.capture.sink = true`, which
//!   the session manager links to the default sink's monitor ports and follows when the
//!   default output changes. PipeWire converts to F32LE/48 kHz/stereo itself. ~10 ms quantum.
//! - PulseAudio (feature `pulse`, also served by `pipewire-pulse`): `pa_simple` recording
//!   from `@DEFAULT_MONITOR@` in 10 ms fragments.
//!
//! Like Windows, an `AudioSender` owns one thread; dropping it (or `stop_detached`) ends it
//! and releases the capture, so audio off costs no CPU. Starting never blocks the caller.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use log::{info, warn};
use nanocast_core::audio::AudioEncoder;
use nanocast_core::protocol::{AUDIO_BITRATE, AUDIO_CHANNELS, AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE};

/// Low complexity keeps the encoder at a fraction of one core (same as Windows).
pub const OPUS_COMPLEXITY: i32 = 3;
/// No capture data for this long = nothing is playing: switch to silence packets.
const SILENCE_AFTER: Duration = Duration::from_millis(60);
/// Samples queued between the capture callback and the encoder beyond this are dropped.
const MAX_PENDING_MS: usize = 100;

/// Counters shared with the session (stats) and the GUI (same fields as Windows).
#[derive(Debug, Default)]
pub struct AudioCounters {
    pub bytes: AtomicU64,
    pub packets: AtomicU64,
    pub silent_packets: AtomicU64,
    /// Capture is open and delivering (or waiting for) sound.
    pub running: AtomicBool,
    pub error: Mutex<Option<String>>,
    /// e.g. "PipeWire default output monitor 48000 Hz 2 ch f32".
    pub device: Mutex<Option<String>>,
}

pub struct AudioSender {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioSender {
    /// Starts capturing and sending to `addr` (the receiver's AUDIO_PORT) in the background.
    /// Failures (no sound server...) end up in `counters.error`.
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
    /// capture on its own). Used when the user switches audio off mid-cast.
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

/// A running capture; dropping it stops it.
trait Capture {}

/// Opens the first sound server that works; chunks of interleaved stereo f32 at 48 kHz go to `tx`.
fn open_capture(tx: SyncSender<Vec<f32>>) -> Result<(Box<dyn Capture>, String)> {
    #[allow(unused_mut)] // no back end compiled in
    let mut errors: Vec<String> = Vec::new();
    #[cfg(feature = "pipewire")]
    match pw_audio::PwCapture::start(tx.clone()) {
        Ok(c) => return Ok((Box::new(c), "PipeWire default output monitor 48000 Hz 2 ch f32".into())),
        Err(e) => errors.push(format!("PipeWire: {e:#}")),
    }
    #[cfg(feature = "pulse")]
    match pulse_audio::PulseCapture::start(tx.clone()) {
        Ok(c) => return Ok((Box::new(c), "PulseAudio @DEFAULT_MONITOR@ 48000 Hz 2 ch f32".into())),
        Err(e) => errors.push(format!("PulseAudio: {e:#}")),
    }
    drop(tx);
    if errors.is_empty() {
        bail!("built without a sound back end (cargo features pipewire/pulse)");
    }
    bail!("no system sound capture: {}", errors.join("; "))
}

fn run(addr: SocketAddr, stop: &AtomicBool, counters: &AudioCounters) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("audio UDP socket")?;
    sock.connect(addr).with_context(|| format!("audio UDP to {addr}"))?;

    // Capture -> this thread. Each chunk is one quantum/fragment (~10 ms), stereo 48 kHz.
    let (tx, rx) = sync_channel::<Vec<f32>>(32);
    let (capture, desc) = open_capture(tx)?;
    info!("audio capture: {desc} -> {addr}");
    *counters.device.lock().unwrap() = Some(desc);
    counters.running.store(true, Ordering::Relaxed);

    // Numbering continues across off/on within a cast (`packets` is per cast).
    let first_seq = counters.packets.load(Ordering::Relaxed) as u32;
    let mut encoder = AudioEncoder::starting_at(AUDIO_BITRATE, OPUS_COMPLEXITY, first_seq)?;
    let frame = Duration::from_millis(AUDIO_FRAME_MS as u64);
    let max_pending = AUDIO_SAMPLE_RATE as usize * AUDIO_CHANNELS * MAX_PENDING_MS / 1000;
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

    let started = Instant::now();
    let mut last_data = Instant::now();
    let mut next_silence = Instant::now() + SILENCE_AFTER;
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(chunk) => {
                last_data = Instant::now();
                if encoder.pending() > max_pending {
                    warn!("audio encoder behind; dropping {} ms", encoder.pending() * 1000 / 2 / AUDIO_SAMPLE_RATE as usize);
                    encoder.clear_pending();
                }
                if let Err(e) = encoder.push(&chunk, &mut send) {
                    break Err(e);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Err(anyhow::anyhow!("audio capture closed")),
        }
        // Nothing captured lately (a suspended sink sends nothing): keep the stream alive
        // with silence packets on a 20 ms clock.
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
    };
    counters.silent_packets.fetch_add(encoder.silent_frames(), Ordering::Relaxed);
    drop(capture);
    let secs = started.elapsed().as_secs_f64().max(1e-3);
    info!(
        "audio stopped after {secs:.1} s: {} packets ({} silent), encoder thread CPU {:.2} % of one core",
        counters.packets.load(Ordering::Relaxed),
        encoder.silent_frames(),
        thread_cpu_secs() / secs * 100.0
    );
    result
}

/// CPU time used so far by the calling thread.
fn thread_cpu_secs() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    } else {
        0.0
    }
}

#[cfg(feature = "pipewire")]
mod pw_audio {
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use anyhow::{anyhow, Context as _, Result};
    use log::{info, warn};
    use nanocast_core::protocol::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
    use pipewire as pw;
    use pw::properties::properties;
    use pw::spa;
    use pw::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
    use pw::spa::pod::Pod;
    use pw::stream::{StreamFlags, StreamState};

    pub(super) struct PwCapture {
        quit: Option<pw::channel::Sender<()>>,
        thread: Option<JoinHandle<()>>,
    }

    impl super::Capture for PwCapture {}

    impl PwCapture {
        pub(super) fn start(tx: SyncSender<Vec<f32>>) -> Result<Self> {
            let (quit_tx, quit_rx) = pw::channel::channel::<()>();
            let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
            let thread = std::thread::Builder::new().name("pw-audio".into()).spawn(move || {
                if let Err(e) = run(tx, quit_rx, &ready_tx) {
                    let _ = ready_tx.try_send(Err(format!("{e:#}")));
                }
            })?;
            let mut cap = Self { quit: Some(quit_tx), thread: Some(thread) };
            match ready_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(())) => Ok(cap),
                Ok(Err(e)) => {
                    cap.stop();
                    Err(anyhow!(e))
                }
                Err(_) => {
                    cap.stop();
                    Err(anyhow!("PipeWire did not answer within 3 s"))
                }
            }
        }

        fn stop(&mut self) {
            if let Some(q) = self.quit.take() {
                let _ = q.send(());
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    impl Drop for PwCapture {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn run(tx: SyncSender<Vec<f32>>, quit: pw::channel::Receiver<()>, ready: &SyncSender<Result<(), String>>) -> Result<()> {
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw_main_loop_new")?;
        let context = pw::context::ContextRc::new(&mainloop, None).context("pw_context_new")?;
        // Fails when no PipeWire daemon runs (plain PulseAudio systems): caller falls back.
        let core = context.connect_rc(None).context("connect to PipeWire")?;
        let stream = pw::stream::StreamBox::new(
            &core,
            "nanocast-audio",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::APP_NAME => "NANOCast",
                *pw::keys::NODE_NAME => "nanocast-system-sound",
                // Record what the default output plays, not the microphone.
                *pw::keys::STREAM_CAPTURE_SINK => "true",
                // 480 samples = 10 ms at 48 kHz.
                *pw::keys::NODE_LATENCY => "480/48000",
            },
        )
        .context("pw_stream_new")?;

        let _listener = stream
            .add_local_listener_with_user_data(tx)
            .state_changed(|_, _, old, new| {
                info!("PipeWire audio stream: {old:?} -> {new:?}");
                if let StreamState::Error(e) = new {
                    warn!("PipeWire audio stream error: {e}");
                }
            })
            .process(|stream, tx| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let Some(d) = buffer.datas_mut().first_mut() else { return };
                let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                let Some(bytes) = d.data() else { return };
                let Some(bytes) = bytes.get(offset..offset + size) else { return };
                let samples: Vec<f32> =
                    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                if !samples.is_empty() {
                    // Full queue = encoder thread stalled; drop rather than block the graph.
                    let _ = tx.try_send(samples);
                }
            })
            .register()
            .context("register audio listener")?;

        let mut info = AudioInfoRaw::new();
        info.set_format(AudioFormat::F32LE);
        info.set_rate(AUDIO_SAMPLE_RATE);
        info.set_channels(AUDIO_CHANNELS as u32);
        let mut position = [0u32; MAX_CHANNELS];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(position);
        let obj = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        };
        let bytes = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
            .map_err(|e| anyhow!("serialize audio format: {e:?}"))?
            .0
            .into_inner();
        let mut params = [Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("bad audio format pod"))?];
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("pw_stream_connect (audio)")?;

        let ml = mainloop.clone();
        let _quit = quit.attach(mainloop.loop_(), move |()| ml.quit());
        let _ = ready.try_send(Ok(()));
        mainloop.run();
        let _ = stream.disconnect();
        Ok(())
    }
}

#[cfg(feature = "pulse")]
mod pulse_audio {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use libpulse_binding::def::BufferAttr;
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    use libpulse_simple_binding::Simple;
    use log::warn;
    use nanocast_core::protocol::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};

    /// 10 ms of stereo f32 at 48 kHz.
    const FRAGMENT_BYTES: usize = AUDIO_SAMPLE_RATE as usize / 100 * AUDIO_CHANNELS * 4;

    pub(super) struct PulseCapture {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl super::Capture for PulseCapture {}

    impl PulseCapture {
        pub(super) fn start(tx: SyncSender<Vec<f32>>) -> Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
            let thread = {
                let stop = stop.clone();
                std::thread::Builder::new().name("pulse-audio".into()).spawn(move || {
                    let spec = Spec { format: Format::F32le, rate: AUDIO_SAMPLE_RATE, channels: AUDIO_CHANNELS as u8 };
                    let attr = BufferAttr {
                        maxlength: u32::MAX,
                        tlength: u32::MAX,
                        prebuf: u32::MAX,
                        minreq: u32::MAX,
                        fragsize: FRAGMENT_BYTES as u32,
                    };
                    let simple = match Simple::new(
                        None,
                        "NANOCast",
                        Direction::Record,
                        Some("@DEFAULT_MONITOR@"),
                        "system sound",
                        &spec,
                        None,
                        Some(&attr),
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("pa_simple_new: {e}")));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));
                    let mut buf = vec![0u8; FRAGMENT_BYTES];
                    while !stop.load(Ordering::Relaxed) {
                        if let Err(e) = simple.read(&mut buf) {
                            warn!("PulseAudio read: {e}");
                            break;
                        }
                        let samples: Vec<f32> =
                            buf.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                        if tx.try_send(samples).is_err() && stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                })?
            };
            let mut cap = Self { stop, thread: Some(thread) };
            match ready_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(())) => Ok(cap),
                Ok(Err(e)) => {
                    cap.halt();
                    Err(anyhow!(e))
                }
                Err(_) => {
                    // pa_simple_new can block a long time without a server; leave it behind.
                    cap.stop.store(true, Ordering::Relaxed);
                    cap.thread.take();
                    Err(anyhow!("PulseAudio did not answer within 3 s"))
                }
            }
        }

        fn halt(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    impl Drop for PulseCapture {
        fn drop(&mut self) {
            self.halt();
        }
    }
}
