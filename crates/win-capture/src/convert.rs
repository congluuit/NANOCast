//! BGRA8 -> NV12 (BT.709, limited range) on the CPU. Rows are processed in
//! pairs in parallel. This is the temporary path; a later step moves this to a GPU
//! shader so the frame never leaves VRAM.

use rayon::prelude::*;

const BLACK_Y: u8 = 16;
const BLACK_UV: u8 = 128;

/// Converts `bgra` (tightly packed, `src_w*4` bytes per row) into NV12 in `out`.
/// `w`/`h` must be even and `out` must hold `w*h*3/2` bytes.
pub fn bgra_to_nv12(bgra: &[u8], src_w: u32, w: u32, h: u32, out: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    let src_stride = src_w as usize * 4;
    let (y_plane, uv_plane) = out.split_at_mut(w * h);

    y_plane
        .par_chunks_mut(w * 2)
        .zip(uv_plane.par_chunks_mut(w))
        .enumerate()
        .for_each(|(pair, (y_rows, uv_row))| {
            let r0 = pair * 2;
            let src0 = &bgra[r0 * src_stride..r0 * src_stride + w * 4];
            let src1 = &bgra[(r0 + 1) * src_stride..(r0 + 1) * src_stride + w * 4];
            let (y0, y1) = y_rows.split_at_mut(w);
            convert_row_pair(src0, src1, (0..w).step_by(2).map(|x| (x, x + 1)), y0, y1, uv_row);
        });
}

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
/// (never upscales). Used to pick the encoder size for a monitor.
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return ((w & !1).max(2), (h & !1).max(2));
    }
    let (_, _, fw, fh) = fit_rect(w, h, max_w, max_h);
    (fw, fh)
}

/// Converts a `src_w`x`src_h` BGRA picture (tightly packed) into an NV12 canvas of
/// `dst_w`x`dst_h`, scaled to fit with black bars. When the sizes already match (after
/// dropping an odd last row/column) this is the plain conversion.
/// Scaling is nearest-neighbour: cheap, fine for 2:1 and close ratios; the GPU path will replace it.
pub fn bgra_to_nv12_fit(bgra: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32, out: &mut [u8]) {
    if src_w & !1 == dst_w && src_h & !1 == dst_h {
        bgra_to_nv12(bgra, src_w, dst_w, dst_h, out);
        return;
    }
    let (rx, ry, rw, rh) = fit_rect(src_w, src_h, dst_w, dst_h);
    let (dw, dh) = (dst_w as usize, dst_h as usize);
    let (y_plane, uv_plane) = out.split_at_mut(dw * dh);
    y_plane.fill(BLACK_Y);
    uv_plane.fill(BLACK_UV);

    // Sample at pixel centres.
    let xmap: Vec<usize> =
        (0..rw).map(|x| (((2 * x + 1) as u64 * src_w as u64) / (2 * rw as u64)).min(src_w as u64 - 1) as usize).collect();
    let ymap: Vec<usize> =
        (0..rh).map(|y| (((2 * y + 1) as u64 * src_h as u64) / (2 * rh as u64)).min(src_h as u64 - 1) as usize).collect();
    let src_stride = src_w as usize * 4;
    let (rx, ry, rw, rh) = (rx as usize, ry as usize, rw as usize, rh as usize);

    y_plane[ry * dw..(ry + rh) * dw]
        .par_chunks_mut(dw * 2)
        .zip(uv_plane[ry / 2 * dw..(ry + rh) / 2 * dw].par_chunks_mut(dw))
        .enumerate()
        .for_each(|(pair, (y_rows, uv_row))| {
            let s0 = ymap[pair * 2] * src_stride;
            let s1 = ymap[pair * 2 + 1] * src_stride;
            let src0 = &bgra[s0..s0 + src_stride];
            let src1 = &bgra[s1..s1 + src_stride];
            let (y0, y1) = y_rows.split_at_mut(dw);
            let cols = (0..rw).step_by(2).map(|x| (xmap[x], xmap[x + 1]));
            convert_row_pair(src0, src1, cols, &mut y0[rx..rx + rw], &mut y1[rx..rx + rw], &mut uv_row[rx..rx + rw]);
        });
}

/// Converts two source rows into two Y rows and one interleaved UV row. `cols` yields,
/// for each output pixel pair, the source column of the left and right pixel.
#[inline]
fn convert_row_pair(
    src0: &[u8],
    src1: &[u8],
    cols: impl Iterator<Item = (usize, usize)>,
    y0: &mut [u8],
    y1: &mut [u8],
    uv: &mut [u8],
) {
    for (n, (sx0, sx1)) in cols.enumerate() {
        let x = n * 2;
        let mut sum_u = 0i32;
        let mut sum_v = 0i32;
        for (src, ydst) in [(src0, &mut *y0), (src1, &mut *y1)] {
            for (dx, sx) in [sx0, sx1].into_iter().enumerate() {
                let i = sx * 4;
                let b = src[i] as i32;
                let g = src[i + 1] as i32;
                let r = src[i + 2] as i32;
                let y = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
                ydst[x + dx] = y.clamp(16, 235) as u8;
                sum_u += (-26 * r - 87 * g + 112 * b + 128) >> 8;
                sum_v += (112 * r - 102 * g - 10 * b + 128) >> 8;
            }
        }
        uv[x] = ((sum_u >> 2) + 128).clamp(16, 240) as u8;
        uv[x + 1] = ((sum_v >> 2) + 128).clamp(16, 240) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.iter().copied().cycle().take((w * h * 4) as usize).collect()
    }

    #[test]
    fn fit_rect_letterbox_and_pillarbox() {
        assert_eq!(fit_rect(1920, 1080, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(fit_rect(3840, 2160, 1920, 1080), (0, 0, 1920, 1080));
        // 4:3 window on a 16:9 canvas: bars left and right.
        assert_eq!(fit_rect(800, 600, 1920, 1080), (240, 0, 1440, 1080));
        // Very wide window: bars top and bottom.
        assert_eq!(fit_rect(1920, 540, 1920, 1080), (0, 270 & !1, 1920, 540));
        let (x, y, w, h) = fit_rect(1001, 777, 1920, 1080);
        assert!(x % 2 == 0 && y % 2 == 0 && w % 2 == 0 && h % 2 == 0);
        assert!(x + w <= 1920 && y + h <= 1080);
    }

    #[test]
    fn fit_size_caps_and_keeps_even() {
        assert_eq!(fit_size(1366, 768, 1920, 1080), (1366, 768));
        assert_eq!(fit_size(1921, 1081, 4000, 4000), (1920, 1080));
        assert_eq!(fit_size(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(2560, 1600, 1920, 1080), (1728, 1080));
    }

    #[test]
    fn same_size_matches_plain_conversion() {
        let (w, h) = (64u32, 32u32);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut a = vec![0u8; (w * h * 3 / 2) as usize];
        let mut b = vec![1u8; (w * h * 3 / 2) as usize];
        bgra_to_nv12(&src, w, w, h, &mut a);
        bgra_to_nv12_fit(&src, w, h, w, h, &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn scaled_frame_has_black_bars() {
        // White 4:3 source into a 16:9 canvas.
        let (dw, dh) = (64u32, 36u32);
        let src = solid(40, 30, [255, 255, 255, 255]);
        let mut out = vec![0u8; (dw * dh * 3 / 2) as usize];
        bgra_to_nv12_fit(&src, 40, 30, dw, dh, &mut out);
        let (x, _, w, _) = fit_rect(40, 30, dw, dh);
        let row = &out[(dw * 10) as usize..(dw * 11) as usize];
        assert_eq!(row[0], BLACK_Y);
        assert_eq!(row[(x + w) as usize], BLACK_Y);
        assert_eq!(row[(x + w / 2) as usize], 235);
        let uv = &out[(dw * dh) as usize..];
        assert_eq!(uv[0], BLACK_UV);
    }
}
