//! YUV → RGBA conversion for software decoders.
//!
//! The host encodes with the colorimetry the client asks for in ANNOUNCE
//! (`x-nv-video[0].encoderCscMode = (colorspace << 1) | full_range`); the
//! session currently asks for mode 0, BT.601 limited range.

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

/// How many horizontal bands to convert at once (one per core, up to 4).
fn bands(height: usize) -> usize {
    if height < PARALLEL_MIN_ROWS {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 4)
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
    let n = bands(h);
    if n <= 1 {
        return i420_rows(src, colorimetry, out);
    }
    std::thread::scope(|scope| {
        let mut rest = &mut out[..w * h * 4];
        for (row, rows) in band_rows(h, n) {
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
            scope.spawn(move || i420_rows(&sub, colorimetry, band));
        }
    });
}

fn i420_rows(src: &I420<'_>, colorimetry: Colorimetry, out: &mut [u8]) {
    let [y_off, ys, rv, gu, gv, bu] = colorimetry.coefficients();
    let (w, h) = (src.width, src.height);
    assert!(out.len() >= w * h * 4, "RGBA buffer too small");
    let chroma_w = w.div_ceil(2);

    for (row, out_row) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
        let y_row = &src.y[row * src.y_stride..row * src.y_stride + w];
        let c = (row / 2) * src.uv_stride;
        let u_row = &src.u[c..c + chroma_w];
        let v_row = &src.v[c..c + chroma_w];

        let mut pixels = out_row.chunks_exact_mut(8);
        let mut lumas = y_row.chunks_exact(2);
        for ((px, yy), (&u, &v)) in (&mut pixels).zip(&mut lumas).zip(u_row.iter().zip(v_row)) {
            let (u, v) = (u as i32 - 128, v as i32 - 128);
            let (dr, dg, db) = (rv * v + 128, gu * u + gv * v + 128, bu * u + 128);
            for (p, &l) in px.chunks_exact_mut(4).zip(yy) {
                let l = (l as i32 - y_off) * ys;
                p[0] = clamp8((l + dr) >> 8);
                p[1] = clamp8((l + dg) >> 8);
                p[2] = clamp8((l + db) >> 8);
                p[3] = 255;
            }
        }
        // Odd width: one pixel left.
        if let ([l], p) = (lumas.remainder(), pixels.into_remainder()) {
            if p.len() == 4 {
                let (u, v) = (u_row[chroma_w - 1] as i32 - 128, v_row[chroma_w - 1] as i32 - 128);
                let l = (*l as i32 - y_off) * ys;
                p[0] = clamp8((l + rv * v + 128) >> 8);
                p[1] = clamp8((l + gu * u + gv * v + 128) >> 8);
                p[2] = clamp8((l + bu * u + 128) >> 8);
                p[3] = 255;
            }
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
    let n = bands(h);
    if n <= 1 {
        return yuv420_rows(src, colorimetry, out);
    }
    std::thread::scope(|scope| {
        let mut rest = &mut out[..w * h * 4];
        for (row, rows) in band_rows(h, n) {
            let (band, tail) = std::mem::take(&mut rest).split_at_mut(rows * w * 4);
            rest = tail;
            let sub = Yuv420 {
                y: &src.y[row * src.y_stride..],
                u: src.u.get((row / 2) * src.u_stride..).unwrap_or_default(),
                v: src.v.get((row / 2) * src.v_stride..).unwrap_or_default(),
                height: rows,
                ..*src
            };
            scope.spawn(move || yuv420_rows(&sub, colorimetry, band));
        }
    });
}

fn yuv420_rows(src: &Yuv420<'_>, colorimetry: Colorimetry, out: &mut [u8]) {
    let [y_off, ys, rv, gu, gv, bu] = colorimetry.coefficients();
    let (w, h) = (src.width, src.height);
    let ps = src.uv_pixel_stride.max(1);

    for (row, out_row) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
        let y_row = &src.y[row * src.y_stride..row * src.y_stride + w];
        let u_row = src.u.get((row / 2) * src.u_stride..).unwrap_or_default();
        let v_row = src.v.get((row / 2) * src.v_stride..).unwrap_or_default();
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
