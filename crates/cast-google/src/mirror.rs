//! Path B: Cast Streaming (the protocol Chrome's tab / desktop mirroring uses), after
//! Chromium's openscreen `cast/streaming` (offer_messages.cc, rtp_packetizer.cc,
//! frame_crypto.cc, compound_rtcp_parser.cc, sender_report_builder.cc).
//!
//! - Launch the mirroring receiver app (`0F5096E8`), send an OFFER on
//!   `urn:x-cast:com.google.cast.webrtc` describing one H.264 and one Opus stream with
//!   their SSRCs and AES keys; the ANSWER gives the UDP port and which streams it accepts.
//! - Each encoded frame is encrypted whole with AES-128-CTR (nonce = `aesIvMask` XOR the
//!   frame id at bytes 8..12), then cut into Cast RTP packets: 12-byte RTP header (marker on
//!   the last packet) + Cast header (key flag, frame id, packet id, max packet id,
//!   referenced frame id).
//! - RTCP on the same UDP port: we send Sender Reports (NTP <-> RTP mapping, needed by
//!   the receiver to schedule playout); the receiver sends Cast feedback (ACK checkpoint,
//!   NACKs we answer with retransmissions) and picture loss indications (-> keyframe).
//!
//! NANOCast's choices vs. Chrome's built-in mirroring: 1080p **60 fps**, a higher bitrate
//! (12 Mbps default vs ~5), hardware encoder with no B-frames, frames paced on the wire.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes::Aes128;
use anyhow::{bail, Context as _, Result};
use ctr::cipher::{KeyIvInit, StreamCipher};
use log::{debug, info, warn};
use serde_json::{json, Value};

pub const MIRROR_APP_ID: &str = "0F5096E8";
/// Payload types Chrome uses (openscreen `RtpPayloadType::k*HackForAndroidTV`).
pub const VIDEO_PT: u8 = 96;
pub const AUDIO_PT: u8 = 127;
pub const VIDEO_CLOCK: u32 = 90_000;
pub const AUDIO_CLOCK: u32 = 48_000;
/// Whole RTP packet, headers included (fits a 1500-byte Ethernet / Wi-Fi MTU with margin).
const MAX_PACKET: usize = 1400;
const RTP_HEADER: usize = 12;
const CAST_HEADER: usize = 7;
/// Frames kept for retransmission (per stream).
const CACHE_FRAMES: usize = 128;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamKeys {
    pub ssrc: u32,
    pub key: [u8; 16],
    pub iv_mask: [u8; 16],
}

impl StreamKeys {
    pub fn random() -> Result<Self> {
        let mut b = [0u8; 36];
        getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("random: {e}"))?;
        let ssrc = u32::from_be_bytes(b[0..4].try_into().unwrap()) | 1;
        Ok(Self { ssrc, key: b[4..20].try_into().unwrap(), iv_mask: b[20..36].try_into().unwrap() })
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub struct OfferParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub target_delay_ms: u32,
    pub video: StreamKeys,
    pub audio: Option<StreamKeys>,
}

/// The OFFER message (stream index 0 = audio when present, video is the last one).
pub fn offer(seq: u64, p: &OfferParams) -> Value {
    let mut streams = Vec::new();
    if let Some(a) = &p.audio {
        streams.push(json!({
            "index": 0,
            "type": "audio_source",
            "codecName": "opus",
            "rtpProfile": "cast",
            "rtpPayloadType": AUDIO_PT,
            "ssrc": a.ssrc,
            "targetDelay": p.target_delay_ms,
            "aesKey": hex(&a.key),
            "aesIvMask": hex(&a.iv_mask),
            "bitRate": 128_000,
            "sampleRate": AUDIO_CLOCK,
            "timeBase": format!("1/{AUDIO_CLOCK}"),
            "channels": 2,
            "receiverRtcpEventLog": false,
        }));
    }
    streams.push(json!({
        "index": streams.len(),
        "type": "video_source",
        "codecName": "h264",
        "rtpProfile": "cast",
        "rtpPayloadType": VIDEO_PT,
        "ssrc": p.video.ssrc,
        "targetDelay": p.target_delay_ms,
        "aesKey": hex(&p.video.key),
        "aesIvMask": hex(&p.video.iv_mask),
        "maxFrameRate": format!("{}000/1000", p.fps),
        "timeBase": format!("1/{VIDEO_CLOCK}"),
        "maxBitRate": p.bitrate_bps,
        "profile": "main",
        "level": if p.fps > 30 { "42" } else { "41" },
        "resolutions": [{"width": p.width, "height": p.height}],
        "receiverRtcpEventLog": false,
    }));
    json!({
        "type": "OFFER",
        "seqNum": seq,
        "offer": {"castMode": "mirroring", "receiverGetStatus": true, "supportedStreams": streams},
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub udp_port: u16,
    /// Offer stream indexes the receiver accepted.
    pub send_indexes: Vec<u32>,
}

pub fn parse_answer(v: &Value) -> Result<Answer> {
    if v["type"] != "ANSWER" {
        bail!("không phải ANSWER: {v}");
    }
    if v["result"] != "ok" {
        let err = v["error"]["description"].as_str().or(v["error"].as_str()).unwrap_or("không rõ");
        bail!("TV từ chối phát chiếu màn hình: {err}");
    }
    let a = &v["answer"];
    let udp_port = a["udpPort"].as_u64().filter(|&p| p > 0 && p < 65536).context("ANSWER thiếu udpPort")? as u16;
    let send_indexes = a["sendIndexes"].as_array().map(|x| x.iter().filter_map(|i| i.as_u64()).map(|i| i as u32).collect()).unwrap_or_default();
    Ok(Answer { udp_port, send_indexes })
}

/// AES-128-CTR over the whole frame; encrypting twice decrypts.
pub fn crypt_frame(keys: &StreamKeys, frame_id: u32, data: &mut [u8]) {
    let mut nonce = [0u8; 16];
    nonce[8..12].copy_from_slice(&frame_id.to_be_bytes());
    for (n, m) in nonce.iter_mut().zip(keys.iv_mask) {
        *n ^= m;
    }
    let mut c = Aes128Ctr::new(&keys.key.into(), &nonce.into());
    c.apply_keystream(data);
}

pub struct FrameInfo {
    pub frame_id: u32,
    pub referenced_frame_id: u32,
    pub keyframe: bool,
    pub rtp_timestamp: u32,
}

/// Cuts an (encrypted) frame into Cast RTP packets; `seq` is the stream's RTP sequence.
pub fn packetize(pt: u8, ssrc: u32, seq: &mut u16, f: &FrameInfo, payload: &[u8]) -> Vec<Vec<u8>> {
    let max_payload = MAX_PACKET - RTP_HEADER - CAST_HEADER;
    let count = payload.len().div_ceil(max_payload).max(1);
    let per = payload.len().div_ceil(count).max(1);
    let max_packet_id = (count - 1) as u16;
    (0..count)
        .map(|i| {
            let chunk = &payload[(i * per).min(payload.len())..((i + 1) * per).min(payload.len())];
            let mut p = Vec::with_capacity(RTP_HEADER + CAST_HEADER + chunk.len());
            p.push(0x80);
            p.push(if i == count - 1 { 0x80 } else { 0 } | (pt & 0x7F));
            p.extend_from_slice(&seq.to_be_bytes());
            *seq = seq.wrapping_add(1);
            p.extend_from_slice(&f.rtp_timestamp.to_be_bytes());
            p.extend_from_slice(&ssrc.to_be_bytes());
            p.push(if f.keyframe { 0x80 } else { 0 } | 0x40);
            p.push(f.frame_id as u8);
            p.extend_from_slice(&(i as u16).to_be_bytes());
            p.extend_from_slice(&max_packet_id.to_be_bytes());
            p.push(f.referenced_frame_id as u8);
            p.extend_from_slice(chunk);
            p
        })
        .collect()
}

/// NTP timestamp (seconds since 1900 << 32 | fraction) of a wall-clock time.
pub fn ntp(t: SystemTime) -> u64 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() + 2_208_988_800;
    let frac = ((d.subsec_nanos() as u64) << 32) / 1_000_000_000;
    secs << 32 | frac
}

pub fn sender_report(ssrc: u32, ntp_ts: u64, rtp_ts: u32, packets: u32, octets: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(28);
    p.push(0x80);
    p.push(200);
    p.extend_from_slice(&6u16.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&ntp_ts.to_be_bytes());
    p.extend_from_slice(&rtp_ts.to_be_bytes());
    p.extend_from_slice(&packets.to_be_bytes());
    p.extend_from_slice(&octets.to_be_bytes());
    p
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Feedback {
    /// Media SSRC the feedback is about.
    pub ssrc: u32,
    /// Last frame (8-bit) the receiver has completely, in order.
    pub checkpoint: u8,
    pub target_delay_ms: u16,
    /// (frame id 8-bit, packet ids; empty = whole frame).
    pub nacks: Vec<(u8, Vec<u16>)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RtcpInfo {
    pub feedback: Vec<Feedback>,
    /// SSRCs a picture loss indication was sent for.
    pub pli: Vec<u32>,
    /// Receiver reports: (media ssrc, fraction lost /256, jitter).
    pub reports: Vec<(u32, u8, u32)>,
}

pub fn is_rtcp(p: &[u8]) -> bool {
    p.len() >= 8 && p[0] >> 6 == 2 && (200..=207).contains(&p[1])
}

/// Parses a compound RTCP packet from the receiver.
pub fn parse_rtcp(mut p: &[u8]) -> RtcpInfo {
    let mut info = RtcpInfo::default();
    while p.len() >= 4 {
        let count = p[0] & 0x1F;
        let pt = p[1];
        let len = (u16::from_be_bytes([p[2], p[3]]) as usize + 1) * 4;
        if len > p.len() {
            break;
        }
        let body = &p[4..len];
        match pt {
            201 if body.len() >= 4 => {
                for i in 0..count as usize {
                    let b = &body[4 + i * 24..];
                    if b.len() < 24 {
                        break;
                    }
                    let ssrc = u32::from_be_bytes(b[0..4].try_into().unwrap());
                    let jitter = u32::from_be_bytes(b[12..16].try_into().unwrap());
                    info.reports.push((ssrc, b[4], jitter));
                }
            }
            206 if body.len() >= 8 => {
                let media = u32::from_be_bytes(body[4..8].try_into().unwrap());
                if count == 1 {
                    info.pli.push(media);
                } else if count == 15 && body.len() >= 16 && &body[8..12] == b"CAST" {
                    let mut fb = Feedback {
                        ssrc: media,
                        checkpoint: body[12],
                        target_delay_ms: u16::from_be_bytes([body[14], body[15]]),
                        nacks: Vec::new(),
                    };
                    let losses = body[13] as usize;
                    for i in 0..losses {
                        let l = &body[16 + i * 4..];
                        if l.len() < 4 {
                            break;
                        }
                        let (frame, pid, mask) = (l[0], u16::from_be_bytes([l[1], l[2]]), l[3]);
                        let mut ids = Vec::new();
                        if pid != 0xFFFF {
                            ids.push(pid);
                            for bit in 0..8 {
                                if mask & (1 << bit) != 0 {
                                    ids.push(pid.wrapping_add(bit + 1));
                                }
                            }
                        }
                        match fb.nacks.iter_mut().find(|(f, _)| *f == frame) {
                            Some((_, v)) if !ids.is_empty() && !v.is_empty() => v.extend(ids),
                            Some((_, v)) => v.clear(),
                            None => fb.nacks.push((frame, ids)),
                        }
                    }
                    info.feedback.push(fb);
                }
            }
            _ => {}
        }
        p = &p[len..];
    }
    info
}

/// One outgoing stream (audio or video): frame numbering, encryption, packets, cache.
pub struct StreamTx {
    pub keys: StreamKeys,
    pt: u8,
    clock: u32,
    seq: u16,
    next_frame: u32,
    pub packets: u32,
    pub octets: u32,
}

impl StreamTx {
    pub fn new(keys: StreamKeys, pt: u8, clock: u32) -> Self {
        Self { keys, pt, clock, seq: 0, next_frame: 0, packets: 0, octets: 0 }
    }

    /// Encrypts and packetizes one frame; `media_us` is its time on the session clock.
    pub fn frame(&mut self, data: &[u8], keyframe: bool, media_us: u64) -> (u32, Vec<Vec<u8>>) {
        let id = self.next_frame;
        self.next_frame += 1;
        let mut payload = data.to_vec();
        crypt_frame(&self.keys, id, &mut payload);
        let info = FrameInfo {
            frame_id: id,
            referenced_frame_id: if keyframe || id == 0 { id } else { id - 1 },
            keyframe: keyframe || id == 0,
            rtp_timestamp: self.rtp_at(media_us),
        };
        let pkts = packetize(self.pt, self.keys.ssrc, &mut self.seq, &info, &payload);
        self.packets = self.packets.wrapping_add(pkts.len() as u32);
        self.octets = self.octets.wrapping_add(payload.len() as u32);
        (id, pkts)
    }

    pub fn rtp_at(&self, media_us: u64) -> u32 {
        (media_us as u128 * self.clock as u128 / 1_000_000) as u32
    }
}

/// Counters for stats and the fallback decision.
#[derive(Default)]
pub struct MirrorCounters {
    pub rtcp_packets: AtomicU64,
    pub nacked_packets: AtomicU64,
    pub keyframe_requests: AtomicU64,
    pub bytes: AtomicU64,
    /// Receiver's target delay (ms) from its feedback.
    pub delay_ms: AtomicU32,
}

type Cache = HashMap<(u32, u8), Vec<Vec<u8>>>;

/// The UDP side: a paced send thread and an RTCP thread (feedback, retransmissions).
pub struct MirrorSender {
    sock: UdpSocket,
    queue: std::sync::mpsc::SyncSender<Vec<Vec<u8>>>,
    cache: Arc<Mutex<Cache>>,
    order: std::collections::VecDeque<(u32, u8)>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub counters: Arc<MirrorCounters>,
    pub video: StreamTx,
    pub audio: Option<StreamTx>,
    t0: Instant,
    t0_wall: SystemTime,
    last_sr: Instant,
    pub want_keyframe: Arc<AtomicBool>,
}

impl MirrorSender {
    pub fn start(peer: SocketAddr, video: StreamTx, audio: Option<StreamTx>, t0: Instant, pace_bps: u64) -> Result<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0").context("UDP socket")?;
        sock.connect(peer)?;
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(MirrorCounters::default());
        let cache: Arc<Mutex<Cache>> = Arc::default();
        let want_keyframe = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<Vec<u8>>>(32);
        let mut threads = Vec::new();
        {
            let (sock, counters) = (sock.try_clone()?, counters.clone());
            threads.push(std::thread::Builder::new().name("cast-rtp".into()).spawn(move || {
                // Token bucket: bursts of at most ~16 KB, then wait for the schedule.
                let rate = pace_bps as f64 / 8.0;
                let mut credit = 16_000.0f64;
                let mut last = Instant::now();
                while let Ok(pkts) = rx.recv() {
                    for p in pkts {
                        let now = Instant::now();
                        credit = (credit + now.duration_since(last).as_secs_f64() * rate).min(16_000.0);
                        last = now;
                        if credit < 0.0 {
                            std::thread::sleep(Duration::from_secs_f64(-credit / rate));
                        }
                        credit -= p.len() as f64;
                        if sock.send(&p).is_ok() {
                            counters.bytes.fetch_add(p.len() as u64, Ordering::Relaxed);
                        }
                    }
                }
            })?);
        }
        {
            let (sock, counters, cache, stop, want) = (sock.try_clone()?, counters.clone(), cache.clone(), stop.clone(), want_keyframe.clone());
            let (vssrc, assrc) = (video.keys.ssrc, audio.as_ref().map(|a| a.keys.ssrc));
            sock.set_read_timeout(Some(Duration::from_millis(100)))?;
            threads.push(std::thread::Builder::new().name("cast-rtcp".into()).spawn(move || {
                let mut buf = [0u8; 2048];
                let mut last_checkpoint: Option<(u8, Instant)> = None;
                while !stop.load(Ordering::Relaxed) {
                    let n = match sock.recv(&mut buf) {
                        Ok(n) => n,
                        Err(_) => continue,
                    };
                    if !is_rtcp(&buf[..n]) {
                        continue;
                    }
                    counters.rtcp_packets.fetch_add(1, Ordering::Relaxed);
                    let info = parse_rtcp(&buf[..n]);
                    if info.pli.contains(&vssrc) {
                        want.store(true, Ordering::Relaxed);
                        counters.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    for fb in &info.feedback {
                        if fb.ssrc == vssrc {
                            counters.delay_ms.store(fb.target_delay_ms as u32, Ordering::Relaxed);
                            // Checkpoint stuck for a while: the receiver lost a frame for
                            // good; a keyframe lets it start over.
                            match last_checkpoint {
                                Some((c, since)) if c == fb.checkpoint && !fb.nacks.is_empty() && since.elapsed() > Duration::from_millis(600) => {
                                    want.store(true, Ordering::Relaxed);
                                    counters.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                                    last_checkpoint = Some((c, Instant::now()));
                                }
                                Some((c, _)) if c == fb.checkpoint => {}
                                _ => last_checkpoint = Some((fb.checkpoint, Instant::now())),
                            }
                        }
                        if fb.ssrc != vssrc && Some(fb.ssrc) != assrc {
                            continue;
                        }
                        let cache = cache.lock().unwrap();
                        for (frame, ids) in &fb.nacks {
                            let Some(pkts) = cache.get(&(fb.ssrc, *frame)) else { continue };
                            let resend: Vec<&Vec<u8>> = if ids.is_empty() {
                                pkts.iter().collect()
                            } else {
                                ids.iter().filter_map(|&i| pkts.get(i as usize)).collect()
                            };
                            for p in resend {
                                let _ = sock.send(p);
                                counters.nacked_packets.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            })?);
        }
        let mut s = Self {
            sock,
            queue: tx,
            cache,
            order: Default::default(),
            stop,
            threads,
            counters,
            video,
            audio,
            t0,
            t0_wall: SystemTime::now() - t0.elapsed(),
            last_sr: Instant::now() - Duration::from_secs(10),
            want_keyframe,
        };
        s.sender_reports();
        Ok(s)
    }

    fn remember(&mut self, ssrc: u32, frame_id: u32, pkts: &[Vec<u8>]) {
        let key = (ssrc, frame_id as u8);
        let mut cache = self.cache.lock().unwrap();
        cache.insert(key, pkts.to_vec());
        self.order.push_back(key);
        while self.order.len() > CACHE_FRAMES * 2 {
            if let Some(old) = self.order.pop_front() {
                if !self.order.contains(&old) {
                    cache.remove(&old);
                }
            }
        }
    }

    pub fn send_video(&mut self, data: &[u8], keyframe: bool, media_us: u64) -> bool {
        let (id, pkts) = self.video.frame(data, keyframe, media_us);
        let ssrc = self.video.keys.ssrc;
        self.remember(ssrc, id, &pkts);
        self.sender_reports();
        self.queue.try_send(pkts).is_ok()
    }

    pub fn send_audio(&mut self, opus: &[u8], media_us: u64) {
        let Some(a) = self.audio.as_mut() else { return };
        let (id, pkts) = a.frame(opus, true, media_us);
        let ssrc = a.keys.ssrc;
        self.remember(ssrc, id, &pkts);
        let _ = self.queue.try_send(pkts);
    }

    /// Sender Reports every 500 ms (NTP <-> RTP of each stream at the same instant).
    pub fn sender_reports(&mut self) {
        if self.last_sr.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_sr = Instant::now();
        let media_us = self.t0.elapsed().as_micros() as u64;
        let ntp_ts = ntp(self.t0_wall + Duration::from_micros(media_us));
        let mut srs = vec![sender_report(self.video.keys.ssrc, ntp_ts, self.video.rtp_at(media_us), self.video.packets, self.video.octets)];
        if let Some(a) = &self.audio {
            srs.push(sender_report(a.keys.ssrc, ntp_ts, a.rtp_at(media_us), a.packets, a.octets));
        }
        for sr in srs {
            if let Err(e) = self.sock.send(&sr) {
                debug!("sender report: {e}");
            }
        }
    }
}

impl Drop for MirrorSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let (tx, _) = std::sync::mpsc::sync_channel(1);
        drop(std::mem::replace(&mut self.queue, tx));
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        info!(
            "mirroring sender stopped: {} RTCP packets, {} retransmitted, {} keyframe requests",
            self.counters.rtcp_packets.load(Ordering::Relaxed),
            self.counters.nacked_packets.load(Ordering::Relaxed),
            self.counters.keyframe_requests.load(Ordering::Relaxed)
        );
    }
}

/// Waits for the ANSWER to OFFER `seq` among the channel's unsolicited messages.
pub fn wait_answer(events: &crate::channel::Events, seq: u64, timeout: Duration) -> Result<Answer> {
    let deadline = Instant::now() + timeout;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(msg) = events.recv_timeout(left) else { break };
        let Some(v) = msg.payload_json() else { continue };
        if msg.namespace == crate::channel::NS_WEBRTC && v["type"] == "ANSWER" {
            if v["seqNum"].as_u64().is_some_and(|s| s != seq) {
                warn!("ANSWER for another offer ({})", v["seqNum"]);
                continue;
            }
            return parse_answer(&v);
        }
        if msg.namespace == crate::channel::NS_CONNECTION && v["type"] == "CLOSE" {
            bail!("TV đóng ứng dụng chiếu màn hình");
        }
    }
    bail!("TV không trả lời đề nghị chiếu màn hình (OFFER) — có thể TV không hỗ trợ Cast Streaming")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> StreamKeys {
        StreamKeys { ssrc: 0x1234_5679, key: [7; 16], iv_mask: [0xA5; 16] }
    }

    #[test]
    fn crypto_roundtrip_and_nonce_depends_on_frame() {
        let k = keys();
        let plain: Vec<u8> = (0..100u8).collect();
        let mut a = plain.clone();
        crypt_frame(&k, 5, &mut a);
        assert_ne!(a, plain);
        let mut b = plain.clone();
        crypt_frame(&k, 6, &mut b);
        assert_ne!(a, b);
        crypt_frame(&k, 5, &mut a);
        assert_eq!(a, plain);
    }

    #[test]
    fn crypto_matches_reference_ctr() {
        // AES-128-CTR, key = 0, frame 0, iv mask 0: the first keystream block is AES(0, 0).
        let k = StreamKeys { ssrc: 1, key: [0; 16], iv_mask: [0; 16] };
        let mut data = [0u8; 16];
        crypt_frame(&k, 0, &mut data);
        // AES-128(key 0, block 0) = 66e94bd4ef8a2c3b884cfa59ca342b2e
        assert_eq!(hex(&data), "66e94bd4ef8a2c3b884cfa59ca342b2e");
    }

    #[test]
    fn packets_have_cast_headers() {
        let mut seq = 65534u16;
        let f = FrameInfo { frame_id: 300, referenced_frame_id: 299, keyframe: false, rtp_timestamp: 0xAABBCCDD };
        let payload = vec![0x42u8; 3000];
        let pkts = packetize(VIDEO_PT, 0x01020304, &mut seq, &f, &payload);
        assert_eq!(pkts.len(), 3);
        assert_eq!(seq, 1);
        let total: usize = pkts.iter().map(|p| p.len() - RTP_HEADER - CAST_HEADER).sum();
        assert_eq!(total, 3000);
        for (i, p) in pkts.iter().enumerate() {
            assert!(p.len() <= MAX_PACKET);
            assert_eq!(p[0], 0x80);
            assert_eq!(p[1] & 0x80 != 0, i == 2, "marker only on the last packet");
            assert_eq!(p[1] & 0x7F, VIDEO_PT);
            assert_eq!(&p[4..8], &[0xAA, 0xBB, 0xCC, 0xDD]);
            assert_eq!(&p[8..12], &[1, 2, 3, 4]);
            assert_eq!(p[12], 0x40);
            assert_eq!(p[13], 300u32 as u8);
            assert_eq!(u16::from_be_bytes([p[14], p[15]]), i as u16);
            assert_eq!(u16::from_be_bytes([p[16], p[17]]), 2);
            assert_eq!(p[18], 299u32 as u8);
        }
        let key = FrameInfo { frame_id: 0, referenced_frame_id: 0, keyframe: true, rtp_timestamp: 0 };
        let pkts = packetize(AUDIO_PT, 1, &mut seq, &key, &[1, 2, 3]);
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0][12], 0xC0);
        assert_eq!(pkts[0].len(), RTP_HEADER + CAST_HEADER + 3);
    }

    #[test]
    fn offer_and_answer() {
        let p = OfferParams { width: 1920, height: 1080, fps: 60, bitrate_bps: 12_000_000, target_delay_ms: 300, video: keys(), audio: Some(keys()) };
        let o = offer(7, &p);
        let s = o["offer"]["supportedStreams"].as_array().unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0]["codecName"], "opus");
        assert_eq!(s[1]["codecName"], "h264");
        assert_eq!(s[1]["index"], 1);
        assert_eq!(s[1]["maxFrameRate"], "60000/1000");
        assert_eq!(s[1]["aesKey"].as_str().unwrap().len(), 32);
        let a = parse_answer(&json!({"type":"ANSWER","seqNum":7,"result":"ok","answer":{"udpPort":51000,"sendIndexes":[0,1],"ssrcs":[1,2]}})).unwrap();
        assert_eq!(a, Answer { udp_port: 51000, send_indexes: vec![0, 1] });
        assert!(parse_answer(&json!({"type":"ANSWER","result":"error","error":{"code":1,"description":"no"}})).is_err());
    }

    #[test]
    fn sender_report_layout() {
        let sr = sender_report(9, 0x0102030405060708, 77, 3, 4000);
        assert_eq!(sr.len(), 28);
        assert_eq!(&sr[..4], &[0x80, 200, 0, 6]);
        assert_eq!(u32::from_be_bytes(sr[16..20].try_into().unwrap()), 77);
        let t = UNIX_EPOCH + Duration::from_millis(1500);
        assert_eq!(ntp(t) >> 32, 2_208_988_801);
        assert_eq!(ntp(t) & 0xFFFF_FFFF, 1 << 31);
    }

    #[test]
    fn sender_loopback_reassembles_and_retransmits() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let k = keys();
        let t0 = Instant::now();
        let mut s = MirrorSender::start(rx.local_addr().unwrap(), StreamTx::new(k.clone(), VIDEO_PT, VIDEO_CLOCK), None, t0, 100_000_000).unwrap();
        let frame: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        assert!(s.send_video(&frame, true, 1_000_000));
        // Sender report first, then the frame's packets.
        let mut buf = [0u8; 2048];
        let mut got: Vec<Vec<u8>> = Vec::new();
        let mut sr = 0;
        let mut from = None;
        while got.len() < 4 {
            let (n, addr) = rx.recv_from(&mut buf).unwrap();
            from = Some(addr);
            if is_rtcp(&buf[..n]) {
                sr += 1;
                continue;
            }
            got.push(buf[..n].to_vec());
        }
        assert!(sr >= 1);
        assert_eq!(u32::from_be_bytes(got[0][4..8].try_into().unwrap()), 90_000);
        let mut payload: Vec<u8> = got.iter().flat_map(|p| p[RTP_HEADER + CAST_HEADER..].to_vec()).collect();
        crypt_frame(&k, 0, &mut payload);
        assert_eq!(payload, frame);
        // NACK packet 2 of frame 0 -> it comes again.
        let mut fb = vec![0x8F, 206, 0, 4];
        fb.extend_from_slice(&1u32.to_be_bytes());
        fb.extend_from_slice(&k.ssrc.to_be_bytes());
        fb.extend_from_slice(b"CAST");
        fb.extend_from_slice(&[0xFF, 1, 0x01, 0x2C]);
        fb.extend_from_slice(&[0, 0, 2, 0]);
        let fb_len = (fb.len() / 4 - 1) as u16;
        fb[2..4].copy_from_slice(&fb_len.to_be_bytes());
        rx.send_to(&fb, from.unwrap()).unwrap();
        loop {
            let n = rx.recv(&mut buf).unwrap();
            if !is_rtcp(&buf[..n]) {
                assert_eq!(&buf[..n], &got[2][..]);
                break;
            }
        }
        assert_eq!(s.counters.rtcp_packets.load(Ordering::Relaxed), 1);
        assert_eq!(s.counters.nacked_packets.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn parses_cast_feedback_and_pli() {
        let mut p = Vec::new();
        // RR, 1 report block
        p.extend_from_slice(&[0x81, 201, 0, 7]);
        p.extend_from_slice(&5u32.to_be_bytes());
        p.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        p.extend_from_slice(&[10, 0, 0, 1]);
        p.extend_from_slice(&[0; 4]);
        p.extend_from_slice(&33u32.to_be_bytes());
        p.extend_from_slice(&[0; 8]);
        // Cast feedback: 2 loss fields
        p.extend_from_slice(&[0x8F, 206, 0, 6]);
        p.extend_from_slice(&5u32.to_be_bytes());
        p.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        p.extend_from_slice(b"CAST");
        p.extend_from_slice(&[41, 2, 0x01, 0x2C]);
        p.extend_from_slice(&[42, 0, 3, 0b0000_0101]);
        p.extend_from_slice(&[43, 0xFF, 0xFF, 0]);
        // PLI
        p.extend_from_slice(&[0x81, 206, 0, 2]);
        p.extend_from_slice(&5u32.to_be_bytes());
        p.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        assert!(is_rtcp(&p));
        let info = parse_rtcp(&p);
        assert_eq!(info.reports, vec![(0x0102_0304, 10, 33)]);
        assert_eq!(info.pli, vec![0x0102_0304]);
        let fb = &info.feedback[0];
        assert_eq!(fb.checkpoint, 41);
        assert_eq!(fb.target_delay_ms, 300);
        assert_eq!(fb.nacks, vec![(42, vec![3, 4, 6]), (43, vec![])]);
    }
}
