# Hermes Datagram Transfer — experimental receiver feedback v1

HDT starts with one negotiated client-to-host message for WAN streaming. It lets a future Hermes sender distinguish FEC repairs from frames the receiver actually lost and see how those counters change alongside RTT. Video, audio, input and recovery continue to use the existing session. This version introduces no new port, pairing flow or media packet format.

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

## Design reference

Punktfunk's opt-in protocol extension alongside GameStream informed the negotiation approach. The HDT SDP attribute, message layout and counters are defined here. [Punktfunk project overview](https://git.unom.io/unom/punktfunk/src/commit/249da9beb34e1fe06a0693c5f0ec6169b36e4f63)

The client-side decision to request rate changes under persistent damage also draws on the [Punktfunk session overview](https://docs.punktfunk.unom.io/docs/developers/sessions). HDT uses its own counters, timing rules and absolute advisory message.
