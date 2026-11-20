# Roadmap

## Milestone 0 — prototype

- [x] Modular workspace, pure-Rust crypto and TLS
- [x] Discovery, manual add, host store
- [x] PIN and OTP pairing
- [x] App list, box art, launch / resume / quit
- [x] Hermes Hestia API v1 client and session preparation
- [x] Slint UI shell (Linux desktop + Android NativeActivity)
- [x] CLI
- [x] CI: fmt, clippy, tests, Linux build, Android build

## Milestone 1 — first frame

- [x] RTSP handshake (OPTIONS / DESCRIBE / SETUP ×3 / ANNOUNCE / PLAY) with SDP
      generation, plain and encrypted (`rtspenc://`)
- [x] UDP pings (`X-SS-Ping-Payload`) to open the video/audio paths
- [x] Video RTP reassembly + Reed–Solomon FEC (compatible with nanors as used by
      Sunshine/Hermes), frame depacketizer, IDR requests
- [x] ENet-compatible control stream (start, pings, IDR, termination, rumble/HDR
      messages), verified against the C ENet
- [x] Software H.264 decode (OpenH264) and a video view inside the Slint window
- [ ] **Validate against a live Hermes host** (and Sunshine/Apollo)
- [ ] Linux VA-API decode (HEVC/AV1, DMA-BUF into the renderer)
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
- [ ] Input: scancode-based keyboard, desktop pointer capture, stylus /
      S Pen
- [x] Stream overlay: stats, full screen, keyframe request, disconnect, quit
- [x] Hermes host tools: display recovery, server commands, clipboard,
      permissions, diagnostics (app and CLI)
- [x] Session reports with every log line (share sheet / file / host clipboard)
- [ ] Control-stream loss statistics to the host (adaptive bitrate)
- [x] Hermes clipboard sync during the stream (and one-off send / fetch)
- [x] Named stream profiles, chosen per host

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
- [ ] APK signing and release workflow, Arch/CachyOS package
- [ ] Android multicast lock / NSD fallback if legacy-unicast mDNS misses hosts
