//! Session-owned handoff from the decoder to Slint and the native renderer.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_core::StreamSettings;
#[cfg(target_os = "android")]
use hestia_media::gpu_video::{GpuOutput, GpuRendererStatus};
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

#[derive(Default)]
struct PendingPicture {
    latest: Option<DecodedFrame>,
    spare: Option<DecodedFrame>,
    scheduled: bool,
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

/// One stream's picture slot and presentation metrics. A slow renderer
/// consumes the newest picture and releases any superseded native buffer.
#[derive(Default)]
pub(crate) struct Presentation {
    pending: Mutex<PendingPicture>,
    closed: AtomicBool,
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
        self.closed.store(true, Ordering::Release);
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).latest.take();
    }

    pub(crate) fn superseded(&self) -> u64 {
        let count = self.superseded.load(Ordering::Relaxed);
        #[cfg(target_os = "android")]
        let count = count + self.gpu.superseded.load(Ordering::Relaxed);
        count
    }

    /// Returns whether a callback needs to be scheduled on the UI thread.
    fn enqueue(&self, frame: &mut DecodedFrame) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        #[cfg(target_os = "android")]
        if frame.rgba().is_none() && !self.gpu_available() {
            return false;
        }
        let reusable = if let Some(mut older) = pending.latest.take() {
            if older.rgba().is_some() {
                self.superseded.fetch_add(1, Ordering::Relaxed);
                older
            } else {
                #[cfg(target_os = "android")]
                self.gpu.superseded.fetch_add(1, Ordering::Relaxed);
                older.discard_native();
                pending.spare.take().unwrap_or(older)
            }
        } else {
            pending.spare.take().unwrap_or_default()
        };
        pending.latest = Some(std::mem::replace(frame, reusable));
        !std::mem::replace(&mut pending.scheduled, true)
    }

    #[cfg(target_os = "android")]
    pub(crate) fn gpu_available(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.output.as_ref().is_some_and(|output| output.available())
    }

    #[cfg(target_os = "android")]
    pub(crate) fn discard_gpu(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.latest.as_ref().is_some_and(|frame| frame.rgba().is_none()) {
            pending.latest.take();
        }
    }

    #[cfg(target_os = "android")]
    pub(crate) fn take_gpu(&self) -> Option<DecodedFrame> {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.latest.as_ref()?.rgba().is_some() {
            return None;
        }
        let frame = pending.latest.take()?;
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
            let mut pending = presentation.pending.lock().unwrap_or_else(|p| p.into_inner());
            pending.scheduled = false;
            if presentation.closed.load(Ordering::Acquire) {
                return;
            }
            #[cfg(target_os = "android")]
            if pending.latest.as_ref().is_some_and(|frame| frame.rgba().is_none()) {
                if presentation.gpu_available() {
                    presentation.gpu.redraw_requests.fetch_add(1, Ordering::Relaxed);
                    drop(pending);
                    ui.window().request_redraw();
                } else {
                    pending.latest.take();
                }
                return;
            }

            let Some(frame) = pending.latest.take() else { return };
            drop(pending);
            let Some(data) = frame.rgba() else { return };
            let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(data, frame.width, frame.height);
            if presentation.closed.load(Ordering::Acquire) {
                return;
            }
            ui.set_stream_frame(Image::from_rgba8(buffer));
            let delivered_at = Instant::now();
            presentation.recovery.record(frame.idr_requested_at, delivered_at);
            presentation.handoff.record(delivered_at.duration_since(frame.decoded_at));
            presentation
                .receive_to_ui
                .record(delivered_at.duration_since(frame.received_at));
            if presentation.delivered.fetch_add(1, Ordering::Relaxed) == 0 {
                ui.set_stream_status(SharedString::new());
            }
            presentation.pending.lock().unwrap_or_else(|p| p.into_inner()).spare = Some(frame);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_frame_wins_and_a_closed_session_cannot_queue_more() {
        let previous = Presentation::default();
        let next = Presentation::default();
        let mut frame = DecodedFrame::default();
        let pixels = frame.reshape(2, 1).as_ptr();
        frame.frame_number = 1;
        assert!(previous.enqueue(&mut frame));
        frame.reshape(2, 1);
        frame.frame_number = 2;
        assert!(!previous.enqueue(&mut frame), "only one UI callback is scheduled");
        assert_eq!(previous.superseded(), 1);
        assert_eq!(frame.rgba().unwrap().as_ptr(), pixels, "reuse the previous CPU buffer");
        assert_eq!(previous.pending.lock().unwrap().latest.as_ref().unwrap().frame_number, 2);

        previous.close();
        frame.frame_number = 3;
        assert!(!previous.enqueue(&mut frame));
        assert!(previous.pending.lock().unwrap().latest.is_none());
        assert!(next.enqueue(&mut frame));
        assert_eq!(next.pending.lock().unwrap().latest.as_ref().unwrap().frame_number, 3);
    }
}
