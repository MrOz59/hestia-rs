//! The stream screen: runs a [`StreamSession`] after a launch, shows decoded
//! frames and forwards keyboard, mouse and touch input to the host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_core::LaunchedSession;
use hestia_core::diagnosis::{self, Bottleneck, SpikeHistory};
use hestia_core::report::StatsSample;
use hestia_core::stream::input::{Modifiers, MouseButton};
use hestia_core::stream::{InputEvent, SessionListener, StreamEvent, StreamSession, StreamStats};
use hestia_media::{Colorimetry, DecodeStats, DecodingSink, FramePresenter, RgbaFrame};
use log::{info, warn};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use super::report::{self, SharedReport};
use super::{Controller, Shared, spawn};
use crate::AppWindow;
use crate::hostinfo;
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
/// What the screen actually showed: decoded pictures that reached the
/// window, and those replaced by a newer one before the UI drew them.
#[derive(Default)]
struct PresentStats {
    presented: std::sync::atomic::AtomicU64,
    superseded: std::sync::atomic::AtomicU64,
}

struct UiPresenter {
    weak: slint::Weak<AppWindow>,
    slot: Arc<Mutex<Option<SharedPixelBuffer<Rgba8Pixel>>>>,
    scheduled: Arc<AtomicBool>,
    first: bool,
    stats: Arc<PresentStats>,
}

impl FramePresenter for UiPresenter {
    fn present(&mut self, frame: &RgbaFrame) {
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&frame.data, frame.width, frame.height);
        if self
            .slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(buffer)
            .is_some()
        {
            self.stats.superseded.fetch_add(1, Ordering::Relaxed);
        }
        let first = std::mem::take(&mut self.first);
        if self.scheduled.swap(true, Ordering::AcqRel) && !first {
            return;
        }
        let slot = self.slot.clone();
        let scheduled = self.scheduled.clone();
        let stats = self.stats.clone();
        let _ = self.weak.upgrade_in_event_loop(move |ui| {
            scheduled.store(false, Ordering::Release);
            if let Some(buffer) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                ui.set_stream_frame(Image::from_rgba8(buffer));
                stats.presented.fetch_add(1, Ordering::Relaxed);
            }
            if first {
                ui.set_stream_status(SharedString::new());
            }
        });
    }
}

#[derive(Clone)]
struct Totals {
    at: Instant,
    stats: StreamStats,
    decoded: u64,
    decode_us: u64,
    decoder_dropped: u64,
    superseded: u64,
}

struct Listener {
    controller: Shared,
    generation: u64,
    decode: Arc<DecodeStats>,
    /// "1920×1080 H264" etc.
    label: String,
    last: Mutex<Option<Totals>>,
    report: SharedReport,
    present: Arc<PresentStats>,
    target_fps: u32,
    spikes: Mutex<SpikeHistory>,
    host_id: String,
    /// Hermes host: ask it how it serves the stream once frames flow.
    hermes: bool,
    /// "1920×1080@60 H264", known once the handshake is done.
    video: Mutex<String>,
    first_frame: AtomicBool,
    provenance_requested: AtomicBool,
}

impl Listener {
    /// Once frames flow and the RTT has been measured, asks Hermes how it
    /// serves the stream (capture path, encoder, GPU) and shows it briefly.
    fn request_provenance(&self, rtt_ms: u32) {
        if !self.hermes || self.provenance_requested.swap(true, Ordering::Relaxed) {
            return;
        }
        let c = self.controller.clone();
        let host_id = self.host_id.clone();
        let report = self.report.clone();
        let video = self.video.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let generation = self.generation;
        spawn("hestia-stream-info", move || {
            let Some(core) = c.core() else { return };
            let info = match core.with_api(&host_id, |api| api.stream_info()) {
                Ok(Ok(info)) => info,
                Ok(Err(e)) => return log::debug!("Hermes stream info unavailable: {e}"),
                Err(e) => return log::debug!("Hermes stream info unavailable: {e}"),
            };
            {
                let mut r = report::lock(&report);
                r.field("Hermes stream", "capture check", if info.ok { "ok" } else { "FAILED" });
                r.field("Hermes stream", "capture", &info.capture);
                r.field("Hermes stream", "encoder", info.encoder_name());
                r.field("Hermes stream", "GPU", info.gpu_name());
                r.field("Hermes stream", "frame seen", info.frame_seen);
                if !info.failure.is_empty() {
                    r.field("Hermes stream", "failure", &info.failure);
                }
            }
            let text = hostinfo::stream_banner(&info, &video, Some(rtt_ms).filter(|&r| r > 0));
            info!("Hermes stream: {text}");
            c.show_stream_banner(generation, text);
        });
    }
}

impl Listener {
    /// Per-second rates from the running totals, with the bottleneck
    /// verdict for that second.
    fn sample(&self, stats: StreamStats) -> Option<(StatsSample, diagnosis::Diagnosis)> {
        let now = Totals {
            at: Instant::now(),
            stats,
            decoded: self.decode.decoded.load(Ordering::Relaxed),
            decode_us: self.decode.decode_time_us.load(Ordering::Relaxed),
            decoder_dropped: self.decode.dropped.load(Ordering::Relaxed),
            superseded: self.present.superseded.load(Ordering::Relaxed),
        };
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        let prev = last.replace(now.clone())?;
        let secs = prev.at.elapsed().as_secs_f64().max(0.001);
        let delta = |a: u64, b: u64| a.saturating_sub(b);
        let received = delta(now.stats.frames, prev.stats.frames);
        let lost = delta(now.stats.lost_frames, prev.stats.lost_frames);
        let decoded = delta(now.decoded, prev.decoded);
        let decoder_dropped = delta(now.decoder_dropped, prev.decoder_dropped);
        let superseded = delta(now.superseded, prev.superseded);
        let host_frames = delta(now.stats.host_latency_frames, prev.stats.host_latency_frames);
        let host_ms = (host_frames > 0)
            .then(|| delta(now.stats.host_latency_us, prev.stats.host_latency_us) as f64 / host_frames as f64 / 1000.0);
        let percent = |part: u64, whole: u64| {
            if whole == 0 {
                0.0
            } else {
                part as f64 * 100.0 / whole as f64
            }
        };
        let window = diagnosis::Window {
            target_fps: self.target_fps,
            received_fps: received as f64 / secs,
            decoded_fps: decoded as f64 / secs,
            decode_ms: if decoded == 0 {
                0.0
            } else {
                delta(now.decode_us, prev.decode_us) as f64 / decoded as f64 / 1000.0
            },
            network_drop_pct: percent(lost, received + lost),
            display_drop_pct: percent(superseded + decoder_dropped, decoded + decoder_dropped),
            host_ms,
        };
        let verdict = diagnosis::diagnose(&window);
        let sample = StatsSample {
            fps: window.received_fps,
            mbps: delta(now.stats.bytes, prev.stats.bytes) as f64 * 8.0 / secs / 1e6,
            rtt_ms: now.stats.rtt_ms,
            rtt_variance_ms: now.stats.rtt_variance_ms,
            fec_recovered: now.stats.recovered_packets,
            lost_frames: now.stats.lost_frames,
            decoded_fps: Some(window.decoded_fps),
            decode_ms: Some(window.decode_ms),
            dropped: Some(now.decoder_dropped + now.superseded),
            host_ms,
            verdict: match verdict.bottleneck {
                Bottleneck::None => None,
                other => Some(other.label()),
            },
        };
        Some((sample, verdict))
    }

    fn stats_line(&self, s: &StatsSample, verdict: &diagnosis::Diagnosis) -> String {
        let mut line = format!(
            "{}  ·  {:.0} fps  ·  {:.1} Mbps  ·  RTT {} ms  ·  decode {:.1} ms{}  ·  FEC {}  ·  lost {}  ·  dropped {}",
            self.label,
            s.decoded_fps.unwrap_or(s.fps),
            s.mbps,
            s.rtt_ms,
            s.decode_ms.unwrap_or(0.0),
            s.host_ms.map(|h| format!("  ·  host {h:.1} ms")).unwrap_or_default(),
            s.fec_recovered,
            s.lost_frames,
            s.dropped.unwrap_or(0),
        );
        let mut spikes = self.spikes.lock().unwrap_or_else(|p| p.into_inner());
        spikes.record(verdict.bottleneck);
        if verdict.bottleneck != Bottleneck::None {
            line += &format!("\n⚠ {} ({})", verdict.summary, verdict.key_metric);
        }
        if let Some(history) = spikes.summarize() {
            line += &format!("\n{history}");
        }
        line
    }
}

impl SessionListener for Listener {
    fn on_event(&self, event: StreamEvent) {
        match event {
            StreamEvent::Stats(stats) => {
                if self.first_frame.load(Ordering::Relaxed) {
                    self.request_provenance(stats.rtt_ms);
                }
                if let Some((sample, verdict)) = self.sample(stats) {
                    let line = self.stats_line(&sample, &verdict);
                    report::lock(&self.report).sample(sample);
                    self.controller.ui(move |ui| ui.set_stream_stats(line.into()));
                }
            }
            StreamEvent::FirstFrame => {
                self.first_frame.store(true, Ordering::Relaxed);
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
        launch_seq: u64,
    ) -> Result<(), String> {
        let (decoder, formats) = hestia_media::default_decoder(Colorimetry::default())?;
        let config = launched.stream_config(&formats).map_err(|e| e.to_string())?;
        hestia_core::add_stream_config_to_report(&config, decoder.name(), &mut report::lock(&session_report));
        let generation = self.stream_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let present = Arc::new(PresentStats::default());
        let sink = DecodingSink::new(
            decoder,
            Box::new(UiPresenter {
                weak,
                slot: Arc::new(Mutex::new(None)),
                scheduled: Arc::new(AtomicBool::new(false)),
                first: true,
                stats: present.clone(),
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
            present,
            target_fps: config.fps,
            spikes: Mutex::new(SpikeHistory::default()),
            host_id: host_id.clone(),
            hermes: self
                .state()
                .snapshots
                .get(&host_id)
                .is_some_and(|s| s.capabilities.is_some()),
            video: Mutex::new(size.clone()),
            first_frame: AtomicBool::new(false),
            provenance_requested: AtomicBool::new(false),
        });
        let session = StreamSession::start(
            config,
            Box::new(sink),
            hestia_media::default_audio_sink(),
            listener.clone(),
        )
        .map_err(|e| e.to_string())?;
        if !self.launch_current(launch_seq) {
            spawn("hestia-stream-stop", move || session.stop());
            return Err("cancelled".into());
        }
        let codec = format!("{:?}", session.negotiated().video_format.codec);
        info!("streaming {size} {codec} from {}", launched.host.name);
        *listener.video.lock().unwrap_or_else(|p| p.into_inner()) = format!("{size}@{} {codec}", launched.request.fps);
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
            // Like Hestia desktop: let Hermes release this client's session
            // (virtual display, Gamescope) before the GameStream teardown.
            let hermes = c
                .state()
                .snapshots
                .get(&host_id)
                .is_some_and(|s| s.capabilities.is_some());
            if hermes {
                if let Some(core) = c.core() {
                    core.end_hermes_session(&host_id);
                }
                report::lock(&session_report).event("Hermes session released");
            }
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
                ui.set_stream_banner(SharedString::new());
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

    /// Shows `text` at the bottom of the stream for a few seconds.
    fn show_stream_banner(self: &Shared, generation: u64, text: String) {
        if self.active_stream().as_ref().is_none_or(|a| a.generation != generation) {
            return;
        }
        self.ui(move |ui| {
            let shown: SharedString = text.into();
            ui.set_stream_banner(shown.clone());
            let weak = ui.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_secs(8), move || {
                if let Some(ui) = weak.upgrade() {
                    if ui.get_stream_banner() == shown {
                        ui.set_stream_banner(SharedString::new());
                    }
                }
            });
        });
    }
}
