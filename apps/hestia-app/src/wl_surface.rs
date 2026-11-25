//! The window's Wayland surface, for what Slint doesn't do with it: when a
//! frame with a new video picture reaches the screen, by the compositor's
//! own clock (`wp_presentation`), and whether a stream's frames may reach it
//! without waiting for a refresh (`wp_tearing_control_v1`).
//!
//! The stream report times a picture up to its draw submission; after that
//! the toolkit hands the frame to the compositor, which composites it (or
//! puts it on a plane) at a later refresh. Only the compositor knows when:
//! asked before a commit, `wp_presentation` answers with the time that
//! commit's content was shown, or that it never was.
//!
//! The frame is Slint's: the GL renderer draws into the window's surface and
//! the buffer swap commits it. A feedback requested after the scene is
//! drawn (`AfterRendering`) rides that commit. winit owns the Wayland
//! connection and reads its socket; this attaches to the same display with
//! a queue of its own, which the renderer dispatches every frame.
//!
//! Tearing: a compositor normally shows a commit at the next refresh. With
//! the asynchronous presentation hint, one that allows it (KWin, for a
//! full-screen window, when "Allow tearing in full screen" is on) flips to
//! it at once, mid-refresh: the picture can tear, and it shows up to a
//! refresh sooner. The hint is set only while a stream that allows it runs.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hestia_frontend::presentation::Presentation;
use log::{debug, info};
use slint::winit_030::WinitWindowAccessor;
use slint::winit_030::winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::presentation_time::client::wp_presentation::{self, WpPresentation};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{self, Kind, WpPresentationFeedback};
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_manager_v1::{
    self, WpTearingControlManagerV1,
};
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_v1::{
    self, PresentationHint, WpTearingControlV1,
};

/// A window's surface, with the compositor's presentation times and the
/// tearing hint where the compositor has them.
pub(crate) struct WindowSurface {
    // Kept for the queue: a connection outliving nothing it serves.
    _connection: Connection,
    queue: EventQueue<State>,
    handle: QueueHandle<State>,
    surface: WlSurface,
    state: State,
    /// The surface's tearing hint object, once one was asked for.
    tearing: Option<WpTearingControlV1>,
    tearing_allowed: bool,
}

#[derive(Default)]
struct State {
    presentation: Option<WpPresentation>,
    /// The clock the compositor's times are on (`clock_gettime` id).
    clock: Option<libc::clockid_t>,
    tearing_manager: Option<WpTearingControlManagerV1>,
}

/// One frame that carried a new picture, awaiting the compositor's answer.
struct Watched {
    stream: Arc<Presentation>,
    received_at: Instant,
    drawn_at: Instant,
}

impl WindowSurface {
    /// For the window's surface, when it is a Wayland one and the compositor
    /// reports presentation times or takes a tearing hint. Call on the UI
    /// thread.
    pub(crate) fn new(window: &slint::Window) -> Option<Self> {
        let (display, surface) = window.with_winit_window(|window| {
            let display = match window.display_handle().ok()?.as_raw() {
                RawDisplayHandle::Wayland(handle) => handle.display,
                _ => return None,
            };
            let surface = match window.window_handle().ok()?.as_raw() {
                RawWindowHandle::Wayland(handle) => handle.surface,
                _ => return None,
            };
            Some((display, surface))
        })??;
        // SAFETY: winit's display, alive as long as its event loop, and the
        // window's surface, alive as long as the window: both outlive the
        // window's renderer, which owns this clock.
        unsafe { Self::from_raw(display, surface) }
    }

    /// For a Wayland surface of `display`.
    ///
    /// # Safety
    ///
    /// `display` must be a live `wl_display` and `surface` a live
    /// `wl_surface` of it, both outliving the clock. The display is never
    /// closed and the surface never destroyed here.
    unsafe fn from_raw(display: NonNull<c_void>, surface: NonNull<c_void>) -> Option<Self> {
        // SAFETY: the caller's display; a foreign backend never closes it.
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        // SAFETY: the caller's surface. The proxy made here is only ever
        // passed as an argument, never destroyed.
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }.ok()?;
        let surface = WlSurface::from_id(&connection, id).ok()?;

        let mut state = State::default();
        connection.display().get_registry(&handle, ());
        // The globals, then the clock the presentation global announces.
        queue.roundtrip(&mut state).ok()?;
        if state.presentation.is_some() {
            queue.roundtrip(&mut state).ok()?;
        }
        if state.clock.is_none() {
            state.presentation = None;
        }
        match state.clock {
            Some(clock) => info!("compositor presentation times available (clock {clock})"),
            None => info!("compositor presentation times unavailable"),
        }
        if state.tearing_manager.is_some() {
            info!("compositor takes a tearing hint");
        }
        if state.presentation.is_none() && state.tearing_manager.is_none() {
            return None;
        }
        Some(Self {
            _connection: connection,
            queue,
            handle,
            surface,
            state,
            tearing: None,
            tearing_allowed: false,
        })
    }

    /// Let the window's frames reach the screen without waiting for a
    /// refresh, or make them wait again. Sends nothing when unchanged.
    pub(crate) fn set_tearing(&mut self, allowed: bool) {
        if allowed == self.tearing_allowed {
            return;
        }
        let Some(manager) = &self.state.tearing_manager else {
            return;
        };
        let control = self
            .tearing
            .get_or_insert_with(|| manager.get_tearing_control(&self.surface, &self.handle, ()));
        control.set_presentation_hint(if allowed {
            PresentationHint::Async
        } else {
            PresentationHint::Vsync
        });
        self.tearing_allowed = allowed;
        info!("tearing {}", if allowed { "allowed" } else { "off" });
    }

    /// Ask when the window's next commit, which carries the picture drawn at
    /// `drawn_at`, reaches the screen (nothing without presentation times).
    pub(crate) fn watch_next_commit(&mut self, stream: Arc<Presentation>, received_at: Instant, drawn_at: Instant) {
        if let Some(presentation) = &self.state.presentation {
            presentation.feedback(
                &self.surface,
                &self.handle,
                Watched {
                    stream,
                    received_at,
                    drawn_at,
                },
            );
        }
    }

    /// Record the answers that arrived since the last call.
    pub(crate) fn dispatch(&mut self) {
        if let Err(e) = self.queue.dispatch_pending(&mut self.state) {
            debug!("compositor presentation times: {e}");
        }
    }
}

/// When a time on `clock` was, as an [`Instant`]: how long ago it was on that
/// clock, subtracted from now.
fn instant_on(clock: libc::clockid_t, time: Duration) -> Option<Instant> {
    // SAFETY: an all-zero timespec is valid, and the call only writes it.
    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `now` is a valid timespec for the call to write.
    if unsafe { libc::clock_gettime(clock, &mut now) } != 0 {
        return None;
    }
    let now_on_clock = Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
    let instant_now = Instant::now();
    Some(
        instant_now
            .checked_sub(now_on_clock.saturating_sub(time))
            .unwrap_or(instant_now),
    )
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        if interface == WpPresentation::interface().name && state.presentation.is_none() {
            state.presentation = Some(registry.bind(name, version.min(1), handle, ()));
        } else if interface == WpTearingControlManagerV1::interface().name && state.tearing_manager.is_none() {
            state.tearing_manager = Some(registry.bind(name, version.min(1), handle, ()));
        }
    }
}

impl Dispatch<WpTearingControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpTearingControlManagerV1,
        _: wp_tearing_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpTearingControlV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpTearingControlV1,
        _: wp_tearing_control_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock = Some(clk_id as libc::clockid_t);
        }
    }
}

impl Dispatch<WpPresentationFeedback, Watched> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        watched: &Watched,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let glass = &watched.stream.gpu.glass;
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                flags,
                ..
            } => {
                let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                let Some(on_glass) = state
                    .clock
                    .and_then(|clock| instant_on(clock, Duration::new(seconds, tv_nsec)))
                else {
                    return;
                };
                let kind = match flags {
                    WEnum::Value(kind) => kind,
                    WEnum::Unknown(bits) => Kind::from_bits_truncate(bits),
                };
                glass.record_presented(
                    watched.received_at,
                    watched.drawn_at,
                    on_glass,
                    kind.contains(Kind::ZeroCopy),
                    !kind.contains(Kind::Vsync),
                );
            }
            wp_presentation_feedback::Event::Discarded => glass.record_discarded(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_on_the_monotonic_clock_becomes_that_instant() {
        // SAFETY: as in `instant_on`.
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        let monotonic_now = Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
        let before = Instant::now();
        let then = instant_on(libc::CLOCK_MONOTONIC, monotonic_now - Duration::from_millis(16)).unwrap();
        let elapsed = before.saturating_duration_since(then);
        assert!(
            elapsed >= Duration::from_millis(15) && elapsed < Duration::from_millis(30),
            "{elapsed:?}"
        );
        // A time not yet reached on the clock is taken as now, not the future.
        assert!(instant_on(libc::CLOCK_MONOTONIC, monotonic_now + Duration::from_secs(5)).unwrap() >= before);
    }

    /// Against a running compositor: `WAYLAND_DISPLAY` must name one with
    /// `wp_presentation`, such as a headless one that disturbs no desktop:
    ///
    /// ```sh
    /// weston --backend=headless --socket=hestia-glass &
    /// WAYLAND_DISPLAY=hestia-glass cargo test -p hestia-app --lib wl_surface -- --ignored
    /// ```
    #[test]
    #[ignore = "needs a Wayland compositor"]
    fn a_committed_frame_is_reported_on_screen() {
        use std::os::fd::{AsFd, FromRawFd, OwnedFd};

        use wayland_client::globals::{GlobalListContents, registry_queue_init};
        use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_shm, wl_shm_pool};
        use wayland_client::{delegate_noop, protocol::wl_surface};
        use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

        /// A client as a real one behaves: two buffers, each attached again
        /// only once the compositor released it, at the pace of frame callbacks.
        struct Window {
            configured: bool,
            released: [bool; 2],
            frame_done: bool,
        }
        impl Dispatch<wl_buffer::WlBuffer, usize> for Window {
            fn event(
                state: &mut Self,
                _: &wl_buffer::WlBuffer,
                event: wl_buffer::Event,
                index: &usize,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let wl_buffer::Event::Release = event {
                    state.released[*index] = true;
                }
            }
        }
        impl Dispatch<wl_callback::WlCallback, ()> for Window {
            fn event(
                state: &mut Self,
                _: &wl_callback::WlCallback,
                event: wl_callback::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let wl_callback::Event::Done { .. } = event {
                    state.frame_done = true;
                }
            }
        }
        impl Dispatch<WlRegistry, GlobalListContents> for Window {
            fn event(
                _: &mut Self,
                _: &WlRegistry,
                _: wl_registry::Event,
                _: &GlobalListContents,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
        impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Window {
            fn event(
                _: &mut Self,
                base: &xdg_wm_base::XdgWmBase,
                event: xdg_wm_base::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let xdg_wm_base::Event::Ping { serial } = event {
                    base.pong(serial);
                }
            }
        }
        impl Dispatch<xdg_surface::XdgSurface, ()> for Window {
            fn event(
                state: &mut Self,
                surface: &xdg_surface::XdgSurface,
                event: xdg_surface::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let xdg_surface::Event::Configure { serial } = event {
                    surface.ack_configure(serial);
                    state.configured = true;
                }
            }
        }
        delegate_noop!(Window: ignore wl_compositor::WlCompositor);
        delegate_noop!(Window: ignore wl_surface::WlSurface);
        delegate_noop!(Window: ignore wl_shm::WlShm);
        delegate_noop!(Window: ignore wl_shm_pool::WlShmPool);
        delegate_noop!(Window: ignore xdg_toplevel::XdgToplevel);

        let connection = Connection::connect_to_env().expect("a Wayland compositor (WAYLAND_DISPLAY)");
        let (globals, mut queue) = registry_queue_init::<Window>(&connection).unwrap();
        let handle = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&handle, 1..=4, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&handle, 1..=1, ()).unwrap();
        let shell: xdg_wm_base::XdgWmBase = globals.bind(&handle, 1..=1, ()).unwrap();
        let surface = compositor.create_surface(&handle, ());
        let xdg = shell.get_xdg_surface(&surface, &handle, ());
        let toplevel = xdg.get_toplevel(&handle, ());
        toplevel.set_title("hestia glass test".into());
        toplevel.set_app_id("hestia-glass-test".into());
        surface.commit();
        let mut window = Window {
            configured: false,
            released: [true; 2],
            frame_done: true,
        };
        while !window.configured {
            queue.blocking_dispatch(&mut window).unwrap();
        }

        let (width, height) = (64, 64);
        let frame_size = width * height * 4;
        let size = frame_size * 2;
        // SAFETY: a fresh anonymous file, owned from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(libc::memfd_create(c"hestia-glass".as_ptr(), libc::MFD_CLOEXEC)) };
        assert_eq!(
            unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), size as libc::off_t) },
            0
        );
        let pool = shm.create_pool(fd.as_fd(), size as i32, &handle, ());
        let buffers = [0, 1].map(|index| {
            pool.create_buffer(
                (index * frame_size) as i32,
                width as i32,
                height as i32,
                width as i32 * 4,
                wl_shm::Format::Xrgb8888,
                &handle,
                index,
            )
        });

        let display = NonNull::new(connection.backend().display_ptr().cast::<c_void>()).unwrap();
        let surface_ptr = NonNull::new(surface.id().as_ptr().cast::<c_void>()).unwrap();
        // SAFETY: this test's display and surface, both alive to its end.
        let mut glass = unsafe { WindowSurface::from_raw(display, surface_ptr) }.expect("a Wayland surface");
        assert!(glass.state.presentation.is_some(), "wp_presentation");
        // The hint is accepted (or there is none to send) without a protocol error.
        glass.set_tearing(true);
        glass.set_tearing(false);
        let stream = Arc::new(Presentation::default());
        let glass_stats = &stream.gpu.glass;
        let count = |counter: &std::sync::atomic::AtomicU64| counter.load(std::sync::atomic::Ordering::Relaxed);

        // A new picture in every frame the compositor asks for, as a stream draws them.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sent = 0;
        while count(&glass_stats.presented) < 10 && Instant::now() < deadline {
            let free = window.released.iter().position(|&released| released);
            if let (true, Some(index)) = (window.frame_done && sent < 120, free) {
                let drawn_at = Instant::now();
                surface.frame(&handle, ());
                glass.watch_next_commit(stream.clone(), drawn_at - Duration::from_millis(5), drawn_at);
                surface.attach(Some(&buffers[index]), 0, 0);
                surface.damage_buffer(0, 0, width as i32, height as i32);
                surface.commit();
                window.released[index] = false;
                window.frame_done = false;
                sent += 1;
            }
            connection.flush().unwrap();
            // This queue's read takes in the clock's events too, as winit's
            // does in the app.
            if let Some(guard) = queue.prepare_read() {
                let _ = guard.read();
            }
            queue.dispatch_pending(&mut window).unwrap();
            glass.dispatch();
            std::thread::sleep(Duration::from_millis(1));
        }
        let (presented, discarded) = (count(&glass_stats.presented), count(&glass_stats.discarded));
        eprintln!(
            "{sent} frames: {presented} on screen, {discarded} discarded; draw to glass {}; first packet to glass {}",
            glass_stats.draw_to_glass.summary().unwrap_or_default(),
            glass_stats.receive_to_glass.summary().unwrap_or_default()
        );
        assert!(presented >= 10, "frames reached the screen");
        assert!(presented + discarded <= sent);
        drop(glass);
        for buffer in buffers {
            buffer.destroy();
        }
    }
}
