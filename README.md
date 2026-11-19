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
- **Decoding**: OpenH264 software decoding on every platform (H.264 only for
  now), on its own thread, with frames handed to the UI newest-first so a slow
  renderer drops frames instead of adding latency.
- **Stream screen**: video letterboxed in the window, keyboard (US-layout key
  mapping, text fallback for other characters), absolute mouse, buttons,
  scroll, touch as mouse, stats overlay, full screen, disconnect / quit.
  `Ctrl+Alt+Shift+Q` disconnects, `Ctrl+Alt+Shift+S` toggles statistics.
- **Audio**: stereo Opus playback through cpal on desktop Linux.
- **UI** in Slint (dark theme, responsive grid) for desktop Linux and Android
  (NativeActivity), and a `hestia` CLI for scripting and testing.

### Known limits of the prototype

- H.264 only (software). HEVC/AV1, VA-API and MediaCodec decoders are next.
- No audio on Android yet; surround layouts are not played (stereo only).
- Keys are mapped by the character they produce (US layout), not by physical
  scancode; characters without a US key are sent as text.
- Mouse is absolute only (no pointer capture / relative mode), no gamepads in
  the UI yet.

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

### Android (arm64)

Use **NDK r30** (or whichever NDK matches skia-bindings' prebuilt Skia, see
the CI workflow): Slint renders with Skia on Android, and an older NDK's libc++
lacks symbols the prebuilt library needs, so the app builds but fails to load
on the device. Point `ANDROID_NDK_HOME` / `ANDROID_NDK_ROOT` at it.

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk cargo-apk
# native library only
cargo ndk -t arm64-v8a --platform 26 build --release -p hestia-app --lib
# or an installable APK
cargo apk build --release -p hestia-app --lib
```

CI builds both and uploads the Linux binaries, the Android library and (best
effort) an APK as workflow artifacts. It also checks that `libhestia_app.so`
has no unresolved C++ runtime symbols. The CI APK is signed with a throwaway
key per run: uninstall the previous one before installing a new build.

## Nightly builds

Every push to `main` that passes CI replaces the **`nightly`** pre-release
(Releases page) with an Android APK (`hestia-rs-android-arm64.apk`), a Linux
tarball and `SHA256SUMS`. The release notes list the commits since the
previous nightly; Settings → About in the app shows which commit a build is.

So that each nightly APK installs over the previous one, CI signs it with the
key in the `ANDROID_KEYSTORE_BASE64` / `ANDROID_KEYSTORE_PASSWORD` repository
secrets (without them it falls back to a throwaway key per build):

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
