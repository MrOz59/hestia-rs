# Architecture

Hestia RS is split into small crates with one direction of dependencies:

```
          hestia-app (Slint)      hestia-cli
                    \              /
                     hestia-core                      hestia-media
              /      |       |      \                      |
   hestia-nvhttp  hestia-api  hestia-discovery        hestia-stream
         |            |
   hestia-crypto  (hestia-nvhttp)
```

## Principles

- **Pure Rust protocol stack.** No OpenSSL, no `moonlight-common-c`: the same
  code cross-compiles to Android without native dependencies. TLS is rustls
  (ring), RSA/AES/SHA come from RustCrypto.
- **UI-agnostic core.** `hestia-core` exposes blocking, thread-safe calls. Front
  ends run them on worker threads and never see wire formats, so the Slint UI
  could be replaced (or joined by another shell) without touching protocol code.
- **Optional Hermes.** Every Hermes extension is probed first and falls back to
  plain GameStream behaviour, like Hestia does.
- **Swappable media.** Streaming produces `EncodedFrame`s and Opus packets into
  `VideoSink` / `AudioSink` traits. Decoding produces a `DecodedFrame` that
  owns its CPU or native GPU buffer until presentation.

## Crates

### hestia-crypto
Client identity: RSA-2048 key, self-signed X.509 v3 certificate built with a
tiny DER writer (`der.rs`), PEM helpers, and the pairing primitives (salted PIN
key, AES-128-ECB, SHA-1/SHA-256 by host generation, signature verification,
Apollo/Hermes OTP proof).

### hestia-nvhttp
GameStream HTTP control plane. A small HTTP/1.1 client over `std::net` +
rustls (one request per connection, as hosts expect), a forgiving XML reader,
and typed calls: `serverinfo` (HTTPS when paired, HTTP fallback), the five-stage
pairing handshake (ported from Moonlight's `NvPairingManager`), `applist`,
`appasset`, `launch`/`resume` (with `corever=1` for encrypted RTSP, Apollo's
`virtualDisplay`/`scaleFactor`) and `cancel`.

The host certificate obtained during pairing is pinned byte-for-byte by a custom
rustls verifier; our client certificate authenticates every HTTPS request,
including the Hestia API on the web port (HTTP port + 1).

### hestia-api
Typed client for `/api/hestia/v1/*`: capabilities, session prepare/stop, display
status/recovery, client permissions, diagnostics, clipboard, server commands.
JSON parsing is lenient (defaults for missing fields, unknown fields ignored).

### hestia-discovery
mDNS browsing for `_nvstream._tcp.local` using legacy-unicast one-shot queries
(RFC 6762 §6.7): queries go to 224.0.0.251:5353 from an ephemeral port and the
responder (Avahi on Hermes hosts) replies by unicast. No port-5353 bind next to
the system responder, and no Android `MulticastLock`.

### hestia-core
The facade the apps use: `HestiaClient::open(data_dir)` loads or creates the
identity and loads hosts and settings (JSON, atomic writes). Host records are
keyed by the host's unique id once known and merged when the same PC is both
discovered and added by hand. `launch()` prepares a Hermes session when
available, then launches or resumes, and returns a `LaunchedSession` carrying
the AES key material the streaming layer needs; `LaunchedSession::
stream_config()` turns it into a `StreamConfig` (session URL, codec modes,
  audio layout, LAN/WAN packet size, codec preference). `ManagedStream` owns
  the host preparation and transport session. It exposes input, keyframes,
  negotiated media, shutdown and final counters to the frontends.

### hestia-stream
Pure-Rust data plane, following moonlight-common-c's Sunshine path:

- `rtsp` + `sdp`: the handshake over TCP (AES-GCM framed for `rtspenc://`),
  DESCRIBE parsing (codecs, feature flags, encryption, Opus layout) and the
  ANNOUNCE body with every key Hermes reads.
- `enet`: a small ENet 1.3 client (cgutman fork wire format) — connect,
  reliable and unsequenced sends, acks, RTT-based retransmission, fragments.
- `control`: the encrypted control stream (START A/B, 100 ms pings, IDR
  requests, input; termination, rumble, HDR from the host).
- `video` + `fec`: RTP/NV video packets, Reed–Solomon recovery identical to
  nanors, frame reassembly, waiting for an IDR after unrecoverable loss.
- `audio`: RTP Opus receiver, optional AES-CBC, loss signalled to the sink.
- `input`: event model → input packets on per-class channels.
- `session`: `StreamSession` brings it all up in order and reports
  `StreamEvent`s (stages, first frame, stats, termination).

### hestia-media
Backends for the sink traits. `DecodingSink` runs a `FrameDecoder` on its own
thread and hands each `DecodedFrame` to a `FramePresenter`. A decoded frame
owns either reusable RGBA pixels or a `NativePicture`. Platform buffers live
inline in that type and retain their own release rules. The decoder thread and
mailbox move the buffer without depending on Android's image type. Queue overflow
and decoder errors use the same keyframe recovery policy. Backends include
OpenH264, Android MediaCodec, Opus + cpal playback, Android audio, and null and
dump sinks. Linux VA-API and surround playback are planned.

The GUI and CLI use `video_decoder::select_decoder` to pair a decoder with
the formats it can advertise. The hardware probe is supplied at session startup.
The Android GUI matches codecs to the requested size and rate and passes its
native output, while headless callers can use the default probe. The selector
uses OpenH264 when hardware is disabled or unavailable.

Audio playback uses a shared `PlaybackSink` to connect an `AudioDecoder`
to a `PcmOutput`. Desktop composes libopus with cpal, and Android composes
MediaCodec with AAudio. The sink owns the FIFO, stereo downmix, and session
lifecycle. Output callbacks only read PCM through a `PcmReader`, which
keeps the same buffering and underrun behavior with either decoder.

### hestia-app
Slint UI (`ui/*.slint`, fluent-dark base style with a custom theme). A
`Controller` owns the core and pushes view models to the window through
`upgrade_in_event_loop`; no network call ever runs on the UI thread. The same
crate builds a desktop binary and an Android `cdylib` (`android_main`). The
stream screen (`ui/stream.slint`) takes input and shows frames from
`presentation.rs`. That adapter maps `PictureMailbox` updates to Slint image
uploads and native redraws. The mailbox lives in `hestia-media` and is shared
by the decoder, event loop, and native renderer. It keeps the newest pending
picture, coalesces display callbacks, and returns CPU storage to the decoder
after upload. Replaced native pictures are released outside its lock. The
Android renderer imports the hardware picture, draws it without a CPU copy,
and releases it with a GPU fence. The decoder receives renderer availability
explicitly at construction by `video_backend.rs`.

`stream_input.rs` owns the per-session touch and controller gesture state and
converts input into protocol events. The controller handles UI updates, timers,
and delivery to the session. System services are grouped under `platform/`
by capability, including display, keyboard, clipboard, controllers, and Android
network routing. Android's service adapters share the activity and JNI helper.

### hestia-cli
`hestia` binary over the same core, for scripting and for testing hosts without
the GUI.
