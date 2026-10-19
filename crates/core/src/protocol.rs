//! NANOCast wire protocol (PLAN.md section 4). Kept deliberately tiny.
//!
//! Video packet = 14-byte big-endian header + H.264 Annex B payload (<= MAX_PAYLOAD):
//!   magic u16 | seq u32 | frame_id u32 | idx u16 | count u16
//! `seq` counts packets over the whole stream (loss detection), `frame_id` counts
//! access units, `idx`/`count` place the packet inside its frame. A 1080p
//! keyframe can exceed 300 KB, so idx/count are 16-bit (255 x 1200 B was too small).
//!
//! Audio packet (separate UDP port AUDIO_PORT) = 10-byte big-endian header + one Opus frame:
//!   magic u16 = AUDIO_MAGIC | seq u32 | pts_ms u32
//! One packet = one 20 ms Opus frame, 48 kHz stereo. `seq` counts packets (loss detection),
//! `pts_ms` is the stream time of the frame's first sample (wraps after ~49 days).
//! An empty payload (header only) means 20 ms of silence: play zeros, do not decode.
//!
//! The Kotlin receiver mirrors these constants in `Protocol.kt`.

use std::io;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

pub const MAGIC: u16 = 0x4E43; // "NC"
pub const VIDEO_PORT: u16 = 48322;
/// TCP control channel (JSON lines, see `control`). Receivers may override it with TXT `ctrl`.
pub const CONTROL_PORT: u16 = 48323;
/// UDP audio (Opus) stream, only while audio is on.
pub const AUDIO_PORT: u16 = 48324;
pub const AUDIO_MAGIC: u16 = 0x4E41; // "NA"
pub const AUDIO_HEADER_LEN: usize = 10;
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;
pub const AUDIO_FRAME_MS: u32 = 20;
/// Samples per channel in one packet (20 ms at 48 kHz).
pub const AUDIO_FRAME_SAMPLES: usize = 960;
pub const AUDIO_BITRATE: i32 = 96_000;
/// Largest Opus payload we produce (a 20 ms frame at 96 kbps averages 240 B).
pub const AUDIO_MAX_PAYLOAD: usize = 1200;
pub const HEADER_LEN: usize = 14;
pub const MAX_PAYLOAD: usize = 1200;
/// Receivers preallocate this many slots per frame: 1024 x 1200 B = 1.2 MB per frame.
pub const MAX_PACKETS_PER_FRAME: usize = 1024;
pub const SERVICE_TYPE: &str = "_nanocast._udp.local.";
pub const PROTOCOL_VERSION: &str = "0.1";

/// Video codec of the stream (`start.codec`, `caps.codecs`, TXT `codec`). The packet format
/// is the same for both: one Annex B access unit per frame. H.264 is the default; HEVC
/// only when the receiver lists it in `caps.codecs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    #[default]
    H264,
    Hevc,
}

impl VideoCodec {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
        }
    }

    /// "h264" / "avc" / "hevc" / "h265" (any case); `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" => Some(Self::H264),
            "hevc" | "h265" => Some(Self::Hevc),
            _ => None,
        }
    }

    /// What to send when `wanted` is asked for and the receiver decodes `codecs`
    /// (from `caps`; `None` = unknown, e.g. no control channel): HEVC only if listed.
    pub fn negotiate(wanted: Self, codecs: Option<&[String]>) -> Self {
        let listed = |c: Self| codecs.is_some_and(|list| list.iter().any(|s| Self::parse(s) == Some(c)));
        if wanted == Self::Hevc && listed(Self::Hevc) {
            Self::Hevc
        } else {
            Self::H264
        }
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioHeader {
    pub seq: u32,
    pub pts_ms: u32,
}

impl AudioHeader {
    pub fn write(&self, buf: &mut [u8]) {
        buf[0..2].copy_from_slice(&AUDIO_MAGIC.to_be_bytes());
        buf[2..6].copy_from_slice(&self.seq.to_be_bytes());
        buf[6..10].copy_from_slice(&self.pts_ms.to_be_bytes());
    }

    /// Parses a header; `None` if the packet is too short or the magic does not match.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < AUDIO_HEADER_LEN || u16::from_be_bytes([buf[0], buf[1]]) != AUDIO_MAGIC {
            return None;
        }
        Some(Self {
            seq: u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]),
            pts_ms: u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]),
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
    fn codec_negotiation() {
        let both = vec!["h264".to_string(), "hevc".to_string()];
        let avc = vec!["h264".to_string()];
        assert_eq!(VideoCodec::negotiate(VideoCodec::Hevc, Some(&both)), VideoCodec::Hevc);
        assert_eq!(VideoCodec::negotiate(VideoCodec::Hevc, Some(&avc)), VideoCodec::H264);
        assert_eq!(VideoCodec::negotiate(VideoCodec::Hevc, None), VideoCodec::H264);
        assert_eq!(VideoCodec::negotiate(VideoCodec::H264, Some(&both)), VideoCodec::H264);
        assert_eq!(VideoCodec::parse(" HEVC "), Some(VideoCodec::Hevc));
        assert_eq!(VideoCodec::parse("vp9"), None);
        assert_eq!(serde_json::to_string(&VideoCodec::Hevc).unwrap(), "\"hevc\"");
    }

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
    fn audio_header_roundtrip() {
        let h = AudioHeader { seq: 7, pts_ms: 0xFFFF_FFF0 };
        let mut buf = [0u8; AUDIO_HEADER_LEN];
        h.write(&mut buf);
        assert_eq!(&buf[..2], b"NA");
        assert_eq!(AudioHeader::parse(&buf), Some(h));
        assert_eq!(AudioHeader::parse(&buf[..9]), None);
        // A video packet is not an audio packet.
        let mut v = [0u8; HEADER_LEN];
        VideoHeader { seq: 1, frame_id: 1, idx: 0, count: 1 }.write(&mut v);
        assert_eq!(AudioHeader::parse(&v), None);
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
