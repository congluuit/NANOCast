//! Picture size helpers (same rules as `nanocast_win::convert`). ScreenCaptureKit scales
//! and converts to NV12 on the GPU, so unlike Windows there is no CPU pixel conversion.

/// Where a `src_w`x`src_h` picture lands inside a `dst_w`x`dst_h` canvas when scaled to fit
/// with its aspect ratio kept (letterbox / pillarbox). Returns `(x, y, w, h)`, all even.
pub fn fit_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0, dst_w, dst_h);
    }
    // Compare aspect ratios without floats: src_w/src_h vs dst_w/dst_h.
    let (w, h) = if src_w as u64 * dst_h as u64 >= dst_w as u64 * src_h as u64 {
        (dst_w, (dst_w as u64 * src_h as u64 / src_w as u64) as u32)
    } else {
        ((dst_h as u64 * src_w as u64 / src_h as u64) as u32, dst_h)
    };
    let (w, h) = ((w & !1).max(2).min(dst_w), (h & !1).max(2).min(dst_h));
    (((dst_w - w) / 2) & !1, ((dst_h - h) / 2) & !1, w, h)
}

/// Largest even size that fits in `max_w`x`max_h` with the aspect ratio of `w`x`h`
/// (never upscales). Used to pick the encoder size for a display.
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return ((w & !1).max(2), (h & !1).max(2));
    }
    let (_, _, fw, fh) = fit_rect(w, h, max_w, max_h);
    (fw, fh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retina_displays_fit_1080p() {
        // 13" MacBook Air (2560x1600) and Retina 5K into a 1080p TV.
        assert_eq!(fit_size(2560, 1600, 1920, 1080), (1728, 1080));
        assert_eq!(fit_size(5120, 2880, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(1920, 1080, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(1366, 768, 1920, 1080), (1366, 768));
    }
}
