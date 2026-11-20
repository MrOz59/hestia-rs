//! Slint and native renderer adapters for a stream's picture mailbox.

use std::sync::Arc;
#[cfg(target_os = "android")]
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use hestia_core::StreamSettings;
#[cfg(target_os = "android")]
use hestia_media::gpu_video::{GpuOutput, GpuRendererStatus};
use hestia_media::picture_mailbox::{DisplayUpdate, PictureKind, PictureMailbox};
use hestia_media::{Colorimetry, DecodedFrame, DecoderChoice, FramePresenter, LatencyHistogram, RecoveryLatency};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use crate::AppWindow;

pub(crate) struct StreamVideo {
    pub(crate) decoder: DecoderChoice,
    #[cfg(target_os = "android")]
    pub(crate) gpu: Option<Arc<GpuOutput>>,
}

impl StreamVideo {
    pub(crate) fn select(
        settings: &StreamSettings,
        #[cfg(target_os = "android")] window: &GpuWindow,
    ) -> Result<Self, String> {
        let colorimetry = Colorimetry::default();
        #[cfg(target_os = "android")]
        let gpu = settings
            .gpu_video
            .then(|| Arc::new(GpuOutput::new(window.status.clone())));
        #[cfg(target_os = "android")]
        let decoder = if settings.hardware_decoding {
            let candidates = crate::platform::video_decoders(settings.width, settings.height, settings.fps)
                .unwrap_or_else(|e| {
                    log::warn!("could not check Android video decoders: {e}");
                    Vec::new()
                });
            hestia_media::decoder_with_android_capabilities(colorimetry, candidates, gpu.clone())?
        } else {
            hestia_media::decoder_with(colorimetry, false)?
        };
        #[cfg(not(target_os = "android"))]
        let decoder = hestia_media::decoder_with(colorimetry, settings.hardware_decoding)?;
        Ok(Self {
            decoder,
            #[cfg(target_os = "android")]
            gpu,
        })
    }
}

#[cfg(target_os = "android")]
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
    #[cfg(target_os = "android")]
    pub(crate) gpu: GpuVideoStats,
    #[cfg(target_os = "android")]
    output: Option<Arc<GpuOutput>>,
    #[cfg(target_os = "android")]
    pub(crate) generation: u64,
}

impl Presentation {
    pub(crate) fn new(
        #[cfg(target_os = "android")] generation: u64,
        #[cfg(target_os = "android")] output: Option<Arc<GpuOutput>>,
    ) -> Self {
        Self {
            #[cfg(target_os = "android")]
            generation,
            #[cfg(target_os = "android")]
            output,
            ..Self::default()
        }
    }

    pub(crate) fn close(&self) {
        self.pictures.close();
    }

    pub(crate) fn superseded(&self) -> u64 {
        let count = self.superseded.load(Ordering::Relaxed);
        #[cfg(target_os = "android")]
        let count = count + self.gpu.superseded.load(Ordering::Relaxed);
        count
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
            #[cfg(target_os = "android")]
            Some(PictureKind::Native) => {
                self.gpu.superseded.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        update.schedule
    }

    pub(crate) fn gpu_available(&self) -> bool {
        #[cfg(target_os = "android")]
        {
            !self.pictures.is_closed() && self.output.as_ref().is_some_and(|output| output.available())
        }
        #[cfg(not(target_os = "android"))]
        {
            false
        }
    }

    #[cfg(target_os = "android")]
    pub(crate) fn discard_gpu(&self) {
        self.pictures.discard_native();
    }

    #[cfg(target_os = "android")]
    pub(crate) fn take_gpu(&self) -> Option<DecodedFrame> {
        let frame = self.pictures.take_native()?;
        self.gpu_available().then_some(frame)
    }
}

/// The window's native renderer knows only which session is active.
#[cfg(target_os = "android")]
#[derive(Default)]
pub(crate) struct GpuWindow {
    pub(crate) status: Arc<GpuRendererStatus>,
    active: Mutex<std::sync::Weak<Presentation>>,
    active_generation: AtomicU64,
}

#[cfg(target_os = "android")]
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
                    #[cfg(target_os = "android")]
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
