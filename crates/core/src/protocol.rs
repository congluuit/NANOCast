//! NANOCast wire protocol (PLAN.md section 4). Kept deliberately tiny.
//!
//! Video packet = 14-byte big-endian header + H.264 Annex B payload (<= MAX_PAYLOAD):
//!   magic u16 | seq u32 | frame_id u32 | idx u16 | count u16
//! `seq` counts packets over the whole stream (loss detection), `frame_id` counts
//! access units, `idx`/`count` place the packet inside its frame. A 1080p
//! keyframe can exceed 300 KB, so idx/count are 16-bit (255 x 1200 B was too small).
//!
//! The Kotlin receiver mirrors these constants in `Protocol.kt`.

use std::io;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

pub const MAGIC: u16 = 0x4E43; // "NC"
pub const VIDEO_PORT: u16 = 48322;
/// TCP control channel (JSON lines, see `control`). Receivers may override it with TXT `ctrl`.
pub const CONTROL_PORT: u16 = 48323;
pub const HEADER_LEN: usize = 14;
pub const MAX_PAYLOAD: usize = 1200;
/// Receivers preallocate this many slots per frame: 1024 x 1200 B = 1.2 MB per frame.
pub const MAX_PACKETS_PER_FRAME: usize = 1024;
pub const SERVICE_TYPE: &str = "_nanocast._udp.local.";
pub const PROTOCOL_VERSION: &str = "0.1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoHeader {
    pub seq: u32,
    pub frame_id: u32,
    pub idx: u16,
    pub count: u16,
}

impl VideoHeader {
    pub fn write(&self, buf: &mut [u8]) {
        buf[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        buf[2..6].copy_from_slice(&self.seq.to_be_bytes());
        buf[6..10].copy_from_slice(&self.frame_id.to_be_bytes());
        buf[10..12].copy_from_slice(&self.idx.to_be_bytes());
        buf[12..14].copy_from_slice(&self.count.to_be_bytes());
    }

    /// Parses a header; `None` if the packet is too short or the magic does not match.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN || u16::from_be_bytes([buf[0], buf[1]]) != MAGIC {
            return None;
        }
        Some(Self {
            seq: u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]),
            frame_id: u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]),
            idx: u16::from_be_bytes([buf[10], buf[11]]),
            count: u16::from_be_bytes([buf[12], buf[13]]),
        })
    }
}

/// Splits access units into numbered packets.
pub struct Packetizer {
    seq: u32,
    frame_id: u32,
    payload: usize,
}

impl Packetizer {
    pub fn new(payload: usize) -> Self {
        assert!(payload > 0 && payload <= MAX_PAYLOAD);
        Self { seq: 0, frame_id: 0, payload }
    }

    /// Returns the packets for one access unit, or `None` if it would need more
    /// than MAX_PACKETS_PER_FRAME packets (lower the bitrate / resolution).
    pub fn packets(&mut self, au: &[u8]) -> Option<Vec<Vec<u8>>> {
        let count = au.len().div_ceil(self.payload);
        if count == 0 || count > MAX_PACKETS_PER_FRAME {
            return None;
        }
        let mut out = Vec::with_capacity(count);
        for (idx, chunk) in au.chunks(self.payload).enumerate() {
            let mut pkt = vec![0u8; HEADER_LEN + chunk.len()];
            VideoHeader { seq: self.seq, frame_id: self.frame_id, idx: idx as u16, count: count as u16 }
                .write(&mut pkt);
            pkt[HEADER_LEN..].copy_from_slice(chunk);
            self.seq = self.seq.wrapping_add(1);
            out.push(pkt);
        }
        self.frame_id = self.frame_id.wrapping_add(1);
        Some(out)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SendStats {
    pub packets: usize,
    pub bytes: usize,
    /// How long the frame's packets were spread over.
    pub spread: Duration,
}

/// Sends frames over UDP, spacing packets so the wire rate never exceeds
/// `pace_bps`. A 230 KB keyframe fired in one burst overflows the receiver's
/// socket buffer (seen on Android: 24 packets lost); spreading it over a few
/// milliseconds costs nothing noticeable and avoids that.
pub struct PacedSender {
    sock: UdpSocket,
    packetizer: Packetizer,
    pace_bps: u64,
}

impl PacedSender {
    pub fn new(sock: UdpSocket, pace_bps: u64, payload: usize) -> Self {
        Self { sock, packetizer: Packetizer::new(payload), pace_bps: pace_bps.max(1_000_000) }
    }

    /// Sends one access unit. Returns `Ok(None)` if the frame is too large to packetize.
    pub fn send_frame(&mut self, au: &[u8]) -> io::Result<Option<SendStats>> {
        let Some(packets) = self.packetizer.packets(au) else { return Ok(None) };
        let started = Instant::now();
        let mut next_at = started;
        let mut bytes = 0usize;
        let last = packets.len() - 1;
        for (i, pkt) in packets.iter().enumerate() {
            match self.sock.send(pkt) {
                Ok(_) => {}
                // ICMP port-unreachable from a receiver that is not up yet: keep going.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(e) => return Err(e),
            }
            bytes += pkt.len();
            if i < last {
                next_at += Duration::from_nanos(pkt.len() as u64 * 8 * 1_000_000_000 / self.pace_bps);
                sleep_until(next_at);
            }
        }
        Ok(Some(SendStats { packets: packets.len(), bytes, spread: started.elapsed() }))
    }
}

/// Sleeps until `deadline`: coarse sleep first, then spins the last millisecond
/// (Windows timer resolution is too coarse for sub-millisecond pacing).
pub fn sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let left = deadline - now;
        if left > Duration::from_micros(1500) {
            std::thread::sleep(left - Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = VideoHeader { seq: 0xDEADBEEF, frame_id: 42, idx: 300, count: 900 };
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);
        assert_eq!(VideoHeader::parse(&buf), Some(h));
        assert_eq!(VideoHeader::parse(&buf[..13]), None);
        buf[0] = 0;
        assert_eq!(VideoHeader::parse(&buf), None);
    }

    #[test]
    fn packetizer_numbers_packets() {
        let mut p = Packetizer::new(10);
        let pkts = p.packets(&[7u8; 25]).unwrap();
        assert_eq!(pkts.len(), 3);
        let h = VideoHeader::parse(&pkts[2]).unwrap();
        assert_eq!((h.seq, h.frame_id, h.idx, h.count), (2, 0, 2, 3));
        assert_eq!(pkts[2].len(), HEADER_LEN + 5);
        let next = p.packets(&[1u8; 1]).unwrap();
        let h = VideoHeader::parse(&next[0]).unwrap();
        assert_eq!((h.seq, h.frame_id), (3, 1));
        assert!(p.packets(&[0u8; 10 * (MAX_PACKETS_PER_FRAME + 1)]).is_none());
        assert!(p.packets(&[0u8; 10 * MAX_PACKETS_PER_FRAME]).is_some());
    }
}
