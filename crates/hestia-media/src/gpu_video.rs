//! Hand-off of hardware-decoded pictures to the UI's GPU renderer (Android).
//!
//! In GPU mode MediaCodec renders into an `AImageReader` whose buffers the
//! GPU samples directly (`AIMAGE_FORMAT_PRIVATE`, GPU usage): no CPU copy,
//! no CPU colour conversion. The decoder thread leaves the newest picture
//! in [`GpuVideo::global`]; the UI thread's renderer takes it, draws it
//! (YUV to RGB in the GPU sampler) into the texture the stream view shows,
//! and returns it with a fence for the GPU's last read.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use log::warn;
use ndk::media::image_reader::{Image, ImageReader};

use crate::decode::{FrameTiming, LatencyHistogram};

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
    generation: u64,
    timing: FrameTiming,
    ready_at: Instant,
    stats: Arc<GpuVideoStats>,
}

// SAFETY: an AImage may be used and deleted from any thread.
unsafe impl Send for GpuPicture {}

impl GpuPicture {
    pub(crate) fn new(
        image: Image,
        reader: Arc<SharedReader>,
        session: &GpuSession,
        timing: FrameTiming,
    ) -> Result<Self, String> {
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
            generation: session.generation,
            timing,
            ready_at: Instant::now(),
            stats: session.stats.clone(),
        })
    }

    /// The picture's `AHardwareBuffer *`, valid while `self` lives.
    pub fn hardware_buffer(&self) -> *mut std::ffi::c_void {
        self.image
            .hardware_buffer()
            .map_or(std::ptr::null_mut(), |b| b.as_ptr().cast())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Record the submission of the GPU pass. This is earlier than scanout.
    pub fn record_draw(&self) {
        let now = Instant::now();
        self.stats.ready_to_draw.record(now.duration_since(self.ready_at));
        self.stats
            .receive_to_draw
            .record(now.duration_since(self.timing.received_at));
        self.stats.drawn.fetch_add(1, Ordering::Relaxed);
    }

    /// The fence protects the decoder from reusing the image before the GPU
    /// finishes sampling it. The reader stays live through the release call.
    pub fn release(self, fence: OwnedFd) {
        self.image.delete_async(fence);
    }
}

#[derive(Default)]
pub struct GpuVideoStats {
    pub redraw_requests: AtomicU64,
    pub drawn: AtomicU64,
    pub superseded: AtomicU64,
    pub ready_to_draw: LatencyHistogram,
    pub receive_to_draw: LatencyHistogram,
}

#[derive(Clone)]
pub struct GpuSession {
    generation: u64,
    pub stats: Arc<GpuVideoStats>,
}

#[derive(Default)]
struct Mailbox {
    generation: u64,
    session: Option<GpuSession>,
    latest: Option<GpuPicture>,
}

/// Meeting point of the decoder thread and the UI's renderer.
#[derive(Default)]
pub struct GpuVideo {
    renderer_ready: AtomicBool,
    failure: Mutex<Option<String>>,
    mailbox: Mutex<Mailbox>,
}

static GPU_VIDEO: OnceLock<GpuVideo> = OnceLock::new();

impl GpuVideo {
    pub fn global() -> &'static GpuVideo {
        GPU_VIDEO.get_or_init(GpuVideo::default)
    }

    fn mailbox(&self) -> MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The renderer can draw hardware buffers in its current GL context.
    pub fn renderer_ready(&self) {
        *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.renderer_ready.store(true, Ordering::Release);
    }

    /// A paused Android window can acquire another context on resume.
    pub fn renderer_paused(&self) {
        self.renderer_ready.store(false, Ordering::Release);
        self.mailbox().latest.take();
    }

    /// The renderer couldn't initialize or draw in this context.
    pub fn fail(&self, why: String) {
        self.renderer_ready.store(false, Ordering::Release);
        let mut failure = self.failure.lock().unwrap_or_else(|p| p.into_inner());
        if failure.is_none() {
            warn!("GPU video path failed: {why}");
            *failure = Some(why);
        }
        drop(failure);
        self.mailbox().latest.take();
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn prepare(&'static self, wanted: bool) -> GpuSessionGuard {
        let mut mailbox = self.mailbox();
        mailbox.latest.take();
        mailbox.generation += 1;
        let session = GpuSession {
            generation: mailbox.generation,
            stats: Arc::new(GpuVideoStats::default()),
        };
        mailbox.session = wanted.then(|| session.clone());
        GpuSessionGuard { gpu: self, session }
    }

    pub(crate) fn active_session(&self) -> Option<GpuSession> {
        if !self.renderer_ready.load(Ordering::Acquire) {
            return None;
        }
        self.mailbox().session.clone()
    }

    pub fn generation(&self) -> u64 {
        self.mailbox().generation
    }

    fn end(&self, generation: u64) {
        let mut mailbox = self.mailbox();
        if mailbox.generation == generation {
            mailbox.session = None;
            mailbox.latest.take();
            mailbox.generation += 1;
        }
    }

    /// The decoder switched away from GPU output for this session.
    pub(crate) fn retire_output(&self, session: &GpuSession) {
        let mut mailbox = self.mailbox();
        if mailbox.generation == session.generation {
            mailbox.session = None;
            mailbox.latest.take();
        }
    }

    /// The newest picture (an older one not drawn yet goes back to the codec).
    pub(crate) fn put(&self, picture: GpuPicture) {
        let mut mailbox = self.mailbox();
        if self.renderer_ready.load(Ordering::Acquire)
            && mailbox.generation == picture.generation
            && mailbox.session.is_some()
        {
            if let Some(old) = mailbox.latest.replace(picture) {
                old.stats.superseded.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn take(&self) -> Option<GpuPicture> {
        self.mailbox().latest.take()
    }
}

pub struct GpuSessionGuard {
    gpu: &'static GpuVideo,
    session: GpuSession,
}

impl GpuSessionGuard {
    pub fn stats(&self) -> Arc<GpuVideoStats> {
        self.session.stats.clone()
    }
}

impl Drop for GpuSessionGuard {
    fn drop(&mut self) {
        self.gpu.end(self.session.generation);
    }
}
