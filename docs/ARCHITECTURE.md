# Architecture

Hestia RS is split into small crates with one direction of dependencies:

```
      hestia-app (Slint shell)
                |
         hestia-frontend                hestia-cli
                 \                     /
                     hestia-core                      hestia-media
              /      |       |      \                      |
   hestia-nvhttp  hestia-api  hestia-discovery        hestia-stream
         |            |
   hestia-crypto  (hestia-nvhttp)
```

(`hestia-frontend` also uses `hestia-media` directly, for decoder selection
and video presentation.)

## Principles

- **Pure Rust protocol stack.** No OpenSSL, no `moonlight-common-c`: the same
  code cross-compiles to Android without native dependencies. TLS is rustls
  (ring), RSA/AES/SHA come from RustCrypto.
- **UI-agnostic core, toolkit behind a port.** `hestia-core` exposes blocking,
  thread-safe calls; front ends run them on worker threads and never see wire
  formats. The graphical client's logic lives in `hestia-frontend`, which knows
  no toolkit; Slint is a thin shell over it, so the toolkit can be swapped
  without touching the app ([FRONTENDS.md](FRONTENDS.md)).
- **Optional Hermes.** Every Hermes extension is probed first and falls back to
  plain GameStream behaviour, like [Hestia](https://github.com/MrOz59/Hestia)
  (the desktop client) does.
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
the AES key material the streaming layer needs;
`LaunchedSession::stream_config()` turns it into a `StreamConfig` (session
URL, codec modes, audio layout, LAN/WAN packet size, codec preference).
`ManagedStream` owns the host preparation and transport session. It exposes
input, keyframes, negotiated media, shutdown and final counters to the front
ends.

Alongside: stream settings and named profiles (`settings`, `profiles`),
presets from the display's native mode (`presets`, with an EDID parser), the
per-second bottleneck classifier (`diagnosis`), stream reports (`report`), the
log capture that puts every log line into them (`logcapture`), and
Wake-on-LAN (`wol`).

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
  nanors (solving only for the missing shards), frame reassembly, waiting for
  an IDR after unrecoverable loss, and a watchdog that probes the host when
  video stops arriving.
- `audio`: RTP Opus receiver, optional AES-CBC, loss signalled to the sink.
- `input`: event model → input packets on per-class channels.
- `session`: `StreamSession` brings it all up in order and reports
  `StreamEvent`s (stages, first frame, stats, termination).

### hestia-media
Backends for the sink traits. `DecodingSink` runs a `FrameDecoder` on its own
thread and hands each `DecodedFrame` to a `FramePresenter`. A decoded frame
owns either reusable RGBA pixels or a `NativePicture`: something the window's
GPU renderer draws itself: an Android hardware buffer, 4:2:0 planes from a
software decoder (`PlanarPicture`, whose storage returns to the decoder's pool
when dropped), or a VA-API NV12 picture shared as a DMA-BUF
(`DmabufPicture`, which keeps the frame away from the decoder while it
lives). Each keeps its own release rules, so the
decoder thread and mailbox move pictures without knowing their kind. A
`GpuOutput` tells a decoder whether that renderer is available right now
and whether it imports DMA-BUFs (`GpuRendererStatus`, set by the renderer,
which also names its GPU's render node); decoders check it per frame and
fall back to planes or RGBA. Queue overflow and decoder errors use the same
keyframe recovery policy. Backends include OpenH264 (planes for the GPU, or
RGBA through the SIMD converters in `color`), Android MediaCodec, Linux
VA-API (`vaapi`, feature `vaapi`: cros-codecs' stateless decoders over libva,
decoding into exported surfaces it shares as DMA-BUFs), Opus + cpal
playback, Android audio, and null and dump sinks. Surround playback is
planned.

The GUI and CLI use `video_decoder::select_decoder_for_renderer` (or
`select_decoder`, without a renderer) to pair a decoder with the formats it
can advertise. The hardware probe is supplied at session startup. The
Android GUI matches codecs to the requested size and rate and passes its
native output; the Linux GUI probes VA-API on the renderer's GPU and passes
its output to it and to OpenH264; headless callers can use the default probe
(VA-API with RGBA read-back on Linux). The selector uses OpenH264 when hardware
is disabled or unavailable. [VIDEO.md](VIDEO.md) follows a frame through all
of this.

Audio playback uses a shared `PlaybackSink` to connect an `AudioDecoder`
to a `PcmOutput`. Desktop composes libopus with cpal, and Android composes
MediaCodec with AAudio. The sink owns the FIFO, stereo downmix, and session
lifecycle. Output callbacks only read PCM through a `PcmReader`, which
keeps the same buffering and underrun behavior with either decoder.

### hestia-frontend
The graphical client without a UI toolkit, so the toolkit can be swapped
(see [FRONTENDS.md](FRONTENDS.md)). The `Controller` owns the core; its
public methods are the user's intents, called by the shell on the UI thread,
and no network call ever runs there. Results reach the screen through the
port in `ui.rs`: `UiHandle` queues closures for the UI thread (now or after
a delay), and `View` is what the screens show, as plain Rust values (strings,
numbers, `HostCard`/`AppCard`/… structs, a `SettingsForm`).

`presentation.rs` connects the decoder to the window. Its `UiPresenter`
feeds a `PictureMailbox` (in `hestia-media`, shared by the decoder, the UI
thread and the native renderer). The mailbox keeps the newest pending
picture, coalesces display callbacks, returns CPU storage to the decoder
after upload, and releases replaced native pictures outside its lock. RGBA
pictures go to `View::show_video_frame`; native ones only ask for a redraw,
and the shell's GPU renderer takes them from the `Presentation` that
`GpuWindow` marks active. `gl_yuv.rs` is the toolkit-free GL pass that
converts the desktop's YUV planes: it loads GL through a
`get_proc_address`, works with OpenGL 3 and OpenGL ES 3 contexts, restores
the GL state it touches, and its test runs headless on Mesa. It also draws
NV12 pictures held in EGL images, which `gl_dmabuf.rs` makes from
`DmabufPicture`s on the window's EGL display (loaded from `libEGL.so.1` at
run time). The decoder
receives renderer availability explicitly at construction by
`video_backend.rs`.

`stream_input.rs` owns the per-session touch and controller gesture state and
converts input into protocol events. System services are grouped under
`platform/` by capability, including display, keyboard, clipboard,
controllers, and Android network routing. Android's service adapters share
the activity (handed over by the shell) and the JNI helper, whose Java side
(`java/HestiaHelper.java`) this crate's build script compiles into a dex.

### hestia-app
The Slint shell: the screens (`ui/*.slint`, fluent-dark base style with a
custom theme), `slint_ui.rs` (the `View` over the generated `AppWindow`, the
`UiHandle` over Slint's event loop, and the callbacks wired to the
controller's intents), and the GPU renderers that need Slint's rendering
notifier. Both draw native pictures into one RGBA texture the stream view
shows as a borrowed texture:

- `gpu_render.rs` (Android) imports the hardware picture, draws it without a
  CPU copy, and releases it with a GPU fence.
- `gl_video.rs` (desktop) runs `gl_yuv`'s pass with Slint's GL context.

The same crate builds a desktop binary and an Android `cdylib`
(`android_main`, which also installs the patched backend's controller hook).

### hestia-cli
`hestia` binary over the same core, for scripting and for testing hosts without
the GUI: pairing, apps, launch, a headless stream with per-second statistics
(software decoding, a frame snapshot, or the raw elementary stream), the
Hermes tools, and the same stream reports and data directory as the app.

## Outside the crates

- `third_party/i-slint-backend-android-activity`: Slint's Android backend
  (pinned to the Slint version in the root `Cargo.toml`, used through
  `[patch.crates-io]`) with three changes listed in its `PATCHES.md`: an
  input hook for game controllers, a mouse-wheel fix, and remote/controller
  keys for the UI.
- `third_party/cros-codecs`: ChromeOS's codec crate (path dependency of
  `hestia-media`), its H.264 and H.265 decoders given a low-delay mode, and
  cros-libva taken from git for libva 2.23; its `PATCHES.md` lists the
  changes.
- `tools/enet-interop`: the ENet client against the C ENet hosts use, over a
  clean and a lossy link (CI runs it). `tools/fec-vectors`: reference parity
  from nanors that `fec.rs`'s tests compare against.
- `scripts/`: Android packaging (universal and per-ABI APKs) and the
  install/update check CI runs on an emulator.
