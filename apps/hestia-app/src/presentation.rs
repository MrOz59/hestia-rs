//! Slint and native renderer adapters for a stream's picture mailbox.
//!
//! RGBA pictures are uploaded as Slint images on the event loop. Native
//! pictures (Android hardware buffers, desktop YUV planes) only trigger a
//! redraw; the window's GPU renderer (`gpu_render` on Android, `gl_video` on
//! the desktop) takes the newest one while Slint renders.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_media::native_picture::{GpuOutput, GpuRendererStatus};
use hestia_media::picture_mailbox::{DisplayUpdate, PictureKind, PictureMailbox};
use hestia_media::{DecodedFrame, FramePresenter, LatencyHistogram, RecoveryLatency};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use crate::AppWindow;

/// Pictures drawn by the window's own GPU renderer (Android hardware
/// buffers, desktop YUV planes).
#[derive(Default)]
pub(crate) struct GpuVideoStats {
    pub(crate) redraw_requests: AtomicU64,
    pub(crate) drawn: AtomicU64,
    pub(crate) superseded: AtomicU64,
    pub(crate) ready_to_draw: LatencyHistogram,
    pub(crate) receive_to_draw: LatencyHistogram,
    pub(crate) recovery: RecoveryLatency,
}

/// One stream's link to the window and its presentation metrics.
#[derive(Default)]
pub(crate) struct Presentation {
    pictures: PictureMailbox,
    pub(crate) delivered: AtomicU64,
    pub(crate) superseded: AtomicU64,
    pub(crate) handoff: LatencyHistogram,
    pub(crate) receive_to_ui: LatencyHistogram,
    pub(crate) recovery: RecoveryLatency,
    pub(crate) gpu: GpuVideoStats,
    output: Option<Arc<GpuOutput>>,
    pub(crate) generation: u64,
}

impl Presentation {
    pub(crate) fn new(generation: u64, output: Option<Arc<GpuOutput>>) -> Self {
        Self {
            generation,
            output,
            ..Self::default()
        }
    }

    pub(crate) fn close(&self) {
        self.pictures.close();
    }

    pub(crate) fn superseded(&self) -> u64 {
        self.superseded.load(Ordering::Relaxed) + self.gpu.superseded.load(Ordering::Relaxed)
    }

    /// Returns whether a callback needs to be scheduled on the UI thread.
    fn enqueue(&self, frame: &mut DecodedFrame) -> bool {
        let Some(update) = self.pictures.push(frame, || self.gpu_available()) else {
            return false;
        };
        match update.superseded {
            Some(PictureKind::Cpu) => {
                self.superseded.fetch_add(1, Ordering::Relaxed);
            }
            Some(PictureKind::Native) => {
                self.gpu.superseded.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        update.schedule
    }

    pub(crate) fn gpu_available(&self) -> bool {
        !self.pictures.is_closed() && self.output.as_ref().is_some_and(|output| output.available())
    }

    pub(crate) fn discard_gpu(&self) {
        self.pictures.discard_native();
    }

    pub(crate) fn take_gpu(&self) -> Option<DecodedFrame> {
        let frame = self.pictures.take_native()?;
        self.gpu_available().then_some(frame)
    }
}

/// The window's native renderer knows only which session is active.
#[derive(Default)]
pub(crate) struct GpuWindow {
    pub(crate) status: Arc<GpuRendererStatus>,
    active: Mutex<std::sync::Weak<Presentation>>,
    active_generation: AtomicU64,
}

impl GpuWindow {
    pub(crate) fn set_active(&self, session: &Arc<Presentation>) {
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        self.active_generation.store(session.generation, Ordering::Release);
        *active = Arc::downgrade(session);
    }

    pub(crate) fn active(&self) -> Option<Arc<Presentation>> {
        self.active.lock().unwrap_or_else(|p| p.into_inner()).upgrade()
    }

    pub(crate) fn is_active(&self, generation: u64) -> bool {
        self.active_generation.load(Ordering::Acquire) == generation
    }
}

pub(crate) struct UiPresenter {
    pub(crate) weak: slint::Weak<AppWindow>,
    pub(crate) presentation: Arc<Presentation>,
}

impl FramePresenter for UiPresenter {
    fn present(&mut self, frame: &mut DecodedFrame) {
        if !self.presentation.enqueue(frame) {
            return;
        }
        let presentation = self.presentation.clone();
        let _ = self.weak.upgrade_in_event_loop(move |ui| {
            let frame = match presentation.pictures.next_for_display(|| presentation.gpu_available()) {
                Some(DisplayUpdate::Cpu(frame)) => frame,
                Some(DisplayUpdate::RedrawNative) => {
                    presentation.gpu.redraw_requests.fetch_add(1, Ordering::Relaxed);
                    ui.window().request_redraw();
                    return;
                }
                None => return,
            };
            let Some(data) = frame.rgba() else { return };
            let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(data, frame.width, frame.height);
            if presentation.pictures.is_closed() {
                return;
            }
            ui.set_stream_frame(Image::from_rgba8(buffer));
            let delivered_at = Instant::now();
            presentation.recovery.record(frame.idr_requested_at, delivered_at);
            presentation
                .handoff
                .record(delivered_at.duration_since(frame.decoded_at));
            presentation
                .receive_to_ui
                .record(delivered_at.duration_since(frame.received_at));
            if presentation.delivered.fetch_add(1, Ordering::Relaxed) == 0 {
                ui.set_stream_status(SharedString::new());
            }
            presentation.pictures.recycle_cpu(frame);
        });
    }
}
