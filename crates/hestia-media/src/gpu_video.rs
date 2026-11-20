//! Android hardware picture ownership and renderer availability.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ndk::media::image_reader::{Image, ImageReader};

/// An image reader shared by the codec and the pictures acquired from it:
/// the reader is deleted only after its last picture.
pub(crate) struct SharedReader(pub(crate) ImageReader);

// SAFETY: an AImageReader may be used from any thread one call at a time;
// only the decoder thread acquires from it, and the last reference (on any
// thread) deletes it.
unsafe impl Send for SharedReader {}
unsafe impl Sync for SharedReader {}

/// One decoded picture, in a GPU buffer.
pub struct GpuPicture {
    // Declared first: dropped (AImage_delete) before the reader.
    image: Image,
    _reader: Arc<SharedReader>,
    /// The visible picture's size.
    pub width: u32,
    pub height: u32,
    /// Where it lies in the buffer, in texture coordinates: x, y, width,
    /// height (the buffer can be taller, e.g. 1088 rows for 1080).
    pub crop: [f32; 4],
}

// SAFETY: an AImage may be used and deleted from any thread.
unsafe impl Send for GpuPicture {}

impl GpuPicture {
    pub(crate) fn new(image: Image, reader: Arc<SharedReader>) -> Result<Self, String> {
        let buffer = image.hardware_buffer().map_err(|e| format!("hardware buffer: {e:?}"))?;
        let desc = buffer.describe();
        let (buffer_w, buffer_h) = (desc.width.max(1) as i32, desc.height.max(1) as i32);
        let (left, top, right, bottom) = match image.crop_rect() {
            Ok(r) if r.right > r.left && r.bottom > r.top => (
                r.left.max(0),
                r.top.max(0),
                r.right.min(buffer_w),
                r.bottom.min(buffer_h),
            ),
            _ => (0, 0, buffer_w, buffer_h),
        };
        let (bw, bh) = (buffer_w as f32, buffer_h as f32);
        Ok(Self {
            image,
            _reader: reader,
            width: (right - left).max(1) as u32,
            height: (bottom - top).max(1) as u32,
            crop: [
                left as f32 / bw,
                top as f32 / bh,
                (right - left) as f32 / bw,
                (bottom - top) as f32 / bh,
            ],
        })
    }

    /// The picture's `AHardwareBuffer *`, valid while `self` lives.
    pub fn hardware_buffer(&self) -> *mut std::ffi::c_void {
        self.image
            .hardware_buffer()
            .map_or(std::ptr::null_mut(), |b| b.as_ptr().cast())
    }

    /// The fence protects the decoder from reusing the image before the GPU
    /// finishes sampling it. The reader stays live through the release call.
    pub fn release(self, fence: OwnedFd) {
        self.image.delete_async(fence);
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

    pub(crate) fn failure(&self) -> Option<String> {
        self.renderer.failure.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
}
