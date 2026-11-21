//! A stream's pictures on their way to the window, without a toolkit.
//!
//! The decoder thread hands each picture to [`UiPresenter`], which keeps the
//! newest one in a mailbox and schedules the UI thread through the port:
//! RGBA pictures go to [`View::show_video_frame`]; native pictures (Android
//! hardware buffers, desktop YUV planes) only ask for a redraw, and the
//! window's GPU renderer — part of the shell, since it hooks into the
//! toolkit's rendering — takes the newest one with [`Presentation::take_gpu`]
//! while the window renders. [`GpuWindow`] tells that renderer which stream
//! is active.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_media::native_picture::{GpuOutput, GpuRendererStatus};
use hestia_media::picture_mailbox::{DisplayUpdate, PictureKind, PictureMailbox};
use hestia_media::{DecodedFrame, FramePresenter, LatencyHistogram, RecoveryLatency};

use crate::ui::{UiHandle, View};

/// Pictures drawn by the window's own GPU renderer (Android hardware
/// buffers, desktop YUV planes); the renderer records them.
#[derive(Default)]
pub struct GpuVideoStats {
    pub redraw_requests: AtomicU64,
    pub drawn: AtomicU64,
    pub superseded: AtomicU64,
    pub ready_to_draw: LatencyHistogram,
    pub receive_to_draw: LatencyHistogram,
    pub recovery: RecoveryLatency,
}

/// One stream's link to the window and its presentation metrics.
#[derive(Default)]
pub struct Presentation {
    pictures: PictureMailbox,
    pub(crate) delivered: AtomicU64,
    pub(crate) superseded: AtomicU64,
    pub(crate) handoff: LatencyHistogram,
    pub(crate) receive_to_ui: LatencyHistogram,
    pub(crate) recovery: RecoveryLatency,
    pub gpu: GpuVideoStats,
    output: Option<Arc<GpuOutput>>,
    /// The stream this belongs to (see [`GpuWindow::is_active`]).
    pub generation: u64,
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

    /// The window's GPU renderer draws this stream's native pictures now.
    pub fn gpu_available(&self) -> bool {
        !self.pictures.is_closed() && self.output.as_ref().is_some_and(|output| output.available())
    }

    /// Drops a pending native picture (the renderer failed or went away).
    pub fn discard_gpu(&self) {
        self.pictures.discard_native();
    }

    /// The newest native picture, for the renderer to draw now.
    pub fn take_gpu(&self) -> Option<DecodedFrame> {
        let frame = self.pictures.take_native()?;
        self.gpu_available().then_some(frame)
    }
}

/// The window's native renderer knows only which session is active.
#[derive(Default)]
pub struct GpuWindow {
    /// Set by the renderer: ready, paused (no GL context) or failed.
    pub status: Arc<GpuRendererStatus>,
    active: Mutex<std::sync::Weak<Presentation>>,
    active_generation: AtomicU64,
}

impl GpuWindow {
    pub(crate) fn set_active(&self, session: &Arc<Presentation>) {
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        self.active_generation.store(session.generation, Ordering::Release);
        *active = Arc::downgrade(session);
    }

    /// The stream being shown, if any.
    pub fn active(&self) -> Option<Arc<Presentation>> {
        self.active.lock().unwrap_or_else(|p| p.into_inner()).upgrade()
    }

    pub fn is_active(&self, generation: u64) -> bool {
        self.active_generation.load(Ordering::Acquire) == generation
    }
}

pub(crate) struct UiPresenter {
    pub(crate) ui: Arc<dyn UiHandle>,
    pub(crate) presentation: Arc<Presentation>,
}

impl FramePresenter for UiPresenter {
    fn present(&mut self, frame: &mut DecodedFrame) {
        if !self.presentation.enqueue(frame) {
            return;
        }
        let presentation = self.presentation.clone();
        self.ui.run(Box::new(move |ui: &dyn View| {
            let frame = match presentation.pictures.next_for_display(|| presentation.gpu_available()) {
                Some(DisplayUpdate::Cpu(frame)) => frame,
                Some(DisplayUpdate::RedrawNative) => {
                    presentation.gpu.redraw_requests.fetch_add(1, Ordering::Relaxed);
                    ui.request_redraw();
                    return;
                }
                None => return,
            };
            if frame.rgba().is_none() || presentation.pictures.is_closed() {
                return;
            }
            ui.show_video_frame(&frame);
            let delivered_at = Instant::now();
            presentation.recovery.record(frame.idr_requested_at, delivered_at);
            presentation
                .handoff
                .record(delivered_at.duration_since(frame.decoded_at));
            presentation
                .receive_to_ui
                .record(delivered_at.duration_since(frame.received_at));
            if presentation.delivered.fetch_add(1, Ordering::Relaxed) == 0 {
                ui.set_stream_status(String::new());
            }
            presentation.pictures.recycle_cpu(frame);
        }));
    }
}
