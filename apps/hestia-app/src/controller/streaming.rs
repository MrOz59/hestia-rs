//! The stream screen: runs a [`StreamSession`] after a launch, shows decoded
//! frames and forwards keyboard, mouse and touch input to the host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_core::LaunchedSession;
use hestia_core::stream::input::{Modifiers, MouseButton};
use hestia_core::stream::{InputEvent, SessionListener, StreamEvent, StreamSession, StreamStats};
use hestia_media::{Colorimetry, DecodeStats, DecodingSink, FramePresenter, RgbaFrame};
use log::{info, warn};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use super::{Controller, Shared, spawn};
use crate::AppWindow;
use crate::keymap::{self, Mapped};

pub(super) struct ActiveStream {
    generation: u64,
    session: StreamSession,
    host_id: String,
}

/// Hands decoded pictures to the UI thread. Only the newest picture is kept:
/// if the UI is still busy with the previous one, it is replaced, so a slow
/// renderer drops frames instead of building up latency.
struct UiPresenter {
    weak: slint::Weak<AppWindow>,
    slot: Arc<Mutex<Option<SharedPixelBuffer<Rgba8Pixel>>>>,
    scheduled: Arc<AtomicBool>,
    first: bool,
}

impl FramePresenter for UiPresenter {
    fn present(&mut self, frame: &RgbaFrame) {
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&frame.data, frame.width, frame.height);
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(buffer);
        let first = std::mem::take(&mut self.first);
        if self.scheduled.swap(true, Ordering::AcqRel) && !first {
            return;
        }
        let slot = self.slot.clone();
        let scheduled = self.scheduled.clone();
        let _ = self.weak.upgrade_in_event_loop(move |ui| {
            scheduled.store(false, Ordering::Release);
            if let Some(buffer) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                ui.set_stream_frame(Image::from_rgba8(buffer));
            }
            if first {
                ui.set_stream_status(SharedString::new());
            }
        });
    }
}

struct Listener {
    controller: Shared,
    generation: u64,
    decode: Arc<DecodeStats>,
    /// "1920×1080 H264" etc.
    label: String,
    last: Mutex<Option<(Instant, StreamStats, u64)>>,
}

impl Listener {
    fn stats_line(&self, stats: StreamStats) -> Option<String> {
        let decoded = self.decode.decoded.load(Ordering::Relaxed);
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        let previous = last.replace((Instant::now(), stats, decoded));
        let (at, prev, prev_decoded) = previous?;
        let secs = at.elapsed().as_secs_f64().max(0.001);
        Some(format!(
            "{}  ·  {:.0} fps  ·  {:.1} Mbps  ·  RTT {} ms  ·  decode {:.1} ms  ·  FEC {}  ·  lost {}  ·  dropped {}",
            self.label,
            (decoded.saturating_sub(prev_decoded)) as f64 / secs,
            stats.bytes.saturating_sub(prev.bytes) as f64 * 8.0 / secs / 1e6,
            stats.rtt_ms,
            self.decode.mean_decode_ms(),
            stats.recovered_packets,
            stats.lost_frames,
            self.decode.dropped.load(Ordering::Relaxed),
        ))
    }
}

impl SessionListener for Listener {
    fn on_event(&self, event: StreamEvent) {
        match event {
            StreamEvent::Stats(stats) => {
                if let Some(line) = self.stats_line(stats) {
                    self.controller.ui(move |ui| ui.set_stream_stats(line.into()));
                }
            }
            StreamEvent::FirstFrame => {
                self.controller
                    .ui(|ui| ui.set_stream_status("Decoding the first frame…".into()));
            }
            StreamEvent::HdrMode(on) => info!("host HDR mode: {on}"),
            StreamEvent::StageFailed(stage, e) => warn!("{stage:?} failed: {e}"),
            StreamEvent::Terminated { reason, graceful } => {
                info!("stream ended (graceful: {graceful}): {reason}");
                self.controller.end_stream(Some(self.generation), Some(reason), false);
            }
            _ => {}
        }
    }
}

impl Controller {
    fn active_stream(&self) -> std::sync::MutexGuard<'_, Option<ActiveStream>> {
        self.stream.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Opens the stream for a launched app; runs on a worker thread (the RTSP
    /// handshake blocks). On success the window switches to the stream view.
    pub(super) fn start_stream(self: &Shared, host_id: String, launched: &LaunchedSession) -> Result<(), String> {
        let (decoder, formats) = hestia_media::default_decoder(Colorimetry::default())?;
        let config = launched.stream_config(&formats).map_err(|e| e.to_string())?;
        let generation = self.stream_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let sink = DecodingSink::new(
            decoder,
            Box::new(UiPresenter {
                weak,
                slot: Arc::new(Mutex::new(None)),
                scheduled: Arc::new(AtomicBool::new(false)),
                first: true,
            }),
        );
        let decode = sink.stats();
        let size = format!("{}×{}", config.width, config.height);
        let listener = Arc::new(Listener {
            controller: self.clone(),
            generation,
            decode,
            label: size.clone(),
            last: Mutex::new(None),
        });
        let session = StreamSession::start(config, Box::new(sink), hestia_media::default_audio_sink(), listener)
            .map_err(|e| e.to_string())?;
        let codec = format!("{:?}", session.negotiated().video_format.codec);
        info!("streaming {size} {codec} from {}", launched.host.name);

        let previous = self.active_stream().replace(ActiveStream {
            generation,
            session,
            host_id,
        });
        if let Some(previous) = previous {
            spawn("hestia-stream-stop", move || previous.session.stop());
        }
        self.ui(move |ui| {
            ui.set_stream_frame(Image::default());
            ui.set_stream_stats(format!("{size} {codec}").into());
            ui.set_stream_status("Waiting for video…".into());
            ui.set_screen(4);
        });
        Ok(())
    }

    /// Stops the stream (`generation`: only if it is still that stream).
    /// The app keeps running on the host unless `quit` is set.
    pub(super) fn end_stream(self: &Shared, generation: Option<u64>, reason: Option<String>, quit: bool) {
        let active = {
            let mut guard = self.active_stream();
            let stale = matches!((guard.as_ref(), generation), (Some(a), Some(g)) if a.generation != g);
            if stale { None } else { guard.take() }
        };
        let Some(active) = active else { return };
        let c = self.clone();
        spawn("hestia-stream-stop", move || {
            let host_id = active.host_id.clone();
            active.session.stop();
            c.ui(|ui| {
                if ui.get_stream_fullscreen() {
                    ui.window().set_fullscreen(false);
                    ui.set_stream_fullscreen(false);
                }
                if ui.get_screen() == 4 {
                    ui.set_screen(1);
                }
                ui.set_stream_frame(Image::default());
                ui.set_stream_stats(SharedString::new());
            });
            if let Some(reason) = reason {
                c.toast(reason);
            }
            if quit && c.state().current_host.as_deref() == Some(host_id.as_str()) {
                c.quit_app();
            } else {
                c.refresh_current_running();
            }
        });
    }

    fn send_input(&self, event: InputEvent) {
        if let Some(active) = self.active_stream().as_ref() {
            active.session.send_input(&event);
        }
    }

    pub(super) fn stream_key(self: &Shared, text: &str, pressed: bool, repeat: bool, modifiers: Modifiers) {
        // The host repeats held keys itself.
        if repeat {
            return;
        }
        if pressed && modifiers.ctrl && modifiers.alt && modifiers.shift {
            match text.to_ascii_lowercase().as_str() {
                "q" => return self.end_stream(None, None, false),
                "s" => return self.ui(|ui| ui.set_stream_show_stats(!ui.get_stream_show_stats())),
                _ => {}
            }
        }
        match keymap::map(text) {
            Some(Mapped::Key(key)) => self.send_input(InputEvent::Key {
                key,
                pressed,
                modifiers,
            }),
            Some(Mapped::Text(text)) if pressed => self.send_input(InputEvent::Text(text)),
            _ => {}
        }
    }

    pub(super) fn stream_pointer(&self, x: f32, y: f32, width: f32, height: f32) {
        if width < 1.0 || height < 1.0 {
            return;
        }
        let clamp = |v: f32, max: f32| v.clamp(0.0, max - 1.0).round() as i16;
        self.send_input(InputEvent::MousePosition {
            x: clamp(x, width),
            y: clamp(y, height),
            width: width.round().min(i16::MAX as f32) as i16,
            height: height.round().min(i16::MAX as f32) as i16,
        });
    }

    pub(super) fn stream_button(&self, button: i32, pressed: bool) {
        let button = match button {
            1 => MouseButton::Left,
            2 => MouseButton::Middle,
            3 => MouseButton::Right,
            _ => return,
        };
        self.send_input(InputEvent::MouseButton { button, pressed });
    }

    pub(super) fn stream_scroll(&self, dx: f32, dy: f32) {
        // Slint reports pixels (about 60 per wheel notch); GameStream wants
        // 120 per notch.
        let amount = |v: f32| (v * 2.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        let (horizontal, vertical) = (amount(dx), amount(dy));
        if horizontal != 0 || vertical != 0 {
            self.send_input(InputEvent::Scroll { vertical, horizontal });
        }
    }

    pub(super) fn toggle_fullscreen(self: &Shared) {
        self.ui(|ui| {
            let on = !ui.get_stream_fullscreen();
            ui.window().set_fullscreen(on);
            ui.set_stream_fullscreen(on);
        });
    }
}
