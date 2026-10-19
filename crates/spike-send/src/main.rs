//! Spike: stream an H.264 Annex B file (e.g. the `out.h264` written by `spike-win`) to a
//! NANOCast receiver over UDP using the core protocol, paced at a fixed fps.
//!
//! Usage:
//!   spike-send <file.h264> [--to 192.168.1.50:48322] [--fps 60] [--pace-mbps 80]
//!              [--loop] [--seconds N]
//!
//! For an Android emulator: `adb emu redir add udp:48322:48322`, then `--to 127.0.0.1:48322`.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use nanocast_core::au::{is_keyframe, split_access_units};
use nanocast_core::protocol::{sleep_until, PacedSender, MAX_PACKETS_PER_FRAME, MAX_PAYLOAD, VIDEO_PORT};

struct Args {
    file: String,
    to: String,
    fps: u32,
    pace_mbps: u64,
    looping: bool,
    seconds: Option<f64>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        file: String::new(),
        to: format!("127.0.0.1:{VIDEO_PORT}"),
        fps: 60,
        pace_mbps: 80,
        looping: false,
        seconds: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--to" => {
                let v = val()?;
                a.to = if v.contains(':') { v } else { format!("{v}:{VIDEO_PORT}") };
            }
            "--fps" => a.fps = val()?.parse()?,
            "--pace-mbps" => a.pace_mbps = val()?.parse()?,
            "--loop" => a.looping = true,
            "--seconds" => a.seconds = Some(val()?.parse()?),
            "-h" | "--help" => {
                println!("spike-send <file.h264> [--to host[:port]] [--fps 60] [--pace-mbps 80] [--loop] [--seconds N]");
                std::process::exit(0);
            }
            s if s.starts_with("--") => bail!("unknown option {s}"),
            s => a.file = s.to_string(),
        }
    }
    if a.file.is_empty() {
        bail!("missing input file (try --help)");
    }
    if a.fps == 0 || a.pace_mbps == 0 {
        bail!("--fps and --pace-mbps must be > 0");
    }
    Ok(a)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let data = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file))?;
    let aus = split_access_units(&data);
    if aus.is_empty() {
        bail!("no H.264 NAL units found in {}", args.file);
    }
    let keyframes = aus.iter().filter(|au| is_keyframe(au)).count();
    let max_au = aus.iter().map(|au| au.len()).max().unwrap_or(0);
    if max_au.div_ceil(MAX_PAYLOAD) > MAX_PACKETS_PER_FRAME {
        bail!("largest frame is {max_au} B = more than {MAX_PACKETS_PER_FRAME} packets; lower the bitrate");
    }
    println!(
        "{}: {:.1} MB, {} access units ({} keyframes), largest {} KB -> {} @ {} fps, pace {} Mbps{}",
        args.file,
        data.len() as f64 / 1e6,
        aus.len(),
        keyframes,
        max_au / 1024,
        args.to,
        args.fps,
        args.pace_mbps,
        if args.looping { ", looping" } else { "" }
    );

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(&args.to).with_context(|| format!("resolving {}", args.to))?;
    let mut sender = PacedSender::new(sock, args.pace_mbps * 1_000_000, MAX_PAYLOAD);

    let period = Duration::from_secs_f64(1.0 / args.fps as f64);
    let started = Instant::now();
    let stop_at = args.seconds.map(|s| started + Duration::from_secs_f64(s));
    let mut next = started;

    let mut rep_at = started + Duration::from_secs(1);
    let (mut rep_frames, mut rep_bytes, mut rep_pkts, mut rep_late) = (0u32, 0usize, 0u32, 0u32);
    let mut rep_spread = Duration::ZERO;
    let (mut total_frames, mut total_pkts) = (0u64, 0u64);

    'outer: loop {
        for au in &aus {
            if stop_at.is_some_and(|t| Instant::now() >= t) {
                break 'outer;
            }
            sleep_until(next);
            if Instant::now() > next + period / 2 {
                rep_late += 1;
            }
            next += period;
            if Instant::now() > next + period * 4 {
                next = Instant::now() + period; // fell far behind: resync instead of bursting
            }

            if let Some(st) = sender.send_frame(au)? {
                rep_pkts += st.packets as u32;
                total_pkts += st.packets as u64;
                rep_bytes += st.bytes;
                rep_spread = rep_spread.max(st.spread);
            }
            rep_frames += 1;
            total_frames += 1;

            let now = Instant::now();
            if now >= rep_at {
                let dt = (now - rep_at + Duration::from_secs(1)).as_secs_f64();
                println!(
                    "t={:6.1}s  {:5.1} fps  {:5.1} Mbps  {:5} pkt/s  late {}  max spread {:.1} ms",
                    (now - started).as_secs_f64(),
                    rep_frames as f64 / dt,
                    rep_bytes as f64 * 8.0 / dt / 1e6,
                    rep_pkts,
                    rep_late,
                    rep_spread.as_secs_f64() * 1e3
                );
                rep_at = now + Duration::from_secs(1);
                (rep_frames, rep_bytes, rep_pkts, rep_late) = (0, 0, 0, 0);
                rep_spread = Duration::ZERO;
            }
        }
        if !args.looping {
            break;
        }
    }

    let secs = started.elapsed().as_secs_f64();
    println!(
        "sent {total_frames} frames / {total_pkts} packets in {secs:.1}s ({:.1} fps)",
        total_frames as f64 / secs
    );
    Ok(())
}
