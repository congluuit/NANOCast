//! H.264 Annex B helpers: locate NAL units, split a byte stream into access units.

/// Byte offsets of every NAL unit start code: (start code begin, NAL header index).
pub fn nal_starts(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let begin = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            out.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Splits an Annex B stream into access units (one per encoded frame), keeping start codes.
/// A new AU begins at an AUD, at SPS/PPS/SEI/prefix NALs following a slice, or at a slice
/// with first_mb_in_slice == 0 following a slice (H.264 7.4.1.2.3, simplified).
pub fn split_access_units(data: &[u8]) -> Vec<&[u8]> {
    let starts = nal_starts(data);
    let mut aus = Vec::new();
    let mut au_begin: Option<usize> = None;
    let mut au_has_slice = false;
    for &(begin, hdr) in &starts {
        let Some(&h) = data.get(hdr) else { break };
        let nal_type = h & 0x1F;
        let is_slice = matches!(nal_type, 1 | 5);
        let first_mb_zero = is_slice && data.get(hdr + 1).is_some_and(|b| b & 0x80 != 0);
        let starts_new = match nal_type {
            9 => true,
            6 | 7 | 8 | 14..=18 => au_has_slice,
            1 | 5 => au_has_slice && first_mb_zero,
            _ => false,
        };
        if starts_new {
            if let Some(b) = au_begin {
                aus.push(&data[b..begin]);
            }
            au_begin = Some(begin);
            au_has_slice = false;
        }
        au_begin.get_or_insert(begin);
        au_has_slice |= is_slice;
    }
    if let Some(b) = au_begin {
        aus.push(&data[b..]);
    }
    aus
}

/// True if the access unit contains an IDR slice.
pub fn is_keyframe(au: &[u8]) -> bool {
    nal_starts(au).iter().any(|&(_, h)| au.get(h).is_some_and(|b| b & 0x1F == 5))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_aud_and_first_mb() {
        let stream: Vec<u8> = [
            &[0, 0, 0, 1, 0x09, 0xF0][..],       // AUD
            &[0, 0, 0, 1, 0x67, 0x42, 0x00][..], // SPS
            &[0, 0, 0, 1, 0x68, 0xCE][..],       // PPS
            &[0, 0, 1, 0x65, 0x88, 0x01][..],    // IDR, first_mb = 0
            &[0, 0, 1, 0x65, 0x40, 0x02][..],    // IDR, second slice (first_mb != 0)
            &[0, 0, 0, 1, 0x41, 0x9A, 0x03][..], // P slice, first_mb = 0 -> new AU (no AUD)
            &[0, 0, 0, 1, 0x09, 0xF0][..],       // AUD -> new AU
            &[0, 0, 0, 1, 0x41, 0x9A, 0x04][..],
        ]
        .concat();
        let aus = split_access_units(&stream);
        assert_eq!(aus.len(), 3);
        assert!(is_keyframe(aus[0]));
        assert_eq!(aus[0].len(), 6 + 7 + 6 + 6 + 6);
        assert_eq!(aus[1], &[0, 0, 0, 1, 0x41, 0x9A, 0x03]);
        assert!(!is_keyframe(aus[2]));
        assert_eq!(aus.iter().map(|a| a.len()).sum::<usize>(), stream.len());
    }
}
