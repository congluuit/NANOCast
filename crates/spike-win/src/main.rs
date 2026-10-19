//! Spike: Windows Graphics Capture -> NV12 -> Media Foundation H.264 (HW) -> .h264 file.
//! Prints per-frame encode latency and end-to-end (capture -> encoded) latency.
//!
//! With `--to` or `--discover` it also streams live to a NANOCast receiver (TV) over UDP,
//! and opens the TCP control channel (hello/start/stop; the receiver's keyframe_request
//! forces an IDR). Without a control port on the receiver it streams UDP only.
//!
//! Usage:
//!   spike-win [--window <title substring>] [--monitor <index>] [--seconds 10]
//!             [--fps 60] [--bitrate-mbps 20] [--encoder <index>] [--out out.h264] [--list]
//!             [--to <ip[:port]>] [--discover] [--pace-mbps 80] [--keyframe-every <sec>]

use std::fs::File;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use nanocast_core::control::Message;
use nanocast_core::discovery;
use nanocast_core::protocol::{PacedSender, CONTROL_PORT, MAX_PAYLOAD, VIDEO_PORT};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, GraphicsCaptureItemType,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use nanocast_win::control_link::ControlLink;
use nanocast_win::convert;
use nanocast_win::mf_encoder::{self, EncoderConfig, MfEncoder, MfRuntime};

struct FrameMsg {
    nv12: Vec<u8>,
    width: u32,
    height: u32,
    captured: Instant,
    convert_us: u64,
}

struct Flags {
    tx: SyncSender<FrameMsg>,
    deadline: Instant,
    /// Minimum spacing between accepted frames (pacing to the target fps).
    min_interval: Duration,
    captured: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

struct Capture {
    flags: Flags,
    scratch: Vec<u8>,
    next_due: Option<Instant>,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { flags: ctx.flags, scratch: Vec::new(), next_due: None })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        let captured = Instant::now();
        if captured >= self.flags.deadline {
            control.stop();
            return Ok(());
        }
        // Pace to the target fps: a 144 Hz monitor delivers up to 144 frames/s.
        // Keep a running "next due" time so the average rate is exact even when
        // the monitor period does not divide the target period.
        match self.next_due {
            Some(due) if captured + Duration::from_millis(1) < due => return Ok(()),
            Some(due) => {
                let mut next = due + self.flags.min_interval;
                if next < captured {
                    next = captured + self.flags.min_interval; // fell behind: resync
                }
                self.next_due = Some(next);
            }
            None => self.next_due = Some(captured + self.flags.min_interval),
        }
        let (src_w, src_h) = (frame.width(), frame.height());
        let (w, h) = (src_w & !1, src_h & !1);
        if w < 16 || h < 16 {
            return Ok(());
        }
        let buffer = frame.buffer()?;
        let bgra = buffer.as_nopadding_buffer(&mut self.scratch);

        let t = Instant::now();
        let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        convert::bgra_to_nv12(bgra, src_w, w, h, &mut nv12);
        let convert_us = t.elapsed().as_micros() as u64;

        self.flags.captured.fetch_add(1, Ordering::Relaxed);
        match self.flags.tx.try_send(FrameMsg { nv12, width: w, height: h, captured, convert_us }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.flags.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => control.stop(),
        }
        Ok(())
    }
}

struct Args {
    window: Option<String>,
    monitor: usize,
    seconds: u64,
    fps: u32,
    bitrate_mbps: u32,
    /// None: first encoder that starts, in merit order.
    encoder: Option<usize>,
    out: String,
    list: bool,
    to: Option<String>,
    discover: bool,
    pace_mbps: u64,
    keyframe_every: Option<f64>,
}

fn parse_args() -> Args {
    let mut a = Args {
        window: None,
        monitor: 0,
        seconds: 10,
        fps: 60,
        bitrate_mbps: 20,
        encoder: None,
        out: "out.h264".into(),
        list: false,
        to: None,
        discover: false,
        pace_mbps: 80,
        keyframe_every: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().expect("missing value");
        match k.as_str() {
            "--window" => a.window = Some(val()),
            "--monitor" => a.monitor = val().parse().unwrap(),
            "--seconds" => a.seconds = val().parse().unwrap(),
            "--fps" => a.fps = val().parse().unwrap(),
            "--bitrate-mbps" => a.bitrate_mbps = val().parse().unwrap(),
            "--encoder" => a.encoder = Some(val().parse().unwrap()),
            "--out" => a.out = val(),
            "--list" => a.list = true,
            "--to" => {
                let v = val();
                a.to = Some(if v.contains(':') { v } else { format!("{v}:{VIDEO_PORT}") });
            }
            "--discover" => a.discover = true,
            "--pace-mbps" => a.pace_mbps = val().parse().unwrap(),
            "--keyframe-every" => a.keyframe_every = Some(val().parse().unwrap()),
            other => panic!("unknown arg {other}"),
        }
    }
    a
}

fn start_capture<T>(item: T, _fps: u32, flags: Flags) -> Result<windows_capture::capture::CaptureControl<Capture, <Capture as GraphicsCaptureApiHandler>::Error>>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let settings = Settings::new(
        item,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        // Custom intervals need Windows 11; pacing is done in on_frame_arrived instead.
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    Capture::start_free_threaded(settings).map_err(|e| anyhow::anyhow!("capture start: {e:?}"))
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn main() -> Result<()> {
    let args = parse_args();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let _rt = MfRuntime::init()?;

    let encoders = mf_encoder::list_hw_h264_encoders()?;
    println!("Hardware H.264 encoders (Media Foundation):");
    for (i, (name, _)) in encoders.iter().enumerate() {
        println!("  [{i}] {name}");
    }
    if args.list {
        println!("Windows:");
        for w in Window::enumerate().unwrap_or_default() {
            if let (Ok(t), Ok(p)) = (w.title(), w.process_name()) {
                if !t.is_empty() {
                    println!("  {p}: {t}");
                }
            }
        }
        return Ok(());
    }

    // Where to stream: explicit --to, or the single receiver found on the LAN.
    let mut target = args.to.clone();
    let mut ctrl_port = CONTROL_PORT;
    if args.discover {
        println!("Looking for receivers (_nanocast._udp) for 3 s...");
        let found = discovery::discover(Duration::from_secs(3))?;
        for r in &found {
            println!("  {} at {}:{} ctrl {} ({}x{}, {}, v{})", r.name, r.addr, r.port, r.ctrl_port,
                r.width.unwrap_or(0), r.height.unwrap_or(0), r.codec, r.version);
        }
        if target.is_none() {
            match found.as_slice() {
                [] => bail!("no receiver found; start the NANOCast app on the TV or pass --to"),
                [one] => {
                    target = Some(format!("{}:{}", one.addr, one.port));
                    ctrl_port = one.ctrl_port;
                }
                _ => bail!("several receivers found; pick one with --to"),
            }
        }
    }

    // Network sender thread: encoded frames go through a small queue so pacing
    // never stalls the encoder loop.
    let net_sent = Arc::new(AtomicUsize::new(0));
    let net_dropped = Arc::new(AtomicUsize::new(0));
    let net_max_spread_us = Arc::new(AtomicUsize::new(0));
    let want_keyframe = Arc::new(AtomicBool::new(false));
    let mut control_link: Option<ControlLink> = None;
    let net_tx: Option<SyncSender<Vec<u8>>> = if let Some(to) = &target {
        let addr: SocketAddr = to
            .to_socket_addrs()
            .with_context(|| format!("resolving {to}"))?
            .next()
            .with_context(|| format!("no address for {to}"))?;
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.connect(addr)?;
        println!("Streaming to {to} (pace {} Mbps)", args.pace_mbps);

        // Control channel is best effort: without it the UDP stream still plays, it just
        // cannot ask for a keyframe after a loss.
        let ctrl_addr = SocketAddr::new(addr.ip(), ctrl_port);
        let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "NANOCast PC".into());
        match ControlLink::open(ctrl_addr, &name, want_keyframe.clone()) {
            Ok(link) => {
                println!("Control channel: connected to {ctrl_addr}");
                control_link = Some(link);
            }
            Err(e) => eprintln!("warning: control channel to {ctrl_addr} unavailable ({e}); streaming UDP only"),
        }

        let mut sender = PacedSender::new(sock, args.pace_mbps * 1_000_000, MAX_PAYLOAD);
        let (stx, srx) = sync_channel::<Vec<u8>>(4);
        let (sent, spread) = (net_sent.clone(), net_max_spread_us.clone());
        std::thread::spawn(move || {
            while let Ok(au) = srx.recv() {
                match sender.send_frame(&au) {
                    Ok(Some(st)) => {
                        sent.fetch_add(1, Ordering::Relaxed);
                        spread.fetch_max(st.spread.as_micros() as usize, Ordering::Relaxed);
                    }
                    Ok(None) => eprintln!("frame of {} B too large to packetize; lower the bitrate", au.len()),
                    Err(e) => eprintln!("send error: {e}"),
                }
            }
        });
        Some(stx)
    } else {
        None
    };

    let (tx, rx): (SyncSender<FrameMsg>, Receiver<FrameMsg>) = sync_channel(2);
    let captured = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let flags = Flags {
        tx,
        deadline: Instant::now() + Duration::from_secs(args.seconds),
        min_interval: Duration::from_micros(1_000_000 / args.fps as u64),
        captured: captured.clone(),
        dropped: dropped.clone(),
    };

    let control = if let Some(title) = &args.window {
        let w = Window::from_contains_name(title).context("window not found")?;
        println!("Capturing window: {:?} ({}x{})", w.title()?, w.width()?, w.height()?);
        start_capture(w, args.fps, flags)?
    } else {
        // windows-capture indexes monitors from 1; 0 means primary.
        let m = if args.monitor == 0 {
            Monitor::primary().context("primary monitor not found")?
        } else {
            Monitor::from_index(args.monitor).context("monitor not found")?
        };
        println!("Capturing monitor {}: {} ({}x{} @ {} Hz)", args.monitor, m.name()?, m.width()?, m.height()?, m.refresh_rate()?);
        start_capture(m, args.fps, flags)?
    };
    println!("Running for {} s... move windows / scroll to generate frames.", args.seconds);

    let mut file = File::create(&args.out)?;
    let mut encoder: Option<MfEncoder> = None;
    let mut enc_dims = (0u32, 0u32);
    let t0 = Instant::now();

    let mut encode_lat = Vec::<u64>::new();
    let mut e2e_lat = Vec::<u64>::new();
    let mut convert_us = Vec::<u64>::new();
    let mut bytes = 0usize;
    let mut keyframes = 0usize;
    let mut encoded = 0usize;
    let mut first_out: Option<Instant> = None;
    let mut last_out: Option<Instant> = None;
    let mut next_keyframe_at: Option<Instant> = None;

    let net_dropped_c = net_dropped.clone();
    let mut on_output = |f: mf_encoder::EncodedFrame, file: &mut File| {
        let now = Instant::now();
        if let Some(stx) = &net_tx {
            if stx.try_send(f.data.clone()).is_err() {
                net_dropped_c.fetch_add(1, Ordering::Relaxed);
            }
        }
        file.write_all(&f.data).expect("write");
        bytes += f.data.len();
        encoded += 1;
        if f.keyframe {
            keyframes += 1;
        }
        encode_lat.push(f.encode_latency_us);
        e2e_lat.push(now.duration_since(f.capture_instant).as_micros() as u64);
        first_out.get_or_insert(now);
        last_out = Some(now);
    };

    // CaptureControl keeps the handler (and its sender) alive, so the channel
    // never disconnects on its own: stop on the deadline instead.
    let stop_at = t0 + Duration::from_secs(args.seconds) + Duration::from_millis(300);
    loop {
        if Instant::now() >= stop_at {
            break;
        }
        let msg = match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(enc) = encoder.as_mut() {
                    enc.poll_output(|f| on_output(f, &mut file))?;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if encoder.is_none() {
            let cfg = EncoderConfig {
                width: msg.width,
                height: msg.height,
                fps: args.fps,
                bitrate_bps: args.bitrate_mbps * 1_000_000,
                // Keyframes only on request (or --keyframe-every) once intra refresh works.
                gop_frames: args.fps * 10,
                encoder_index: args.encoder.unwrap_or(0),
            };
            let enc = match args.encoder {
                Some(_) => MfEncoder::new(&cfg)?,
                None => MfEncoder::open_first_working(&cfg, None, &[])?,
            };
            println!("Encoder: {} -> {}x{} @ {} fps, {} Mbps, NV12 in", enc.name, msg.width, msg.height, args.fps, args.bitrate_mbps);
            enc_dims = (msg.width, msg.height);
            encoder = Some(enc);
            if let Some(link) = control_link.as_mut() {
                link.send(&Message::Start { w: msg.width, h: msg.height, fps: args.fps, codec: "h264".into(), audio: false });
            }
        }
        if (msg.width, msg.height) != enc_dims {
            // Window resized: the spike just skips those frames.
            continue;
        }
        let enc = encoder.as_mut().unwrap();
        convert_us.push(msg.convert_us);
        if let Some(every) = args.keyframe_every {
            if next_keyframe_at.is_none_or(|t| msg.captured >= t) {
                enc.request_keyframe();
                next_keyframe_at = Some(msg.captured + Duration::from_secs_f64(every));
            }
        }
        // keyframe_request from the receiver (set by the control reader thread).
        if want_keyframe.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
        }
        enc.wait_need_input(|f| on_output(f, &mut file))?;
        let pts_hns = msg.captured.duration_since(t0).as_nanos() as i64 / 100;
        enc.submit(&msg.nv12, pts_hns, msg.captured)?;
        enc.poll_output(|f| on_output(f, &mut file))?;
    }

    // Give the encoder a moment to flush the last frames.
    if let Some(enc) = encoder.as_mut() {
        let until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < until {
            enc.poll_output(|f| on_output(f, &mut file))?;
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let _ = control.stop();
    drop(on_output);
    let keyframe_requests = control_link.as_ref().map(|l| l.keyframe_requests.load(Ordering::Relaxed));
    if let Some(link) = control_link.take() {
        link.close();
    }
    file.flush()?;

    encode_lat.sort_unstable();
    e2e_lat.sort_unstable();
    let avg = |v: &[u64]| if v.is_empty() { 0 } else { v.iter().sum::<u64>() / v.len() as u64 };
    let span = match (first_out, last_out) {
        (Some(a), Some(b)) if b > a => b.duration_since(a).as_secs_f64(),
        _ => 0.0,
    };

    println!();
    println!("=== Results ({}x{}) ===", enc_dims.0, enc_dims.1);
    println!("captured frames : {}", captured.load(Ordering::Relaxed));
    println!("dropped (queue) : {}", dropped.load(Ordering::Relaxed));
    println!("encoded frames  : {encoded} ({keyframes} keyframes)");
    println!("output fps      : {:.1}", if span > 0.0 { encoded as f64 / span } else { 0.0 });
    println!("bitrate actual  : {:.1} Mbps", if span > 0.0 { bytes as f64 * 8.0 / span / 1e6 } else { 0.0 });
    println!("bgra->nv12 cpu  : avg {:.2} ms", avg(&convert_us) as f64 / 1000.0);
    println!(
        "encode latency  : avg {:.2} ms  p50 {:.2}  p95 {:.2}  max {:.2}",
        avg(&encode_lat) as f64 / 1000.0,
        percentile(&encode_lat, 0.5) as f64 / 1000.0,
        percentile(&encode_lat, 0.95) as f64 / 1000.0,
        encode_lat.last().copied().unwrap_or(0) as f64 / 1000.0
    );
    println!(
        "capture->encoded: avg {:.2} ms  p50 {:.2}  p95 {:.2}  max {:.2}",
        avg(&e2e_lat) as f64 / 1000.0,
        percentile(&e2e_lat, 0.5) as f64 / 1000.0,
        percentile(&e2e_lat, 0.95) as f64 / 1000.0,
        e2e_lat.last().copied().unwrap_or(0) as f64 / 1000.0
    );
    println!("file            : {} ({:.1} MB)  -> play: ffplay -f h264 {}", args.out, bytes as f64 / 1e6, args.out);
    if let Some(to) = &target {
        println!(
            "network         : {} -> sent {} frames, dropped {} (queue full), max frame spread {:.1} ms",
            to,
            net_sent.load(Ordering::Relaxed),
            net_dropped.load(Ordering::Relaxed),
            net_max_spread_us.load(Ordering::Relaxed) as f64 / 1000.0
        );
        match keyframe_requests {
            Some(n) => println!("control         : was connected, {n} keyframe_request(s) received"),
            None => println!("control         : not connected (UDP only)"),
        }
    }
    Ok(())
}
