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
(byte-compatible with nanors, as Hermes uses it), and
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
| Linux desktop | VA-API (H.264, HEVC, AV1), hardware | NV12 DMA-BUFs drawn by the GPU (zero-copy), or read back as YUV planes for the GPU or RGBA |
| Linux desktop | OpenH264, software (fallback, H.264) | YUV planes converted by the GPU, or RGBA on the CPU |

`video_decoder::select_decoder_for_renderer` picks the decoder before the
stream starts and tells the host which formats it can take. "Hardware
decoding" off forces OpenH264 everywhere.

### VA-API (Linux)

`hestia-media`'s `vaapi` module, built with the `vaapi` feature (on by
default in the app and the CLI). cros-codecs, ChromeOS's stateless decoders
(vendored in `third_party/cros-codecs`), parses the bitstream and keeps the
references; libva runs the GPU's decoder. 8-bit 4:2:0 H.264, HEVC and AV1.

* **Probe**, at stream start: the render node of the window's GPU first (EGL
  names it; DMA-BUFs import only within one GPU), then every
  `/dev/dri/renderD*`. A device must decode H.264 Main (cros-codecs starts
  every decoder with it) and let a surface be exported as a DMA-BUF and
  imported back, which decoding into shared memory needs; NVIDIA's VA-API
  driver can't import, so NVIDIA falls back to OpenH264. The formats offered
  to the host are the codecs the device decodes.
* **Memory**: once the stream's size is known, the driver allocates the NV12
  surfaces (the codec's minimum plus four: one being decoded into, one
  waiting for the window, one drawn, one kept until the next draw), and each
  is exported once as a DMA-BUF. cros-codecs decodes into frames that import
  those DMA-BUFs back, so every picture is in memory the window can sample.
  A frame goes back to the pool when the decoder no longer needs it as a
  reference and the window has let it go.
* **Latency**: upstream cros-codecs finishes a picture only when the next
  one starts and outputs it only when its DPB needs the room (one to
  sixteen frames late for a game stream). The vendored copy has a
  low-delay mode, on here: each access unit is finished as soon as it is
  fed, and each picture leaves as soon as it is decoded (hosts send no
  B-frames, so decoding order is output order). Decoding is blocking: the
  picture is complete when `decode` returns.
* **Output**, per picture:
  * **zero-copy** (`VA-API (hardware, zero-copy)`): when the window imports
    DMA-BUFs, the picture goes to it as a `DmabufPicture` (section 4);
  * **copied, GPU colour**: when the window draws planes but cannot import
    (X11 with GLX, or an import that failed), the picture is read back
    (`vaGetImage`) and its chroma split into I420 planes;
  * **copied, CPU colour**: without a GPU renderer (the CLI, the software
    renderer, Settings → GPU colour conversion off), read back and converted
    to RGBA.
* **Failures**: decode errors go through the usual keyframe recovery; a
  reset flushes the decoder, which resumes at the next keyframe. If the
  device cannot be opened mid-stream, or cros-codecs panics, the stream
  stops with the reason (no switch to software mid-stream).

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
    bilinearly.
  * Linux, VA-API pictures (`gl_dmabuf.rs`): the DMA-BUF becomes two EGL
    images, `R8` for luma and `GR88` for the interleaved chroma (with the
    buffer's format modifier, and Intel's compression planes when there are
    any), bound to two textures; the same kind of pass converts them, its
    sampling kept inside the picture when the buffer is larger (1088 rows
    for 1080p). Nothing is uploaded. The last picture drawn is kept until
    the next draw, so its buffer returns to the decoder only after the GPU
    has read it. EGL is loaded from `libEGL.so.1` at run time; a window that
    doesn't render through EGL, or an import that fails, turns DMA-BUF
    import off and VA-API copies its pictures instead. Needs OpenGL 3.0 or OpenGL ES 3.0; Slint's default FemtoVG
    renderer on winit asks for ES 2.0 and Mesa, NVIDIA and AMD drivers hand
    out 3.x. With the software renderer (`SLINT_BACKEND=winit-software`) or
    Qt, or an older context, the desktop converts on the CPU.

Settings → Decoding → **Zero-copy video** (Android) / **GPU colour
conversion** (desktop) is the same setting (`gpu_video`); off forces the CPU
path, for comparison or to work around a driver.

## 5. Cost per 1080p frame (desktop)

Measured on a 2-core 2.1 GHz x86-64 CPU (AVX2) with an 18 Mbit/s 1080p60
High-profile stream; a desktop CPU is faster, but the proportions hold.

| Step | GPU colour (default) | CPU colour |
| --- | --- | --- |
| OpenH264 decode | 12 ms (16 ms without SIMD) | the same |
| YUV → RGB | in the shader, while drawing | 1.6 ms on the decoder thread |
| UI thread | 3 MB of planes into persistent textures | 8 MB copied into a Slint image, then uploaded into a new texture |

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
  * *codec output to frame ready*: CPU colour conversion, the plane copy
    with GPU colour, or VA-API's read-back when it copies (nothing when it
    hands the DMA-BUF over);
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

### Chasing lag

Lag with Hermes is the open problem (see the ROADMAP). A report splits the
picture's way into parts that can be compared:

* **Host**: each per-second line carries the host's capture-to-send time
  (`host … ms`, from the frame headers) when the host reports it.
* **Network**: `rtt` and its variance, FEC recoveries and lost frames, and
  *first packet to decode queue* (how long a frame takes to arrive whole).
* **Client**: *decoder queue*, *codec input to output* and *codec output to
  frame ready*, then *… to Slint handoff* or *… to GPU draw submission*.
  *First packet to Slint handoff* / *first packet to GPU draw submission*
  add these up, from the frame's first packet to the moment it is handed to
  the toolkit.

What the report doesn't see: one-way network delay (only the round trip),
input's way back to the host, and the display's own delay after the toolkit
draws (compositor, vsync, the screen).

## 7. Next

* VA-API on real hardware: latency next to OpenH264, compressed (Intel
  CCS) surfaces, AMD; then keeping each buffer's EGL images instead of
  importing per picture.
* HDR: 10-bit formats and BT.2020/PQ end to end.
