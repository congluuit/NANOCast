//! Checks the audio path on one PC: WASAPI loopback -> Opus -> UDP 127.0.0.1:AUDIO_PORT,
//! received and decoded again here. Prints kbps, silence packets, losses and the level
//! once per second; optionally writes what was received to a WAV file, or the raw
//! packets to a dump file that `spike-send --audio-dump` can replay (e.g. inside an
//! Android emulator, which UDP from the host cannot reach).
//!
//!   cargo run --release -p nanocast-win --example audio_probe -- [seconds] [--wav out.wav] [--dump out.nca]
//!
//! Play something on the PC while it runs.

use std::io::Write as _;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use nanocast_core::audio::{OpusDecoder, FRAME_LEN};
use nanocast_core::protocol::{AudioHeader, AUDIO_HEADER_LEN, AUDIO_PORT};
use nanocast_win::audio::{AudioCounters, AudioSender};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut seconds = 10u64;
    let mut wav_path = None;
    let mut dump_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--wav" => wav_path = args.next(),
            "--dump" => dump_path = args.next(),
            s => seconds = s.parse()?,
        }
    }

    let rx = UdpSocket::bind(("127.0.0.1", AUDIO_PORT))?;
    rx.set_read_timeout(Some(Duration::from_millis(100)))?;
    let counters = Arc::new(AudioCounters::default());
    let sender = AudioSender::start(SocketAddr::from(([127, 0, 0, 1], AUDIO_PORT)), counters.clone());

    let mut dec = OpusDecoder::new()?;
    let mut pcm = vec![0f32; FRAME_LEN];
    let mut wav: Vec<i16> = Vec::new();
    let mut dump = dump_path.as_ref().map(std::fs::File::create).transpose()?;
    let mut buf = [0u8; 2048];
    let (mut expect_seq, mut lost, mut packets, mut silent, mut bytes, mut peak) = (None, 0u64, 0u64, 0u64, 0u64, 0f32);
    let start = Instant::now();
    let mut last_print = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        if let Ok(n) = rx.recv(&mut buf) {
            let Some(h) = AudioHeader::parse(&buf[..n]) else { continue };
            if let Some(e) = expect_seq {
                lost += h.seq.wrapping_sub(e) as u64;
            }
            expect_seq = Some(h.seq.wrapping_add(1));
            packets += 1;
            bytes += n as u64;
            if let Some(f) = dump.as_mut() {
                f.write_all(&(n as u16).to_le_bytes())?;
                f.write_all(&buf[..n])?;
            }
            let payload = &buf[AUDIO_HEADER_LEN..n];
            if payload.is_empty() {
                silent += 1;
                pcm.fill(0.0);
            } else {
                dec.decode(payload, &mut pcm)?;
            }
            peak = pcm.iter().fold(peak, |m, s| m.max(s.abs()));
            if wav_path.is_some() {
                wav.extend(pcm.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
            }
        }
        if last_print.elapsed() >= Duration::from_secs(1) {
            let secs = last_print.elapsed().as_secs_f64();
            let db = if peak > 0.0 { 20.0 * peak.log10() } else { f32::NEG_INFINITY };
            println!(
                "{:>3}s  {:>5.1} kbps  {:>3} pkts ({} silent)  lost {}  peak {:.1} dBFS",
                start.elapsed().as_secs(),
                bytes as f64 * 8.0 / secs / 1e3,
                packets,
                silent,
                lost,
                db
            );
            (packets, silent, bytes, peak) = (0, 0, 0, 0.0);
            last_print = Instant::now();
        }
        if let Some(e) = counters.error.lock().unwrap().as_ref() {
            anyhow::bail!("audio sender failed: {e}");
        }
    }
    drop(sender);
    println!("device: {:?}, total lost {lost}", counters.device.lock().unwrap());
    if let Some(p) = wav_path {
        write_wav(&p, &wav)?;
        println!("wrote {p} ({:.1} s)", wav.len() as f64 / 96_000.0);
    }
    Ok(())
}

fn write_wav(path: &str, samples: &[i16]) -> Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&48_000u32.to_le_bytes())?;
    f.write_all(&(48_000u32 * 4).to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}
