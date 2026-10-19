//! Keeps SPS/PPS in band before every IDR, whatever the encoder does.
//!
//! The receivers join (and recover after loss) at any keyframe, so each IDR access unit
//! must carry the parameter sets, like the Windows Media Foundation encoder produces.
//! VA-API, NVENC, x264 and OpenH264 already repeat them, but `h264_v4l2m2m` (Raspberry Pi)
//! sends them once, sometimes as a packet of their own. `ParamSets` remembers the latest
//! SPS/PPS and prepends them to an IDR that lacks them; header-only packets are absorbed.
//! Platform-neutral so it is unit-tested on every OS.

use nanocast_core::au::nal_starts;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

#[derive(Default, Debug)]
pub struct ParamSets {
    /// Latest SPS / PPS NAL units (header byte included, no start code).
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl ParamSets {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once both parameter sets have been seen.
    pub fn complete(&self) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty()
    }

    /// Learns SPS/PPS from encoder extradata in Annex B form (ignored if it is not).
    pub fn learn(&mut self, annexb: &[u8]) {
        for (_, nal) in nals(annexb) {
            self.remember(nal);
        }
    }

    fn remember(&mut self, nal: &[u8]) {
        match nal.first().map(|h| h & 0x1F) {
            Some(NAL_SPS) => self.sps = nal.to_vec(),
            Some(NAL_PPS) => self.pps = nal.to_vec(),
            _ => {}
        }
    }

    /// Takes one encoded access unit (Annex B) and returns it ready to send, with SPS/PPS
    /// in front of an IDR that lacks them, plus whether it is a keyframe. Returns `None`
    /// for a packet without any picture (parameter sets or SEI only): those are kept for
    /// the next IDR instead of being sent as a frame.
    pub fn fix(&mut self, au: &[u8]) -> Option<(Vec<u8>, bool)> {
        let mut has_sps = false;
        let mut has_pps = false;
        let mut has_slice = false;
        let mut idr = false;
        for (_, nal) in nals(au) {
            let Some(&h) = nal.first() else { continue };
            match h & 0x1F {
                NAL_SPS => has_sps = true,
                NAL_PPS => has_pps = true,
                NAL_IDR => {
                    idr = true;
                    has_slice = true;
                }
                NAL_SLICE..=4 => has_slice = true,
                _ => {}
            }
            self.remember(nal);
        }
        if !has_slice {
            return None;
        }
        let mut out = Vec::with_capacity(au.len() + self.sps.len() + self.pps.len() + 8);
        if idr && !(has_sps && has_pps) && self.complete() {
            for ps in [&self.sps, &self.pps] {
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(ps);
            }
        }
        out.extend_from_slice(au);
        Some((out, idr))
    }
}

/// NAL units of an Annex B buffer as (offset of the start code, NAL bytes).
fn nals(data: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let starts = nal_starts(data);
    let ends: Vec<usize> = starts.iter().skip(1).map(|&(begin, _)| begin).chain(std::iter::once(data.len())).collect();
    starts.into_iter().zip(ends).map(move |((begin, hdr), end)| (begin, trim_trailing_zeros(&data[hdr..end.max(hdr)])))
}

/// Trailing zero bytes before the next start code belong to it (or are padding).
fn trim_trailing_zeros(nal: &[u8]) -> &[u8] {
    let len = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &nal[..len]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xEE, 0x3C];
    const IDR: &[u8] = &[0, 0, 0, 1, 0x65, 0x88, 0x84];
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A, 0x02];

    #[test]
    fn header_only_packet_is_absorbed_and_prepended_later() {
        let mut ps = ParamSets::new();
        assert!(ps.fix(&[SPS, PPS].concat()).is_none());
        assert!(ps.complete());
        let (au, key) = ps.fix(IDR).unwrap();
        assert!(key);
        assert_eq!(au, [SPS, PPS, IDR].concat());
        let (au, key) = ps.fix(P).unwrap();
        assert!(!key);
        assert_eq!(au, P);
    }

    #[test]
    fn idr_with_parameter_sets_is_untouched() {
        let mut ps = ParamSets::new();
        let full = [SPS, PPS, IDR].concat();
        let (au, key) = ps.fix(&full).unwrap();
        assert!(key);
        assert_eq!(au, full);
        // Later IDR without them (v4l2m2m) gets the remembered ones.
        let (au, _) = ps.fix(IDR).unwrap();
        assert_eq!(au, full);
    }

    #[test]
    fn idr_before_any_parameter_set_passes_through() {
        let mut ps = ParamSets::new();
        let (au, key) = ps.fix(IDR).unwrap();
        assert!(key);
        assert_eq!(au, IDR);
    }

    #[test]
    fn learns_from_extradata() {
        let mut ps = ParamSets::new();
        ps.learn(&[SPS, PPS].concat());
        assert_eq!(ps.fix(IDR).unwrap().0, [SPS, PPS, IDR].concat());
    }
}
