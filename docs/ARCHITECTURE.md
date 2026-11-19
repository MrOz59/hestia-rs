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
  `VideoSink` / `AudioSink` traits; decoders (MediaCodec, VA-API, software) are
  backends behind them.

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
the AES key material the streaming layer needs.

### hestia-stream (in progress)
Pure-Rust data plane. Done: session URL parsing, RTSP message codec, AES-GCM
framing of encrypted RTSP, the input event model, sink traits. Next: the RTSP
handshake and SDP, then the ENet control stream, video RTP + Reed–Solomon FEC,
audio. See the table in `crates/hestia-stream/src/lib.rs`.

### hestia-media (in progress)
Backends implementing the sink traits. Today: a null sink with counters and an
elementary-stream dump sink. Planned: Android MediaCodec into a `Surface`,
Linux VA-API (DMA-BUF into the renderer), software fallback, Opus + AAudio /
PipeWire.

### hestia-app
Slint UI (`ui/*.slint`, fluent-dark base style with a custom theme). A
`Controller` owns the core and pushes view models to the window through
`upgrade_in_event_loop`; no network call ever runs on the UI thread. The same
crate builds a desktop binary and an Android `cdylib` (`android_main`).

### hestia-cli
`hestia` binary over the same core, for scripting and for testing hosts without
the GUI.
