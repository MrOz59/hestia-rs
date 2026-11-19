# Hestia RS

A cross-platform game-streaming client for [Hermes](https://github.com/MrOz59/Hermes),
written in Rust: one modular core, a modern [Slint](https://slint.dev) UI, and
Linux and Android as the first targets.

Hestia RS speaks the standard GameStream protocol, so it works with Sunshine and
Apollo hosts too, and it uses Hermes' **Hestia protocol v1** extensions
(capabilities, session preparation, virtual displays, diagnostics, clipboard,
server commands) when the host offers them.

> **Status: prototype.** The control plane is implemented: discovery, pairing,
> app list with box art, Hermes capabilities, launching and quitting apps. It is
> covered by unit tests and CI builds but has not yet been exercised against a
> live Hermes host. The streaming data plane (video, audio, input) is the next
> milestone; see [docs/ROADMAP.md](docs/ROADMAP.md).

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
- **UI** in Slint (dark theme, responsive grid) for desktop Linux and Android
  (NativeActivity), and a `hestia` CLI for scripting and testing.

## Layout

```
crates/
  hestia-crypto      identity (RSA-2048 + self-signed cert), pairing crypto — pure Rust
  hestia-nvhttp      GameStream HTTP(S): serverinfo, pairing, applist, box art, launch/quit
  hestia-api         Hermes "Hestia protocol v1" client (/api/hestia/v1/*)
  hestia-discovery   mDNS discovery (legacy-unicast queries, no multicast lock needed)
  hestia-core        UI-agnostic facade: identity + host store, pairing, apps, launch
  hestia-stream      streaming data plane (RTSP, control, video, audio, input) — WIP
  hestia-media       decoder/renderer backends behind VideoSink/AudioSink traits — WIP
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
# Debian/Ubuntu build deps for Slint
sudo apt install libfontconfig1-dev libxkbcommon-dev libwayland-dev
# Arch/CachyOS
sudo pacman -S --needed fontconfig libxkbcommon wayland

cargo run --release -p hestia-app      # GUI
cargo run --release -p hestia-cli -- --help
```

### Android (arm64)

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk cargo-apk
# native library only
cargo ndk -t arm64-v8a --platform 26 build --release -p hestia-app --lib
# or an installable APK
cargo apk build --release -p hestia-app --lib
```

CI builds both and uploads the Linux binaries, the Android library and (best
effort) an APK as workflow artifacts.

## CLI

```sh
hestia discover                 # scan the LAN
hestia add 192.168.0.10         # add by address
hestia pair cachy               # prints a PIN; enter it in Hermes → PIN
hestia pair cachy --otp 1234 --passphrase hunter2   # Hermes OTP pairing
hestia status cachy             # online/paired/codecs + Hermes capabilities
hestia apps cachy
hestia launch cachy Desktop --width 2560 --height 1440 --fps 120
hestia quit cachy
hestia diagnostics cachy        # Hermes diagnostics JSON
```

The CLI and the GUI share the same data directory (`~/.local/share/hestia-rs`,
overridable with `HESTIA_DATA_DIR`), so pairing in one shows up in the other.

## License

GPL-3.0, like Hermes and the Moonlight protocol code it follows.
