//! The stream screen: runs a [`StreamSession`] after a launch, shows decoded
//! frames and forwards keyboard, mouse and touch input to the host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_core::LaunchedSession;
use hestia_core::report::StatsSample;
use hestia_core::stream::input::{Modifiers, MouseButton};
use hestia_core::stream::{InputEvent, SessionListener, StreamEvent, StreamSession, StreamStats};
use hestia_media::{Colorimetry, DecodeStats, DecodingSink, FramePresenter, RgbaFrame};
use log::{info, warn};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use super::report::{self, SharedReport};
use super::{Controller, Shared, spawn};
use crate::AppWindow;
use crate::keymap::{self, Mapped};
use crate::touch::{self, Trackpad};
use crate::typing;

/// Trackpad speed on top of the on-screen → host pixel ratio.
const TRACKPAD_SPEED: f32 = 1.4;

pub(super) struct ActiveStream {
    generation: u64,
    session: StreamSession,
    host_id: String,
    /// Stream resolution, for trackpad scaling.
    width: u32,
    trackpad: Trackpad,
    /// Identifies the current touch, so a stale long-press timer is ignored.
    touch_id: u64,
    /// Current content of the keyboard bar.
    typed: String,
    report: SharedReport,
    decode: Arc<DecodeStats>,
}

impl ActiveStream {
    fn send(&self, events: impl IntoIterator<Item = InputEvent>) {
        for event in events {
            self.session.send_input(&event);
        }
    }
}

fn trackpad_events(outputs: Vec<touch::Output>) -> impl Iterator<Item = InputEvent> {
    outputs.into_iter().map(|o| match o {
        touch::Output::Move { dx, dy } => InputEvent::MouseMove { dx, dy },
        touch::Output::Button { button, pressed } => InputEvent::MouseButton { button, pressed },
    })
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
    report: SharedReport,
}

impl Listener {
    /// Per-second rates from the running totals.
    fn sample(&self, stats: StreamStats) -> Option<StatsSample> {
        let decoded = self.decode.decoded.load(Ordering::Relaxed);
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        let previous = last.replace((Instant::now(), stats, decoded));
        let (at, prev, prev_decoded) = previous?;
        let secs = at.elapsed().as_secs_f64().max(0.001);
        let decoded_fps = decoded.saturating_sub(prev_decoded) as f64 / secs;
        Some(StatsSample {
            fps: stats.frames.saturating_sub(prev.frames) as f64 / secs,
            mbps: stats.bytes.saturating_sub(prev.bytes) as f64 * 8.0 / secs / 1e6,
            rtt_ms: stats.rtt_ms,
            rtt_variance_ms: stats.rtt_variance_ms,
            fec_recovered: stats.recovered_packets,
            lost_frames: stats.lost_frames,
            decoded_fps: Some(decoded_fps),
            decode_ms: Some(self.decode.mean_decode_ms()),
            dropped: Some(self.decode.dropped.load(Ordering::Relaxed)),
        })
    }

    fn stats_line(&self, s: &StatsSample) -> String {
        format!(
            "{}  ·  {:.0} fps  ·  {:.1} Mbps  ·  RTT {} ms  ·  decode {:.1} ms  ·  FEC {}  ·  lost {}  ·  dropped {}",
            self.label,
            s.decoded_fps.unwrap_or(s.fps),
            s.mbps,
            s.rtt_ms,
            s.decode_ms.unwrap_or(0.0),
            s.fec_recovered,
            s.lost_frames,
            s.dropped.unwrap_or(0),
        )
    }
}

impl SessionListener for Listener {
    fn on_event(&self, event: StreamEvent) {
        match event {
            StreamEvent::Stats(stats) => {
                if let Some(sample) = self.sample(stats) {
                    let line = self.stats_line(&sample);
                    report::lock(&self.report).sample(sample);
                    self.controller.ui(move |ui| ui.set_stream_stats(line.into()));
                }
            }
            StreamEvent::FirstFrame => {
                report::lock(&self.report).first_frame();
                self.controller
                    .ui(|ui| ui.set_stream_status("Decoding the first frame…".into()));
            }
            StreamEvent::HdrMode(on) => {
                info!("host HDR mode: {on}");
                report::lock(&self.report).event(format!("host HDR mode: {on}"));
            }
            StreamEvent::StageStarting(stage) => report::lock(&self.report).event(format!("starting {stage:?}")),
            StreamEvent::StageFailed(stage, e) => {
                warn!("{stage:?} failed: {e}");
                report::lock(&self.report).event(format!("{stage:?} failed: {e}"));
            }
            StreamEvent::Connected => report::lock(&self.report).event("connected"),
            StreamEvent::Rumble { .. } => {}
            StreamEvent::Terminated { reason, graceful } => {
                info!("stream ended (graceful: {graceful}): {reason}");
                self.controller.end_stream(Some(self.generation), Some(reason), false);
            }
        }
    }
}

impl Controller {
    fn active_stream(&self) -> std::sync::MutexGuard<'_, Option<ActiveStream>> {
        self.stream.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Opens the stream for a launched app; runs on a worker thread (the RTSP
    /// handshake blocks). On success the window switches to the stream view.
    pub(super) fn start_stream(
        self: &Shared,
        host_id: String,
        launched: &LaunchedSession,
        session_report: SharedReport,
    ) -> Result<(), String> {
        let (decoder, formats) = hestia_media::default_decoder(Colorimetry::default())?;
        let config = launched.stream_config(&formats).map_err(|e| e.to_string())?;
        hestia_core::add_stream_config_to_report(&config, decoder.name(), &mut report::lock(&session_report));
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
        let stream_width = config.width;
        let size = format!("{}×{}", config.width, config.height);
        let listener = Arc::new(Listener {
            controller: self.clone(),
            generation,
            decode: decode.clone(),
            label: size.clone(),
            last: Mutex::new(None),
            report: session_report.clone(),
        });
        let session = StreamSession::start(config, Box::new(sink), hestia_media::default_audio_sink(), listener)
            .map_err(|e| e.to_string())?;
        let codec = format!("{:?}", session.negotiated().video_format.codec);
        info!("streaming {size} {codec} from {}", launched.host.name);
        hestia_core::add_negotiated_to_report(session.negotiated(), &mut report::lock(&session_report));

        let previous = self.active_stream().replace(ActiveStream {
            generation,
            session,
            host_id,
            width: stream_width,
            trackpad: Trackpad::new(),
            touch_id: 0,
            typed: String::new(),
            report: session_report,
            decode,
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
            let ActiveStream {
                session,
                host_id,
                report: session_report,
                decode,
                ..
            } = active;
            session.stop();
            {
                let mut r = report::lock(&session_report);
                r.field("Decoder totals", "decoded", decode.decoded.load(Ordering::Relaxed));
                r.field("Decoder totals", "dropped", decode.dropped.load(Ordering::Relaxed));
                r.field("Decoder totals", "errors", decode.errors.load(Ordering::Relaxed));
                r.field(
                    "Decoder totals",
                    "mean decode",
                    format!("{:.2} ms", decode.mean_decode_ms()),
                );
            }
            let outcome = match (&reason, quit) {
                (Some(reason), _) => reason.clone(),
                (None, true) => "quit by the user".to_owned(),
                (None, false) => "disconnected by the user".to_owned(),
            };
            let show_report = c.finish_report(session_report, &outcome, Some(host_id.clone()));
            c.ui(move |ui| {
                if ui.get_stream_fullscreen() {
                    ui.window().set_fullscreen(false);
                    ui.set_stream_fullscreen(false);
                }
                if ui.get_screen() == 4 {
                    ui.set_screen(if show_report { 6 } else { 1 });
                }
                ui.set_stream_frame(Image::default());
                ui.set_stream_stats(SharedString::new());
                ui.set_stream_keyboard_open(false);
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

    /// `kind`: 0 down, 1 up, 2 move, 3 cancel. Touch goes through the
    /// trackpad when it is enabled; mice (and direct touch) position the
    /// host cursor absolutely inside the video.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stream_pointer(
        self: &Shared,
        kind: i32,
        button: i32,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        touch: bool,
    ) {
        if touch && self.trackpad.load(Ordering::Relaxed) {
            return self.trackpad_event(kind, x, y, width);
        }
        if width < 1.0 || height < 1.0 {
            return;
        }
        let clamp = |v: f32, max: f32| v.clamp(0.0, max - 1.0).round() as i16;
        let mut events = vec![InputEvent::MousePosition {
            x: clamp(x, width),
            y: clamp(y, height),
            width: width.round().min(i16::MAX as f32) as i16,
            height: height.round().min(i16::MAX as f32) as i16,
        }];
        let button = match button {
            1 => Some(MouseButton::Left),
            2 => Some(MouseButton::Middle),
            3 => Some(MouseButton::Right),
            _ => None,
        };
        // Touch reports no button; it is the left one.
        let button = button.or(touch.then_some(MouseButton::Left));
        if let (Some(button), 0 | 1) = (button, kind) {
            events.push(InputEvent::MouseButton {
                button,
                pressed: kind == 0,
            });
        }
        if let Some(active) = self.active_stream().as_ref() {
            active.send(events);
        }
    }

    fn trackpad_event(self: &Shared, kind: i32, x: f32, y: f32, video_width: f32) {
        let now = Instant::now();
        let mut guard = self.active_stream();
        let Some(active) = guard.as_mut() else { return };
        let outputs = match kind {
            0 => {
                active.touch_id += 1;
                let (generation, touch_id) = (active.generation, active.touch_id);
                let c = self.clone();
                slint::Timer::single_shot(touch::LONG_PRESS, move || c.trackpad_long_press(generation, touch_id));
                active.trackpad.down(x, y, now)
            }
            1 => active.trackpad.up(now),
            2 => {
                let scale = TRACKPAD_SPEED * active.width as f32 / video_width.max(1.0);
                active.trackpad.moved(x, y, scale)
            }
            _ => active.trackpad.cancel(),
        };
        active.send(trackpad_events(outputs));
    }

    fn trackpad_long_press(&self, generation: u64, touch_id: u64) {
        let mut guard = self.active_stream();
        let Some(active) = guard.as_mut() else { return };
        if active.generation == generation && active.touch_id == touch_id {
            let outputs = active.trackpad.long_press(Instant::now());
            active.send(trackpad_events(outputs));
        }
    }

    pub(super) fn toggle_trackpad(self: &Shared) {
        let on = !self.trackpad.load(Ordering::Relaxed);
        self.trackpad.store(on, Ordering::Relaxed);
        if let Some(active) = self.active_stream().as_mut() {
            let released = active.trackpad.cancel();
            active.send(trackpad_events(released));
        }
        self.ui(move |ui| ui.set_stream_trackpad(on));
        self.toast(if on {
            "Touch: trackpad. Tap to click, hold to right-click, tap then drag to drag."
        } else {
            "Touch: direct. The cursor goes where you touch."
        });
        let Some(core) = self.core() else { return };
        spawn("hestia-settings", move || {
            let mut settings = core.settings();
            settings.touch_trackpad = on;
            if let Err(e) = core.set_settings(settings) {
                warn!("saving the touch mode failed: {e}");
            }
        });
    }

    pub(super) fn stream_text_edited(&self, text: &str) {
        let mut guard = self.active_stream();
        let Some(active) = guard.as_mut() else { return };
        let events = typing::edit_events(&active.typed, text);
        active.typed = text.to_owned();
        active.send(events);
    }

    pub(super) fn stream_text_enter(&self) {
        let mut guard = self.active_stream();
        let Some(active) = guard.as_mut() else { return };
        active.typed.clear();
        active.send(typing::special_key_events("enter"));
    }

    pub(super) fn stream_special_key(&self, name: &str) {
        if let Some(active) = self.active_stream().as_ref() {
            active.send(typing::special_key_events(name));
        }
    }

    pub(super) fn request_keyframe(self: &Shared) {
        if let Some(active) = self.active_stream().as_ref() {
            active.session.request_keyframe();
        }
        self.toast("Asked the host for a fresh frame");
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
