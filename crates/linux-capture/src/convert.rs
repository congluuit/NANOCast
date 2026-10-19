//! Pixel conversion on the CPU, platform-neutral so it is unit-tested on every OS:
//! 32-bit RGB (BGRx/BGRA from PipeWire and X11, RGBx/RGBA from some compositors) or NV12
//! -> NV12 (BT.709, limited range) at a fixed canvas size, scaled to fit with black bars
//! (nearest neighbour, like `nanocast_win::convert`), plus the X11 cursor overlay.
//! Rows are processed in pairs in parallel (rayon).

use rayon::prelude::*;

const BLACK_Y: u8 = 16;
const BLACK_UV: u8 = 128;

/// Byte order of a 32-bit pixel in memory. The fourth byte (x or alpha) is ignored:
/// screen content is opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rgb32 {
    /// B, G, R, x (`BGRx`/`BGRA` in PipeWire/SPA naming, X11 24/32-bit TrueColor on x86/ARM).
    Bgrx,
    /// R, G, B, x (`RGBx`/`RGBA`).
    Rgbx,
}

impl Rgb32 {
    /// Offsets of red and blue inside one pixel (green is always 1).
    #[inline]
    fn rb(self) -> (usize, usize) {
        match self {
            Rgb32::Bgrx => (2, 0),
            Rgb32::Rgbx => (0, 2),
        }
    }
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

/// Destination rectangle and source sampling maps for a `src` -> `dst` fit. When the
/// sizes already match (after dropping an odd last row/column) the maps are the identity.
fn sampling(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> ((usize, usize, usize, usize), Vec<usize>, Vec<usize>) {
    let (rx, ry, rw, rh) =
        if src_w & !1 == dst_w && src_h & !1 == dst_h { (0, 0, dst_w, dst_h) } else { fit_rect(src_w, src_h, dst_w, dst_h) };
    // Sample at pixel centres.
    let xmap: Vec<usize> =
        (0..rw).map(|x| (((2 * x + 1) as u64 * src_w as u64) / (2 * rw as u64)).min(src_w as u64 - 1) as usize).collect();
    let ymap: Vec<usize> =
        (0..rh).map(|y| (((2 * y + 1) as u64 * src_h as u64) / (2 * rh as u64)).min(src_h as u64 - 1) as usize).collect();
    ((rx as usize, ry as usize, rw as usize, rh as usize), xmap, ymap)
}

/// Converts a `src_w`x`src_h` 32-bit RGB picture with `stride` bytes per row into an NV12
/// canvas of `dst_w`x`dst_h` (even sizes), scaled to fit with black bars.
/// `out` must hold `dst_w*dst_h*3/2` bytes; `src` at least `stride*(src_h-1) + src_w*4`.
#[allow(clippy::too_many_arguments)]
pub fn rgb32_to_nv12_fit(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    stride: usize,
    layout: Rgb32,
    dst_w: u32,
    dst_h: u32,
    out: &mut [u8],
) {
    if src_w == 0 || src_h == 0 || dst_w < 2 || dst_h < 2 {
        return;
    }
    let ((rx, ry, rw, rh), xmap, ymap) = sampling(src_w, src_h, dst_w, dst_h);
    let (dw, dh) = (dst_w as usize, dst_h as usize);
    let (y_plane, uv_plane) = out[..dw * dh * 3 / 2].split_at_mut(dw * dh);
    if rw != dw || rh != dh {
        y_plane.fill(BLACK_Y);
        uv_plane.fill(BLACK_UV);
    }
    let row_len = src_w as usize * 4;
    let (ri, bi) = layout.rb();

    y_plane[ry * dw..(ry + rh) * dw]
        .par_chunks_mut(dw * 2)
        .zip(uv_plane[ry / 2 * dw..(ry + rh) / 2 * dw].par_chunks_mut(dw))
        .enumerate()
        .for_each(|(pair, (y_rows, uv_row))| {
            let s0 = ymap[pair * 2] * stride;
            let s1 = ymap[pair * 2 + 1] * stride;
            let src0 = &src[s0..s0 + row_len];
            let src1 = &src[s1..s1 + row_len];
            let (y0, y1) = y_rows.split_at_mut(dw);
            let (y0, y1, uv) = (&mut y0[rx..rx + rw], &mut y1[rx..rx + rw], &mut uv_row[rx..rx + rw]);
            for n in 0..rw / 2 {
                let x = n * 2;
                let mut sum_u = 0i32;
                let mut sum_v = 0i32;
                for (row, ydst) in [(src0, &mut *y0), (src1, &mut *y1)] {
                    for dx in 0..2 {
                        let i = xmap[x + dx] * 4;
                        let r = row[i + ri] as i32;
                        let g = row[i + 1] as i32;
                        let b = row[i + bi] as i32;
                        let y = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
                        ydst[x + dx] = y.clamp(16, 235) as u8;
                        sum_u += (-26 * r - 87 * g + 112 * b + 128) >> 8;
                        sum_v += (112 * r - 102 * g - 10 * b + 128) >> 8;
                    }
                }
                uv[x] = ((sum_u >> 2) + 128).clamp(16, 240) as u8;
                uv[x + 1] = ((sum_v >> 2) + 128).clamp(16, 240) as u8;
            }
        });
}

/// Copies / scales an NV12 picture (`y` plane with `y_stride`, interleaved `uv` plane with
/// `uv_stride`) into an NV12 canvas of `dst_w`x`dst_h`, scaled to fit with black bars.
#[allow(clippy::too_many_arguments)]
pub fn nv12_fit(
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    out: &mut [u8],
) {
    if src_w < 2 || src_h < 2 || dst_w < 2 || dst_h < 2 {
        return;
    }
    let ((rx, ry, rw, rh), xmap, ymap) = sampling(src_w, src_h, dst_w, dst_h);
    let (dw, dh) = (dst_w as usize, dst_h as usize);
    let (y_plane, uv_plane) = out[..dw * dh * 3 / 2].split_at_mut(dw * dh);
    if rw != dw || rh != dh {
        y_plane.fill(BLACK_Y);
        uv_plane.fill(BLACK_UV);
    }
    y_plane[ry * dw..(ry + rh) * dw].par_chunks_mut(dw).enumerate().for_each(|(row, dst)| {
        let s = &y[ymap[row] * y_stride..];
        for (x, d) in dst[rx..rx + rw].iter_mut().enumerate() {
            *d = s[xmap[x]];
        }
    });
    uv_plane[ry / 2 * dw..(ry + rh) / 2 * dw].par_chunks_mut(dw).enumerate().for_each(|(row, dst)| {
        let s = &uv[(ymap[row * 2] / 2) * uv_stride..];
        for n in 0..rw / 2 {
            let sx = (xmap[n * 2] / 2) * 2;
            dst[rx + n * 2] = s[sx];
            dst[rx + n * 2 + 1] = s[sx + 1];
        }
    });
}

/// NV12 -> I420 (planar Y, U, V), for encoders that only take planar input (OpenH264).
pub fn nv12_to_i420(nv12: &[u8], w: u32, h: u32, out: &mut Vec<u8>) {
    let (w, h) = (w as usize, h as usize);
    let (y, uv) = nv12[..w * h * 3 / 2].split_at(w * h);
    out.clear();
    out.reserve(w * h * 3 / 2);
    out.extend_from_slice(y);
    out.extend(uv.iter().step_by(2));
    out.extend(uv.iter().skip(1).step_by(2));
}

/// Draws a cursor image (premultiplied ARGB, one `u32` per pixel as XFixes returns it) onto
/// a 32-bit RGB picture with its top-left corner at (`x`, `y`), clipped to the picture.
#[allow(clippy::too_many_arguments)]
pub fn blend_cursor(
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    stride: usize,
    layout: Rgb32,
    cursor: &[u32],
    cur_w: u32,
    cur_h: u32,
    x: i32,
    y: i32,
) {
    let (ri, bi) = layout.rb();
    for cy in 0..cur_h as i32 {
        let py = y + cy;
        if py < 0 || py >= dst_h as i32 {
            continue;
        }
        for cx in 0..cur_w as i32 {
            let px = x + cx;
            if px < 0 || px >= dst_w as i32 {
                continue;
            }
            let Some(&argb) = cursor.get((cy * cur_w as i32 + cx) as usize) else { return };
            let a = argb >> 24;
            if a == 0 {
                continue;
            }
            let i = py as usize * stride + px as usize * 4;
            let Some(p) = dst.get_mut(i..i + 4) else { return };
            let inv = 255 - a;
            let over = |d: u8, c: u32| (c + (d as u32 * inv + 127) / 255).min(255) as u8;
            p[ri] = over(p[ri], (argb >> 16) & 0xFF);
            p[1] = over(p[1], (argb >> 8) & 0xFF);
            p[bi] = over(p[bi], argb & 0xFF);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        px.iter().copied().cycle().take((w * h * 4) as usize).collect()
    }

    #[test]
    fn fit_rect_letterbox_and_pillarbox() {
        assert_eq!(fit_rect(1920, 1080, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(fit_rect(800, 600, 1920, 1080), (240, 0, 1440, 1080));
        assert_eq!(fit_rect(1920, 540, 1920, 1080), (0, 270 & !1, 1920, 540));
        let (x, y, w, h) = fit_rect(1001, 777, 1920, 1080);
        assert!(x % 2 == 0 && y % 2 == 0 && w % 2 == 0 && h % 2 == 0);
        assert!(x + w <= 1920 && y + h <= 1080);
    }

    #[test]
    fn fit_size_caps_and_keeps_even() {
        assert_eq!(fit_size(1366, 768, 1920, 1080), (1366, 768));
        assert_eq!(fit_size(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(2560, 1600, 1920, 1080), (1728, 1080));
        assert_eq!(fit_size(1920, 1080, 1280, 720), (1280, 720));
    }

    #[test]
    fn bgrx_and_rgbx_give_the_same_colour() {
        let (w, h) = (8u32, 4u32);
        // Pure red in both byte orders.
        let bgrx = solid(w, h, [0, 0, 255, 0]);
        let rgbx = solid(w, h, [255, 0, 0, 0]);
        let mut a = vec![0u8; (w * h * 3 / 2) as usize];
        let mut b = vec![0u8; (w * h * 3 / 2) as usize];
        rgb32_to_nv12_fit(&bgrx, w, h, (w * 4) as usize, Rgb32::Bgrx, w, h, &mut a);
        rgb32_to_nv12_fit(&rgbx, w, h, (w * 4) as usize, Rgb32::Rgbx, w, h, &mut b);
        assert_eq!(a, b);
        // BT.709 limited red: Y ~ 63, V high.
        assert!((60..=66).contains(&a[0]), "Y = {}", a[0]);
        assert!(a[(w * h) as usize + 1] > 200);
    }

    #[test]
    fn padded_stride_is_respected() {
        let (w, h) = (4u32, 2u32);
        let stride = 32usize; // 16 bytes of padding per row
        let mut src = vec![0xAAu8; stride * h as usize];
        for row in 0..h as usize {
            src[row * stride..row * stride + 16].copy_from_slice(&solid(4, 1, [255, 255, 255, 0]));
        }
        let mut out = vec![0u8; (w * h * 3 / 2) as usize];
        rgb32_to_nv12_fit(&src, w, h, stride, Rgb32::Bgrx, w, h, &mut out);
        assert!(out[..(w * h) as usize].iter().all(|&y| y == 235));
    }

    #[test]
    fn scaled_frame_has_black_bars() {
        let (dw, dh) = (64u32, 36u32);
        let src = solid(40, 30, [255, 255, 255, 255]);
        let mut out = vec![0u8; (dw * dh * 3 / 2) as usize];
        rgb32_to_nv12_fit(&src, 40, 30, 160, Rgb32::Bgrx, dw, dh, &mut out);
        let (x, _, w, _) = fit_rect(40, 30, dw, dh);
        let row = &out[(dw * 10) as usize..(dw * 11) as usize];
        assert_eq!(row[0], BLACK_Y);
        assert_eq!(row[(x + w) as usize], BLACK_Y);
        assert_eq!(row[(x + w / 2) as usize], 235);
        assert_eq!(out[(dw * dh) as usize], BLACK_UV);
    }

    #[test]
    fn nv12_same_size_is_a_copy_and_i420_splits_planes() {
        let (w, h) = (6u32, 4u32);
        let src: Vec<u8> = (0..w * h * 3 / 2).map(|i| i as u8).collect();
        let (y, uv) = src.split_at((w * h) as usize);
        let mut out = vec![0u8; src.len()];
        nv12_fit(y, w as usize, uv, w as usize, w, h, w, h, &mut out);
        assert_eq!(out, src);
        let mut i420 = Vec::new();
        nv12_to_i420(&src, w, h, &mut i420);
        assert_eq!(&i420[24..30], &[24, 26, 28, 30, 32, 34]);
        assert_eq!(&i420[30..36], &[25, 27, 29, 31, 33, 35]);
    }

    #[test]
    fn nv12_downscale_fills_bars() {
        let (sw, sh) = (8u32, 8u32);
        let src = vec![200u8; (sw * sh * 3 / 2) as usize];
        let (y, uv) = src.split_at((sw * sh) as usize);
        let (dw, dh) = (8u32, 4u32);
        let mut out = vec![0u8; (dw * dh * 3 / 2) as usize];
        nv12_fit(y, sw as usize, uv, sw as usize, sw, sh, dw, dh, &mut out);
        assert_eq!(out[0], BLACK_Y); // pillarbox: 4x4 picture centred in 8x4
        assert_eq!(out[2], 200);
    }

    #[test]
    fn cursor_blends_and_clips() {
        let (w, h) = (4u32, 4u32);
        let mut img = solid(w, h, [0, 0, 0, 0]);
        // Opaque white 2x2 cursor, half off the left/top edge.
        let cursor = [0xFFFF_FFFFu32; 4];
        blend_cursor(&mut img, w, h, 16, Rgb32::Bgrx, &cursor, 2, 2, -1, -1);
        assert_eq!(&img[0..4], &[255, 255, 255, 0]);
        assert_eq!(&img[4..8], &[0, 0, 0, 0]);
        // Half-transparent premultiplied grey over white.
        let mut white = solid(1, 1, [255, 255, 255, 0]);
        blend_cursor(&mut white, 1, 1, 4, Rgb32::Bgrx, &[0x8000_0000], 1, 1, 0, 0);
        assert_eq!(white[0], 127);
    }
}
