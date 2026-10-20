//! Minimal fragmented MP4 (ISO BMFF) muxer for a live stream: one init segment
//! (`ftyp` + `moov` with an `avc1` and optionally an `mp4a` track, `mvex`/`trex`), then
//! `moof` + `mdat` fragments of one track each. Enough for Chrome / the Cast Default Media
//! Receiver to play it progressively over HTTP; no ffmpeg involved.
//!
//! Video: H.264 Annex B in, AVCC (4-byte lengths) out, SPS / PPS moved into `avcC`.
//! Audio: raw AAC frames (no ADTS) with the AudioSpecificConfig in `esds`.

pub const VIDEO_TRACK: u32 = 1;
pub const AUDIO_TRACK: u32 = 2;
pub const VIDEO_TIMESCALE: u32 = 90_000;

#[derive(Clone, Debug)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    /// AudioSpecificConfig (AAC-LC 48 kHz stereo = `11 90`).
    pub asc: Vec<u8>,
    pub bitrate: u32,
}

pub struct Sample<'a> {
    pub data: &'a [u8],
    pub duration: u32,
    pub keyframe: bool,
}

/// AudioSpecificConfig for AAC-LC (object type 2).
pub fn aac_lc_asc(sample_rate: u32, channels: u16) -> Vec<u8> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let idx = RATES.iter().position(|&r| r == sample_rate).unwrap_or(3) as u16;
    let v: u16 = (2 << 11) | (idx << 7) | ((channels & 0xF) << 3);
    v.to_be_bytes().to_vec()
}

struct W(Vec<u8>);

impl W {
    fn new() -> Self {
        Self(Vec::with_capacity(1024))
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    fn zeros(&mut self, n: usize) -> &mut Self {
        self.0.resize(self.0.len() + n, 0);
        self
    }
    /// Writes a box: size, type, then `body` fills it.
    fn boxed(&mut self, kind: &[u8; 4], body: impl FnOnce(&mut W)) -> &mut Self {
        let start = self.0.len();
        self.u32(0).bytes(kind);
        body(self);
        let size = (self.0.len() - start) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_be_bytes());
        self
    }
    fn full(&mut self, kind: &[u8; 4], version: u8, flags: u32, body: impl FnOnce(&mut W)) -> &mut Self {
        self.boxed(kind, |w| {
            w.u32((version as u32) << 24 | (flags & 0xFF_FFFF));
            body(w);
        })
    }
    fn matrix(&mut self) -> &mut Self {
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            self.u32(v);
        }
        self
    }
}

/// `ftyp` + `moov`.
pub fn init_segment(video: &VideoParams, audio: Option<&AudioParams>) -> Vec<u8> {
    let mut w = W::new();
    w.boxed(b"ftyp", |w| {
        w.bytes(b"isom").u32(0x200);
        for b in [b"isom", b"iso6", b"iso2", b"avc1", b"mp41"] {
            w.bytes(b);
        }
    });
    w.boxed(b"moov", |w| {
        w.full(b"mvhd", 0, 0, |w| {
            w.u32(0).u32(0).u32(1000).u32(0); // times, timescale, duration
            w.u32(0x0001_0000).u16(0x0100).zeros(10).matrix().zeros(24);
            w.u32(if audio.is_some() { 3 } else { 2 }); // next_track_ID
        });
        trak(w, VIDEO_TRACK, VIDEO_TIMESCALE, Some((video.width, video.height)), |w| avc1(w, video));
        if let Some(a) = audio {
            trak(w, AUDIO_TRACK, a.sample_rate, None, |w| mp4a(w, a));
        }
        w.boxed(b"mvex", |w| {
            let mut tracks = vec![VIDEO_TRACK];
            if audio.is_some() {
                tracks.push(AUDIO_TRACK);
            }
            for t in tracks {
                w.full(b"trex", 0, 0, |w| {
                    w.u32(t).u32(1).u32(0).u32(0).u32(0);
                });
            }
        });
    });
    w.0
}

fn trak(w: &mut W, id: u32, timescale: u32, size: Option<(u32, u32)>, entry: impl FnOnce(&mut W)) {
    let video = size.is_some();
    w.boxed(b"trak", |w| {
        w.full(b"tkhd", 0, 3, |w| {
            w.u32(0).u32(0).u32(id).u32(0).u32(0); // times, track_ID, reserved, duration
            w.zeros(8).u16(0).u16(0).u16(if video { 0 } else { 0x0100 }).u16(0).matrix();
            let (wd, ht) = size.unwrap_or((0, 0));
            w.u32(wd << 16).u32(ht << 16);
        });
        w.boxed(b"mdia", |w| {
            w.full(b"mdhd", 0, 0, |w| {
                w.u32(0).u32(0).u32(timescale).u32(0).u16(0x55C4).u16(0); // "und"
            });
            w.full(b"hdlr", 0, 0, |w| {
                w.u32(0).bytes(if video { b"vide" } else { b"soun" }).zeros(12);
                w.bytes(if video { b"NANOCast video\0" } else { b"NANOCast audio\0" });
            });
            w.boxed(b"minf", |w| {
                if video {
                    w.full(b"vmhd", 0, 1, |w| {
                        w.zeros(8);
                    });
                } else {
                    w.full(b"smhd", 0, 0, |w| {
                        w.zeros(4);
                    });
                }
                w.boxed(b"dinf", |w| {
                    w.full(b"dref", 0, 0, |w| {
                        w.u32(1).full(b"url ", 0, 1, |_| {});
                    });
                });
                w.boxed(b"stbl", |w| {
                    w.full(b"stsd", 0, 0, |w| {
                        w.u32(1);
                        entry(w);
                    });
                    for kind in [b"stts", b"stsc", b"stco"] {
                        w.full(kind, 0, 0, |w| {
                            w.u32(0);
                        });
                    }
                    w.full(b"stsz", 0, 0, |w| {
                        w.u32(0).u32(0);
                    });
                });
            });
        });
    });
}

fn avc1(w: &mut W, v: &VideoParams) {
    w.boxed(b"avc1", |w| {
        w.zeros(6).u16(1); // reserved, data_reference_index
        w.zeros(16).u16(v.width as u16).u16(v.height as u16);
        w.u32(0x0048_0000).u32(0x0048_0000).u32(0).u16(1);
        let mut name = [0u8; 32];
        name[0] = 8;
        name[1..9].copy_from_slice(b"NANOCast");
        w.bytes(&name).u16(0x0018).u16(0xFFFF);
        w.boxed(b"avcC", |w| {
            let s = &v.sps;
            let (profile, compat, level) = (s.get(1).copied().unwrap_or(77), s.get(2).copied().unwrap_or(0), s.get(3).copied().unwrap_or(40));
            w.u8(1).u8(profile).u8(compat).u8(level).u8(0xFF).u8(0xE1);
            w.u16(s.len() as u16).bytes(s);
            w.u8(1).u16(v.pps.len() as u16).bytes(&v.pps);
            // High profiles carry chroma / bit depth fields (4:2:0, 8 bit).
            if matches!(profile, 100 | 110 | 122 | 144) {
                w.u8(0xFC | 1).u8(0xF8).u8(0xF8).u8(0);
            }
        });
    });
}

fn mp4a(w: &mut W, a: &AudioParams) {
    w.boxed(b"mp4a", |w| {
        w.zeros(6).u16(1).zeros(8);
        w.u16(a.channels).u16(16).u16(0).u16(0).u32(a.sample_rate << 16);
        w.full(b"esds", 0, 0, |w| {
            let dsi_len = a.asc.len();
            let dcd_len = 13 + 2 + dsi_len;
            let es_len = 3 + 2 + dcd_len + 3;
            w.u8(0x03).u8(es_len as u8).u16(AUDIO_TRACK as u16).u8(0);
            w.u8(0x04).u8(dcd_len as u8).u8(0x40).u8(0x15);
            w.u8(0).u16(0); // bufferSizeDB (24 bit)
            w.u32(a.bitrate).u32(a.bitrate);
            w.u8(0x05).u8(dsi_len as u8).bytes(&a.asc);
            w.u8(0x06).u8(1).u8(0x02);
        });
    });
}

const FLAGS_SYNC: u32 = 0x0200_0000;
const FLAGS_NON_SYNC: u32 = 0x0101_0000;

/// One `moof` + `mdat` for `track`. `seq` counts fragments from 1 across both tracks.
pub fn fragment(seq: u32, track: u32, base_decode_time: u64, samples: &[Sample]) -> Vec<u8> {
    let payload: usize = samples.iter().map(|s| s.data.len()).sum();
    let mut w = W::new();
    let mut data_offset_at = 0;
    w.boxed(b"moof", |w| {
        w.full(b"mfhd", 0, 0, |w| {
            w.u32(seq);
        });
        w.boxed(b"traf", |w| {
            w.full(b"tfhd", 0, 0x02_0000, |w| {
                w.u32(track);
            });
            w.full(b"tfdt", 1, 0, |w| {
                w.u64(base_decode_time);
            });
            w.full(b"trun", 0, 0x000701, |w| {
                w.u32(samples.len() as u32);
                data_offset_at = w.0.len();
                w.u32(0);
                for s in samples {
                    w.u32(s.duration).u32(s.data.len() as u32);
                    w.u32(if s.keyframe { FLAGS_SYNC } else { FLAGS_NON_SYNC });
                }
            });
        });
    });
    let data_offset = (w.0.len() + 8) as u32;
    w.0[data_offset_at..data_offset_at + 4].copy_from_slice(&data_offset.to_be_bytes());
    w.u32((payload + 8) as u32).bytes(b"mdat");
    for s in samples {
        w.bytes(s.data);
    }
    w.0
}

/// Splits Annex B into NAL units (without start codes).
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                out.push(&data[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            out.push(&data[s..]);
        }
    }
    out
}

/// Annex B access unit -> AVCC sample. Returns (sample, SPS, PPS) with the parameter
/// sets and access unit delimiters left out of the sample.
pub fn annexb_to_avcc(data: &[u8]) -> (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut out = Vec::with_capacity(data.len() + 16);
    let (mut sps, mut pps) = (None, None);
    for nal in annexb_nals(data) {
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1F {
            7 => sps = Some(nal.to_vec()),
            8 => pps = Some(nal.to_vec()),
            9 => {}
            _ => {
                out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    (out, sps, pps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks a box tree, returning (path, size) of every box.
    fn boxes(data: &[u8], prefix: &str, out: &mut Vec<(String, usize)>) {
        let containers = ["moov", "trak", "mdia", "minf", "dinf", "stbl", "mvex", "moof", "traf"];
        let mut i = 0;
        while i + 8 <= data.len() {
            let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            let kind = std::str::from_utf8(&data[i + 4..i + 8]).unwrap().to_string();
            assert!(size >= 8 && i + size <= data.len(), "bad box {kind} size {size} at {i}");
            let path = format!("{prefix}/{kind}");
            if containers.contains(&kind.as_str()) {
                boxes(&data[i + 8..i + size], &path, out);
            }
            out.push((path, size));
            i += size;
        }
        assert_eq!(i, data.len(), "trailing bytes under {prefix}");
    }

    fn params() -> (VideoParams, AudioParams) {
        let v = VideoParams { width: 1920, height: 1080, sps: vec![0x67, 77, 0, 40, 0xAA], pps: vec![0x68, 0xCE, 0x3C, 0x80] };
        let a = AudioParams { sample_rate: 48000, channels: 2, asc: aac_lc_asc(48000, 2), bitrate: 128_000 };
        (v, a)
    }

    #[test]
    fn asc_for_48k_stereo() {
        assert_eq!(aac_lc_asc(48000, 2), vec![0x11, 0x90]);
        assert_eq!(aac_lc_asc(44100, 2), vec![0x12, 0x10]);
    }

    #[test]
    fn init_segment_structure() {
        let (v, a) = params();
        let init = init_segment(&v, Some(&a));
        let mut out = Vec::new();
        boxes(&init, "", &mut out);
        let paths: Vec<&str> = out.iter().map(|(p, _)| p.as_str()).collect();
        for want in [
            "/ftyp",
            "/moov/mvhd",
            "/moov/trak/tkhd",
            "/moov/trak/mdia/minf/stbl/stsd",
            "/moov/mvex/trex",
        ] {
            assert!(paths.contains(&want), "{want} missing in {paths:?}");
        }
        assert_eq!(paths.iter().filter(|p| **p == "/moov/trak").count(), 2);
        // avcC carries profile / level from the SPS.
        let at = init.windows(4).position(|w| w == b"avcC").unwrap();
        assert_eq!(&init[at + 4..at + 10], &[1, 77, 0, 40, 0xFF, 0xE1]);
        // esds carries the ASC.
        let at = init.windows(4).position(|w| w == b"esds").unwrap();
        assert!(init[at..].windows(4).any(|w| w == [0x05, 2, 0x11, 0x90]));
        // Video-only init has one track and next_track_ID 2.
        let init = init_segment(&v, None);
        let mut out = Vec::new();
        boxes(&init, "", &mut out);
        assert_eq!(out.iter().filter(|(p, _)| p == "/moov/trak").count(), 1);
    }

    #[test]
    fn fragment_offsets_point_at_mdat_payload() {
        let s1 = [1u8, 2, 3, 4, 5];
        let s2 = [9u8; 7];
        let frag = fragment(
            3,
            VIDEO_TRACK,
            90_000 * 5,
            &[Sample { data: &s1, duration: 1500, keyframe: true }, Sample { data: &s2, duration: 1500, keyframe: false }],
        );
        let mut out = Vec::new();
        boxes(&frag, "", &mut out);
        let moof = out.iter().find(|(p, _)| p == "/moof").unwrap().1;
        let trun = frag.windows(4).position(|w| w == b"trun").unwrap();
        let flags = u32::from_be_bytes(frag[trun + 4..trun + 8].try_into().unwrap());
        assert_eq!(flags, 0x000701);
        let count = u32::from_be_bytes(frag[trun + 8..trun + 12].try_into().unwrap());
        assert_eq!(count, 2);
        let off = u32::from_be_bytes(frag[trun + 12..trun + 16].try_into().unwrap()) as usize;
        assert_eq!(off, moof + 8);
        assert_eq!(&frag[off..off + 5], &s1);
        assert_eq!(&frag[off + 5..], &s2);
        let tfdt = frag.windows(4).position(|w| w == b"tfdt").unwrap();
        assert_eq!(frag[tfdt + 4], 1);
        assert_eq!(u64::from_be_bytes(frag[tfdt + 8..tfdt + 16].try_into().unwrap()), 450_000);
    }

    #[test]
    fn annexb_conversion() {
        let au = [
            0, 0, 0, 1, 0x09, 0xF0, // AUD
            0, 0, 0, 1, 0x67, 77, 0, 40, // SPS
            0, 0, 1, 0x68, 0xCE, // PPS
            0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01, // IDR with emulation prevention
        ];
        let (avcc, sps, pps) = annexb_to_avcc(&au);
        assert_eq!(sps.unwrap(), vec![0x67, 77, 0, 40]);
        assert_eq!(pps.unwrap(), vec![0x68, 0xCE]);
        assert_eq!(&avcc[..4], &[0, 0, 0, 7]);
        assert_eq!(&avcc[4..], &[0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01]);
    }
}
