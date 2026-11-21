//! The stream screen: runs a [`ManagedStream`] after a launch, shows decoded
//! frames and forwards keyboard, mouse and touch input to the host.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_core::TouchMode;
use hestia_core::diagnosis::{self, Bottleneck, SpikeHistory};
use hestia_core::report::StatsSample;
use hestia_core::stream::input::Modifiers;
use hestia_core::stream::{InputEvent, SessionListener, StreamEvent, StreamStats};
use hestia_core::{LaunchedSession, ManagedStream, StreamHost};
use hestia_media::{DecodeStats, DecodingSink};
use log::{info, warn};
use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel};

use super::report::{self, SharedReport};
use super::{Controller, Shared, spawn};
use crate::presentation::{Presentation, UiPresenter};
use crate::stream_input::{self, GestureAction, ImeEvent, Picture, StreamInput};
use crate::typing;
use crate::video_backend::StreamVideo;
use crate::virtual_pad::PadState;
use crate::{hostinfo, platform};

pub(super) struct ActiveStream {
    generation: u64,
    session: ManagedStream,
    host_id: String,
    input: StreamInput,
    report: SharedReport,
    decode: Arc<DecodeStats>,
    present: Arc<Presentation>,
}

impl ActiveStream {
    fn send(&self, events: impl IntoIterator<Item = InputEvent>) {
        for event in events {
            self.session.send_input(&event);
        }
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
    present: Arc<Presentation>,
    target_fps: u32,
    spikes: Mutex<SpikeHistory>,
    host_id: String,
    /// Hermes host: ask it how it serves the stream once frames flow.
    hermes: bool,
    /// "1920×1080@60 H264", known once the handshake is done.
    video: Mutex<String>,
    first_frame: AtomicBool,
    provenance_requested: AtomicBool,
    /// Seconds in a row with frames arriving and none decoded.
    undecoded_secs: AtomicU32,
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
            superseded: self.present.superseded(),
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
            dropped_waiting_idr: now.stats.dropped_waiting_idr,
            idr_requests: now.stats.idr_requests,
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
                    // Frames arrive, none decode: say so on the picture even
                    // with statistics hidden (it says "Waiting for video").
                    let undecoded = sample.fps >= 10.0 && sample.decoded_fps.is_some_and(|f| f < 0.5);
                    let secs = if undecoded {
                        self.undecoded_secs.fetch_add(1, Ordering::Relaxed) + 1
                    } else {
                        self.undecoded_secs.store(0, Ordering::Relaxed);
                        0
                    };
                    if secs == 3 {
                        warn!("frames arrive but none decode: {}", verdict.key_metric);
                        let status = format!("Video arrives but can't be decoded. {}", verdict.summary);
                        self.controller.ui(move |ui| ui.set_stream_status(status.into()));
                    }
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

    /// Stream `generation` is still the one running.
    pub(super) fn stream_is(&self, generation: u64) -> bool {
        self.active_stream()
            .as_ref()
            .is_some_and(|a| a.generation == generation)
    }

    /// The host of the running stream.
    pub(super) fn stream_host(&self) -> Option<String> {
        self.active_stream().as_ref().map(|a| a.host_id.clone())
    }

    /// Opens the stream for a launched app; runs on a worker thread (the RTSP
    /// handshake blocks). On success the window switches to the stream view.
    pub(super) fn start_stream(
        self: &Shared,
        host_id: String,
        launched: &LaunchedSession,
        host: StreamHost,
        video: StreamVideo,
        session_report: SharedReport,
        launch_seq: u64,
    ) -> Result<(), String> {
        let (decoder, formats) = video.decoder;
        let config = launched.stream_config(&formats).map_err(|e| e.to_string())?;
        hestia_core::add_stream_config_to_report(&config, decoder.name(), &mut report::lock(&session_report));
        let generation = self.stream_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let present = Arc::new(Presentation::new(generation, video.gpu));
        self.gpu_window.set_active(&present);
        let sink = DecodingSink::new(
            decoder,
            Box::new(UiPresenter {
                weak,
                presentation: present.clone(),
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
            undecoded_secs: AtomicU32::new(0),
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
                present.close();
                e.to_string()
            })?;
        if !self.launch_current(launch_seq) {
            present.close();
            session.stop();
            return Err("cancelled".into());
        }
        let codec = format!("{:?}", session.details().video_format.codec);
        info!("streaming {size} {codec} from {}", launched.host.name);
        *listener.video.lock().unwrap_or_else(|p| p.into_inner()) = format!("{size}@{} {codec}", launched.request.fps);
        session.report_negotiation(&mut report::lock(&session_report));

        // A stream replacing another keeps the laid-out view (no new layout
        // event comes for it).
        let view = self
            .active_stream()
            .as_ref()
            .map_or((0.0, 0.0), |a| a.input.view_size());
        // Hermes host offering clipboard sync (the menu's actions, and the
        // setting's live sync).
        let clipboard = self
            .state()
            .snapshots
            .get(&host_id)
            .and_then(|s| s.capabilities.as_ref())
            .is_some_and(|c| c.features.clipboard_sync);
        let clipboard_host = host_id.clone();
        let previous = self.active_stream().replace(ActiveStream {
            generation,
            session,
            host_id,
            input: StreamInput::new(stream_width, view, launched.settings.onscreen_controller),
            report: session_report,
            decode,
            present,
        });
        if let Some(previous) = previous {
            previous.present.close();
            spawn("hestia-stream-stop", move || previous.session.stop());
        }
        let pad = launched.settings.onscreen_controller;
        if clipboard && launched.settings.clipboard_sync {
            self.start_clipboard_sync(generation, clipboard_host);
        }
        self.ui(move |ui| {
            ui.set_stream_stats(format!("{size} {codec}").into());
            ui.set_stream_pad_visible(pad);
            ui.set_stream_clipboard(clipboard);
            ui.set_stream_menu_open(false);
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
        active.present.close();
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
            let video_stats = session.stop_with_stats();
            {
                let mut r = report::lock(&session_report);
                if let Some(video) = video_stats {
                    r.video_transport(&video);
                }
                r.decoder_totals(
                    decode.decoded.load(Ordering::Relaxed),
                    decode.decode_time_us.load(Ordering::Relaxed),
                    decode.dropped.load(Ordering::Relaxed),
                );
                r.field(
                    "Decoder totals",
                    "decoder",
                    *decode.decoder.lock().unwrap_or_else(|p| p.into_inner()),
                );
                r.field("Decoder totals", "decoded", decode.decoded.load(Ordering::Relaxed));
                r.field("Decoder totals", "dropped", decode.dropped.load(Ordering::Relaxed));
                r.field("Decoder totals", "errors", decode.errors.load(Ordering::Relaxed));
                r.field("Decoder totals", "resets", decode.resets.load(Ordering::Relaxed));
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
                    ("IDR request to first Slint handoff", &present.recovery.latency),
                ]) {
                    if let Some(summary) = histogram.summary() {
                        r.field("Video latency", stage, summary);
                    }
                }
                #[cfg(target_os = "android")]
                {
                    for (label, counter) in [
                        ("frames collected while input idle", &decode.idle_output),
                        ("frames with codec input pressure", &decode.codec_input_busy),
                        ("codec input timeouts", &decode.codec_input_timeouts),
                        ("image reader attempts at buffer limit", &decode.codec_image_reader_full),
                        (
                            "codec outputs replaced before presentation",
                            &decode.codec_output_superseded,
                        ),
                    ] {
                        r.field("MediaCodec", label, counter.load(Ordering::Relaxed));
                    }
                }
                // Pictures the window's GPU renderer drew itself (Android
                // hardware buffers, desktop YUV planes).
                let gpu = &present.gpu;
                r.field(
                    "GPU video",
                    "redraw requests",
                    gpu.redraw_requests.load(Ordering::Relaxed),
                );
                r.field("GPU video", "draws submitted", gpu.drawn.load(Ordering::Relaxed));
                r.field(
                    "GPU video",
                    "replaced before draw",
                    gpu.superseded.load(Ordering::Relaxed),
                );
                for (stage, histogram) in [
                    ("GPU picture ready to draw submission", &gpu.ready_to_draw),
                    ("first packet to GPU draw submission", &gpu.receive_to_draw),
                    ("IDR request to first GPU draw submission", &gpu.recovery.latency),
                ] {
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
        if let Some(event) = stream_input::key(text, pressed, modifiers) {
            self.send_input(event);
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
            if let Some(event) = stream_input::native_touch(kind, finger, (x, y), (width, height)) {
                self.send_input(event);
            }
            return;
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
        if let Some(active) = self.active_stream().as_ref() {
            active.send(stream_input::absolute_pointer(
                kind,
                button,
                (x, y),
                (width, height),
                touch,
            ));
        }
    }

    fn multi_finger(self: &Shared, kind: i32, finger: i32, point: (f32, f32), picture: Picture) -> bool {
        let action = {
            let mut guard = self.active_stream();
            let Some(ActiveStream { input, session, .. }) = guard.as_mut() else {
                return false;
            };
            input.multi_finger(kind, finger, point, picture, Instant::now(), |event| {
                session.send_input(&event)
            })
        };
        match action {
            GestureAction::Pointer => return false,
            GestureAction::Consumed => {}
            GestureAction::Zoom(factor, (px, py)) => self.ui(move |ui| {
                ui.set_stream_zoom(factor);
                ui.set_stream_pan_x(px);
                ui.set_stream_pan_y(py);
            }),
            GestureAction::ToggleKeyboard => self.ui(|ui| {
                ui.set_stream_keyboard_open(!ui.get_stream_keyboard_open());
            }),
        }
        true
    }

    /// Menu → Zoom: two fingers pinch and pan; turning it off resets.
    pub(super) fn toggle_zoom(self: &Shared) {
        let on = {
            let mut guard = self.active_stream();
            let Some(active) = guard.as_mut() else { return };
            active.input.toggle_zoom()
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

    /// Touches on the on-screen controller; true when the pad took it.
    fn pad_event(self: &Shared, kind: i32, finger: i32, point: (f32, f32)) -> bool {
        let now = Instant::now();
        let (state, click, generation) = {
            let mut guard = self.active_stream();
            let Some(active) = guard.as_mut() else { return false };
            let Some((state, click)) = active.input.pad_event(kind, finger, point, now) else {
                return false;
            };
            (state, click, active.generation)
        };
        Self::send_pad(state, click);
        if click != 0 {
            // A stick tap clicks it: release shortly after.
            let c = self.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(60), move || {
                let state = c
                    .active_stream()
                    .as_ref()
                    .filter(|active| active.generation == generation)
                    .and_then(|active| active.input.visible_pad_state());
                if let Some(state) = state {
                    Self::send_pad(state, 0);
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
            crate::pad_controls(active.input.pad())
        };
        self.ui(move |ui| ui.set_stream_pad_controls(ModelRc::new(VecModel::from(controls))));
    }

    /// Menu → On-screen controller (remembered for next time).
    pub(super) fn toggle_pad(self: &Shared) {
        let on = {
            let mut guard = self.active_stream();
            let Some(active) = guard.as_mut() else { return };
            active.input.toggle_pad()
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
            active.input.resize(width, height)
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
    pub(super) fn stream_ime(self: &Shared, event: ImeEvent) {
        if let Some(active) = self.active_stream().as_ref() {
            stream_input::ime(event, |event| active.session.send_input(&event));
        }
    }

    fn trackpad_event(self: &Shared, kind: i32, x: f32, y: f32, video_width: f32) {
        let mut guard = self.active_stream();
        let Some(ActiveStream {
            input,
            session,
            generation,
            ..
        }) = guard.as_mut()
        else {
            return;
        };
        if let Some(touch_id) = input.trackpad_event(kind, (x, y), video_width, Instant::now(), |event| {
            session.send_input(&event);
        }) {
            let generation = *generation;
            let c = self.clone();
            slint::Timer::single_shot(crate::touch::LONG_PRESS, move || {
                c.trackpad_long_press(generation, touch_id)
            });
        }
    }

    fn trackpad_long_press(&self, generation: u64, touch_id: u64) {
        let mut guard = self.active_stream();
        let Some(active) = guard.as_mut().filter(|active| active.generation == generation) else {
            return;
        };
        let ActiveStream { input, session, .. } = active;
        input.long_press(touch_id, Instant::now(), |event| session.send_input(&event));
    }

    /// Menu → Touch: trackpad → mouse → touchscreen (remembered).
    pub(super) fn cycle_touch_mode(self: &Shared) {
        let mode = self.touch_mode().next();
        self.touch_mode.store(mode.index() as u8, Ordering::Relaxed);
        if let Some(ActiveStream { input, session, .. }) = self.active_stream().as_mut() {
            input.reset_touch(|event| session.send_input(&event));
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
        if let Some(event) = stream_input::scroll(dx, dy) {
            self.send_input(event);
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
