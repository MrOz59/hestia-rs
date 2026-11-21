//! YUV → RGBA conversion for software decoders and CPU readback paths.
//!
//! The host encodes with the colorimetry the client asks for in ANNOUNCE
//! (`x-nv-video[0].encoderCscMode = (colorspace << 1) | full_range`); the
//! session currently asks for mode 0, BT.601 limited range.
//!
//! The CPU conversion is 8.8 fixed point. Rows are converted two pixels (one
//! chroma sample) at a time; on x86-64 an SSE2 kernel (part of the baseline
//! ISA, no runtime detection) does eight pixels per step with exactly the
//! same arithmetic, about six times faster than the scalar loop. Large
//! pictures are split into bands over up to four threads.
//!
//! [`Colorimetry::gpu_conversion`] gives the same conversion as a matrix for
//! GPU shaders (the desktop renderer converts there when it can).

/// Matrix + range of the decoded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Colorimetry {
    #[default]
    Bt601Limited,
    Bt601Full,
    Bt709Limited,
    Bt709Full,
}

impl Colorimetry {
    /// Decodes an `encoderCscMode` value (BT.2020 maps to BT.709 for SDR output).
    pub fn from_csc_mode(mode: u32) -> Self {
        let full = mode & 1 != 0;
        match (mode >> 1, full) {
            (0, false) => Self::Bt601Limited,
            (0, true) => Self::Bt601Full,
            (_, false) => Self::Bt709Limited,
            (_, true) => Self::Bt709Full,
        }
    }

    /// `(y_offset, y_scale, r_v, g_u, g_v, b_u)` in 8.8 fixed point.
    fn coefficients(self) -> [i32; 6] {
        match self {
            Self::Bt601Limited => [16, 298, 409, -100, -208, 516],
            Self::Bt601Full => [0, 256, 359, -88, -183, 454],
            Self::Bt709Limited => [16, 298, 459, -55, -136, 541],
            Self::Bt709Full => [0, 256, 403, -48, -120, 475],
        }
    }

    /// The conversion for a shader, on samples normalised to 0..1:
    /// `rgb = matrix * (yuv - offset)`, `matrix` column-major (Y, U, V
    /// columns), as `glUniformMatrix3fv` takes it.
    pub fn gpu_conversion(self) -> ([f32; 9], [f32; 3]) {
        let (kr, kb) = match self {
            Self::Bt601Limited | Self::Bt601Full => (0.299f32, 0.114f32),
            Self::Bt709Limited | Self::Bt709Full => (0.2126, 0.0722),
        };
        let full = matches!(self, Self::Bt601Full | Self::Bt709Full);
        let (y_scale, c_scale, y_offset) = if full {
            (1.0, 1.0, 0.0)
        } else {
            (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
        };
        let kg = 1.0 - kr - kb;
        let r_v = 2.0 * (1.0 - kr);
        let b_u = 2.0 * (1.0 - kb);
        let g_u = -b_u * kb / kg;
        let g_v = -r_v * kr / kg;
        (
            [
                y_scale,
                y_scale,
                y_scale,
                0.0,
                g_u * c_scale,
                b_u * c_scale,
                r_v * c_scale,
                g_v * c_scale,
                0.0,
            ],
            [y_offset, 128.0 / 255.0, 128.0 / 255.0],
        )
    }
}

/// Borrowed planar 4:2:0 picture.
#[derive(Debug, Clone, Copy)]
pub struct I420<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
    pub width: usize,
    pub height: usize,
}

#[inline(always)]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Pictures shorter than this convert on one thread.
const PARALLEL_MIN_ROWS: usize = 360;

fn conversion_pool(height: usize) -> Option<&'static rayon::ThreadPool> {
    use std::sync::OnceLock;

    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    if height < PARALLEL_MIN_ROWS {
        return None;
    }
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
        if threads < 2 {
            return None;
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|n| format!("hestia-color-{n}"))
            .build()
            .ok()
    })
    .as_ref()
}

/// Splits `height` rows into `n` bands of an even number of rows (so each
/// band starts on a chroma row): `(first row, rows)`.
fn band_rows(height: usize, n: usize) -> Vec<(usize, usize)> {
    let per = (height.div_ceil(n.max(1)) + 1) & !1;
    (0..height)
        .step_by(per.max(2))
        .map(|r| (r, per.min(height - r)))
        .collect()
}

/// Writes `width * height` RGBA pixels (alpha 255) into `out`, using up to
/// four threads for large pictures.
///
/// Panics if the planes or `out` are smaller than the dimensions require.
pub fn i420_to_rgba(src: &I420<'_>, colorimetry: Colorimetry, out: &mut [u8]) {
    let (w, h) = (src.width, src.height);
    assert!(out.len() >= w * h * 4, "RGBA buffer too small");
    let Some(pool) = conversion_pool(h) else {
        return i420_rows(src, colorimetry, out);
    };
    pool.scope(|scope| {
        let mut rest = &mut out[..w * h * 4];
        for (row, rows) in band_rows(h, pool.current_num_threads()) {
            let (band, tail) = std::mem::take(&mut rest).split_at_mut(rows * w * 4);
            rest = tail;
            let c = (row / 2) * src.uv_stride;
            let sub = I420 {
                y: &src.y[row * src.y_stride..],
                u: &src.u[c..],
                v: &src.v[c..],
                height: rows,
                ..*src
            };
            scope.spawn(move |_| i420_rows(&sub, colorimetry, band));
        }
    });
}

fn i420_rows(src: &I420<'_>, colorimetry: Colorimetry, out: &mut [u8]) {
    let k = colorimetry.coefficients();
    let (w, h) = (src.width, src.height);
    assert!(out.len() >= w * h * 4, "RGBA buffer too small");
    let chroma_w = w.div_ceil(2);

    for (row, out_row) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
        let y_row = &src.y[row * src.y_stride..row * src.y_stride + w];
        let c = (row / 2) * src.uv_stride;
        let u_row = &src.u[c..c + chroma_w];
        let v_row = &src.v[c..c + chroma_w];
        #[cfg(target_arch = "x86_64")]
        let done = sse2::row(y_row, u_row, v_row, out_row, k);
        #[cfg(not(target_arch = "x86_64"))]
        let done = 0;
        planar_row(
            &y_row[done..],
            &u_row[done / 2..],
            &v_row[done / 2..],
            &mut out_row[done * 4..],
            k,
        );
    }
}

/// One RGBA pixel, 8.8 fixed point: `l` is the scaled luma, `d*` the chroma
/// terms with rounding included.
#[inline(always)]
fn pixel(l: i32, dr: i32, dg: i32, db: i32) -> u32 {
    u32::from(clamp8((l + dr) >> 8))
        | u32::from(clamp8((l + dg) >> 8)) << 8
        | u32::from(clamp8((l + db) >> 8)) << 16
        | 0xff00_0000
}

/// Converts a row two pixels (one chroma sample) at a time; `u_row`/`v_row`
/// hold `ceil(y_row.len() / 2)` samples, `uv_step` bytes apart.
#[inline(always)]
fn pairs_row(y_row: &[u8], u_row: &[u8], v_row: &[u8], uv_step: usize, out_row: &mut [u8], k: [i32; 6]) {
    let [y_off, ys, rv, gu, gv, bu] = k;
    let chroma = |u: u8, v: u8| {
        let (u, v) = (i32::from(u) - 128, i32::from(v) - 128);
        (rv * v + 128, gu * u + gv * v + 128, bu * u + 128)
    };
    let mut samples = u_row.iter().step_by(uv_step).zip(v_row.iter().step_by(uv_step));
    let mut out_pairs = out_row.chunks_exact_mut(8);
    let mut luma_pairs = y_row.chunks_exact(2);
    for ((out, luma), (&u, &v)) in (&mut out_pairs).zip(&mut luma_pairs).zip(&mut samples) {
        let (dr, dg, db) = chroma(u, v);
        let l0 = (i32::from(luma[0]) - y_off) * ys;
        let l1 = (i32::from(luma[1]) - y_off) * ys;
        let both = u64::from(pixel(l0, dr, dg, db)) | u64::from(pixel(l1, dr, dg, db)) << 32;
        out.copy_from_slice(&both.to_le_bytes());
    }
    // Odd width: one pixel left.
    if let ([l], Some(out), Some((&u, &v))) = (
        luma_pairs.remainder(),
        out_pairs.into_remainder().get_mut(..4),
        samples.next(),
    ) {
        let (dr, dg, db) = chroma(u, v);
        out.copy_from_slice(&pixel((i32::from(*l) - y_off) * ys, dr, dg, db).to_le_bytes());
    }
}

fn planar_row(y_row: &[u8], u_row: &[u8], v_row: &[u8], out_row: &mut [u8], k: [i32; 6]) {
    pairs_row(y_row, u_row, v_row, 1, out_row, k);
}

/// Eight pixels per step with SSE2 (always present on x86-64), bit-exact
/// with [`pixel`]: 16×16→32-bit luma products, `pmaddwd` for the chroma
/// terms, and saturating packs for the clamp to 0..255.
#[cfg(target_arch = "x86_64")]
mod sse2 {
    use std::arch::x86_64::*;

    /// Converts the leading multiple of eight pixels; returns how many.
    pub(super) fn row(y_row: &[u8], u_row: &[u8], v_row: &[u8], out_row: &mut [u8], k: [i32; 6]) -> usize {
        let blocks = (y_row.len() / 8)
            .min(u_row.len() / 4)
            .min(v_row.len() / 4)
            .min(out_row.len() / 32);
        for b in 0..blocks {
            // SAFETY: SSE2 is part of x86-64; block `b` reads 8 luma, 4 + 4
            // chroma bytes and writes 32 bytes, all within the bounds above.
            unsafe {
                eight(
                    y_row.as_ptr().add(b * 8),
                    u_row.as_ptr().add(b * 4),
                    v_row.as_ptr().add(b * 4),
                    out_row.as_mut_ptr().add(b * 32),
                    k,
                );
            }
        }
        blocks * 8
    }

    #[inline(always)]
    unsafe fn eight(y: *const u8, u: *const u8, v: *const u8, out: *mut u8, k: [i32; 6]) {
        let [y_off, ys, rv, gu, gv, bu] = k;
        unsafe {
            let zero = _mm_setzero_si128();
            let luma = _mm_loadl_epi64(y.cast());
            let u4 = _mm_cvtsi32_si128(u.cast::<i32>().read_unaligned());
            let v4 = _mm_cvtsi32_si128(v.cast::<i32>().read_unaligned());
            // (Y - offset) * scale, as eight 32-bit products.
            let luma = _mm_sub_epi16(_mm_unpacklo_epi8(luma, zero), _mm_set1_epi16(y_off as i16));
            let scale = _mm_set1_epi16(ys as i16);
            let (lo, hi) = (_mm_mullo_epi16(luma, scale), _mm_mulhi_epi16(luma, scale));
            let (l0, l1) = (_mm_unpacklo_epi16(lo, hi), _mm_unpackhi_epi16(lo, hi));
            // Per chroma sample: U and V interleaved, one pmaddwd per channel.
            let half = _mm_set1_epi16(128);
            let u16 = _mm_sub_epi16(_mm_unpacklo_epi8(u4, zero), half);
            let v16 = _mm_sub_epi16(_mm_unpacklo_epi8(v4, zero), half);
            let uv = _mm_unpacklo_epi16(u16, v16);
            let weights = |wu: i32, wv: i32| _mm_set1_epi32((wu & 0xffff) | (wv << 16));
            let round = _mm_set1_epi32(128);
            let channel = |wu: i32, wv: i32| {
                let c = _mm_add_epi32(_mm_madd_epi16(uv, weights(wu, wv)), round);
                // Each chroma term serves two neighbouring pixels.
                let (c0, c1) = (_mm_unpacklo_epi32(c, c), _mm_unpackhi_epi32(c, c));
                let a = _mm_srai_epi32(_mm_add_epi32(l0, c0), 8);
                let b = _mm_srai_epi32(_mm_add_epi32(l1, c1), 8);
                let words = _mm_packs_epi32(a, b);
                _mm_packus_epi16(words, words)
            };
            let (r, g, b) = (channel(0, rv), channel(gu, gv), channel(bu, 0));
            let rg = _mm_unpacklo_epi8(r, g);
            let ba = _mm_unpacklo_epi8(b, _mm_set1_epi8(-1));
            _mm_storeu_si128(out.cast(), _mm_unpacklo_epi16(rg, ba));
            _mm_storeu_si128(out.add(16).cast(), _mm_unpackhi_epi16(rg, ba));
        }
    }
}

/// Borrowed 4:2:0 picture in Android's `YUV_420_888` layout: chroma
/// samples `uv_pixel_stride` bytes apart (1 = planar like I420, 2 = the
/// U and V planes interleave, as NV12/NV21 do).
#[derive(Debug, Clone, Copy)]
pub struct Yuv420<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub u_stride: usize,
    pub v_stride: usize,
    pub uv_pixel_stride: usize,
    pub width: usize,
    pub height: usize,
}

/// Writes `width * height` RGBA pixels (alpha 255) into `out`. Chroma
/// samples outside the planes (short last rows) read as neutral grey.
pub fn yuv420_to_rgba(src: &Yuv420<'_>, colorimetry: Colorimetry, out: &mut [u8]) {
    if src.uv_pixel_stride == 1 && src.u_stride == src.v_stride {
        return i420_to_rgba(
            &I420 {
                y: src.y,
                u: src.u,
                v: src.v,
                y_stride: src.y_stride,
                uv_stride: src.u_stride,
                width: src.width,
                height: src.height,
            },
            colorimetry,
            out,
        );
    }
    let (w, h) = (src.width, src.height);
    assert!(out.len() >= w * h * 4, "RGBA buffer too small");
    let Some(pool) = conversion_pool(h) else {
        return yuv420_rows(src, colorimetry, out);
    };
    pool.scope(|scope| {
        let mut rest = &mut out[..w * h * 4];
        for (row, rows) in band_rows(h, pool.current_num_threads()) {
            let (band, tail) = std::mem::take(&mut rest).split_at_mut(rows * w * 4);
            rest = tail;
            let sub = Yuv420 {
                y: &src.y[row * src.y_stride..],
                u: src.u.get((row / 2) * src.u_stride..).unwrap_or_default(),
                v: src.v.get((row / 2) * src.v_stride..).unwrap_or_default(),
                height: rows,
                ..*src
            };
            scope.spawn(move |_| yuv420_rows(&sub, colorimetry, band));
        }
    });
}

fn yuv420_rows(src: &Yuv420<'_>, colorimetry: Colorimetry, out: &mut [u8]) {
    let k = colorimetry.coefficients();
    let [y_off, ys, rv, gu, gv, bu] = k;
    let (w, h) = (src.width, src.height);
    let ps = src.uv_pixel_stride.max(1);
    // Bytes a full chroma row spans.
    let chroma_span = (w.div_ceil(2).max(1) - 1) * ps + 1;

    for (row, out_row) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
        let y_row = &src.y[row * src.y_stride..row * src.y_stride + w];
        let u_row = src.u.get((row / 2) * src.u_stride..).unwrap_or_default();
        let v_row = src.v.get((row / 2) * src.v_stride..).unwrap_or_default();
        if u_row.len() >= chroma_span && v_row.len() >= chroma_span {
            pairs_row(y_row, u_row, v_row, ps, out_row, k);
            continue;
        }
        // A short last row (some codecs' buffers end early): missing chroma
        // reads as neutral grey.
        for (x, (p, &l)) in out_row.chunks_exact_mut(4).zip(y_row).enumerate() {
            let c = (x / 2) * ps;
            let u = i32::from(u_row.get(c).copied().unwrap_or(128)) - 128;
            let v = i32::from(v_row.get(c).copied().unwrap_or(128)) - 128;
            let l = (i32::from(l) - y_off) * ys;
            p[0] = clamp8((l + rv * v + 128) >> 8);
            p[1] = clamp8((l + gu * u + gv * v + 128) >> 8);
            p[2] = clamp8((l + bu * u + 128) >> 8);
            p[3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(y: u8, u: u8, v: u8, c: Colorimetry) -> [u8; 4] {
        let (yp, up, vp) = ([y; 6], [u; 4], [v; 4]);
        let src = I420 {
            y: &yp,
            u: &up,
            v: &vp,
            y_stride: 3,
            uv_stride: 2,
            width: 3,
            height: 2,
        };
        let mut out = [0u8; 24];
        i420_to_rgba(&src, c, &mut out);
        // Every pixel identical, including the odd last column.
        for px in out.chunks_exact(4) {
            assert_eq!(px, &out[..4]);
        }
        out[..4].try_into().unwrap()
    }

    #[test]
    fn limited_range_extremes() {
        assert_eq!(convert(16, 128, 128, Colorimetry::Bt601Limited), [0, 0, 0, 255]);
        assert_eq!(convert(235, 128, 128, Colorimetry::Bt601Limited), [255, 255, 255, 255]);
        assert_eq!(convert(0, 128, 128, Colorimetry::Bt709Full), [0, 0, 0, 255]);
        assert_eq!(convert(255, 128, 128, Colorimetry::Bt709Full), [255, 255, 255, 255]);
    }

    #[test]
    fn primaries() {
        // BT.601 limited-range red, green and blue.
        let close = |a: [u8; 4], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= 2);
        assert!(close(convert(81, 90, 240, Colorimetry::Bt601Limited), [255, 0, 0]));
        assert!(close(convert(145, 54, 34, Colorimetry::Bt601Limited), [0, 255, 0]));
        assert!(close(convert(41, 240, 110, Colorimetry::Bt601Limited), [0, 0, 255]));
        // BT.709 limited-range red.
        assert!(close(convert(63, 102, 240, Colorimetry::Bt709Limited), [255, 0, 0]));
    }

    #[test]
    fn csc_modes() {
        assert_eq!(Colorimetry::from_csc_mode(0), Colorimetry::Bt601Limited);
        assert_eq!(Colorimetry::from_csc_mode(1), Colorimetry::Bt601Full);
        assert_eq!(Colorimetry::from_csc_mode(2), Colorimetry::Bt709Limited);
        assert_eq!(Colorimetry::from_csc_mode(5), Colorimetry::Bt709Full);
    }

    #[test]
    fn honours_strides() {
        // 2x2 picture inside padded planes; padding bytes would turn pixels red.
        let y = [16, 16, 0xff, 0xff, 235, 235, 0xff, 0xff];
        let u = [128, 0xff, 0xff, 0xff];
        let v = [128, 0xff, 0xff, 0xff];
        let src = I420 {
            y: &y,
            u: &u,
            v: &v,
            y_stride: 4,
            uv_stride: 4,
            width: 2,
            height: 2,
        };
        let mut out = [0u8; 16];
        i420_to_rgba(&src, Colorimetry::Bt601Limited, &mut out);
        assert_eq!(
            out,
            [0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn semi_planar_matches_planar() {
        // 4x2 picture: distinct chroma per 2x2 block.
        let y = [16u8, 100, 180, 235, 50, 120, 200, 90];
        let (u, v) = ([90u8, 240], [240u8, 110]);
        let planar = I420 {
            y: &y,
            u: &u,
            v: &v,
            y_stride: 4,
            uv_stride: 2,
            width: 4,
            height: 2,
        };
        let mut expected = [0u8; 32];
        i420_to_rgba(&planar, Colorimetry::Bt601Limited, &mut expected);

        // NV12: U V U V, with the V plane starting one byte later.
        let nv12 = [90u8, 240, 240, 110];
        let semi = Yuv420 {
            y: &y,
            u: &nv12,
            v: &nv12[1..],
            y_stride: 4,
            u_stride: 4,
            v_stride: 4,
            uv_pixel_stride: 2,
            width: 4,
            height: 2,
        };
        let mut got = [0u8; 32];
        yuv420_to_rgba(&semi, Colorimetry::Bt601Limited, &mut got);
        assert_eq!(got, expected);

        // Planar input takes the I420 path and gives the same pixels.
        let mut again = [0u8; 32];
        let as_888 = Yuv420 {
            u: &u,
            v: &v,
            u_stride: 2,
            v_stride: 2,
            uv_pixel_stride: 1,
            ..semi
        };
        yuv420_to_rgba(&as_888, Colorimetry::Bt601Limited, &mut again);
        assert_eq!(again, expected);
    }

    /// The conversion of one pixel, written out plainly.
    fn reference(y: u8, u: u8, v: u8, c: Colorimetry) -> [u8; 4] {
        let [y_off, ys, rv, gu, gv, bu] = c.coefficients();
        let (u, v) = (i32::from(u) - 128, i32::from(v) - 128);
        let l = (i32::from(y) - y_off) * ys;
        [
            clamp8((l + rv * v + 128) >> 8),
            clamp8((l + gu * u + gv * v + 128) >> 8),
            clamp8((l + bu * u + 128) >> 8),
            255,
        ]
    }

    const ALL: [Colorimetry; 4] = [
        Colorimetry::Bt601Limited,
        Colorimetry::Bt601Full,
        Colorimetry::Bt709Limited,
        Colorimetry::Bt709Full,
    ];

    #[test]
    fn vector_and_scalar_paths_match_the_formula() {
        // Odd width: eight-pixel blocks, then pairs, then a lone pixel.
        let (w, h) = (259usize, 6usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let hash = |i: usize, k: usize| ((i.wrapping_mul(2654435761) >> k) & 0xff) as u8;
        let y: Vec<u8> = (0..w * h).map(|i| hash(i, 3)).collect();
        let u: Vec<u8> = (0..cw * ch).map(|i| hash(i + 7, 9)).collect();
        let v: Vec<u8> = (0..cw * ch).map(|i| hash(i + 13, 17)).collect();
        let src = I420 {
            y: &y,
            u: &u,
            v: &v,
            y_stride: w,
            uv_stride: cw,
            width: w,
            height: h,
        };
        let nv12: Vec<u8> = u.iter().zip(&v).flat_map(|(&a, &b)| [a, b]).collect();
        let semi = Yuv420 {
            y: &y,
            u: &nv12,
            v: &nv12[1..],
            y_stride: w,
            u_stride: cw * 2,
            v_stride: cw * 2,
            uv_pixel_stride: 2,
            width: w,
            height: h,
        };
        for c in ALL {
            let mut planar = vec![0u8; w * h * 4];
            i420_to_rgba(&src, c, &mut planar);
            let mut interleaved = vec![0u8; w * h * 4];
            yuv420_to_rgba(&semi, c, &mut interleaved);
            for (i, (p, q)) in planar.chunks_exact(4).zip(interleaved.chunks_exact(4)).enumerate() {
                let (row, x) = (i / w, i % w);
                let ci = (row / 2) * cw + x / 2;
                let want = reference(y[i], u[ci], v[ci], c);
                assert_eq!(p, want, "{c:?} I420 pixel {x},{row}");
                assert_eq!(q, want, "{c:?} NV12 pixel {x},{row}");
            }
        }
    }

    #[test]
    fn every_luma_and_chroma_extreme_converts_exactly() {
        // One row of 256 luma values per chroma pair, through the SIMD path.
        let y: Vec<u8> = (0..=255).collect();
        for c in ALL {
            for (cu, cv) in [
                (0u8, 0u8),
                (0, 255),
                (255, 0),
                (255, 255),
                (128, 128),
                (16, 240),
                (240, 16),
            ] {
                let (u, v) = ([cu; 128], [cv; 128]);
                let src = I420 {
                    y: &y,
                    u: &u,
                    v: &v,
                    y_stride: 256,
                    uv_stride: 128,
                    width: 256,
                    height: 1,
                };
                let mut out = [0u8; 1024];
                i420_to_rgba(&src, c, &mut out);
                for (l, px) in out.chunks_exact(4).enumerate() {
                    assert_eq!(px, reference(l as u8, cu, cv, c), "{c:?} y={l} u={cu} v={cv}");
                }
            }
        }
    }

    #[test]
    fn short_chroma_rows_read_as_grey() {
        // 4x4 NV12 whose buffer ends two bytes early: the second chroma row
        // has its first U/V pair only.
        let y = [
            16u8, 50, 90, 130, 170, 210, 235, 20, 40, 60, 80, 100, 120, 140, 160, 180,
        ];
        let nv12 = [90u8, 240, 54, 34, 240, 110];
        let src = Yuv420 {
            y: &y,
            u: &nv12,
            v: &nv12[1..],
            y_stride: 4,
            u_stride: 4,
            v_stride: 4,
            uv_pixel_stride: 2,
            width: 4,
            height: 4,
        };
        let mut out = [0u8; 64];
        yuv420_to_rgba(&src, Colorimetry::Bt601Limited, &mut out);
        let c = Colorimetry::Bt601Limited;
        let want = |i: usize, u: u8, v: u8| reference(y[i], u, v, c);
        for (i, (u, v)) in [(90, 240), (90, 240), (54, 34), (54, 34)].into_iter().enumerate() {
            assert_eq!(out[i * 4..i * 4 + 4], want(i, u, v), "row 0 pixel {i}");
        }
        for i in 8..16 {
            let (u, v) = if i % 4 < 2 { (240, 110) } else { (128, 128) };
            assert_eq!(out[i * 4..i * 4 + 4], want(i, u, v), "pixel {i}");
        }
    }

    #[test]
    fn gpu_matrix_agrees_with_the_fixed_point_conversion() {
        for c in ALL {
            let (m, offset) = c.gpu_conversion();
            for (y, u, v) in [
                (16u8, 128u8, 128u8),
                (235, 128, 128),
                (81, 90, 240),
                (145, 54, 34),
                (41, 240, 110),
                (128, 200, 60),
            ] {
                let yuv = [
                    f32::from(y) / 255.0 - offset[0],
                    f32::from(u) / 255.0 - offset[1],
                    f32::from(v) / 255.0 - offset[2],
                ];
                let fixed = reference(y, u, v, c);
                for ch in 0..3 {
                    let f = (m[ch] * yuv[0] + m[3 + ch] * yuv[1] + m[6 + ch] * yuv[2]).clamp(0.0, 1.0) * 255.0;
                    assert!(
                        (f - f32::from(fixed[ch])).abs() <= 2.0,
                        "{c:?} {y},{u},{v} channel {ch}: {f} vs {}",
                        fixed[ch]
                    );
                }
            }
        }
    }

    #[test]
    fn bands_cover_every_row_and_start_even() {
        for (h, n) in [(1080, 4), (720, 3), (361, 4), (1081, 4), (2, 4)] {
            let b = band_rows(h, n);
            assert_eq!(b.iter().map(|&(_, r)| r).sum::<usize>(), h, "{h}/{n}: {b:?}");
            assert!(b.iter().all(|&(r, _)| r % 2 == 0), "{b:?}");
            assert!(b.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0), "{b:?}");
        }
    }

    #[test]
    fn threaded_conversion_matches_one_thread() {
        let (w, h) = (64usize, 720usize);
        let y: Vec<u8> = (0..w * h).map(|i| (i * 7 % 251) as u8).collect();
        let u: Vec<u8> = (0..w * h / 4).map(|i| (i * 13 % 241) as u8).collect();
        let v: Vec<u8> = (0..w * h / 4).map(|i| (i * 29 % 239) as u8).collect();
        let src = I420 {
            y: &y,
            u: &u,
            v: &v,
            y_stride: w,
            uv_stride: w / 2,
            width: w,
            height: h,
        };
        let mut single = vec![0u8; w * h * 4];
        i420_rows(&src, Colorimetry::Bt709Limited, &mut single);
        let mut threaded = vec![0u8; w * h * 4];
        i420_to_rgba(&src, Colorimetry::Bt709Limited, &mut threaded);
        assert_eq!(single, threaded);

        // Interleaved chroma (NV12) through the YUV_420_888 path.
        let nv12: Vec<u8> = u.iter().zip(&v).flat_map(|(&a, &b)| [a, b]).collect();
        let semi = Yuv420 {
            y: &y,
            u: &nv12,
            v: &nv12[1..],
            y_stride: w,
            u_stride: w,
            v_stride: w,
            uv_pixel_stride: 2,
            width: w,
            height: h,
        };
        let mut nv = vec![0u8; w * h * 4];
        yuv420_to_rgba(&semi, Colorimetry::Bt709Limited, &mut nv);
        assert_eq!(nv, single);
    }
}
