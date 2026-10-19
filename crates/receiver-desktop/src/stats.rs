//! Counters shared by the network, decoder, render and control threads (mirrors `Stats.kt`).
//!
//! "Decode ms" = time from the moment the last packet of a frame arrived (frame complete)
//! to the moment the decoder returned the picture, i.e. queue wait + decode.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

/// Decode latencies kept for the rolling average (about one second at 60 fps).
const WINDOW: usize = 60;

#[derive(Default)]
pub struct Stats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub lost_packets: AtomicU64,
    pub bad_packets: AtomicU64,
    pub dropped_frames: AtomicU64,
    pub stream_restarts: AtomicU64,
    pub frames_in: AtomicU64,
    pub frames_decoded: AtomicU64,
    pub frames_presented: AtomicU64,
    pub decoder_errors: AtomicU64,
    pub keyframe_requests: AtomicU64,
    pub audio_packets: AtomicU64,
    pub audio_lost: AtomicU64,
    /// Times the audio buffer ran dry while playing.
    pub audio_underruns: AtomicU64,
    /// Frame complete -> picture presented, summed over the session (µs).
    pub present_us_total: AtomicU64,
    /// Time inside `VideoDecoder::decode` alone (no queue wait), summed (µs).
    pub decode_call_us_total: AtomicU64,
    pub decode_calls: AtomicU64,
    window: Mutex<Window>,
    /// Whole-session decode latency, 0.1 ms buckets up to 200 ms (last = overflow).
    hist: Mutex<Vec<u64>>,
}

struct Window {
    us: [u64; WINDOW],
    len: usize,
    pos: usize,
}

impl Default for Window {
    fn default() -> Self {
        Self { us: [0; WINDOW], len: 0, pos: 0 }
    }
}

impl Stats {
    pub fn new() -> Self {
        Self { hist: Mutex::new(vec![0; 2001]), ..Default::default() }
    }

    pub fn on_decode_call(&self, us: u64) {
        self.decode_calls.fetch_add(1, Relaxed);
        self.decode_call_us_total.fetch_add(us, Relaxed);
    }

    pub fn decode_call_avg_ms(&self) -> f64 {
        let n = self.decode_calls.load(Relaxed);
        if n == 0 {
            return 0.0;
        }
        self.decode_call_us_total.load(Relaxed) as f64 / n as f64 / 1000.0
    }

    pub fn on_presented(&self, latency_us: u64) {
        self.frames_presented.fetch_add(1, Relaxed);
        self.present_us_total.fetch_add(latency_us, Relaxed);
    }

    /// Average frame-complete -> presented time over the session, in ms.
    pub fn present_avg_ms(&self) -> f64 {
        let n = self.frames_presented.load(Relaxed);
        if n == 0 {
            return 0.0;
        }
        self.present_us_total.load(Relaxed) as f64 / n as f64 / 1000.0
    }

    pub fn on_packet(&self, len: usize) {
        self.packets.fetch_add(1, Relaxed);
        self.bytes.fetch_add(len as u64, Relaxed);
    }

    pub fn on_decoded(&self, latency_us: u64) {
        self.frames_decoded.fetch_add(1, Relaxed);
        let mut w = self.window.lock().unwrap();
        let pos = w.pos;
        w.us[pos] = latency_us;
        w.pos = (pos + 1) % WINDOW;
        w.len = (w.len + 1).min(WINDOW);
        drop(w);
        let mut h = self.hist.lock().unwrap();
        let last = h.len() - 1;
        h[((latency_us / 100) as usize).min(last)] += 1;
    }

    /// Average of the last ~60 decode latencies, in ms.
    pub fn window_avg_ms(&self) -> f64 {
        let w = self.window.lock().unwrap();
        if w.len == 0 {
            return 0.0;
        }
        w.us[..w.len].iter().sum::<u64>() as f64 / w.len as f64 / 1000.0
    }

    /// Session (count, avg, p50, p95, p99) decode latency in ms, for the exit summary.
    pub fn session_summary(&self) -> (u64, f64, f64, f64, f64) {
        let h = self.hist.lock().unwrap();
        let n: u64 = h.iter().sum();
        if n == 0 {
            return (0, 0.0, 0.0, 0.0, 0.0);
        }
        let avg = h.iter().enumerate().map(|(i, &c)| (i as f64 + 0.5) * 0.1 * c as f64).sum::<f64>() / n as f64;
        let pct = |p: f64| {
            let target = (n as f64 * p).ceil() as u64;
            let mut acc = 0;
            for (i, &c) in h.iter().enumerate() {
                acc += c;
                if acc >= target {
                    return (i as f64 + 0.5) * 0.1;
                }
            }
            200.0
        };
        (n, avg, pct(0.5), pct(0.95), pct(0.99))
    }
}

/// Per-second rates computed from the cumulative counters (one instance per reader:
/// the overlay and the control channel each keep their own).
pub struct RateMeter {
    at: Instant,
    decoded: u64,
    presented: u64,
    lost: u64,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rates {
    pub fps_decoded: f64,
    pub fps_presented: f64,
    pub lost: u64,
    pub mbps: f64,
    pub decode_ms: f64,
}

impl RateMeter {
    pub fn new(stats: &Stats) -> Self {
        Self {
            at: Instant::now(),
            decoded: stats.frames_decoded.load(Relaxed),
            presented: stats.frames_presented.load(Relaxed),
            lost: stats.lost_packets.load(Relaxed),
            bytes: stats.bytes.load(Relaxed),
        }
    }

    pub fn sample(&mut self, stats: &Stats) -> Rates {
        let now = Instant::now();
        let dt = (now - self.at).as_secs_f64().max(1e-3);
        let decoded = stats.frames_decoded.load(Relaxed);
        let presented = stats.frames_presented.load(Relaxed);
        let lost = stats.lost_packets.load(Relaxed);
        let bytes = stats.bytes.load(Relaxed);
        let r = Rates {
            fps_decoded: (decoded - self.decoded) as f64 / dt,
            fps_presented: (presented - self.presented) as f64 / dt,
            lost: lost.saturating_sub(self.lost),
            mbps: (bytes - self.bytes) as f64 * 8.0 / dt / 1e6,
            // No frames this interval: report 0 rather than the stale average of old frames.
            decode_ms: if decoded == self.decoded { 0.0 } else { stats.window_avg_ms() },
        };
        *self = Self { at: now, decoded, presented, lost, bytes };
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_and_summary() {
        let s = Stats::new();
        assert_eq!(s.window_avg_ms(), 0.0);
        for us in [1000, 2000, 3000] {
            s.on_decoded(us);
        }
        assert!((s.window_avg_ms() - 2.0).abs() < 1e-9);
        let (n, avg, p50, _, p99) = s.session_summary();
        assert_eq!(n, 3);
        assert!((avg - 2.05).abs() < 0.01, "{avg}");
        assert!((p50 - 2.05).abs() < 0.01 && (p99 - 3.05).abs() < 0.01);
        // Older samples fall out of the window.
        for _ in 0..WINDOW {
            s.on_decoded(5000);
        }
        assert!((s.window_avg_ms() - 5.0).abs() < 1e-9);
    }
}
