# Video pipeline

How a frame travels from the host to the screen in Hestia RS, what each
platform does differently, which settings change it, and how to tell from a
stream report where time goes.

```
 host ──UDP──▶ hestia-stream ──EncodedFrame──▶ DecodingSink ──DecodedFrame──▶ presenter ──▶ screen
              (RTP, FEC,                       (queue, decoder   (RGBA or        (Slint image,
               reassembly)                      thread)           native)         or GPU pass)
```

## 1. Network to access unit (`hestia-stream`)

`video.rs` receives the RTP shards on their own thread, decrypts them when
the session is encrypted, recovers lost shards with Reed–Solomon FEC
(byte-compatible with nanors, as Sunshine and Hermes use it), and
reassembles one access unit per frame. A frame that cannot be recovered is
dropped and everything after it is skipped until the next IDR, which is
requested over the control stream (no reference frame invalidation yet).

Each complete frame becomes an `EncodedFrame` (Annex B bytes, frame number,
keyframe flag, when its first packet arrived, and when an IDR was requested
for it) handed to the session's `VideoSink`.

## 2. Decoder thread (`hestia-media::decode`)

`DecodingSink::submit` only queues the frame; a dedicated `hestia-decode`
thread feeds the decoder. Compressed frames are never dropped for being
late: each is a reference for the next, so dropping one would cost a
keyframe. Bursts (Wi-Fi, VPNs, a large keyframe) are decoded through and only
the newest picture is shown. Only a decoder that cannot keep up at all
(48 frames queued, or the oldest one waiting longer than 300 ms, or eight
frame times at low frame rates) is flushed, and an IDR requested. Decoder errors flush the queue and request an IDR too;
repeated failures space the requests out instead of flooding the host.

## 3. Decoders

| Platform | Decoder | Output |
| --- | --- | --- |
| Android | MediaCodec (H.264, HEVC, AV1), hardware | `AHardwareBuffer`s drawn by the GPU (zero-copy), or read back on the CPU through an image reader or the codec's output buffers |
| Android | OpenH264, software (fallback, H.264) | RGBA on the CPU |
| Linux desktop | OpenH264, software (H.264) | YUV planes converted by the GPU, or RGBA on the CPU |

`video_decoder::select_decoder_for_renderer` picks the decoder before the
stream starts and tells the host which formats it can take. "Hardware
decoding" off forces OpenH264 everywhere.

### OpenH264

Each access unit goes through `DecodeFrameNoDelay`, so the picture comes
out of the same call that fed it: no reordering delay (hosts send no
B-frames). The picture then leaves OpenH264's internal buffers in one of two
ways, chosen per frame:

* **GPU colour** (desktop, the default): the three 4:2:0 planes are copied
  into pooled buffers (1.5 bytes per pixel; the pool means no allocation per
  frame) and handed to the window's GL renderer, which converts them to RGB
  while drawing (section 4).
* **CPU colour**: converted to RGBA in `color.rs` (8.8 fixed point, two
  pixels per chroma sample, eight at a time with SSE2 on x86-64, split over
  up to four threads for large pictures) and uploaded by Slint as an image.

If the GL renderer fails or goes away, the next frame is converted on the
CPU and the stream carries on.

**SIMD.** On x86-64 the `openh264` crate assembles OpenH264's SSE2/SSSE3
routines (motion compensation, IDCT, deblocking) with `nasm`, and quietly
builds the plain C decoder when `nasm` is missing. ARM builds compile their
NEON code with the C compiler and need nothing. The decoder checks what it
got at run time: without SIMD its name reads `OpenH264 (software, no SIMD)`
in the stats overlay and the report, a warning is logged, and the
`hestia-media` build prints a cargo warning when it cannot find `nasm`. After
installing `nasm`, rebuild it: `cargo clean -p openh264-sys2`.

## 4. Presentation

The decoder thread hands each picture to a presenter (`presentation.rs` in
`hestia-frontend`), which keeps only the newest one in a `PictureMailbox`
shared with the UI thread. A newer picture replaces one that was not shown
yet (counted as "replaced before handoff" / "replaced before draw"). The
renderers below belong to the Slint shell (`apps/hestia-app`); another UI
toolkit would provide its own (see [FRONTENDS.md](FRONTENDS.md)).

* **RGBA pictures** go to the shell (`View::show_video_frame`); Slint copies
  them into an image on the event loop, then creates a texture for that image
  and uploads it, every frame.
* **Native pictures** only request a redraw. The window's GPU renderer takes
  the newest one inside Slint's rendering notifier, with Slint's GL context
  current, and draws it into one RGBA texture that the stream view keeps
  showing as a borrowed texture. Every bit of GL state it changes is put
  back afterwards.
  * Android (`gpu_render.rs`): the hardware buffer becomes an `EGLImage` on
    an external texture; the GPU's sampler does the YUV to RGB conversion.
    The buffer goes back to MediaCodec with a GPU fence.
  * Desktop (`gl_video.rs`, running the frontend's `gl_yuv.rs`): the planes
    are uploaded into three persistent single-channel textures and one
    shader pass applies the colour matrix (`Colorimetry::gpu_conversion`). Chroma is upsampled
    bilinearly. Needs OpenGL 3.0 or OpenGL ES 3.0; Slint's default FemtoVG
    renderer on winit asks for ES 2.0 and Mesa, NVIDIA and AMD drivers hand
    out 3.x. With the software renderer (`SLINT_BACKEND=winit-software`) or
    Qt, or an older context, the desktop converts on the CPU.

Settings → Decoding → **Zero-copy video** (Android) / **GPU colour
conversion** (desktop) is the same setting (`gpu_video`); off forces the CPU
path, for comparison or to work around a driver.

## 5. Cost per 1080p frame (desktop)

Measured on a 2.1 GHz Xeon (2 cores, AVX2) with an 18 Mbit/s 1080p60
High-profile stream; a desktop CPU is faster, the proportions hold.

| Step | Before | Now |
| --- | --- | --- |
| OpenH264 decode | 16 ms (C build, CI had no `nasm`) | 12 ms (SIMD) |
| YUV → RGBA, decoder thread | 10 ms on one thread (~7 ms on two) | none (GPU colour); 1.6 ms on one thread when on the CPU |
| Copy into a Slint image, UI thread | 8 MB copy | none (GPU colour) |
| Texture upload, UI thread | 8 MB into a new texture every frame | 3 MB into persistent textures |

At 1440p and 4K everything but decoding scales with the pixel count, so the
GPU path matters even more there.

## 6. Reading a stream report

The report's video sections, from the network to the screen:

* **Video transport**: packets, frames assembled, frames lost, FEC
  recoveries, IDR requests by cause.
* **Decoder totals**: the decoder in use as it calls itself (fallbacks and
  the GPU/CPU colour switch show up here), decoded, dropped, errors, resets,
  backlog drops, mean decode time (decode plus CPU colour conversion).
* **Video latency**, as p50/p95/p99 from histograms:
  * *first packet to decode queue*: network reassembly;
  * *decoder queue*: waiting for the decoder thread;
  * *codec input to output*: the decoder itself;
  * *codec output to frame ready*: CPU colour conversion, or the plane copy
    with GPU colour;
  * *RGBA ready to Slint handoff* / *GPU picture ready to draw submission*:
    waiting for the UI thread or the next frame drawn;
  * *IDR request to first …*: how long a recovery took.
* **Video delivery** / **GPU video**: how many pictures went each way and how
  many were replaced before being shown.
* **Settings**: whether hardware decoding and the GPU path were on.

The live diagnosis (stats overlay, and per second in the report) blames
**decode** when decoding plus conversion takes more than 60% of the frame
time, and **display** when decoded pictures are replaced before being shown
(more than 2%).

## 7. Next

* Linux VA-API: hardware H.264/HEVC/AV1 decoding, its NV12 surfaces exported
  as DMA-BUFs and imported into the same GL pass (no CPU copy at all).
* HDR: 10-bit formats and BT.2020/PQ end to end.
