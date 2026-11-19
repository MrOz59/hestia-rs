# Roadmap

## Milestone 0 — prototype (this commit)

- [x] Modular workspace, pure-Rust crypto and TLS
- [x] Discovery, manual add, host store
- [x] PIN and OTP pairing
- [x] App list, box art, launch / resume / quit
- [x] Hermes Hestia API v1 client and session preparation
- [x] Slint UI shell (Linux desktop + Android NativeActivity)
- [x] CLI
- [x] CI: fmt, clippy, tests, Linux build, Android build

## Milestone 1 — first frame

- [ ] RTSP handshake (OPTIONS / DESCRIBE / SETUP ×3 / ANNOUNCE / PLAY) with SDP
      generation, plain and encrypted (`rtspenc://`)
- [ ] UDP pings (`X-SS-Ping-Payload`) to open the video/audio paths
- [ ] Video RTP reassembly + Reed–Solomon FEC (compatible with nanors as used by
      Sunshine/Hermes), frame depacketizer, IDR requests
- [ ] ENet-compatible control stream (start, loss stats, IDR, termination)
- [ ] Linux decode (VA-API, software fallback) and a video view inside the
      Slint window
- [ ] Android MediaCodec into a Surface

## Milestone 2 — playable

- [ ] Audio: AES-CBC, FEC, Opus multistream, PipeWire / AAudio output
- [ ] Input: keyboard, mouse (relative/absolute), scroll, gamepads (with rumble),
      touch and stylus / S Pen
- [ ] Stream overlay: stats, quit, keyboard toggle on Android
- [ ] Hermes clipboard sync during the stream

## Milestone 3 — polish

- [ ] HDR (10-bit, metadata) end to end
- [ ] Host diagnostics page (Hermes runtime status, readiness preflight)
- [ ] Server commands and display recovery in the UI
- [ ] Wake-on-LAN
- [ ] APK signing and release workflow, Arch/CachyOS package
- [ ] Android multicast lock / NSD fallback if legacy-unicast mDNS misses hosts
