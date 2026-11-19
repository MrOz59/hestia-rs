//! The stream screen: runs a [`ManagedStream`] after a launch, shows decoded
//! frames and forwards keyboard, mouse and touch input to the host.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_core::TouchMode;
use hestia_core::diagnosis::{self, Bottleneck, SpikeHistory};
use hestia_core::report::StatsSample;
use hestia_core::stream::input::{Modifiers, MouseButton, TouchPhase};
use hestia_core::stream::{InputEvent, SessionListener, StreamEvent, StreamStats};
use hestia_core::{LaunchedSession, ManagedStream, StreamHost};
use hestia_media::{DecodeStats, DecoderChoice, DecodingSink, FramePresenter, LatencyHistogram, RgbaFrame};
use log::{info, warn};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};

use super::report::{self, SharedReport};
use super::{Controller, Shared, spawn};
use crate::AppWindow;
use crate::keymap::{self, Mapped};
use crate::touch::{self, Trackpad};
use crate::typing;
use crate::virtual_pad::{PadState, VirtualPad};
use crate::{hostinfo, platform};

/// Wheel units (120 per notch) per logical pixel of two-finger drag.
const TOUCH_SCROLL_SPEED: f32 = 2.0;
/// Trackpad speed on top of the on-screen → host pixel ratio.
const TRACKPAD_SPEED: f32 = 1.4;
/// Most a pinch magnifies the stream.
const MAX_ZOOM: f32 = 4.0;

/// Where the picture is on screen (logical px), from a pointer event.
#[derive(Debug, Clone, Copy)]
struct Picture {
    centre: (f32, f32),
    size: (f32, f32),
}

/// Where a pinch started.
#[derive(Debug, Clone, Copy)]
struct PinchStart {
    /// Finger distance and midpoint.
    distance: f32,
    mid: (f32, f32),
    zoom: f32,
    pan: (f32, f32),
    /// The picture's centre, and its size unzoomed.
    centre: (f32, f32),
    size: (f32, f32),
}

/// Pinch-to-zoom state (Artemis' pan/zoom).
#[derive(Debug, Clone, Copy)]
struct Zoom {
    /// Zoom mode on: two fingers pinch and pan instead of scrolling.
    on: bool,
    factor: f32,
    pan: (f32, f32),
    pinch: Option<PinchStart>,
}

impl Default for Zoom {
    fn default() -> Self {
        Self {
            on: false,
            factor: 1.0,
            pan: (0.0, 0.0),
            pinch: None,
        }
    }
}

pub(super) struct ActiveStream {
    generation: u64,
    session: ManagedStream,
    host_id: String,
    /// Stream resolution, for trackpad scaling.
    width: u32,
    trackpad: Trackpad,
    /// Identifies the current touch, so a stale long-press timer is ignored.
    touch_id: u64,
    /// Multi-finger taps (2: right click, 3: keyboard).
    fingers: touch::FingerTaps,
    /// On-screen controller (drawn when `pad_visible`).
    pad: VirtualPad,
    pad_visible: bool,
    /// The stream view's size (logical px).
    view: (f32, f32),
    zoom: Zoom,
    report: SharedReport,
    decode: Arc<DecodeStats>,
    present: Arc<PresentStats>,
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

/// Counts the handoff to Slint, where physical presentation is not observed.
#[derive(Default)]
struct PresentStats {
    delivered: AtomicU64,
    superseded: AtomicU64,
    closed: AtomicBool,
    handoff: LatencyHistogram,
    receive_to_ui: LatencyHistogram,
}

#[derive(Default)]
struct PendingPicture {
    latest: Option<RgbaFrame>,
    spare: Option<RgbaFrame>,
    scheduled: bool,
}

struct UiPresenter {
    weak: slint::Weak<AppWindow>,
    pending: Arc<Mutex<PendingPicture>>,
    stats: Arc<PresentStats>,
}

impl FramePresenter for UiPresenter {
    fn present(&mut self, frame: &mut RgbaFrame) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if self.stats.closed.load(Ordering::Acquire) {
            return;
        }
        let reusable = if let Some(older) = pending.latest.take() {
            self.stats.superseded.fetch_add(1, Ordering::Relaxed);
            older
        } else {
            pending.spare.take().unwrap_or_default()
        };
        pending.latest = Some(std::mem::replace(frame, reusable));
        if pending.scheduled {
            return;
        }
        pending.scheduled = true;
        drop(pending);

        let pending = self.pending.clone();
        let stats = self.stats.clone();
        let _ = self.weak.upgrade_in_event_loop(move |ui| {
            let frame = {
                let mut pending = pending.lock().unwrap_or_else(|p| p.into_inner());
                pending.scheduled = false;
                pending.latest.take()
            };
            if let Some(frame) = frame
                && !stats.closed.load(Ordering::Acquire)
            {
                // Copy only a frame the UI will actually hand to its renderer.
                let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&frame.data, frame.width, frame.height);
                ui.set_stream_frame(Image::from_rgba8(buffer));
                let delivered_at = Instant::now();
                stats.handoff.record(delivered_at.duration_since(frame.decoded_at));
                stats
                    .receive_to_ui
                    .record(delivered_at.duration_since(frame.received_at));
                if stats.delivered.fetch_add(1, Ordering::Relaxed) == 0 {
                    ui.set_stream_status(SharedString::new());
                }
                pending.lock().unwrap_or_else(|p| p.into_inner()).spare = Some(frame);
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
            // Frames the screen skipped. The decoder's own drops (it fell
            // behind) show up as decoded < received, a decode problem.
            display_drop_pct: percent(superseded, decoded),
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
            StreamEvent::Rumble { gamepad, low, high } => platform::rumble(gamepad, low, high),
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
        host: StreamHost,
        (decoder, formats): DecoderChoice,
        session_report: SharedReport,
        launch_seq: u64,
    ) -> Result<(), String> {
        let config = launched.stream_config(&formats).map_err(|e| e.to_string())?;
        hestia_core::add_stream_config_to_report(&config, decoder.name(), &mut report::lock(&session_report));
        let generation = self.stream_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let present = Arc::new(PresentStats::default());
        let sink = DecodingSink::new(
            decoder,
            Box::new(UiPresenter {
                weak,
                pending: Arc::new(Mutex::new(PendingPicture::default())),
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
            present: present.clone(),
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
        self.ui(|ui| {
            ui.set_stream_frame(Image::default());
            ui.set_stream_status("Waiting for video…".into());
        });
        let session = host
            .connect(
                config,
                Box::new(sink),
                hestia_media::default_audio_sink(),
                listener.clone(),
            )
            .map_err(|e| {
                present.closed.store(true, Ordering::Release);
                e.to_string()
            })?;
        if !self.launch_current(launch_seq) {
            present.closed.store(true, Ordering::Release);
            session.stop();
            return Err("cancelled".into());
        }
        let codec = format!("{:?}", session.negotiated().video_format.codec);
        info!("streaming {size} {codec} from {}", launched.host.name);
        *listener.video.lock().unwrap_or_else(|p| p.into_inner()) = format!("{size}@{} {codec}", launched.request.fps);
        hestia_core::add_negotiated_to_report(session.negotiated(), &mut report::lock(&session_report));

        // A stream replacing another keeps the laid-out view (no new layout
        // event comes for it).
        let view = self.active_stream().as_ref().map_or((0.0, 0.0), |a| a.view);
        let mut pad = VirtualPad::default();
        pad.resize(view.0, view.1);
        let previous = self.active_stream().replace(ActiveStream {
            generation,
            session,
            host_id,
            width: stream_width,
            trackpad: Trackpad::new(),
            touch_id: 0,
            fingers: touch::FingerTaps::default(),
            pad,
            pad_visible: launched.settings.onscreen_controller,
            view,
            zoom: Zoom::default(),
            report: session_report,
            decode,
            present,
        });
        if let Some(previous) = previous {
            previous.present.closed.store(true, Ordering::Release);
            spawn("hestia-stream-stop", move || previous.session.stop());
        }
        let pad = launched.settings.onscreen_controller;
        self.ui(move |ui| {
            ui.set_stream_stats(format!("{size} {codec}").into());
            ui.set_stream_pad_visible(pad);
            ui.set_stream_zoom_mode(false);
            ui.set_stream_zoom(1.0);
            ui.set_stream_pan_x(0.0);
            ui.set_stream_pan_y(0.0);
            ui.set_screen(4);
        });
        // Orientation, no system bars, screen on (Android); controllers on.
        platform::set_stream_mode(true, launched.settings.orientation);
        platform::set_gamepads_streaming(true);
        if pad && view.0 > 0.0 {
            self.push_pad_view();
        }
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
        active.present.closed.store(true, Ordering::Release);
        let c = self.clone();
        spawn("hestia-stream-stop", move || {
            let ActiveStream {
                session,
                host_id,
                report: session_report,
                decode,
                present,
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
                    "backlog drops",
                    decode.dropped_backlog.load(Ordering::Relaxed),
                );
                r.field(
                    "Video delivery",
                    "handed to Slint",
                    present.delivered.load(Ordering::Relaxed),
                );
                r.field(
                    "Video delivery",
                    "replaced before handoff",
                    present.superseded.load(Ordering::Relaxed),
                );
                for (stage, histogram) in decode.latency_stages().into_iter().chain([
                    ("RGBA ready to Slint handoff", &present.handoff),
                    ("first packet to Slint handoff", &present.receive_to_ui),
                ]) {
                    if let Some(summary) = histogram.summary() {
                        r.field("Video latency", stage, summary);
                    }
                }
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
            platform::set_keyboard_visible(false);
            platform::set_stream_mode(false, hestia_core::StreamOrientation::default());
            platform::set_gamepads_streaming(false);
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

    /// `kind`: 0 down, 1 up, 2 move, 3 cancel. `(x, y)` is relative to the
    /// video, `size` its size, `screen` the point in the stream view. Touch
    /// goes to the on-screen controller when it lands on it; otherwise it
    /// follows the touch mode (trackpad, mouse or real touches). Mice
    /// position the host cursor absolutely inside the video.
    pub(super) fn stream_pointer(
        self: &Shared,
        kind: i32,
        button: i32,
        (x, y): (f32, f32),
        (width, height): (f32, f32),
        finger: i32,
        screen: (f32, f32),
    ) {
        let touch = finger != 0;
        if touch && self.pad_event(kind, finger, screen) {
            return;
        }
        let mode = self.touch_mode();
        if touch && mode == TouchMode::Touchscreen {
            return self.native_touch(kind, finger, (x, y), (width, height));
        }
        let picture = Picture {
            centre: (screen.0 - x + width / 2.0, screen.1 - y + height / 2.0),
            size: (width, height),
        };
        if touch && self.multi_finger(kind, finger, screen, picture) {
            return; // part of a multi-finger gesture, not a pointer action
        }
        if touch && mode == TouchMode::Trackpad {
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

    /// Tracks every finger for multi-finger taps. True when this event
    /// belongs to a gesture with more than one finger (the trackpad and
    /// direct touch only follow the first finger, and drop it once a
    /// second one lands).
    fn multi_finger(self: &Shared, kind: i32, finger: i32, (x, y): (f32, f32), picture: Picture) -> bool {
        let now = Instant::now();
        let mut guard = self.active_stream();
        let Some(active) = guard.as_mut() else { return false };
        match kind {
            0 => {
                let was_multi = active.fingers.multi();
                active.fingers.down(finger, x, y, now);
                if active.fingers.multi() && !was_multi {
                    // The first finger's tap/drag/long press is off.
                    let released = active.trackpad.cancel();
                    active.touch_id += 1;
                    active.send(trackpad_events(released));
                }
                active.fingers.multi()
            }
            2 => {
                let (dx, dy) = active.fingers.moved(finger, x, y);
                if active.zoom.on && active.fingers.count() == 2 {
                    let update = Self::pinch(active, picture);
                    drop(guard);
                    if let Some((factor, (px, py))) = update {
                        self.ui(move |ui| {
                            ui.set_stream_zoom(factor);
                            ui.set_stream_pan_x(px);
                            ui.set_stream_pan_y(py);
                        });
                    }
                    return true;
                }
                // Two fingers dragging scroll the host, content following
                // the fingers (as on the phone); the first finger leads.
                if active.fingers.count() == 2 && active.fingers.primary() == Some(finger) {
                    let amount = |v: f32| (v * TOUCH_SCROLL_SPEED).round().clamp(-32767.0, 32767.0) as i16;
                    let (horizontal, vertical) = (amount(-dx), amount(dy));
                    if horizontal != 0 || vertical != 0 {
                        active.send([InputEvent::Scroll { vertical, horizontal }]);
                    }
                }
                active.fingers.multi() || active.fingers.primary() != Some(finger)
            }
            1 | 3 => {
                active.zoom.pinch = None;
                let secondary = active.fingers.multi() || active.fingers.primary() != Some(finger);
                let tap = if kind == 1 {
                    active.fingers.up(finger, now)
                } else {
                    active.fingers.reset();
                    None
                };
                match tap {
                    Some(2) => {
                        active.send([
                            InputEvent::MouseButton {
                                button: MouseButton::Right,
                                pressed: true,
                            },
                            InputEvent::MouseButton {
                                button: MouseButton::Right,
                                pressed: false,
                            },
                        ]);
                    }
                    Some(3..) => {
                        drop(guard);
                        self.ui(|ui| ui.set_stream_keyboard_open(!ui.get_stream_keyboard_open()));
                        return true;
                    }
                    _ => {}
                }
                secondary || tap.is_some()
            }
            _ => false,
        }
    }

    /// Pinch-zoom from the first two fingers: zooms about their midpoint
    /// and pans with it. Returns the new zoom and pan when they changed.
    fn pinch(active: &mut ActiveStream, picture: Picture) -> Option<(f32, (f32, f32))> {
        let ((ax, ay), (bx, by)) = active.fingers.pair()?;
        let distance = (bx - ax).hypot(by - ay).max(1.0);
        let mid = ((ax + bx) / 2.0, (ay + by) / 2.0);
        let z = &mut active.zoom;
        let start = *z.pinch.get_or_insert(PinchStart {
            distance,
            mid,
            zoom: z.factor,
            pan: z.pan,
            centre: picture.centre,
            size: (picture.size.0 / z.factor, picture.size.1 / z.factor),
        });
        let factor = (start.zoom * distance / start.distance).clamp(1.0, MAX_ZOOM);
        // The picture point under the fingers (unzoomed px from its centre)
        // stays under them; the centre moves with the pan.
        let anchor = (
            (start.mid.0 - start.centre.0) / start.zoom,
            (start.mid.1 - start.centre.1) / start.zoom,
        );
        let base = (start.centre.0 - start.pan.0, start.centre.1 - start.pan.1);
        // The zoomed picture always covers where it was unzoomed.
        let limit = |size: f32| size * (factor - 1.0) / 2.0;
        let pan = (
            (mid.0 - anchor.0 * factor - base.0).clamp(-limit(start.size.0), limit(start.size.0)),
            (mid.1 - anchor.1 * factor - base.1).clamp(-limit(start.size.1), limit(start.size.1)),
        );
        if (factor, pan) == (z.factor, z.pan) {
            return None;
        }
        (z.factor, z.pan) = (factor, pan);
        Some((factor, pan))
    }

    /// Menu → Zoom: two fingers pinch and pan; turning it off resets.
    pub(super) fn toggle_zoom(self: &Shared) {
        let on = {
            let mut guard = self.active_stream();
            let Some(active) = guard.as_mut() else { return };
            active.zoom = Zoom {
                on: !active.zoom.on,
                ..Zoom::default()
            };
            active.zoom.on
        };
        self.ui(move |ui| {
            ui.set_stream_zoom_mode(on);
            ui.set_stream_zoom(1.0);
            ui.set_stream_pan_x(0.0);
            ui.set_stream_pan_y(0.0);
        });
        if on {
            self.toast("Zoom: pinch to zoom, drag with two fingers to move around");
        }
    }

    fn touch_mode(&self) -> TouchMode {
        TouchMode::ALL
            .get(usize::from(self.touch_mode.load(Ordering::Relaxed)))
            .copied()
            .unwrap_or_default()
    }

    /// Touchscreen mode: every finger goes to the host as a touch, at its
    /// place in the video (Sunshine/Hermes touch events).
    fn native_touch(&self, kind: i32, finger: i32, (x, y): (f32, f32), (w, h): (f32, f32)) {
        if w < 1.0 || h < 1.0 {
            return;
        }
        let phase = match kind {
            0 => TouchPhase::Down,
            1 => TouchPhase::Up,
            2 => TouchPhase::Move,
            _ => TouchPhase::Cancel,
        };
        self.send_input(InputEvent::Touch {
            pointer_id: finger as u32,
            phase,
            x: (x / w).clamp(0.0, 1.0),
            y: (y / h).clamp(0.0, 1.0),
            pressure: if kind == 1 { 0.0 } else { 1.0 },
        });
    }

    /// Touches on the on-screen controller; true when the pad took it.
    fn pad_event(self: &Shared, kind: i32, finger: i32, (x, y): (f32, f32)) -> bool {
        let now = Instant::now();
        let (state, click) = {
            let mut guard = self.active_stream();
            let Some(active) = guard.as_mut() else { return false };
            if !active.pad_visible {
                return false;
            }
            let took = match kind {
                0 => active.pad.down(finger, x, y, now),
                2 if active.pad.owns(finger) => {
                    active.pad.moved(finger, x, y);
                    true
                }
                1 | 3 if active.pad.owns(finger) => {
                    active.pad.up(finger, now);
                    true
                }
                _ => false,
            };
            if !took {
                return false;
            }
            (active.pad.state(), active.pad.take_click())
        };
        Self::send_pad(state, click);
        if click != 0 {
            // A stick tap clicks it: release shortly after.
            let c = self.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(60), move || {
                if let Some(active) = c.active_stream().as_ref() {
                    Self::send_pad(active.pad.state(), 0);
                }
            });
        }
        self.push_pad_view();
        true
    }

    fn send_pad(state: PadState, extra_buttons: u32) {
        platform::onscreen_gamepad(
            state.buttons | extra_buttons,
            state.left_stick,
            state.right_stick,
            (state.left_trigger, state.right_trigger),
        );
    }

    /// Redraws the on-screen controller (layout, what's held, stick knobs).
    fn push_pad_view(self: &Shared) {
        let controls = {
            let guard = self.active_stream();
            let Some(active) = guard.as_ref() else { return };
            crate::pad_controls(&active.pad)
        };
        self.ui(move |ui| ui.set_stream_pad_controls(ModelRc::new(VecModel::from(controls))));
    }

    /// Menu → On-screen controller (remembered for next time).
    pub(super) fn toggle_pad(self: &Shared) {
        let on = {
            let mut guard = self.active_stream();
            let Some(active) = guard.as_mut() else { return };
            active.pad_visible = !active.pad_visible;
            let (w, h) = active.view;
            active.pad.resize(w, h);
            active.pad_visible
        };
        if on {
            self.push_pad_view();
        } else {
            platform::onscreen_gamepad_off();
        }
        self.ui(move |ui| ui.set_stream_pad_visible(on));
        let Some(core) = self.core() else { return };
        spawn("hestia-settings", move || {
            let mut settings = core.settings();
            settings.onscreen_controller = on;
            if let Err(e) = core.set_settings(settings) {
                warn!("saving the controller setting failed: {e}");
            }
        });
    }

    /// The stream view was laid out (or resized): re-lay the controller.
    pub(super) fn stream_view_size(self: &Shared, width: f32, height: f32) {
        let visible = {
            let mut guard = self.active_stream();
            let Some(active) = guard.as_mut() else { return };
            if active.view == (width, height) {
                return;
            }
            active.view = (width, height);
            active.pad.resize(width, height);
            active.pad_visible
        };
        if visible {
            self.push_pad_view();
        }
    }

    /// A game controller's new state, for the host.
    pub(super) fn stream_gamepad(&self, state: hestia_core::stream::input::GamepadState) {
        if let Some(active) = self.active_stream().as_ref() {
            active.send([InputEvent::Gamepad(state)]);
        }
    }

    /// Text and deletions from the on-screen keyboard, typed on the host.
    pub(super) fn stream_ime(self: &Shared, event: platform::ImeEvent) {
        let guard = self.active_stream();
        let Some(active) = guard.as_ref() else { return };
        match event {
            platform::ImeEvent::Text(text) => active.send(typing::edit_events("", &text)),
            platform::ImeEvent::DeleteBefore(count) => {
                for _ in 0..count.min(256) {
                    active.send(typing::special_key_events("backspace"));
                }
            }
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

    /// Menu → Touch: trackpad → mouse → touchscreen (remembered).
    pub(super) fn cycle_touch_mode(self: &Shared) {
        let mode = self.touch_mode().next();
        self.touch_mode.store(mode.index() as u8, Ordering::Relaxed);
        if let Some(active) = self.active_stream().as_mut() {
            let released = active.trackpad.cancel();
            active.send(trackpad_events(released));
            active.fingers.reset();
        }
        self.ui(move |ui| ui.set_stream_touch_mode(mode.index()));
        self.toast(match mode {
            TouchMode::Trackpad => "Touch: trackpad. Tap to click, hold to right-click, tap then drag to drag.",
            TouchMode::Mouse => "Touch: mouse. The cursor goes where you touch.",
            TouchMode::Touchscreen => "Touch: touchscreen. Every finger reaches the host as a real touch.",
        });
        let Some(core) = self.core() else { return };
        spawn("hestia-settings", move || {
            let mut settings = core.settings();
            settings.set_touch(mode);
            if let Err(e) = core.set_settings(settings) {
                warn!("saving the touch mode failed: {e}");
            }
        });
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
