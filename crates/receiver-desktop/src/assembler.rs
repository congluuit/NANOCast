//! Reassembles UDP packets into H.264 access units (port of `FrameAssembler.kt`).
//!
//! Frames are delivered strictly in increasing frame_id order as soon as all of their packets
//! have arrived. When a newer frame completes while an older one is still missing packets, the
//! older one is given up (counted in `Stats::dropped_frames`): waiting for it would only add
//! latency. `on_loss` fires on every gap in the packet sequence so the sender can be asked for
//! a keyframe right away. Not thread-safe: owned by the single network thread.

use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nanocast_core::protocol::{VideoHeader, HEADER_LEN, MAX_PACKETS_PER_FRAME, MAX_PAYLOAD};

use crate::stats::Stats;

const SLOTS: usize = 8;
/// frame_id this far behind the last delivered one means the sender restarted.
const RESTART_GAP: i64 = 60;
/// No packet for this long: forget partial frames and sequence state.
const IDLE_RESET: Duration = Duration::from_secs(1);
/// Tolerated payload beyond MAX_PAYLOAD (same slack as the Kotlin receiver).
const PAYLOAD_SLACK: usize = 64;

pub struct Frame {
    pub frame_id: i64,
    pub data: Vec<u8>,
    /// When the last packet arrived.
    pub complete_at: Instant,
}

struct Slot {
    frame_id: i64,
    count: usize,
    received: usize,
    have: Vec<bool>,
    parts: Vec<Vec<u8>>,
}

impl Slot {
    fn new() -> Self {
        Self { frame_id: -1, count: 0, received: 0, have: vec![false; MAX_PACKETS_PER_FRAME], parts: Vec::new() }
    }

    fn reset(&mut self, id: i64, count: usize) {
        self.frame_id = id;
        self.count = count;
        self.received = 0;
        self.have[..count].fill(false);
        if self.parts.len() < count {
            self.parts.resize_with(count, || Vec::with_capacity(MAX_PAYLOAD));
        }
    }
}

pub struct Assembler {
    slots: Vec<Slot>,
    last_delivered: i64,
    last_seq: i64,
    last_packet: Option<Instant>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self { slots: (0..SLOTS).map(|_| Slot::new()).collect(), last_delivered: -1, last_seq: -1, last_packet: None }
    }

    /// Feeds one datagram. Returns a completed frame, if this packet finished one.
    pub fn on_packet(&mut self, buf: &[u8], now: Instant, stats: &Stats, mut on_loss: impl FnMut()) -> Option<Frame> {
        let Some(h) = VideoHeader::parse(buf) else {
            stats.bad_packets.fetch_add(1, Relaxed);
            return None;
        };
        let (count, idx) = (h.count as usize, h.idx as usize);
        let payload = &buf[HEADER_LEN..];
        if count == 0 || count > MAX_PACKETS_PER_FRAME || idx >= count || payload.len() > MAX_PAYLOAD + PAYLOAD_SLACK {
            stats.bad_packets.fetch_add(1, Relaxed);
            return None;
        }
        let frame_id = h.frame_id as i64;

        // Sender restarted (ids went far backwards, or the stream paused): start over.
        let restarted = self.last_delivered >= 0 && frame_id + RESTART_GAP < self.last_delivered;
        let idle = self.last_packet.is_some_and(|t| now.saturating_duration_since(t) > IDLE_RESET);
        if restarted || idle {
            self.reset();
            stats.stream_restarts.fetch_add(1, Relaxed);
        }
        self.last_packet = Some(now);
        self.track_seq(h.seq as i64, stats, &mut on_loss);
        stats.on_packet(buf.len());

        if frame_id <= self.last_delivered {
            return None; // late packet of a frame we already passed
        }
        let slot = &mut self.slots[(frame_id as usize) % SLOTS];
        // An older incomplete frame in this slot is lost; deliver() counts it as skipped.
        if slot.frame_id != frame_id {
            slot.reset(frame_id, count);
        }
        if idx >= slot.count || slot.have[idx] {
            return None; // duplicate (or count changed mid-frame: garbage)
        }
        slot.have[idx] = true;
        slot.parts[idx].clear();
        slot.parts[idx].extend_from_slice(payload);
        slot.received += 1;
        if slot.received < slot.count {
            return None;
        }

        // Complete: give up on every older frame that is still incomplete.
        if self.last_delivered >= 0 {
            let skipped = frame_id - self.last_delivered - 1;
            if skipped > 0 {
                stats.dropped_frames.fetch_add(skipped as u64, Relaxed);
            }
        }
        let total = slot.parts[..slot.count].iter().map(Vec::len).sum();
        let mut data = Vec::with_capacity(total);
        for p in &slot.parts[..slot.count] {
            data.extend_from_slice(p);
        }
        slot.frame_id = -1;
        self.last_delivered = frame_id;
        stats.frames_in.fetch_add(1, Relaxed);
        Some(Frame { frame_id, data, complete_at: now })
    }

    fn track_seq(&mut self, seq: i64, stats: &Stats, on_loss: &mut impl FnMut()) {
        if self.last_seq >= 0 {
            let gap = seq - self.last_seq - 1;
            if gap > 0 {
                stats.lost_packets.fetch_add(gap as u64, Relaxed);
                on_loss();
            } else if gap < 0 && seq < self.last_seq {
                // Reordered packet filling an earlier gap.
                // Only this thread writes lost_packets, so load-then-sub cannot underflow.
                if stats.lost_packets.load(Relaxed) > 0 {
                    stats.lost_packets.fetch_sub(1, Relaxed);
                }
                return;
            }
        }
        self.last_seq = seq;
    }

    fn reset(&mut self) {
        for s in &mut self.slots {
            s.frame_id = -1;
        }
        self.last_delivered = -1;
        self.last_seq = -1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanocast_core::protocol::Packetizer;

    fn feed(a: &mut Assembler, stats: &Stats, pkts: &[Vec<u8>], now: Instant) -> (Vec<Frame>, u32) {
        let mut out = Vec::new();
        let mut losses = 0;
        for p in pkts {
            if let Some(f) = a.on_packet(p, now, stats, || losses += 1) {
                out.push(f);
            }
        }
        (out, losses)
    }

    #[test]
    fn reassembles_in_order_and_out_of_order() {
        let mut p = Packetizer::new(100);
        let au: Vec<u8> = (0..350u32).map(|i| i as u8).collect();
        let mut pkts = p.packets(&au).unwrap();
        pkts.swap(0, 3);
        let (stats, mut a, now) = (Stats::new(), Assembler::new(), Instant::now());
        let (frames, _) = feed(&mut a, &stats, &pkts, now);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, au);
        assert_eq!(frames[0].frame_id, 0);
        // Packets arriving behind the highest seq seen never count as lost.
        assert_eq!(stats.lost_packets.load(Relaxed), 0);
        // Duplicates of a delivered frame are ignored.
        assert!(feed(&mut a, &stats, &pkts, now).0.is_empty());
    }

    #[test]
    fn drops_incomplete_frame_and_counts_loss() {
        let mut p = Packetizer::new(100);
        let start = p.packets(&[0u8; 50]).unwrap();
        let f0 = p.packets(&[1u8; 250]).unwrap();
        let f1 = p.packets(&[2u8; 250]).unwrap();
        let (stats, mut a, now) = (Stats::new(), Assembler::new(), Instant::now());
        // Lose the middle packet of frame 1.
        let mut pkts = start.clone();
        pkts.extend([f0[0].clone(), f0[2].clone(), f1[0].clone(), f1[1].clone(), f1[2].clone()]);
        let (frames, losses) = feed(&mut a, &stats, &pkts, now);
        assert_eq!(frames.iter().map(|f| f.frame_id).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(frames[1].data, [2u8; 250]);
        assert_eq!(losses, 1);
        assert_eq!(stats.lost_packets.load(Relaxed), 1);
        assert_eq!(stats.dropped_frames.load(Relaxed), 1);
        // The missing packet arriving late is not delivered.
        assert!(feed(&mut a, &stats, &[f0[1].clone()], now).0.is_empty());
    }

    #[test]
    fn sender_restart_and_bad_packets() {
        let (stats, mut a, now) = (Stats::new(), Assembler::new(), Instant::now());
        let mut p = Packetizer::new(100);
        for _ in 0..100 {
            feed(&mut a, &stats, &p.packets(&[0u8; 10]).unwrap(), now);
        }
        // New sender starts again at frame 0 / seq 0.
        let mut q = Packetizer::new(100);
        let (frames, _) = feed(&mut a, &stats, &q.packets(&[9u8; 10]).unwrap(), now);
        assert_eq!(frames.len(), 1);
        assert_eq!(stats.stream_restarts.load(Relaxed), 1);
        // Pause > 1 s also resets.
        let later = now + Duration::from_secs(2);
        let (frames, _) = feed(&mut a, &stats, &q.packets(&[9u8; 10]).unwrap(), later);
        assert_eq!(frames.len(), 1);
        assert_eq!(stats.stream_restarts.load(Relaxed), 2);

        assert!(a.on_packet(&[0u8; 5], now, &stats, || {}).is_none());
        let mut bad = q.packets(&[0u8; 10]).unwrap().remove(0);
        bad[12..14].copy_from_slice(&0u16.to_be_bytes()); // count = 0
        assert!(a.on_packet(&bad, later, &stats, || {}).is_none());
        assert_eq!(stats.bad_packets.load(Relaxed), 2);
    }
}
