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
- [ ] Android MediaCodec into a Surface

## Milestone 2 — playable

- [x] Audio: AES-CBC, Opus stereo, desktop playback (cpal) with drift control
- [ ] Audio: surround (multistream), Android AAudio output
- [x] Input: keyboard, text, absolute mouse, buttons, scroll, touch-as-mouse
- [ ] Input: scancode-based keyboard, relative mouse / pointer capture,
      gamepads (with rumble), native touch and stylus / S Pen, Android keyboard
- [x] Stream overlay: stats, full screen, disconnect, quit
- [ ] Control-stream loss statistics to the host (adaptive bitrate)
- [ ] Hermes clipboard sync during the stream

## Milestone 3 — polish

- [ ] HDR (10-bit, metadata) end to end
- [ ] Host diagnostics page (Hermes runtime status, readiness preflight)
- [ ] Server commands and display recovery in the UI
- [ ] Wake-on-LAN
- [ ] APK signing and release workflow, Arch/CachyOS package
- [ ] Android multicast lock / NSD fallback if legacy-unicast mDNS misses hosts
