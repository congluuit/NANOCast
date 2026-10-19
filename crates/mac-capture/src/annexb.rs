//! VideoToolbox hands out H.264 in AVCC form (each NAL prefixed by its big-endian length,
//! SPS/PPS kept apart in the CMFormatDescription). Receivers want an Annex B byte stream
//! with start codes and the parameter sets in band before every keyframe, like the
//! Windows Media Foundation encoder produces. Platform-neutral so it is unit-tested on
//! every OS.

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// NAL unit types (H.264 table 7-1) that matter here.
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// Converts one AVCC access unit to Annex B, appending to `out`.
///
/// `nal_len_size` is the length-prefix size from the format description (1, 2 or 4;
/// VideoToolbox uses 4). When the AU holds an IDR slice, `param_sets` (SPS, PPS...) are
/// written first; SPS/PPS already inside the AU are dropped so they never appear twice.
/// AUDs are dropped too (MF does not send them either; the receiver splits on frames).
///
/// Returns `Some(keyframe)`, or `None` if the AVCC data is malformed (truncated length).
pub fn avcc_to_annexb(avcc: &[u8], nal_len_size: usize, param_sets: &[&[u8]], out: &mut Vec<u8>) -> Option<bool> {
    if !matches!(nal_len_size, 1 | 2 | 4) {
        return None;
    }
    let nals = split_avcc(avcc, nal_len_size)?;
    let keyframe = nals.iter().any(|n| n.first().is_some_and(|h| h & 0x1F == NAL_IDR));
    out.reserve(avcc.len() + 64);
    if keyframe {
        for ps in param_sets.iter().filter(|p| !p.is_empty()) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(ps);
        }
    }
    for nal in nals {
        let Some(&h) = nal.first() else { continue };
        if matches!(h & 0x1F, NAL_SPS | NAL_PPS | NAL_AUD) {
            continue;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    Some(keyframe)
}

/// Splits length-prefixed NAL units; `None` if a length runs past the end.
fn split_avcc(mut data: &[u8], len_size: usize) -> Option<Vec<&[u8]>> {
    let mut nals = Vec::new();
    while !data.is_empty() {
        if data.len() < len_size {
            return None;
        }
        let len = data[..len_size].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
        data = &data[len_size..];
        if len > data.len() {
            return None;
        }
        nals.push(&data[..len]);
        data = &data[len..];
    }
    Some(nals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avcc(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| (n.len() as u32).to_be_bytes().into_iter().chain(n.iter().copied())).collect()
    }

    #[test]
    fn keyframe_gets_parameter_sets() {
        let sps = [0x67, 0x4D, 0x00, 0x28];
        let pps = [0x68, 0xEE, 0x3C, 0x80];
        let idr = [0x65, 0x88, 0x84, 0x00];
        let mut out = Vec::new();
        let key = avcc_to_annexb(&avcc(&[&idr]), 4, &[&sps, &pps], &mut out).unwrap();
        assert!(key);
        let mut want = vec![0, 0, 0, 1];
        want.extend_from_slice(&sps);
        want.extend_from_slice(&[0, 0, 0, 1]);
        want.extend_from_slice(&pps);
        want.extend_from_slice(&[0, 0, 0, 1]);
        want.extend_from_slice(&idr);
        assert_eq!(out, want);
        // The result parses as one keyframe AU with the core helpers the receiver side uses.
        assert!(nanocast_core::au::is_keyframe(&out));
        assert_eq!(nanocast_core::au::split_access_units(&out).len(), 1);
    }

    #[test]
    fn delta_frame_has_no_parameter_sets_and_drops_aud() {
        let aud = [0x09, 0xF0];
        let sei = [0x06, 0x05, 0x01];
        let p = [0x41, 0x9A, 0x00];
        let mut out = Vec::new();
        let key = avcc_to_annexb(&avcc(&[&aud, &sei, &p]), 4, &[&[0x67, 1], &[0x68, 2]], &mut out).unwrap();
        assert!(!key);
        assert_eq!(out, [0, 0, 0, 1, 0x06, 0x05, 0x01, 0, 0, 0, 1, 0x41, 0x9A, 0x00]);
    }

    #[test]
    fn in_band_sps_pps_not_duplicated() {
        let sps = [0x67, 0x42];
        let pps = [0x68, 0xCE];
        let idr = [0x65, 0x11];
        let mut out = Vec::new();
        avcc_to_annexb(&avcc(&[&sps, &pps, &idr]), 4, &[&sps, &pps], &mut out).unwrap();
        assert_eq!(nanocast_core::au::nal_starts(&out).len(), 3);
    }

    #[test]
    fn short_length_prefixes_and_truncation() {
        let mut out = Vec::new();
        assert_eq!(avcc_to_annexb(&[0, 2, 0x41, 0x01], 2, &[], &mut out), Some(false));
        assert_eq!(out, [0, 0, 0, 1, 0x41, 0x01]);
        assert_eq!(avcc_to_annexb(&[0, 0, 0, 9, 0x41], 4, &[], &mut Vec::new()), None);
        assert_eq!(avcc_to_annexb(&[0, 0], 3, &[], &mut Vec::new()), None);
    }
}
