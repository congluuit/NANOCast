//! Windows implementation of `CastSession`: connects the Cast channel, picks the path
//! (Cast Streaming mirroring first, the Default Media Receiver + live fMP4 as fallback)
//! and runs capture -> encoder -> path on a background thread.
//!
//! Threads: `cast-google` (capture + H.264, owns Media Foundation), `cast-sound` (WASAPI
//! loopback -> AAC or Opus on the wall clock), `cast-monitor` (device messages, media
//! status polling, delay estimate), plus the channel / HTTP / RTP helpers.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use log::{info, warn};
use nanocast_win::mf_encoder::MfRuntime;
use serde_json::json;

use crate::aac::{self, AacEncoder};
use crate::channel::{Channel, Events, ReceiverApp, NS_CONNECTION, NS_MEDIA, NS_RECEIVER};
use crate::fmp4::{self, AudioParams, Sample, VideoParams, AUDIO_TRACK, VIDEO_TIMESCALE, VIDEO_TRACK};
use crate::http::{Chunk, LiveServer, STREAM_PATH};
use crate::mirror::{self, MirrorSender, OfferParams, StreamKeys, StreamTx, MIRROR_APP_ID};
use crate::sound::{SoundClock, RATE};
use crate::video::{self, VideoCtl, VideoFrame};
use crate::{CastConfig, CastDevice, CastMode, CastStats, SourceSpec};

pub const MEDIA_APP_ID: &str = "CC1AD845";
/// Fixed so a Windows firewall rule can name it; another free port is used when taken.
pub const HTTP_PORT: u16 = 47900;
/// AAC frames per audio fragment (~64 ms).
const AAC_PER_FRAGMENT: usize = 3;
/// Keyframe at least this often on the media path (players start / recover at one).
const MEDIA_KEYFRAME_EVERY: Duration = Duration::from_secs(1);
/// Mirroring is given up when the TV sends no RTCP within this time after the ANSWER.
const MIRROR_FIRST_RTCP: Duration = Duration::from_secs(5);

struct Shared {
    stop: AtomicBool,
    audio: Arc<AtomicBool>,
    stats: Mutex<CastStats>,
    video: VideoCtl,
    /// Media path: delay estimate from the player position (ms, 0 = unknown).
    media_delay_ms: AtomicU32,
    frames: AtomicU64,
    bytes: AtomicU64,
    /// Newest encoded frame: (pts on the session clock, capture instant).
    last_frame: Mutex<(u64, Option<Instant>)>,
}

impl Shared {
    fn message(&self, m: impl Into<String>) {
        let m = m.into();
        info!("cast: {m}");
        self.stats.lock().unwrap().message = m;
    }
}

pub struct Session {
    shared: Arc<Shared>,
    channel: Option<Arc<Channel>>,
    app: Option<ReceiverApp>,
    threads: Vec<JoinHandle<()>>,
}

fn mode_from_env() -> Option<CastMode> {
    match std::env::var("NANOCAST_CAST_MODE").unwrap_or_default().to_ascii_lowercase().as_str() {
        "mirroring" | "mirror" | "b" => Some(CastMode::Mirroring),
        "media" | "mediareceiver" | "a" => Some(CastMode::MediaReceiver),
        _ => None,
    }
}

fn new_shared(cfg: &CastConfig) -> Arc<Shared> {
    Arc::new(Shared {
        stop: AtomicBool::new(false),
        // NANOCAST_CAST_MUTE=1: start with the sound track silent (testing on speakers).
        audio: Arc::new(AtomicBool::new(cfg.audio && std::env::var("NANOCAST_CAST_MUTE").map_or(true, |v| v != "1"))),
        stats: Mutex::new(CastStats { message: "Đang kết nối TV…".into(), ..Default::default() }),
        video: VideoCtl::default(),
        media_delay_ms: AtomicU32::new(0),
        frames: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        last_frame: Mutex::new((0, None)),
    })
}

impl Session {
    pub fn start(device: &CastDevice, cfg: CastConfig, source: SourceSpec) -> Result<Self> {
        let peer = SocketAddr::new(device.addr.into(), device.port);
        let shared = new_shared(&cfg);
        let (channel, events) = Channel::connect(peer)?;
        let channel = Arc::new(channel);
        let forced = mode_from_env();

        let mut note = None;
        if forced != Some(CastMode::MediaReceiver) {
            match start_mirroring(&shared, &channel, &events, &cfg, &source) {
                Ok((app, threads)) => {
                    let mut s = Self { shared, channel: Some(channel), app: Some(app), threads };
                    s.spawn_monitor(events, CastMode::Mirroring, None);
                    return Ok(s);
                }
                Err(e) if forced == Some(CastMode::Mirroring) || channel.is_closed() => return Err(e),
                Err(e) => {
                    warn!("mirroring unavailable, falling back to the Default Media Receiver: {e:#}");
                    note = Some(format!("{e:#}"));
                    shared.stop.store(false, Ordering::Relaxed);
                }
            }
        }
        let (app, threads, server) = start_media(&shared, &channel, &events, &cfg, &source)?;
        if let Some(n) = note {
            shared.message(format!("Đang phát qua Default Media Receiver (trễ 1–3 s); chiếu màn hình trực tiếp không được: {n}"));
        }
        let mut s = Self { shared, channel: Some(channel), app: Some(app), threads };
        s.spawn_monitor(events, CastMode::MediaReceiver, Some(server));
        Ok(s)
    }

    fn spawn_monitor(&mut self, events: Events, mode: CastMode, server: Option<Arc<LiveServer>>) {
        let shared = self.shared.clone();
        let ch = self.channel.clone().unwrap();
        let app = self.app.as_ref().map(|a| (a.session_id.clone(), a.transport_id.clone())).unwrap();
        let t = std::thread::Builder::new()
            .name("cast-monitor".into())
            .spawn(move || monitor(&shared, &ch, &events, mode, &app.0, &app.1, server.as_deref()))
            .expect("spawn cast monitor");
        self.threads.push(t);
    }

    pub fn set_audio(&self, on: bool) {
        self.shared.audio.store(on, Ordering::Relaxed);
    }

    pub fn stats(&self) -> CastStats {
        let mut s = self.shared.stats.lock().unwrap().clone();
        if let Some(ch) = &self.channel {
            if ch.is_closed() {
                s.connected = false;
            }
        }
        s
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.video.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(ch) = self.channel.take() {
            if let Some(app) = self.app.take() {
                ch.stop_app(&app.session_id);
                std::thread::sleep(Duration::from_millis(150));
                if let Ok(ch) = Arc::try_unwrap(ch) {
                    ch.close(&[&app.transport_id]);
                }
            }
        }
        let mut s = self.shared.stats.lock().unwrap();
        s.connected = false;
        s.fps = 0.0;
        s.mbps = 0.0;
        s.message = "Đã dừng".into();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.threads.is_empty() || self.channel.is_some() {
            self.stop();
        }
    }
}

/// Runs the video pipeline on its own thread; `on_frame` gets every encoded frame.
fn spawn_video(
    shared: &Arc<Shared>,
    cfg: &CastConfig,
    source: &SourceSpec,
    gop: u32,
    t0: Instant,
    mode: CastMode,
    mut on_frame: impl FnMut(&VideoFrame) + Send + 'static,
) -> Result<JoinHandle<()>> {
    let (shared, cfg, source) = (shared.clone(), cfg.clone(), source.clone());
    Ok(std::thread::Builder::new().name("cast-google".into()).spawn(move || {
        let mut window = (Instant::now(), 0u64, 0u64, 0u64);
        let r = (|| -> Result<()> {
            let _rt = MfRuntime::init()?;
            video::run(&source, &cfg, gop, t0, &shared.video, |f| {
                on_frame(&f);
                *shared.last_frame.lock().unwrap() = (f.pts_us, Some(f.captured));
                shared.frames.fetch_add(1, Ordering::Relaxed);
                shared.bytes.fetch_add(f.data.len() as u64, Ordering::Relaxed);
                window.1 += 1;
                window.2 += f.data.len() as u64;
                window.3 += f.captured.elapsed().as_micros() as u64;
                let secs = window.0.elapsed().as_secs_f32();
                if secs >= 0.5 {
                    let mut s = shared.stats.lock().unwrap();
                    s.fps = window.1 as f32 / secs;
                    s.mbps = window.2 as f32 * 8.0 / secs / 1e6;
                    s.mode = Some(mode);
                    let encode_ms = window.3 / window.1.max(1) / 1000;
                    // Receiver side: target playout delay (mirroring) or the player's lag.
                    let path_ms = shared.media_delay_ms.load(Ordering::Relaxed) as u64;
                    s.latency_ms = (path_ms > 0).then_some((encode_ms + path_ms) as u32);
                    window = (Instant::now(), 0, 0, 0);
                }
            })
        })();
        if let Err(e) = r {
            warn!("cast pipeline: {e:#}");
            let mut s = shared.stats.lock().unwrap();
            s.connected = false;
            s.message = format!("Lỗi: {e:#}");
        }
        shared.stop.store(true, Ordering::Relaxed);
    })?)
}

// ---------------------------------------------------------------- path B: mirroring

fn start_mirroring(
    shared: &Arc<Shared>,
    ch: &Arc<Channel>,
    events: &Events,
    cfg: &CastConfig,
    source: &SourceSpec,
) -> Result<(ReceiverApp, Vec<JoinHandle<()>>)> {
    shared.message("Đang mở chế độ chiếu màn hình trên TV…");
    let app = ch.launch(MIRROR_APP_ID).context("TV không mở được chế độ chiếu màn hình")?;
    let result = (|| -> Result<Vec<JoinHandle<()>>> {
        let fps = cfg.fps.clamp(1, 60);
        let delay = std::env::var("NANOCAST_CAST_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(300u32);
        let params = OfferParams {
            width: cfg.width,
            height: cfg.height,
            fps,
            bitrate_bps: cfg.bitrate_bps,
            target_delay_ms: delay,
            video: StreamKeys::random()?,
            audio: Some(StreamKeys::random()?),
        };
        let seq = 1;
        ch.send(crate::channel::NS_WEBRTC, &app.transport_id, mirror::offer(seq, &params));
        let answer = mirror::wait_answer(events, seq, Duration::from_secs(10))?;
        info!("mirroring ANSWER: {answer:?}");
        let audio_index = 0u32;
        let video_index = 1u32;
        if !answer.send_indexes.contains(&video_index) {
            bail!("TV không nhận luồng hình H.264 ({:?})", answer.send_indexes);
        }
        let with_audio = answer.send_indexes.contains(&audio_index);
        let peer = SocketAddr::new(ch.peer.ip(), answer.udp_port);
        let t0 = Instant::now();
        let pace = (cfg.bitrate_bps as u64 * 3).max(30_000_000);
        let sender = MirrorSender::start(
            peer,
            StreamTx::new(params.video.clone(), mirror::VIDEO_PT, mirror::VIDEO_CLOCK),
            with_audio.then(|| StreamTx::new(params.audio.clone().unwrap(), mirror::AUDIO_PT, mirror::AUDIO_CLOCK)),
            t0,
            pace,
        )?;
        let counters = sender.counters.clone();
        let want_keyframe = sender.want_keyframe.clone();
        let sender = Arc::new(Mutex::new(sender));
        shared.media_delay_ms.store(delay + 20, Ordering::Relaxed);

        let mut threads = Vec::new();
        {
            let (sender, shared2) = (sender.clone(), shared.clone());
            threads.push(spawn_video(shared, cfg, source, fps * 2, t0, CastMode::Mirroring, move |f| {
                if want_keyframe.swap(false, Ordering::Relaxed) {
                    shared2.video.want_keyframe.store(true, Ordering::Relaxed);
                }
                sender.lock().unwrap().send_video(&f.data, f.keyframe, f.pts_us);
            })?);
        }
        if with_audio {
            let sender = sender.clone();
            let clock = SoundClock::start(
                t0,
                nanocast_core::protocol::AUDIO_FRAME_SAMPLES,
                shared.audio.clone(),
                Box::new(move || {
                    let mut enc = nanocast_core::audio::OpusEncoder::new(128_000, 5)?;
                    let mut out = vec![0u8; 1500];
                    Ok(Box::new(move |pcm: &[f32], pos: u64| {
                        let n = enc.encode(pcm, &mut out)?;
                        sender.lock().unwrap().send_audio(&out[..n], pos * 1_000_000 / RATE as u64);
                        Ok(())
                    }))
                }),
            );
            threads.push(hold_until_stop(shared, clock));
        }
        // The receiver answers media with RTCP feedback; silence means it is not playing.
        let deadline = Instant::now() + MIRROR_FIRST_RTCP;
        while counters.rtcp_packets.load(Ordering::Relaxed) == 0 {
            if Instant::now() > deadline || shared.stop.load(Ordering::Relaxed) {
                shared.stop.store(true, Ordering::Relaxed);
                shared.video.stop.store(true, Ordering::Relaxed);
                for t in threads.drain(..) {
                    let _ = t.join();
                }
                shared.video.stop.store(false, Ordering::Relaxed);
                bail!("TV không phản hồi luồng chiếu màn hình (không có RTCP)");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Keep the sender's receiver-side delay in the stats.
        {
            let (shared2, counters) = (shared.clone(), counters.clone());
            threads.push(std::thread::spawn(move || {
                let mut last = (Instant::now(), 0u64);
                while !shared2.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    let d = counters.delay_ms.load(Ordering::Relaxed);
                    if d > 0 {
                        shared2.media_delay_ms.store(d + 20, Ordering::Relaxed);
                    }
                    let rtcp = counters.rtcp_packets.load(Ordering::Relaxed);
                    if rtcp != last.1 {
                        last = (Instant::now(), rtcp);
                    }
                    shared2.stats.lock().unwrap().connected = last.0.elapsed() < Duration::from_secs(3);
                }
                drop(sender);
            }));
        }
        let mut s = shared.stats.lock().unwrap();
        s.connected = true;
        s.mode = Some(CastMode::Mirroring);
        s.message = format!("Đang chiếu màn hình trực tiếp {}x{} @ {fps} fps", cfg.width, cfg.height);
        Ok(threads)
    })();
    match result {
        Ok(threads) => Ok((app, threads)),
        Err(e) => {
            ch.stop_app(&app.session_id);
            std::thread::sleep(Duration::from_millis(500));
            Err(e)
        }
    }
}

/// Keeps a value (sound clock) alive on a thread until the session stops.
fn hold_until_stop<T: Send + 'static>(shared: &Arc<Shared>, value: T) -> JoinHandle<()> {
    let shared = shared.clone();
    std::thread::spawn(move || {
        while !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(value);
    })
}

// ---------------------------------------------------------------- path A: media receiver

struct Muxer {
    server: Arc<LiveServer>,
    seq: AtomicU32,
    audio: bool,
    fps: u32,
    last_key: Mutex<Option<Instant>>,
}

impl Muxer {
    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn video(&self, f: &VideoFrame, ctl: &VideoCtl) {
        let (sample, sps, pps) = fmp4::annexb_to_avcc(&f.data);
        if !self.server.has_init() {
            let (Some(sps), Some(pps)) = (sps, pps) else {
                ctl.want_keyframe.store(true, Ordering::Relaxed);
                return;
            };
            let size = ctl.size.load(Ordering::Relaxed);
            let v = VideoParams { width: (size >> 32) as u32, height: size as u32, sps, pps };
            let a = self.audio.then(|| AudioParams {
                sample_rate: aac::SAMPLE_RATE,
                channels: aac::CHANNELS,
                asc: fmp4::aac_lc_asc(aac::SAMPLE_RATE, aac::CHANNELS),
                bitrate: aac::BITRATE,
            });
            self.server.set_init(fmp4::init_segment(&v, a.as_ref()));
            info!("fMP4 init segment ready ({}x{})", v.width, v.height);
        }
        let mut last_key = self.last_key.lock().unwrap();
        if f.keyframe {
            *last_key = Some(Instant::now());
        } else if last_key.is_none_or(|t| t.elapsed() >= MEDIA_KEYFRAME_EVERY) {
            ctl.want_keyframe.store(true, Ordering::Relaxed);
        }
        let duration = VIDEO_TIMESCALE / self.fps.max(1);
        let base = f.pts_us * VIDEO_TIMESCALE as u64 / 1_000_000;
        let frag = fmp4::fragment(self.next_seq(), VIDEO_TRACK, base, &[Sample { data: &sample, duration, keyframe: f.keyframe }]);
        self.server.push(Chunk { data: Arc::new(frag), keyframe: f.keyframe, pts_us: f.pts_us });
    }

    fn audio(&self, frames: &[Vec<u8>], first_pos: u64) {
        let samples: Vec<Sample> = frames.iter().map(|d| Sample { data: d, duration: aac::FRAME_SAMPLES as u32, keyframe: true }).collect();
        let frag = fmp4::fragment(self.next_seq(), AUDIO_TRACK, first_pos, &samples);
        self.server.push(Chunk { data: Arc::new(frag), keyframe: false, pts_us: first_pos * 1_000_000 / RATE as u64 });
    }
}

fn stop_threads(shared: &Shared, threads: &mut Vec<JoinHandle<()>>) {
    shared.stop.store(true, Ordering::Relaxed);
    shared.video.stop.store(true, Ordering::Relaxed);
    for t in threads.drain(..) {
        let _ = t.join();
    }
}

/// Capture -> H.264 + AAC -> fMP4 -> HTTP server, waiting until the init segment exists.
fn media_pipeline(shared: &Arc<Shared>, cfg: &CastConfig, source: &SourceSpec, port: u16) -> Result<(Vec<JoinHandle<()>>, Arc<LiveServer>)> {
    let server = Arc::new(match LiveServer::start(port) {
        Ok(s) => s,
        Err(e) if port != 0 => {
            warn!("port {port} busy ({e:#}); using any free port");
            LiveServer::start(0)?
        }
        Err(e) => return Err(e),
    });
    let fps = cfg.fps.clamp(1, 60);
    let t0 = Instant::now();
    let muxer = Arc::new(Muxer { server: server.clone(), seq: AtomicU32::new(0), audio: cfg.audio, fps, last_key: Mutex::new(None) });
    let mut threads = Vec::new();
    {
        let (muxer, shared2) = (muxer.clone(), shared.clone());
        threads.push(spawn_video(shared, cfg, source, fps, t0, CastMode::MediaReceiver, move |f| muxer.video(f, &shared2.video))?);
    }
    if cfg.audio {
        let muxer = muxer.clone();
        let clock = SoundClock::start(
            t0,
            aac::FRAME_SAMPLES,
            shared.audio.clone(),
            Box::new(move || {
                let rt = MfRuntime::init()?;
                let mut enc = AacEncoder::new()?;
                let mut first: Option<u64> = None;
                let mut out_frames = 0u64;
                let mut pending: Vec<Vec<u8>> = Vec::new();
                Ok(Box::new(move |pcm: &[f32], pos: u64| {
                    let _rt = &rt;
                    let start = *first.get_or_insert(pos);
                    enc.encode(pcm, pos, |frame| pending.push(frame))?;
                    if pending.len() >= AAC_PER_FRAGMENT {
                        let at = start + out_frames * aac::FRAME_SAMPLES as u64;
                        out_frames += pending.len() as u64;
                        muxer.audio(&pending, at);
                        pending.clear();
                    }
                    Ok(())
                }))
            }),
        );
        threads.push(hold_until_stop(shared, clock));
    }
    // The receiver fetches the URL right after LOAD: have the init segment ready.
    let deadline = Instant::now() + Duration::from_secs(6);
    while !server.has_init() {
        if Instant::now() > deadline || shared.stop.load(Ordering::Relaxed) {
            let msg = shared.stats.lock().unwrap().message.clone();
            stop_threads(shared, &mut threads);
            bail!("không lấy được hình từ máy tính ({msg})");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok((threads, server))
}

fn start_media(
    shared: &Arc<Shared>,
    ch: &Arc<Channel>,
    events: &Events,
    cfg: &CastConfig,
    source: &SourceSpec,
) -> Result<(ReceiverApp, Vec<JoinHandle<()>>, Arc<LiveServer>)> {
    let (mut threads, server) = media_pipeline(shared, cfg, source, HTTP_PORT)?;
    shared.message("Đang mở trình phát trên TV…");
    let app = match ch.launch(MEDIA_APP_ID) {
        Ok(a) => a,
        Err(e) => {
            stop_threads(shared, &mut threads);
            return Err(e);
        }
    };
    let url = format!("http://{}:{}{}", ch.local_ip, server.port, STREAM_PATH);
    info!("LOAD {url}");
    let load = json!({
        "type": "LOAD",
        "sessionId": app.session_id,
        "media": {
            "contentId": url,
            "contentUrl": url,
            "contentType": "video/mp4",
            "streamType": "LIVE",
            "metadata": {"metadataType": 0, "title": "NANOCast"},
        },
        "autoplay": true,
        "currentTime": 0,
    });
    let r = ch.request(NS_MEDIA, &app.transport_id, load, Duration::from_secs(25));
    let mut failed = match &r {
        Ok(v) if v["type"] == "MEDIA_STATUS" => None,
        Ok(v) => Some(v["type"].as_str().unwrap_or("?").to_string()),
        Err(e) => Some(format!("{e:#}")),
    };
    // The LOAD reply can be an IDLE status while the player is still fetching: wait until it
    // plays (or fails) so the caller learns about a blocked firewall now, not later.
    if failed.is_none() {
        failed = wait_playing(r.as_ref().ok(), events, &app.transport_id, Duration::from_secs(20));
    }
    if let Some(why) = failed {
        stop_threads(shared, &mut threads);
        ch.stop_app(&app.session_id);
        if server.requests.load(Ordering::Relaxed) == 0 {
            bail!(
                "TV không tải được hình từ máy tính ({why}). Hãy đặt Wi-Fi của máy tính là mạng Riêng tư (Private) và bấm \"Cho phép\" NANOCast khi Tường lửa Windows hỏi (cổng TCP {})",
                server.port
            );
        }
        bail!("TV không phát được luồng hình ({why})");
    }
    let mut s = shared.stats.lock().unwrap();
    s.connected = true;
    s.mode = Some(CastMode::MediaReceiver);
    s.message = format!("Đang phát qua trình phát Cast của TV ({url}); trễ khoảng 1–3 s");
    drop(s);
    Ok((app, threads, server))
}

/// Path A without a TV: the live fMP4 stream on this PC only (`http://127.0.0.1:<port>/`
/// shows it in a browser). For checking the muxer and measuring the player's delay.
pub struct Preview {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    pub server: Arc<LiveServer>,
}

impl Preview {
    pub fn start(cfg: CastConfig, source: SourceSpec, port: u16) -> Result<Self> {
        let shared = new_shared(&cfg);
        let (threads, server) = media_pipeline(&shared, &cfg, &source, port)?;
        let mut s = shared.stats.lock().unwrap();
        s.mode = Some(CastMode::MediaReceiver);
        s.message = format!("Xem thử: http://127.0.0.1:{}/", server.port);
        drop(s);
        Ok(Self { shared, threads, server })
    }

    pub fn stats(&self) -> CastStats {
        let mut s = self.shared.stats.lock().unwrap().clone();
        s.connected = self.server.connected.load(Ordering::Relaxed) > 0;
        s
    }

    pub fn set_audio(&self, on: bool) {
        self.shared.audio.store(on, Ordering::Relaxed);
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        stop_threads(&self.shared, &mut self.threads);
    }
}

/// `None` once the player reports PLAYING / BUFFERING, else why it did not.
fn wait_playing(first: Option<&serde_json::Value>, events: &Events, transport: &str, timeout: Duration) -> Option<String> {
    let state = |v: &serde_json::Value| -> Option<Result<(), String>> {
        match v["type"].as_str()? {
            "LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST" => Some(Err(v["type"].as_str().unwrap().to_string())),
            "MEDIA_STATUS" => {
                let st = v["status"].as_array()?.first()?;
                match st["playerState"].as_str()? {
                    "PLAYING" | "BUFFERING" | "PAUSED" => Some(Ok(())),
                    "IDLE" if st["idleReason"].is_string() => Some(Err(format!("IDLE/{}", st["idleReason"].as_str().unwrap()))),
                    _ => None,
                }
            }
            _ => None,
        }
    };
    if let Some(r) = first.and_then(state) {
        return r.err();
    }
    let deadline = Instant::now() + timeout;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(msg) = events.recv_timeout(left) else { break };
        if msg.namespace != NS_MEDIA || msg.source_id != transport {
            continue;
        }
        if let Some(r) = msg.payload_json().as_ref().and_then(state) {
            return r.err();
        }
    }
    Some("trình phát trên TV không bắt đầu phát trong 20 s".into())
}

// ---------------------------------------------------------------- monitor

fn monitor(shared: &Shared, ch: &Channel, events: &Events, mode: CastMode, session_id: &str, transport: &str, server: Option<&LiveServer>) {
    let mut last_poll = Instant::now();
    let mut media_session: Option<u64> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        if ch.is_closed() {
            let why = ch.close_reason().unwrap_or_default();
            let mut s = shared.stats.lock().unwrap();
            s.connected = false;
            s.message = format!("Mất kết nối với TV: {why}");
            drop(s);
            shared.stop.store(true, Ordering::Relaxed);
            shared.video.stop.store(true, Ordering::Relaxed);
            return;
        }
        if let Ok(msg) = events.recv_timeout(Duration::from_millis(200)) {
            let Some(v) = msg.payload_json() else { continue };
            let ty = v["type"].as_str().unwrap_or("");
            let ended = match (msg.namespace.as_str(), ty) {
                (NS_CONNECTION, "CLOSE") if msg.source_id == transport => Some("TV đã đóng phiên cast"),
                (NS_RECEIVER, "RECEIVER_STATUS") => {
                    let apps = v["status"]["applications"].as_array().cloned().unwrap_or_default();
                    (!apps.iter().any(|a| a["sessionId"].as_str() == Some(session_id))).then_some("TV đã thoát chế độ cast (có người bấm nút trên TV?)")
                }
                (NS_MEDIA, "MEDIA_STATUS") => {
                    if let Some(st) = v["status"].as_array().and_then(|a| a.first()) {
                        media_session = st["mediaSessionId"].as_u64().or(media_session);
                        if st["playerState"] == "IDLE" && st["idleReason"] == "ERROR" {
                            Some("Trình phát trên TV báo lỗi khi phát luồng hình")
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(why) = ended {
                let mut s = shared.stats.lock().unwrap();
                s.connected = false;
                s.message = why.into();
                drop(s);
                shared.stop.store(true, Ordering::Relaxed);
                shared.video.stop.store(true, Ordering::Relaxed);
                return;
            }
        }
        // Media path: poll the player position for the delay estimate.
        if let (CastMode::MediaReceiver, Some(server)) = (mode, server) {
            if last_poll.elapsed() >= Duration::from_secs(2) {
                last_poll = Instant::now();
                let mut req = json!({"type": "GET_STATUS"});
                if let Some(id) = media_session {
                    req["mediaSessionId"] = json!(id);
                }
                if let Ok(v) = ch.request(NS_MEDIA, transport, req, Duration::from_secs(3)) {
                    if let Some(st) = v["status"].as_array().and_then(|a| a.first()) {
                        media_session = st["mediaSessionId"].as_u64().or(media_session);
                        let connected = server.connected.load(Ordering::Relaxed) > 0;
                        shared.stats.lock().unwrap().connected = connected;
                        if let (Some(pos), "PLAYING") = (st["currentTime"].as_f64(), st["playerState"].as_str().unwrap_or("")) {
                            let now_us = shared_media_now_us(shared);
                            let start = server.start_pts_us.load(Ordering::Relaxed);
                            let pos_us = (pos * 1e6) as u64;
                            // Players either keep the stream's timestamps or start at 0.
                            let played = if pos_us + 500_000 < start / 2 { start + pos_us } else { pos_us };
                            if let Some(now_us) = now_us {
                                let delay = now_us.saturating_sub(played) / 1000;
                                if delay < 30_000 {
                                    shared.media_delay_ms.store(delay as u32, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Media time "now" on the session clock: the newest frame's pts plus its age.
fn shared_media_now_us(shared: &Shared) -> Option<u64> {
    let (pts, at) = *shared.last_frame.lock().unwrap();
    at.map(|at| pts + at.elapsed().as_micros() as u64)
}
