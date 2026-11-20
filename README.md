# Hestia RS

A cross-platform game-streaming client for [Hermes](https://github.com/MrOz59/Hermes),
written in Rust: one modular core, a modern [Slint](https://slint.dev) UI, and
Linux and Android as the first targets.

Hestia RS speaks the standard GameStream protocol, so it works with Sunshine and
Apollo hosts too, and it uses Hermes' **Hestia protocol v1** extensions
(capabilities, session preparation, virtual displays, diagnostics, clipboard,
server commands) when the host offers them.

> **Status: prototype.** The control plane (discovery, pairing, apps, Hermes
> capabilities, launch/quit) and a first streaming pipeline (RTSP, encrypted
> control stream over ENet, video with Reed–Solomon FEC, software H.264
> decode, keyboard/mouse input, stereo audio on desktop) are implemented. They
> are covered by unit tests, an end-to-end test against a mock host, an
> interop test against the real C ENet, and a decode test with a real H.264
> stream, but **have not yet been exercised against a live Hermes host**.
> See [docs/ROADMAP.md](docs/ROADMAP.md).

## What's implemented

- **Discovery** of hosts on the LAN over mDNS (`_nvstream._tcp`), plus adding
  hosts by IP or name.
- **Pairing**, both the classic client PIN (type the PIN into the Hermes web UI)
  and Hermes/Apollo **OTP** pairing (PIN + passphrase shown by the host).
- **App list and box art**, current running app, launch / resume / quit.
- **Hermes integration**: capability probe, `session/prepare` before launch
  (virtual display size, HDR, scale factor, Gamescope launch mode), session
  stop, diagnostics, client permissions, clipboard and server commands in the
  API crate.
- **Streaming** in pure Rust: RTSP handshake (plain and `rtspenc://`), SDP,
  AES-GCM control stream over a built-in ENet client, video RTP reassembly with
  Reed–Solomon FEC (byte-compatible with nanors), IDR recovery, Opus audio
  (AES-CBC), and input packets (keyboard, text, mouse, scroll, touch, pen,
  gamepad).
- **Decoding**: on Android, the hardware decoder (MediaCodec: H.264, HEVC,
  AV1) renders into GPU buffers that the UI draws directly, like Moonlight:
  no CPU copy, YUV to RGB in the GPU's sampler (Settings → Zero-copy video).
  Without it (or if the GPU path fails) pictures are read on the CPU, through
  an image reader or the codec's own output buffers; OpenH264 (software
  H.264) on desktop and as the last fallback. Decoding runs on its own thread
  and, like Moonlight, never drops a compressed frame for being late (each
  one is a reference for the next; dropping one costs a keyframe): bursts
  decode through and only the newest picture is shown. When keyframes don't
  fix a failing decoder, keyframe requests space out instead of flooding
  the host.
- **Stream screen**: video letterboxed in the window, keyboard (US-layout key
  mapping, text fallback for other characters), absolute mouse, buttons,
  scroll, touch as mouse, stats overlay, full screen, disconnect / quit.
  `Ctrl+Alt+Shift+Q` disconnects, `Ctrl+Alt+Shift+S` toggles statistics.
- **Touch modes** (stream menu, remembered): *trackpad* (default; the
  finger moves the host cursor relatively, tap = click, hold = right click,
  tap then drag = drag), *mouse* (the cursor goes where you touch) and
  *touchscreen* (every finger reaches the host as a real touch, for
  touch-aware apps).
- **On-screen controller** (stream menu, remembered): sticks (tap to click
  L3/R3), d-pad, A/B/X/Y, bumpers, analog triggers, Back/Home/Start, every
  finger independent; in portrait it sits under the picture. It takes a
  controller slot like a physical one.
- **Zoom** (stream menu): pinch to magnify the picture up to 4×, drag with
  two fingers to move around.
- **Orientation** (Android settings): landscape, portrait or the device's
  rotation, while streaming. **Custom resolution** in the settings takes any
  size (e.g. 2560x1080).
- **Keyboard bar** in the stream menu: a text field (opens the on-screen
  keyboard) replayed on the host, plus Esc, Tab, Enter, Backspace, Win,
  Alt+Tab and Alt+F4; "Fix picture" asks the host for a fresh keyframe.
- **Host tools** (Hermes): display status and recovery, server commands,
  clipboard send/fetch, this device's permissions, diagnostics, the host's
  readiness preflight (failures first, also summarised on the host card),
  runtime status (encoder, sessions, pipeline), the host's full diagnostic
  report and a shortcut to the Hermes web UI.
- **Stream provenance**: after the first frame, a banner shows what the host
  is actually using (GPU, encoder, capture path) from Hermes' stream info; it
  goes into the report too.
- **Presets** in Settings: Fast (≤1080p60), Balanced (native, ≤60 fps),
  Quality (native resolution and refresh, more bitrate) and Battery saver
  (720p30), computed from this device's display (EDID on Linux,
  `Display.Mode` on Android) with Hestia desktop's bitrate table. Editing
  resolution, frame rate or bitrate by hand switches to Custom.
- **Stream profiles**: the settings page's video, audio and host options
  saved under a name ("Couch TV", "Phone 720p"), loaded back or deleted;
  each host can launch with one (its page → Stream profile), while touch,
  orientation and decoding stay this device's.
- **Clipboard sync** (Hermes, Settings → Clipboard sync): while streaming,
  what's copied on either side can be pasted on the other; the stream menu
  also sends or fetches it once. Android reads the clipboard only when it
  changed (no repeated "pasted" notices); Linux uses wl-clipboard or xclip.
- **Picture scale**: fit (bars), fill (crop) or stretch, in Settings and the
  stream menu.
- **Live diagnosis**: every second the client works out whether the network,
  the decoder, the display or the host is the bottleneck (Hestia desktop's
  classifier); the overlay says which and how often it happened in the last
  five minutes, and the report marks each bad second.
- **Stream reports**: every session (including one that fails to start)
  writes a report with the client, host and launch details, what was
  negotiated, events, per-second statistics and every log line from launch
  to end. It opens when the stream ends (Settings → Diagnostics turns that
  off or reopens the last one), and can be shared (Android share sheet),
  opened in the file manager (desktop) or sent to the host's clipboard.
  Files: `Android/data/com.mroz59.hestia.rs/files/reports/` on Android,
  `~/.local/share/hestia-rs/reports/` on Linux (the CLI writes there too).
- **Audio**: stereo Opus playback through cpal on desktop Linux; on Android,
  Opus decoded by the system's MediaCodec and played with AAudio (surround
  mixed down to stereo).
- **Game controllers**: up to four (buttons, sticks, triggers, d-pad), with
  rumble. On Android, on the controller's motor or the phone's; on desktop
  Linux, read straight from evdev (no SDL/libudev) with force-feedback
  rumble and hot plugging. They drive the host only while streaming;
  otherwise (Android) they navigate the app: A confirms, B goes back.
- **Android TV**: a launcher banner and entry, nothing touch-only required,
  and the whole app works with a remote: arrows move a focus ring over the
  host and app grids and their actions, form screens move from control to
  control, and the stream menu (Back) is walked with the D-pad.
- **Touch extras**: two-finger tap right-clicks, two-finger drag scrolls,
  three-finger tap opens the keyboard.
- **Wake-on-LAN**: tapping an offline host wakes it (once its MAC address is
  known) and waits for it to come up.
- **Slint patch**: `third_party/i-slint-backend-android-activity` is Slint's
  Android backend with an input hook (controllers), a fix for mouse-wheel
  scrolling, which crashed upstream, and remote/controller keys for the UI
  (see its `PATCHES.md`).
- **UI** in Slint (dark theme, responsive grid) for desktop Linux and Android
  (NativeActivity), and a `hestia` CLI for scripting and testing.

### Known limits of the prototype

- Desktop decodes H.264 in software only (VA-API is next).
- Surround audio is mixed down to stereo.
- Keys are mapped by the character they produce (US layout), not by physical
  scancode; characters without a US key are sent as text.
- Desktop mouse is absolute only (no pointer capture), no gamepads yet.

## Layout

```
crates/
  hestia-crypto      identity (RSA-2048 + self-signed cert), pairing crypto — pure Rust
  hestia-nvhttp      GameStream HTTP(S): serverinfo, pairing, applist, box art, launch/quit
  hestia-api         Hermes "Hestia protocol v1" client (/api/hestia/v1/*)
  hestia-discovery   mDNS discovery (legacy-unicast queries, no multicast lock needed)
  hestia-core        UI-agnostic facade: identity + host store, pairing, apps, launch
  hestia-stream      streaming data plane: RTSP, ENet control, video + FEC, audio, input
  hestia-media       decoders/players behind VideoSink/AudioSink (OpenH264, Opus + cpal)
apps/
  hestia-app         Slint GUI (desktop binary + Android cdylib)
  hestia-cli         `hestia` command-line client
```

Each crate has one job and depends only on the layers below it; front-ends only
talk to `hestia-core`. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Building

Rust 1.85+ (edition 2024).

### Linux

```sh
# Debian/Ubuntu: Slint, audio (Opus, ALSA) and a C++ compiler for OpenH264
sudo apt install build-essential libfontconfig1-dev libxkbcommon-dev libwayland-dev \
    libopus-dev libasound2-dev
# Arch/CachyOS
sudo pacman -S --needed base-devel fontconfig libxkbcommon wayland opus alsa-lib

cargo run --release -p hestia-app      # GUI
cargo run --release -p hestia-cli -- --help
```

### Android

Use **NDK r30** (or whichever NDK matches skia-bindings' prebuilt Skia, see
the CI workflow): Slint renders with Skia on Android, and an older NDK's libc++
lacks symbols the prebuilt library needs, so the app builds but fails to load
on the device. Point `ANDROID_NDK_HOME` / `ANDROID_NDK_ROOT` at it.

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk cargo-apk
# native library only
cargo ndk -t arm64-v8a --platform 26 build --release -p hestia-app --lib
# or a quick arm64 APK
cargo apk build --release -p hestia-app --lib
```

Release APKs come from `scripts/android-package.sh` (aapt2, zipalign,
apksigner) with `apps/hestia-app/android/` (manifest with the Android TV
entry and banner, icons): it takes `<abi>/libhestia_app.so` for any of
arm64-v8a, armeabi-v7a, x86_64 and x86 and makes one universal APK plus one
per ABI. CI builds the four ABIs (the 32-bit ones compile Skia from source,
which has no prebuilt binaries for them), checks that each library has no
unresolved C++ runtime symbols and packages them.

## Nightly builds

Every push to `main` that passes CI replaces the **`nightly`** pre-release
(Releases page) with the Android APKs (`hestia-rs-android-universal.apk` for
any device, phones, tablets, Android TV and Chromebooks, or the smaller
`hestia-rs-android-<abi>.apk`), a Linux tarball and `SHA256SUMS`. The release notes list the commits since the
previous nightly; Settings → About in the app shows which commit a build is.

So that each nightly APK installs over the previous one, CI signs it with a
persistent key: the same `HESTIA_NIGHTLY_KEYSTORE_BASE64`,
`HESTIA_NIGHTLY_STORE_PASSWORD`, `HESTIA_NIGHTLY_KEY_ALIAS` and
`HESTIA_NIGHTLY_KEY_PASSWORD` secrets Hestia Android uses, or
`ANDROID_KEYSTORE_BASE64` / `ANDROID_KEYSTORE_PASSWORD` (without either it
falls back to a throwaway key per build):

```sh
keytool -genkeypair -keystore hestia-nightly.p12 -storetype PKCS12 -alias hestia \
    -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Hestia RS"
base64 -w0 hestia-nightly.p12 | gh secret set ANDROID_KEYSTORE_BASE64 -R MrOz59/hestia-rs
gh secret set ANDROID_KEYSTORE_PASSWORD -R MrOz59/hestia-rs   # the password typed above
```

Keep the keystore somewhere safe: a different key means uninstalling once.

## CLI

```sh
hestia discover                 # scan the LAN
hestia add 192.168.0.10         # add by address
hestia pair cachy               # prints a PIN; enter it in Hermes → PIN
hestia pair cachy --otp 1234 --passphrase hunter2   # Hermes OTP pairing
hestia status cachy             # online/paired/codecs + Hermes capabilities
hestia apps cachy
hestia launch cachy Desktop --width 2560 --height 1440 --fps 120
hestia stream cachy Desktop --width 1280 --height 720 --audio   # headless stream + stats
hestia stream cachy Desktop --snapshot frame.ppm --seconds 5     # save a decoded frame
hestia stream cachy Desktop --dump capture --seconds 10          # raw capture.h264
hestia quit cachy
hestia diagnostics cachy        # Hermes diagnostics JSON
hestia commands cachy           # Hermes server commands
hestia run cachy "Restart Steam" [--yes]
hestia clipboard cachy [--set "text"]
hestia display cachy [--recover [--force]]
```

The CLI and the GUI share the same data directory (`~/.local/share/hestia-rs`,
overridable with `HESTIA_DATA_DIR`), so pairing in one shows up in the other.

Overrides given to `launch`/`stream` (`--width`, `--fps`, `--bitrate`…) apply to
that session only; the saved settings are not changed.

## Tests

`cargo test --workspace` runs, among others, an end-to-end session against a
mock host (RTSP, ENet control, FEC-protected video with losses) and a decode of
a real H.264 clip. `tools/enet-interop/run.sh` checks the ENet client against
the C ENet that hosts use (CI runs it too).

## License

GPL-3.0, like Hermes and the Moonlight protocol code it follows.
