//! Test sender for the audio path: a 440 Hz tone (1 s on, 1 s silence) as NANOCast audio
//! packets (20 ms Opus, `nanocast_core::audio`) paced in real time to a receiver.
//!
//!   cargo run --release -p nanocast-receiver --example send_tone -- [host[:port]] [seconds]
//!
//! Default target 127.0.0.1:48324 for 10 s.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use nanocast_core::audio::{AudioEncoder, FRAME_LEN};
use nanocast_core::protocol::{sleep_until, AUDIO_BITRATE, AUDIO_FRAME_SAMPLES, AUDIO_PORT, AUDIO_SAMPLE_RATE};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let to = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let to = if to.contains(':') { to } else { format!("{to}:{AUDIO_PORT}") };
    let seconds: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10.0);

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(&to)?;
    let mut enc = AudioEncoder::new(AUDIO_BITRATE, 5)?;
    let start = Instant::now();
    let mut sent = 0u64;
    let mut sample = 0u64;
    let mut pcm = vec![0f32; FRAME_LEN];
    while start.elapsed().as_secs_f64() < seconds {
        let on = (sample / AUDIO_SAMPLE_RATE as u64).is_multiple_of(2);
        for f in pcm.chunks_exact_mut(2) {
            let t = sample as f32 / AUDIO_SAMPLE_RATE as f32;
            let s = if on { (t * 440.0 * std::f32::consts::TAU).sin() * 0.05 } else { 0.0 };
            f.copy_from_slice(&[s, s]);
            sample += 1;
        }
        enc.push(&pcm, |p| {
            let _ = sock.send(&p);
            sent += 1;
        })?;
        sleep_until(start + Duration::from_micros(sent * AUDIO_FRAME_SAMPLES as u64 * 1_000_000 / AUDIO_SAMPLE_RATE as u64));
    }
    println!("sent {sent} audio packets ({} silent) to {to}", enc.silent_frames());
    Ok(())
}
