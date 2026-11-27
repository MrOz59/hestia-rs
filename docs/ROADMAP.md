# Roadmap

Where Hestia RS stands, milestone by milestone. **Now:** it streams from
Hermes hosts on Linux and Android, with noticeable lag; finding where that
lag comes from is the current priority.

**Scope:** the host is Hermes and the Linux desktop is Wayland. Working
with Sunshine or Apollo (they share GameStream) or on X11 is a side effect,
not a goal: no item here exists for them.

## Milestone 0 — prototype

- [x] Modular workspace, pure-Rust crypto and TLS
- [x] Discovery, manual add, host store
- [x] PIN and OTP pairing
- [x] App list, box art, launch / resume / quit
- [x] Hermes Hestia API v1 client and session preparation
- [x] Slint UI shell (Linux desktop on Wayland + Android NativeActivity)
- [x] App logic in a toolkit-free crate (`hestia-frontend`) behind a UI port,
      so Slint can be swapped (egui, Qt…) by writing another shell
- [x] CLI
- [x] CI: fmt, clippy, tests, Linux build, Android build (four ABIs),
      emulator install test

## Milestone 1 — first frame

- [x] RTSP handshake (OPTIONS / DESCRIBE / SETUP ×3 / ANNOUNCE / PLAY) with SDP
      generation, plain and encrypted (`rtspenc://`)
- [x] UDP pings (`X-SS-Ping-Payload`) to open the video/audio paths
- [x] Video RTP reassembly + Reed–Solomon FEC (compatible with nanors as Hermes
      uses it), frame depacketizer, IDR requests
- [x] ENet-compatible control stream (start, pings, IDR, termination, rumble/HDR
      messages), verified against the C ENet
- [x] Software H.264 decode (OpenH264) and a video view inside the Slint window
- [x] Streams from a live Hermes host (Linux and Android)
- [ ] **Find and fix the lag seen with Hermes** (latency study: reports, live
      diagnosis, per-stage video latency histograms)
- [x] Desktop GPU colour conversion: OpenH264's YUV planes converted to RGB
      in a GL pass inside Slint's context (no CPU conversion or RGBA copy),
      CPU fallback; OpenH264 built with its SIMD code (nasm) in CI
- [x] Linux VA-API decode (H.264/HEVC/AV1 through cros-codecs, patched for
      low delay): DMA-BUFs into the same GL pass on Wayland (EGL), read-back
      copies otherwise
- [ ] Try VA-API on Intel and AMD hardware: latency against OpenH264,
      compressed surfaces
- [x] Android MediaCodec (H.264/HEVC/AV1) via AImageReader, with output
      buffers (NV12/I420/Qualcomm NV12) and software H.264 as fallbacks
- [x] Android zero-copy output (MediaCodec into GPU buffers, drawn as an
      external texture inside Slint's GL context)

## Milestone 2 — playable

- [x] Audio: AES-CBC, Opus stereo, desktop playback (cpal) with drift control
- [x] Audio on Android: Opus via MediaCodec, AAudio output, surround mixed
      down to stereo
- [x] Input: keyboard, text, absolute mouse, buttons, scroll, touch as a
      trackpad, keyboard bar with shortcuts (Android on-screen keyboard)
- [x] Android: game controllers (up to 4; buttons, sticks, triggers, d-pad)
      with rumble on the controller or the phone; two-finger scroll
- [x] Touch modes (trackpad, mouse, native multi-touch), on-screen
      controller, pinch zoom and pan, stream orientation, custom resolution
- [x] Desktop game controllers (Linux evdev, rumble, hot plug)
- [x] Android TV: launcher banner, remote navigation (grids, forms, stream
      menu), controller A/B in the app; universal APK (all four ABIs)
- [ ] Input: scancode-based keyboard, desktop pointer capture (Wayland
      pointer constraints and relative pointer), stylus / S Pen
- [x] Stream overlay: stats, full screen, keyframe request, disconnect, quit
- [x] Hermes host tools: display recovery, server commands, clipboard,
      permissions, diagnostics (app and CLI)
- [x] Session reports with every log line (share sheet / file / host clipboard)
- [ ] Control-stream loss statistics to the host (adaptive bitrate)
- [x] Hermes clipboard sync during the stream (and one-off send / fetch)
- [x] Named stream profiles, owned by each host (legacy migration included)
- [x] Contextual host settings and host name/address editing
- [x] Configurable local keyboard shortcuts; hide editor without a physical keyboard on Android

## Milestone 3 — polish

- [ ] HDR (10-bit, metadata) end to end
- [x] Host diagnostics (Hermes readiness preflight on the host card and tools
      page, runtime status, full diagnostic report, web UI shortcut)
- [x] Server commands and display recovery in the UI
- [x] Stream provenance banner (Hermes stream info: GPU, encoder, capture)
- [x] Presets (Fast / Balanced / Quality / Battery) from this display's native
      mode; picture scale fit / fill / stretch
- [x] Live bottleneck diagnosis (network / decode / display / host) in the
      overlay and per second in the report
- [x] Wake-on-LAN (tap an offline host)
- [x] Signed nightlies on every push to `main` (universal and per-ABI APKs,
      Linux tarball, UI screenshots)
- [ ] Stable releases, Arch/CachyOS package
- [ ] Android multicast lock / NSD fallback if legacy-unicast mDNS misses hosts

## Later (no timeline)

- [ ] Replace GameStream for Hestia–Hermes sessions with Hermes Datagram
      Transfer (HDT). The receiver-feedback prototype runs on GameStream.
      The [HDT transport migration plan](hdt-feedback.md#transport-migration-plan)
      covers opt-in WebTransport control, media over QUIC and legacy fallback.
