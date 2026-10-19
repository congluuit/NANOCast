//! Spike: stream an H.264 Annex B file (e.g. the `out.h264` written by `spike-win`) to a
//! NANOCast receiver over UDP using the core protocol, paced at a fixed fps. A file named
//! `*.h265` / `*.hevc` (`spike-win --codec hevc --out x.h265`) is split as HEVC; the
//! receiver must be told `start {codec: "hevc"}` (e.g. `ctrl_probe --codec hevc`).
//!
//! Usage:
//!   spike-send <file.h264> [--to 192.168.1.50:48322] [--fps 60] [--pace-mbps 80]
//!              [--loop] [--seconds N] [--audio-dump file.nca]
//!   spike-send --audio-dump file.nca [--to host] [--loop] [--seconds N]
//!
//! `--audio-dump` replays audio packets recorded by
//! `cargo run -p nanocast-win --example audio_probe -- 20 --dump file.nca`
//! (u16 LE length + packet, repeated) to UDP AUDIO_PORT on the same host, one per 20 ms,
//! renumbering seq/pts so looping looks like one continuous stream.
//!
//! For an Android emulator: `adb emu redir add udp:48322:48322`, then `--to 127.0.0.1:48322`.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use nanocast_core::au::{is_keyframe, is_keyframe_hevc, split_access_units, split_access_units_hevc};
use nanocast_core::protocol::{
    sleep_until, AudioHeader, PacedSender, AUDIO_FRAME_MS, AUDIO_PORT, MAX_PACKETS_PER_FRAME, MAX_PAYLOAD, VIDEO_PORT,
};

struct Args {
    file: String,
    to: String,
    fps: u32,
    pace_mbps: u64,
    looping: bool,
    seconds: Option<f64>,
    audio_dump: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        file: String::new(),
        to: format!("127.0.0.1:{VIDEO_PORT}"),
        fps: 60,
        pace_mbps: 80,
        looping: false,
        seconds: None,
        audio_dump: None,
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
            "--audio-dump" => a.audio_dump = Some(val()?),
            "-h" | "--help" => {
                println!("spike-send <file.h264> [--to host[:port]] [--fps 60] [--pace-mbps 80] [--loop] [--seconds N]");
                std::process::exit(0);
            }
            s if s.starts_with("--") => bail!("unknown option {s}"),
            s => a.file = s.to_string(),
        }
    }
    if a.file.is_empty() && a.audio_dump.is_none() {
        bail!("missing input file (try --help)");
    }
    if a.fps == 0 || a.pace_mbps == 0 {
        bail!("--fps and --pace-mbps must be > 0");
    }
    Ok(a)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let audio = match &args.audio_dump {
        Some(path) => {
            let packets = read_audio_dump(path)?;
            let host = args.to.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or_else(|| args.to.clone());
            let to = format!("{host}:{AUDIO_PORT}");
            let (looping, seconds) = (args.looping, args.seconds);
            println!("{path}: {} audio packets ({:.1} s) -> {to}", packets.len(), packets.len() as f64 * 0.02);
            Some(std::thread::spawn(move || send_audio(packets, &to, looping, seconds)))
        }
        None => None,
    };
    if args.file.is_empty() {
        return audio.unwrap().join().unwrap();
    }
    let data = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file))?;
    let lower = args.file.to_ascii_lowercase();
    let hevc = lower.ends_with(".h265") || lower.ends_with(".hevc");
    let aus = if hevc { split_access_units_hevc(&data) } else { split_access_units(&data) };
    if aus.is_empty() {
        bail!("no {} NAL units found in {}", if hevc { "HEVC" } else { "H.264" }, args.file);
    }
    let keyframes = aus.iter().filter(|au| if hevc { is_keyframe_hevc(au) } else { is_keyframe(au) }).count();
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
    if let Some(a) = audio {
        a.join().unwrap()?;
    }
    Ok(())
}

fn read_audio_dump(path: &str) -> Result<Vec<Vec<u8>>> {
    let data = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let mut out = Vec::new();
    let mut rest = &data[..];
    while rest.len() >= 2 {
        let n = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        let Some(pkt) = rest.get(2..2 + n) else { break };
        if AudioHeader::parse(pkt).is_some() {
            out.push(pkt.to_vec());
        }
        rest = &rest[2 + n..];
    }
    if out.is_empty() {
        bail!("no audio packets in {path}");
    }
    Ok(out)
}

fn send_audio(mut packets: Vec<Vec<u8>>, to: &str, looping: bool, seconds: Option<f64>) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(to).with_context(|| format!("resolving {to}"))?;
    let period = Duration::from_millis(AUDIO_FRAME_MS as u64);
    let started = Instant::now();
    let mut next = started;
    let mut seq = 0u32;
    'outer: loop {
        for pkt in packets.iter_mut() {
            if seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s) {
                break 'outer;
            }
            AudioHeader { seq, pts_ms: seq.wrapping_mul(AUDIO_FRAME_MS) }.write(pkt);
            seq = seq.wrapping_add(1);
            sleep_until(next);
            next += period;
            let _ = sock.send(pkt);
        }
        if !looping {
            break;
        }
    }
    println!("sent {seq} audio packets in {:.1}s", started.elapsed().as_secs_f64());
    Ok(())
}
