//! Pictures the window's own renderer draws, and that renderer's availability.
//!
//! A decoded frame either carries RGBA pixels, which the UI toolkit uploads
//! as an image, or a [`NativePicture`], which the window's GPU renderer takes
//! at draw time: a hardware buffer on Android, 4:2:0 planes from a software
//! decoder on the desktop (the GPU converts them to RGB). Dropping an unused
//! picture releases it: the hardware image goes back to its reader, planes
//! go back to the decoder's pool.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::Colorimetry;

/// The decoder transfers ownership to the renderer.
pub enum NativePicture {
    #[cfg(target_os = "android")]
    Android(crate::gpu_video::GpuPicture),
    /// 4:2:0 planes in CPU memory, converted to RGB by the renderer.
    Planar(PlanarPicture),
}

/// Spare plane buffers kept for reuse: one being decoded into, one pending,
/// one being drawn.
const POOL_SIZE: usize = 3;

/// Plane storage that circulates between a decoder and the renderer, so
/// steady-state streaming allocates nothing.
#[derive(Default)]
pub struct PlanePool {
    spare: Mutex<Vec<Vec<u8>>>,
}

impl PlanePool {
    /// A buffer of exactly `len` bytes (contents unspecified).
    fn take(&self, len: usize) -> Vec<u8> {
        let reused = self.spare.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let mut data = reused.unwrap_or_default();
        data.resize(len, 0);
        data
    }

    fn put(&self, data: Vec<u8>) {
        if data.capacity() == 0 {
            return;
        }
        let mut spare = self.spare.lock().unwrap_or_else(|e| e.into_inner());
        if spare.len() < POOL_SIZE {
            spare.push(data);
        }
    }
}

/// An I420 picture: the Y plane, then U, then V, each tightly packed
/// (chroma `ceil(width / 2)` × `ceil(height / 2)`).
pub struct PlanarPicture {
    data: Vec<u8>,
    pool: Arc<PlanePool>,
    pub width: u32,
    pub height: u32,
    pub colorimetry: Colorimetry,
}

impl PlanarPicture {
    /// Copies a picture out of a decoder's (strided) planes into pooled
    /// storage. Fails, rather than panicking, when a plane is shorter than
    /// the size and strides imply.
    pub fn copy_from(
        pool: &Arc<PlanePool>,
        (y, u, v): (&[u8], &[u8], &[u8]),
        (y_stride, u_stride, v_stride): (usize, usize, usize),
        width: usize,
        height: usize,
        colorimetry: Colorimetry,
    ) -> Result<Self, String> {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let mut data = pool.take(width * height + 2 * cw * ch);
        let (dy, chroma) = data.split_at_mut(width * height);
        let (du, dv) = chroma.split_at_mut(cw * ch);
        let copied =
            copy_plane(y, y_stride, width, dy) && copy_plane(u, u_stride, cw, du) && copy_plane(v, v_stride, cw, dv);
        if !copied {
            pool.put(data);
            return Err(format!("decoder planes too short for {width}x{height}"));
        }
        Ok(Self {
            data,
            pool: pool.clone(),
            width: width as u32,
            height: height as u32,
            colorimetry,
        })
    }

    /// The Y, U and V planes.
    pub fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        let luma = self.width as usize * self.height as usize;
        let chroma = (self.width as usize).div_ceil(2) * (self.height as usize).div_ceil(2);
        let (y, rest) = self.data.split_at(luma);
        let (u, v) = rest.split_at(chroma);
        (y, u, &v[..chroma])
    }
}

/// Copies `dst.len() / width` rows of `width` bytes from a strided plane.
fn copy_plane(src: &[u8], stride: usize, width: usize, dst: &mut [u8]) -> bool {
    if width == 0 {
        return true;
    }
    for (row, out) in dst.chunks_exact_mut(width).enumerate() {
        let Some(line) = src.get(row * stride..row * stride + width) else {
            return false;
        };
        out.copy_from_slice(line);
    }
    true
}

impl Drop for PlanarPicture {
    fn drop(&mut self) {
        self.pool.put(std::mem::take(&mut self.data));
    }
}

/// Availability of a window's native video renderer. Carries no frames.
#[derive(Default)]
pub struct GpuRendererStatus {
    ready: AtomicBool,
    failure: Mutex<Option<String>>,
}

impl GpuRendererStatus {
    pub fn ready(&self) {
        *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.ready.store(true, Ordering::Release);
    }

    pub fn pause(&self) {
        self.ready.store(false, Ordering::Release);
    }

    pub fn fail(&self, why: String) {
        let mut failure = self.failure.lock().unwrap_or_else(|p| p.into_inner());
        if failure.is_none() {
            log::warn!("GPU video path failed: {why}");
            *failure = Some(why);
        }
        self.pause();
    }
}

/// One stream's native output. Retiring it stops presentation of pending
/// native frames when the decoder falls back to CPU output.
pub struct GpuOutput {
    renderer: Arc<GpuRendererStatus>,
    retired: AtomicBool,
}

impl GpuOutput {
    pub fn new(renderer: Arc<GpuRendererStatus>) -> Self {
        Self {
            renderer,
            retired: AtomicBool::new(false),
        }
    }

    pub fn available(&self) -> bool {
        !self.retired.load(Ordering::Acquire) && self.renderer.ready.load(Ordering::Acquire)
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn failure(&self) -> Option<String> {
        self.renderer.failure.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planes_are_packed_and_storage_is_reused() {
        let pool = Arc::new(PlanePool::default());
        // 3x3 picture in padded planes (stride 4, chroma 2x2 in stride 4).
        let y: Vec<u8> = (0..12).collect();
        let u = [100, 101, 0, 0, 102, 103];
        let v = [200, 201, 0, 0, 202, 203];
        let picture = PlanarPicture::copy_from(&pool, (&y, &u, &v), (4, 4, 4), 3, 3, Colorimetry::default()).unwrap();
        let (py, pu, pv) = picture.planes();
        assert_eq!(py, [0, 1, 2, 4, 5, 6, 8, 9, 10]);
        assert_eq!(pu, [100, 101, 102, 103]);
        assert_eq!(pv, [200, 201, 202, 203]);
        let storage = picture.data.as_ptr();
        drop(picture);
        let again = PlanarPicture::copy_from(&pool, (&y, &u, &v), (4, 4, 4), 3, 3, Colorimetry::default()).unwrap();
        assert_eq!(again.data.as_ptr(), storage, "the dropped picture's storage is reused");
    }

    #[test]
    fn short_planes_are_an_error() {
        let pool = Arc::new(PlanePool::default());
        let y = [0u8; 8];
        let picture = PlanarPicture::copy_from(&pool, (&y, &y, &y), (4, 2, 2), 4, 4, Colorimetry::default());
        assert!(picture.is_err());
    }

    #[test]
    fn availability_follows_the_renderer_and_retirement() {
        let status = Arc::new(GpuRendererStatus::default());
        let output = GpuOutput::new(status.clone());
        assert!(!output.available());
        status.ready();
        assert!(output.available());
        output.retire();
        assert!(!output.available());
        let other = GpuOutput::new(status.clone());
        status.fail("lost".into());
        assert!(!other.available());
        assert_eq!(other.failure().as_deref(), Some("lost"));
    }
}
