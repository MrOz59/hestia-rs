# UI shells

The graphical client is split so that the UI toolkit can be replaced (Slint
today; egui, Qt or a TV-only UI tomorrow) without touching the app itself.

```
 apps/hestia-app (Slint shell)            apps/hestia-egui (a future shell)
   ui/*.slint, slint_ui.rs,                 egui widgets, EguiUi,
   gl_video.rs / gpu_render.rs              a glow paint callback
              \                                /
               \  View + UiHandle  (the port) /
                \      intents  ↑            /
                 crates/hestia-frontend
   controller (hosts, pairing, apps, launch, stream, tools, settings,
   reports), stream input (touch, on-screen and physical controllers,
   keyboard), platform services, video presentation, gl_yuv
                              |
              hestia-core, hestia-media, hestia-stream…
```

`hestia-frontend` has no toolkit dependency, and nothing in it changes when a
shell is added. A shell is a binary (and, on Android, a `cdylib`) that does
four things:

1. **Implements `UiHandle`**: queues work for its UI thread, now
   (`run`) or after a delay (`run_after`).
2. **Implements `View`**: the screens' state as plain Rust values. A
   retained-mode toolkit maps each setter to a property; an immediate-mode one
   stores the values in a struct, reads them back in the getters, and draws
   from that struct every frame.
3. **Calls the controller's intents**: `Controller`'s public methods,
   one per user action (`open_host`, `launch`, `stream_key`,
   `stream_pointer`, `save_settings`…). They must be called on the UI
   thread and never block; the controller sends network work to its own
   threads.
4. **Draws the stream**: RGBA pictures arrive through
   `View::show_video_frame`. Pictures the GPU draws itself only call
   `View::request_redraw`; the shell's renderer takes them with
   `Presentation::take_gpu` (see "Video" below).

## The port (`hestia_frontend::ui`)

| Item | Role |
| --- | --- |
| `UiHandle` | `run(task)`, `run_after(delay, task)`; a `UiTask` is `Box<dyn FnOnce(&dyn View) + Send>` |
| `View` | setters and a few getters for every screen: window (screen, toast, fullscreen, size), startup, hosts, pairing, apps, launch session, report, host tools, stream, settings |
| `Screen` | which page is shown (hosts, apps, session, settings, stream, tools, report) |
| `HostCard`, `AppCard`, `Art`, `CommandRow`, `PreflightRow`, `PadControl` | list items, as plain structs |
| `SettingsForm` | the settings page's fields (choices as indices into option lists); `show_settings` fills it, `settings_form` reads it back |

The getters exist because some state belongs to the screen: the user flips
the stats overlay, edits the settings form, opens the keyboard. The
controller reads them inside a task, on the UI thread.

Every list setter (`set_hosts`, `set_apps`, …) replaces the whole list. The
two exceptions are `set_app_art` (box art arriving one card at a time) and
`set_running_app`, so that a shell doesn't rebuild the app grid for them.

The compiler lists anything a new shell forgets: `View` has no default
methods.

### Platform-specific options

Build the settings and stream menus from the capabilities of the client.
Use the codec choices supplied by `SettingsForm`. For input options, use
the touchscreen and pointer-capture flags supplied to the shell. A Linux
touchscreen can provide both. Only visible rows take a position in keyboard
and D-pad navigation. Options that configure Hermes, including Gamescope and
virtual displays, remain available from either client.

## Startup

What the Slint shell does (`apps/hestia-app/src/lib.rs`), in order:

```rust
let gpu_window = Arc::new(GpuWindow::default());
install_gpu_renderer(&window, gpu_window.clone());   // optional, see Video
let controller = Controller::new(Arc::new(MyUi::new(&window)), gpu_window);
wire_callbacks(&window, &controller);                 // callbacks → intents
#[cfg(target_os = "linux")]
hestia_frontend::desktop_pads::start();               // evdev controllers
controller.start(data_dir);                           // opens the core, fills the screens
run_event_loop();
```

Logging goes through `hestia_core::logcapture::install` before anything
else, so stream reports carry every line (see `main.rs`).

### Android

The shell also:

* hands the activity to the platform layer:
  `hestia_frontend::platform::set_android_app(app)` (an
  `android_activity::AndroidApp`, which every android-activity backend
  provides);
* routes LAN traffic over Wi-Fi:
  `hestia_core::nvhttp::http::set_route_hook(platform::route_for)`;
* feeds game controller events to `platform::android_input_hook`. Slint
  gets them through the patched backend's input hook
  (`third_party/i-slint-backend-android-activity`). Another toolkit needs
  an equivalent place to see `InputEvent`s before its UI does.

TV-remote focus movement (`form-navigation-changed` in `app.slint`) is a
toolkit concern; the Slint shell forwards it to its Android backend.

The Java helper (`crates/hestia-frontend/java`) belongs to the platform
layer and is built by `hestia-frontend`'s build script, so every shell gets
it.

## Video

The decoder thread hands pictures to the frontend's `UiPresenter`, which
keeps only the newest one and schedules the UI thread:

* **RGBA** (software decoding on the CPU path, Android's readback paths):
  `View::show_video_frame(&DecodedFrame)`, with `frame.rgba()` holding the
  pixels. Copy or upload them before returning: the buffer goes back to the
  decoder.
* **Native** (Android hardware buffers, desktop YUV planes):
  `View::request_redraw()`. The shell's renderer, running inside the
  toolkit's rendering with its GL context current, calls
  `GpuWindow::active()` and then `Presentation::take_gpu()`, draws the
  picture, records `presentation.gpu` statistics and sets
  `GpuWindow::status`:
  * `ready()` when the context can draw;
  * `pause()` when it goes away;
  * `fail(why)` when drawing fails.

  While the status is not ready, decoders fall back to RGBA by themselves.

A shell without a GPU renderer can leave the status alone: every picture
then arrives as RGBA.

For the desktop planes, `hestia_frontend::gl_yuv::YuvPass` does all the GL
work and needs only a `get_proc_address` loader and a current OpenGL 3 or
OpenGL ES 3 context. The result is an RGBA texture: show it the way the
toolkit shows external textures. Slint uses a borrowed texture image; with
egui you would use a `glow` paint callback or a native texture id. On
Android, `gpu_render.rs` imports the `AHardwareBuffer` as an EGL image. Its
GL part doesn't depend on Slint either, and would move to the frontend the
same way if a second Android shell needs it.

## Example: an egui shell

A sketch of the pieces. It doesn't compile as is; names follow `eframe`
0.3x.

```rust
/// What the screens show; the egui `update` draws from it.
#[derive(Default)]
struct Screens {
    screen: Screen,
    toast: String,
    hosts: Vec<ui::HostCard>,
    stream_frame: Option<egui::TextureHandle>,
    settings: SettingsForm,
    // … one field per View setter
}

struct EguiView<'a> {
    screens: &'a RefCell<Screens>,
    ctx: &'a egui::Context,
}

impl View for EguiView<'_> {
    fn set_screen(&self, screen: Screen) { self.screens.borrow_mut().screen = screen; }
    fn screen(&self) -> Screen { self.screens.borrow().screen }
    fn show_video_frame(&self, frame: &DecodedFrame) {
        let Some(rgba) = frame.rgba() else { return };
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize], rgba);
        let mut s = self.screens.borrow_mut();
        match &mut s.stream_frame {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => s.stream_frame = Some(self.ctx.load_texture("stream", image, Default::default())),
        }
    }
    fn request_redraw(&self) { self.ctx.request_repaint(); }
    // … the rest
}

/// Tasks go through a channel that `update` drains every frame.
struct EguiUi {
    tx: std::sync::mpsc::Sender<UiTask>,
    ctx: egui::Context,
}

impl UiHandle for EguiUi {
    fn run(&self, task: UiTask) {
        let _ = self.tx.send(task);
        self.ctx.request_repaint();
    }
    fn run_after(&self, delay: Duration, task: UiTask) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let _ = tx.send(task);
            ctx.request_repaint();
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let view = EguiView { screens: &self.screens, ctx };
        while let Ok(task) = self.rx.try_recv() {
            task(&view);
        }
        match self.screens.borrow().screen {
            Screen::Hosts => hosts_page(ctx, &self.screens, &self.controller),
            // …
        }
    }
}
```

`run_after` spawns a thread per delay here for brevity. Long presses and
stick taps come often, so a real shell should keep a timer list that
`update` checks, with `ctx.request_repaint_after`.

## Keeping the boundary

* `hestia-frontend` must not depend on a toolkit. Code that needs one
  belongs in the shell, behind a `View` method or a renderer.
* A new screen or field: add it to `View` (and the model structs), set it
  from the controller, and implement it in every shell.
* A new user action: add a public method to `Controller` and call it from
  every shell.
* `crates/hestia-frontend/src/ui.rs` is the contract; its doc comments say
  what each part means.
