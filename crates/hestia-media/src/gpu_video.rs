//! Hand-off of hardware-decoded pictures to the UI's GPU renderer (Android).
//!
//! In GPU mode MediaCodec renders into an `AImageReader` whose buffers the
//! GPU samples directly (`AIMAGE_FORMAT_PRIVATE`, GPU usage): no CPU copy,
//! no CPU colour conversion. The decoder thread leaves the newest picture
//! in [`GpuVideo::global`]; the UI thread's renderer takes it, draws it
//! (YUV to RGB in the GPU sampler) into the texture the stream view shows,
//! and keeps it until the next one, then lets the codec reuse the buffer.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use log::warn;
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
    /// The stream it belongs to (see [`GpuVideo::generation`]).
    pub generation: u64,
}

// SAFETY: an AImage may be used and deleted from any thread.
unsafe impl Send for GpuPicture {}

impl GpuPicture {
    pub(crate) fn new(image: Image, reader: Arc<SharedReader>, generation: u64) -> Result<Self, String> {
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
            generation,
        })
    }

    /// The picture's `AHardwareBuffer *`, valid while `self` lives.
    pub fn hardware_buffer(&self) -> *mut std::ffi::c_void {
        self.image
            .hardware_buffer()
            .map_or(std::ptr::null_mut(), |b| b.as_ptr().cast())
    }
}

/// Meeting point of the decoder thread and the UI's renderer.
#[derive(Default)]
pub struct GpuVideo {
    /// The UI can draw hardware buffers (its GL context is set up).
    renderer_ready: AtomicBool,
    /// The user's setting (Settings → Zero-copy video).
    wanted: AtomicBool,
    /// Why drawing failed; once set, decoders use CPU output for this run.
    failure: Mutex<Option<String>>,
    latest: Mutex<Option<GpuPicture>>,
    generation: AtomicU64,
}

static GPU_VIDEO: OnceLock<GpuVideo> = OnceLock::new();

impl GpuVideo {
    pub fn global() -> &'static GpuVideo {
        GPU_VIDEO.get_or_init(GpuVideo::default)
    }

    fn latest(&self) -> MutexGuard<'_, Option<GpuPicture>> {
        self.latest.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The renderer can (or no longer can) draw hardware buffers.
    pub fn set_renderer_ready(&self, ready: bool) {
        self.renderer_ready.store(ready, Ordering::Release);
    }

    pub fn set_wanted(&self, wanted: bool) {
        self.wanted.store(wanted, Ordering::Release);
    }

    /// Decoders should render for the GPU.
    pub fn usable(&self) -> bool {
        self.wanted.load(Ordering::Acquire) && self.renderer_ready.load(Ordering::Acquire) && self.failure().is_none()
    }

    /// The renderer couldn't draw a picture: decoders go back to CPU output
    /// (for the rest of this run; the first reason is kept).
    pub fn fail(&self, why: String) {
        let mut failure = self.failure.lock().unwrap_or_else(|p| p.into_inner());
        if failure.is_none() {
            warn!("GPU video path failed: {why}");
            *failure = Some(why);
        }
        drop(failure);
        self.latest().take();
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The current stream's number; pictures of older ones are stale.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// A decoder starts rendering for the GPU: a new generation.
    pub(crate) fn begin(&self) -> u64 {
        self.latest().take();
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The stream ended: drop its pending picture; the renderer lets go of
    /// the one it shows at its next frame.
    pub fn end(&self) {
        self.latest().take();
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// The newest picture (an older one not drawn yet goes back to the codec).
    pub(crate) fn put(&self, picture: GpuPicture) {
        *self.latest() = Some(picture);
    }

    /// For the renderer: the newest picture, if one arrived since.
    pub fn take(&self) -> Option<GpuPicture> {
        self.latest().take()
    }
}
