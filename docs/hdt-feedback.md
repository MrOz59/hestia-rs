# Hermes Datagram Transfer — experimental receiver feedback v1

HDT starts with one negotiated client-to-host message for WAN streaming. It lets a future Hermes sender distinguish FEC repairs from frames the receiver actually lost and see how those counters change alongside RTT. Video, audio, input and recovery continue to use the existing session. This version introduces no new port, pairing flow or media packet format.

HDT is intended to replace GameStream for Hestia–Hermes sessions. The [transport migration plan](#transport-migration-plan) describes how the feedback experiment leads into a standalone protocol.

## Trying it in Hestia RS

Tap **Settings → About** seven times. Turn on **Debug → HDT WAN feedback**, then start a stream. The switch applies to new sessions on Linux and Android, including LAN sessions for testing. Hiding Debug or restarting the app clears it. When requested, the stream report includes **Negotiated → HDT WAN feedback** and the negotiation outcome. Production Hermes needs a corresponding implementation before it can receive these samples. The Hestia loopback test includes a host supporting v1.

## Negotiation

1. In the RTSP DESCRIBE response, the host offers exactly one SDP attribute `a=x-hermes-hdt.feedback:1`. The capability belongs to this session.
2. If Debug opt-in is on and control encryption V2 is negotiated, Hestia echoes the attribute in ANNOUNCE. Missing, repeated, or unknown versions select ordinary streaming without an extra request. The host must accept sessions whose ANNOUNCE omits the attribute.
3. After successful ANNOUNCE and PLAY, the client can send feedback on the existing encrypted control stream. The host must check the negotiated state before consuming it. A lost report needs no response.

## Receiver report

Experimental inner control message type **0x4844**, generic channel 0, ENet unsequenced. The outer control framing follows the existing encrypted control protocol. The inner payload is exactly 64 bytes. Multibyte payload fields are unsigned integers in network byte order.

| Offset | Bytes | Meaning |
| ---: | ---: | --- |
| 0 | 4 | ASCII `HDT1` |
| 4 | 8 | Milliseconds since this client's session statistics timer started, from a monotonic clock |
| 12 | 8 | Video UDP datagrams received from the host IP, before decryption or parsing |
| 20 | 8 | Video UDP payload bytes for those datagrams, including FEC and any encryption prefix |
| 28 | 8 | Assembled frames forwarded to the video sink |
| 36 | 8 | Video packets reconstructed by FEC |
| 44 | 8 | Video frames declared lost by the assembler |
| 52 | 8 | Packets arriving after their frame was declared lost |
| 60 | 2 | Smoothed control RTT in milliseconds |
| 62 | 2 | Control RTT variation in milliseconds |

Both RTT fields are 65535 until a measurement exists. Measured values saturate at 65534. The six counters are session totals sampled from the video receiver. A packet received after its frame's deadline can contribute to both the received and late counters. The frame counter is upstream of decode and display.

The client attempts at most one report per one-second statistics tick, including when video stalls. A delayed tick sends one current sample. There is no retry or catch-up burst. Each report has a timestamp and full totals so a lost report costs one measurement interval. No clocks need to be synchronized.

## Optional bitrate advice

With Debug enabled, Hestia records a suggested video budget in the stream report at the first statistics tick and whenever the suggestion changes. This also works with an existing Hermes or Sunshine host. A local suggestion is a diagnostic estimate. The report states whether the host negotiated advice.

The controller starts at the configured video budget in kbps, using the same units and FEC accounting as `x-ml-video.configuredBitrateKbps`. It samples the receiver roughly once a second. Three consecutive seconds with video traffic and at least 1% lost frames (lost / (forwarded + lost)) cut the target by one fifth. After eight consecutive clean seconds it adds one sixteenth of the starting budget, up to the configured budget. The floor is one quarter of the starting budget. These are experimental tuning values.

A clean interval requires at least four forwarded frames, zero unrecovered frames, at most one repaired packet per hundred received, and measured RTT variation within the greater of 12 ms and one eighth of RTT. An idle stream, heavy FEC repair, or an interval longer than 2.5 seconds clears the streaks. The controller uses RTT variation, so a steady 300 ms path can recover its suggested rate. A zero starting budget disables advice.

To receive advice on the wire, a host offers exactly one additional `a=x-hermes-hdt.rateAdvice:1` alongside feedback v1. The client echoes it only when it has a nonzero budget and feedback v1 negotiated with encrypted control. A feedback-only peer still receives the original 64-byte report. Each stats tick with negotiated advice also carries this independent, unsequenced channel-0 message:

| Offset | Bytes | Inner message type 0x4845, network byte order |
| ---: | ---: | --- |
| 0 | 4 | ASCII `HDA1` |
| 4 | 8 | Client elapsed milliseconds, as in the feedback report |
| 12 | 4 | Absolute suggested video budget, kbps |

The two datagrams may arrive in either order or separately. Accept valid advice independently, keep a separate last-accepted timestamp per session, and ignore duplicate or older advice. Repetition lets the next sample refresh the target after loss. Clamp it to the host and session limits. A suggestion is advisory and carries no claim about the encoder's actual rate.

## Hermes receiver contract

Accept the exact version, type, length and magic on the negotiated session's authenticated control channel. Keep the most recent accepted sample per session. Discard timestamps less than or equal to that sample's timestamp. Reset the baseline on a new session. Use the first sample as a baseline and calculate rates from counter differences divided by the client timestamp difference. Reject counter regressions before unsigned subtraction.

For example, if the host last accepted 10,000 ms / 20 lost frames, missed the next report, then received 12,000 ms / 24 lost frames, it saw **four lost frames across two seconds**. A delayed 11,000 ms report is stale. A gap in feedback alone gives no estimate of video loss. Initial Hermes integration should log these measurements alongside its send rate before using them to change bitrate or FEC.

Run the interoperability test with `cargo test -p hestia-stream hdt_feedback_over_encrypted_control`. It exercises real RTSP and ENet on loopback, a lost shard in each frame, reordered shards, and legacy fallback. WAN and mobile-link measurements are still needed.

## Transport migration plan

The QUIC research proposes WebTransport over HTTP/3 so native and browser clients can speak the same HDT protocol. HTTP connection reuse can already reduce the time to list or launch apps. WebTransport adds reliable streams and time-sensitive datagrams in one session, with QUIC path migration when a client changes networks. [Research and round-trip estimates in issue #33](https://github.com/MrOz59/hestia-rs/issues/33) · [QUIC connection migration](https://www.rfc-editor.org/rfc/rfc9000.html#section-9)

| Stage | Proposed change | How to verify it |
| --- | --- | --- |
| GameStream baseline | Pool HTTP/1.1 connections per paired host and port, enable keep-alive in Hermes for Hestia, and fetch box art with bounded concurrency. Retry a stale connection once for safe reads. | Compare cold and warm refresh, launch and a 20-app library at 100 ms RTT. The figures in #33 are estimates. |
| Opt-in HDT control | Have paired Hermes advertise a WebTransport endpoint and certificate hash. A native client can race connection setup with the existing TCP path, then send the launch once on the selected authenticated connection. Use GameStream if the host lacks HDT or UDP is unreachable. | Exercise a successful handshake, a legacy host, blocked UDP and a rejected host identity. A trust failure must end the attempt. |
| Standalone HDT session | Send session control and input events on reliable streams. Send deadline-sensitive video and audio in datagrams with application-level FEC. Pass receiver loss, repairs and bitrate advice to the encoder while QUIC controls pacing and congestion. | Compare frame loss after FEC, delivered bitrate and input latency with GameStream on the same WAN trace. Test a Wi-Fi to mobile handover during playback before making HDT the default. |

The first Hermes experiment can log v1 feedback alongside the encoder's actual send rate. Those observations give the new transport a baseline. On WebTransport, the application sees media datagrams after QUIC processing. HDT will need to define where it counts payload bytes and which RTT it reports before comparing counters across transports. The SDP attributes and ENet message IDs above apply to the GameStream session. [QUIC datagram congestion control](https://www.rfc-editor.org/rfc/rfc9221.html#section-5.4)

### Shared implementation and pairing

Issue #33 proposes a runtime-independent Rust protocol crate in this workspace. Native Hestia would use `web-transport` with Quinn, rustls/ring and Tokio behind its blocking interface. WASM would use the browser's WebTransport API with local futures. Hermes would link the shared Rust server as a static library through a C ABI and Corrosion. A client/server loopback test could then exercise both ends of the same handshake. [Rust WebTransport backends](https://github.com/moq-dev/web-transport)

Native clients would authenticate with their paired client certificate. A browser would register a non-extractable WebCrypto key during pairing and sign a fresh host challenge. For a self-signed host, the browser needs a trusted source for the certificate hash and a rotating certificate (ECDSA P-256, valid for at most two weeks). How to bootstrap that hash is still open. State-changing operations such as pairing, launch and input must wait for the handshake. Any future use of 0-RTT is limited to replay-safe reads. [WebTransport certificate hashes](https://www.w3.org/TR/webtransport/#verify-a-certificate-hash) · [HTTP early data](https://www.rfc-editor.org/rfc/rfc8470.html)

Issue #33 also tracks the UDP port layout, the choice of congestion controller and the Android runtime cost. These decisions need host and device measurements.

## Design reference

Punktfunk's opt-in protocol extension alongside GameStream informed the negotiation approach. The HDT SDP attribute, message layout and counters are defined here. [Punktfunk project overview](https://git.unom.io/unom/punktfunk/src/commit/249da9beb34e1fe06a0693c5f0ec6169b36e4f63)

Hestia's decision to suggest a lower bitrate after sustained frame loss draws on Punktfunk's documented receiver-side rate decisions. Hestia implements its own controller, and Hermes receivers can use the HDT message layouts, counters and timing rules specified above. [Punktfunk session overview](https://docs.punktfunk.unom.io/docs/developers/sessions)
