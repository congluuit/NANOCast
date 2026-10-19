//! BGRA8 -> NV12 (BT.709, limited range) on the CPU. Rows are processed in
//! pairs in parallel. This is the temporary path; Phase 1 moves this to a GPU
//! shader so the frame never leaves VRAM.

use rayon::prelude::*;

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

            for x in (0..w).step_by(2) {
                let mut sum_u = 0i32;
                let mut sum_v = 0i32;
                for (src, ydst) in [(src0, &mut *y0), (src1, &mut *y1)] {
                    for dx in 0..2 {
                        let i = (x + dx) * 4;
                        let b = src[i] as i32;
                        let g = src[i + 1] as i32;
                        let r = src[i + 2] as i32;
                        let y = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
                        ydst[x + dx] = y.clamp(16, 235) as u8;
                        sum_u += (-26 * r - 87 * g + 112 * b + 128) >> 8;
                        sum_v += (112 * r - 102 * g - 10 * b + 128) >> 8;
                    }
                }
                uv_row[x] = ((sum_u >> 2) + 128).clamp(16, 240) as u8;
                uv_row[x + 1] = ((sum_v >> 2) + 128).clamp(16, 240) as u8;
            }
        });
}
