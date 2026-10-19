//! Adaptive bitrate (PLAN.md Phase 2): pure decision logic, fed once per receiver `stats`
//! report (~1 s) with what the sender sent in that second. Platform sessions apply the
//! result (encoder bitrate / fps) and the GUI shows `target()` and `reason()`.
//!
//! Rules:
//! - Down one step of `LADDER_MBPS` (30 -> 20 -> 15 -> 10) when more than `LOSS_LIMIT` of the
//!   packets of the last second were lost, when keyframe requests keep coming
//!   (`KEYFRAME_BURST` within `KEYFRAME_WINDOW`), or when the TV decodes clearly fewer frames
//!   than were sent and needs longer than a frame interval per frame. At most one step per
//!   `MIN_DOWN_GAP` so the previous step can take effect.
//! - Up one step (never above the configured bitrate) after `CLEAN_FOR_UP` without trouble.
//! - Below `LOW_FPS_BELOW_MBPS` the frame rate drops to 30 fps (fewer, better frames).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const LADDER_MBPS: [u32; 4] = [30, 20, 15, 10];
/// Lost / sent packets in one second above which the bitrate steps down.
pub const LOSS_LIMIT: f64 = 0.01;
/// A second counts as clean below this loss ratio.
pub const CLEAN_LOSS: f64 = 0.0025;
pub const KEYFRAME_BURST: usize = 3;
pub const KEYFRAME_WINDOW: Duration = Duration::from_secs(5);
pub const MIN_DOWN_GAP: Duration = Duration::from_secs(2);
pub const CLEAN_FOR_UP: Duration = Duration::from_secs(10);
pub const LOW_FPS_BELOW_MBPS: u32 = 15;
pub const LOW_FPS: u32 = 30;

/// Why the current target was chosen (the GUI translates it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AdaptReason {
    /// Configured values, nothing happened yet.
    #[default]
    Start,
    /// Loss ratio of the second that triggered the step, in percent.
    PacketLoss { percent: f64 },
    /// Keyframe requests within `KEYFRAME_WINDOW`.
    KeyframeRequests { count: u32 },
    /// The TV decoded `tv_fps` of `sent_fps` frames, `decode_ms` per frame.
    SlowDecoder { tv_fps: f64, sent_fps: f64, decode_ms: f64 },
    /// Stepped back up after a clean period.
    Recovered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub mbps: u32,
    pub fps: u32,
}

/// One second of the stream, as the sender and the receiver saw it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    /// `stats.lost` from the receiver.
    pub lost: u64,
    /// Video packets the sender sent during about the same second.
    pub sent_packets: u64,
    /// Frames the sender sent during that second.
    pub sent_fps: f64,
    /// `keyframe_request`s received since the previous report.
    pub keyframe_requests: u32,
    /// `stats.fps` / `stats.decode_ms` from the receiver.
    pub tv_fps: f64,
    pub decode_ms: f64,
}

pub struct Adaptive {
    ladder: Vec<u32>,
    max_fps: u32,
    level: usize,
    reason: AdaptReason,
    last_change: Instant,
    clean_since: Instant,
    keyframes: VecDeque<Instant>,
}

impl Adaptive {
    /// Starts at the configured values, which are also the ceiling.
    pub fn new(max_mbps: u32, max_fps: u32, now: Instant) -> Self {
        let max_mbps = max_mbps.max(1);
        let mut ladder: Vec<u32> = LADDER_MBPS.iter().copied().filter(|&b| b < max_mbps).collect();
        ladder.insert(0, max_mbps);
        Self {
            ladder,
            max_fps: max_fps.max(1),
            level: 0,
            reason: AdaptReason::Start,
            last_change: now,
            clean_since: now,
            keyframes: VecDeque::new(),
        }
    }

    pub fn target(&self) -> Target {
        let mbps = self.ladder[self.level];
        let fps = if mbps < LOW_FPS_BELOW_MBPS { self.max_fps.min(LOW_FPS) } else { self.max_fps };
        Target { mbps, fps }
    }

    pub fn reason(&self) -> AdaptReason {
        self.reason
    }

    /// Feeds one report; returns the new target when it changed.
    pub fn on_report(&mut self, r: Report, now: Instant) -> Option<Target> {
        for _ in 0..r.keyframe_requests {
            self.keyframes.push_back(now);
        }
        while self.keyframes.front().is_some_and(|&t| now.duration_since(t) > KEYFRAME_WINDOW) {
            self.keyframes.pop_front();
        }
        if r.sent_packets == 0 && r.lost == 0 {
            // Nothing sent (paused, static screen): no evidence either way.
            self.clean_since = now;
            return None;
        }
        let ratio = r.lost as f64 / r.sent_packets.max(1) as f64;
        let slow_decoder = r.sent_fps >= 20.0
            && r.tv_fps < r.sent_fps * 0.8
            && r.decode_ms > 1000.0 / r.sent_fps;

        let trouble = if ratio > LOSS_LIMIT {
            Some(AdaptReason::PacketLoss { percent: (ratio * 1000.0).round() / 10.0 })
        } else if self.keyframes.len() >= KEYFRAME_BURST {
            Some(AdaptReason::KeyframeRequests { count: self.keyframes.len() as u32 })
        } else if slow_decoder {
            Some(AdaptReason::SlowDecoder { tv_fps: r.tv_fps, sent_fps: r.sent_fps, decode_ms: r.decode_ms })
        } else {
            None
        };

        let before = self.target();
        if let Some(reason) = trouble {
            self.clean_since = now;
            if self.level + 1 < self.ladder.len() && now.duration_since(self.last_change) >= MIN_DOWN_GAP {
                self.level += 1;
                self.changed(reason, now);
            }
        } else if ratio >= CLEAN_LOSS || r.keyframe_requests > 1 {
            // Not bad enough to step down, not clean either.
            self.clean_since = now;
        } else if self.level > 0
            && now.duration_since(self.clean_since) >= CLEAN_FOR_UP
            && now.duration_since(self.last_change) >= CLEAN_FOR_UP
        {
            self.level -= 1;
            self.changed(AdaptReason::Recovered, now);
            self.clean_since = now;
        }
        let after = self.target();
        (after != before).then_some(after)
    }

    fn changed(&mut self, reason: AdaptReason, now: Instant) {
        self.reason = reason;
        self.last_change = now;
        // Requests caused by the switch itself (new encoder, IDR) must not count again.
        self.keyframes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(sent: u64) -> Report {
        Report { lost: 0, sent_packets: sent, sent_fps: 60.0, keyframe_requests: 0, tv_fps: 60.0, decode_ms: 8.0 }
    }

    #[test]
    fn steps_down_on_loss_and_back_up_slowly() {
        let t0 = Instant::now();
        let mut a = Adaptive::new(30, 60, t0);
        assert_eq!(a.target(), Target { mbps: 30, fps: 60 });
        let s = |n: u64| t0 + Duration::from_secs(n);
        let lossy = Report { lost: 50, ..clean(2000) }; // 2.5 %
        assert_eq!(a.on_report(lossy, s(1)), None, "first 2 s: wait so the start can settle");
        assert_eq!(a.on_report(lossy, s(2)), Some(Target { mbps: 20, fps: 60 }));
        assert_eq!(a.reason(), AdaptReason::PacketLoss { percent: 2.5 });
        assert_eq!(a.on_report(lossy, s(3)), None, "one step per 2 s");
        assert_eq!(a.on_report(lossy, s(4)), Some(Target { mbps: 15, fps: 60 }));
        assert_eq!(a.on_report(lossy, s(6)), Some(Target { mbps: 10, fps: 30 }), "below 15 Mbps: 30 fps");
        assert_eq!(a.on_report(lossy, s(8)), None, "bottom of the ladder");

        // Last trouble at s(8): up again 10 s later, one step at a time.
        for n in 9..18 {
            assert_eq!(a.on_report(clean(1000), s(n)), None, "t={n}");
        }
        assert_eq!(a.on_report(clean(1000), s(18)), Some(Target { mbps: 15, fps: 60 }));
        assert_eq!(a.reason(), AdaptReason::Recovered);
        assert_eq!(a.on_report(clean(1000), s(27)), None);
        assert_eq!(a.on_report(clean(1000), s(28)), Some(Target { mbps: 20, fps: 60 }));
        assert_eq!(a.on_report(clean(1000), s(38)), Some(Target { mbps: 30, fps: 60 }));
        assert_eq!(a.on_report(clean(1000), s(60)), None, "never above the configured bitrate");
    }

    #[test]
    fn keyframe_request_burst_and_ceiling() {
        let t0 = Instant::now();
        let mut a = Adaptive::new(20, 60, t0);
        let s = |n: u64| t0 + Duration::from_secs(n);
        let kf = Report { keyframe_requests: 1, lost: 1, ..clean(1500) };
        assert_eq!(a.on_report(kf, s(2)), None);
        assert_eq!(a.on_report(kf, s(3)), None);
        assert_eq!(a.on_report(kf, s(4)), Some(Target { mbps: 15, fps: 60 }));
        assert_eq!(a.reason(), AdaptReason::KeyframeRequests { count: 3 });
        // Requests far apart are not a burst; a lone loss still counts as a clean second.
        assert_eq!(a.on_report(kf, s(10)), None);
        assert_eq!(a.target().mbps, 15);
        assert_eq!(a.on_report(kf, s(16)), Some(Target { mbps: 20, fps: 60 }));
    }

    #[test]
    fn slow_decoder_and_odd_ceilings() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut a = Adaptive::new(25, 60, t0);
        let slow = Report { tv_fps: 35.0, decode_ms: 28.0, ..clean(1500) };
        assert_eq!(a.on_report(slow, s(3)), Some(Target { mbps: 20, fps: 60 }));
        assert!(matches!(a.reason(), AdaptReason::SlowDecoder { .. }));
        // A static screen (nothing sent) is neither good nor bad.
        assert_eq!(a.on_report(Report::default(), s(20)), None);

        let mut low = Adaptive::new(10, 30, t0);
        assert_eq!(low.target(), Target { mbps: 10, fps: 30 });
        assert_eq!(low.on_report(Report { lost: 100, ..clean(1000) }, s(5)), None, "nothing below 10");
    }
}
