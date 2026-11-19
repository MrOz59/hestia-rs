//! Android hardware decoding with `AMediaCodec`.
//!
//! The GPU path samples MediaCodec output directly. If a mode is unavailable
//! or fails during playback, try SDR pictures from the image reader, then
//! the codec's own planar/semi-planar buffers, then OpenH264 for H.264.
//! Low-latency keys keep supported codec components from holding frames back.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use hestia_stream::sink::VideoCodec;
use log::{info, warn};
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

use std::sync::Arc;

use ndk::hardware_buffer::HardwareBufferUsage;

use crate::color::{Colorimetry, Yuv420, yuv420_to_rgba};
use crate::decode::{FrameDecoder, FrameTiming, RgbaFrame};
use crate::gpu_video::{GpuPicture, GpuSession, GpuVideo, SharedReader};
use crate::{EncodedFrame, VideoFormat};

/// How long a call waits for an input buffer, and for its picture.
const INPUT_WAIT: Duration = Duration::from_millis(20);
const OUTPUT_WAIT: Duration = Duration::from_millis(12);
/// Rendering to the image reader is asynchronous; wait this long for it.
const IMAGE_WAIT: Duration = Duration::from_millis(20);
const MAX_PENDING_FRAMES: usize = 32;
/// Try another output after repeated failures, including mid-stream.
const FAILURES_BEFORE_FALLBACK: u32 = 3;
const OUTPUT_STALL: Duration = Duration::from_secs(1);
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;

// MediaCodecInfo.CodecCapabilities colour formats.
const COLOR_YUV420_PLANAR: i32 = 19;
const COLOR_YUV420_PACKED_PLANAR: i32 = 20;
const COLOR_YUV420_SEMI_PLANAR: i32 = 21;
const COLOR_YUV420_PACKED_SEMI_PLANAR: i32 = 39;
const COLOR_YUV420_FLEXIBLE: i32 = 0x7F42_0888;
/// Qualcomm's NV12 with 128-byte rows and 32-row planes.
const COLOR_QCOM_YUV420_SEMI_PLANAR_32M: i32 = 0x7FA3_0C04;

/// Where the codec puts its pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    /// GPU buffers the UI draws directly (zero-copy).
    Gpu,
    /// An image reader's surface, read on the CPU.
    Images,
    /// The codec's own output buffers.
    Buffers,
}

fn mime(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "video/avc",
        VideoCodec::Hevc => "video/hevc",
        VideoCodec::Av1 => "video/av01",
    }
}

/// The codec and the image reader it renders into. Neither is `Send` in
/// the ndk crate, but both may be used from any thread one at a time, and
/// only the decoder thread uses them after setup.
struct Running {
    codec: MediaCodec,
    output: Output,
    /// The image reader and its surface (kept referenced while the codec
    /// renders to it); none with buffer output. Shared with the GPU
    /// pictures still out, which must not outlive it.
    images: Option<(Arc<SharedReader>, NativeWindow)>,
    gpu_session: Option<GpuSession>,
    /// Buffer output's picture layout, from the codec's output format.
    layout: Option<BufferLayout>,
    width: i32,
    height: i32,
    pts_us: u64,
    frame_us: u64,
    pending: VecDeque<(u64, FrameTiming)>,
}

// SAFETY: see the type's documentation.
unsafe impl Send for Running {}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.codec.stop();
    }
}

pub struct MediaCodecDecoder {
    colorimetry: Colorimetry,
    components: Vec<(VideoCodec, Option<String>)>,
    running: Option<Running>,
    /// What `configure` was asked for, to restart the codec differently.
    config: Option<(VideoFormat, u32, u32, u32)>,
    last_output: Option<Instant>,
    last_input: Option<Instant>,
    failures: u32,
    /// Nothing left to try: every decode reports this.
    fatal: Option<String>,
    /// Frames queued behind the current one (see `set_backlog`).
    backlog: usize,
    /// Software decoder used when the hardware one can't be set up or its
    /// pictures can't be read (H.264).
    #[cfg(feature = "openh264")]
    fallback: Option<crate::software_h264::OpenH264Decoder>,
}

impl MediaCodecDecoder {
    /// Standalone probe for consumers that cannot query Java's codec list.
    pub fn probe(colorimetry: Colorimetry) -> Option<Self> {
        let components = [VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Av1]
            .into_iter()
            .filter(|&codec| MediaCodec::from_decoder_type(mime(codec)).is_some())
            .map(|codec| (codec, None))
            .collect();
        Self::new(colorimetry, components)
    }

    pub fn with_components(colorimetry: Colorimetry, components: Vec<(VideoCodec, String)>) -> Option<Self> {
        let components = components.into_iter().map(|(codec, name)| (codec, Some(name))).collect();
        Self::new(colorimetry, components)
    }

    fn new(colorimetry: Colorimetry, components: Vec<(VideoCodec, Option<String>)>) -> Option<Self> {
        (!components.is_empty()).then_some(Self {
            colorimetry,
            components,
            running: None,
            config: None,
            last_output: None,
            last_input: None,
            failures: 0,
            fatal: None,
            backlog: 0,
            #[cfg(feature = "openh264")]
            fallback: None,
        })
    }

    pub fn formats(&self) -> Vec<VideoFormat> {
        let mut codecs: Vec<_> = self.components.iter().map(|(codec, _)| *codec).collect();
        if cfg!(feature = "openh264") && !codecs.contains(&VideoCodec::H264) {
            codecs.push(VideoCodec::H264);
        }
        codecs.into_iter().map(|codec| VideoFormat { codec, ten_bit: false, yuv444: false }).collect()
    }

    fn start(
        format: VideoFormat,
        width: u32,
        height: u32,
        fps: u32,
        output: Output,
        component: Option<&str>,
    ) -> Result<Running, String> {
        let mime = mime(format.codec);
        let create_codec = || {
            match component {
                Some(name) => MediaCodec::from_codec_name(name),
                None => MediaCodec::from_decoder_type(mime),
            }.ok_or_else(|| format!("no decoder for {}", component.unwrap_or(mime)))
        };
        let mut codec = create_codec()?;
        let gpu_session = if output == Output::Gpu {
            Some(GpuVideo::global().active_session().ok_or("GPU renderer unavailable")?)
        } else {
            None
        };
        let (w, h) = (width as i32, height as i32);
        let reader = match output {
            // A few buffers: one being drawn, one waiting, the codec's own.
            Output::Gpu => Some(
                ImageReader::new_with_usage(w, h, ImageFormat::PRIVATE, HardwareBufferUsage::GPU_SAMPLED_IMAGE, 4)
                    .map_err(|e| format!("GPU image reader: {e:?}"))?,
            ),
            Output::Images => {
                Some(ImageReader::new(w, h, ImageFormat::YUV_420_888, 4).map_err(|e| format!("image reader: {e:?}"))?)
            }
            Output::Buffers => None,
        };
        let images = match reader {
            Some(reader) => {
                let window = reader.window().map_err(|e| format!("image reader surface: {e:?}"))?;
                Some((Arc::new(SharedReader(reader)), window))
            }
            None => None,
        };
        let media = |color: Option<i32>| {
            let mut media = Self::media_format(mime, w, h, fps);
            if let Some(color) = color {
                media.set_i32("color-format", color);
            }
            media
        };
        match &images {
            Some((_, window)) => codec
                .configure(&media(None), Some(window), MediaCodecDirection::Decoder)
                .map_err(|e| format!("configure {mime}: {e:?}"))?,
            // NV12 if the codec will convert to it (Codec2 does), else its own.
            None => {
                if codec.configure(&media(Some(COLOR_YUV420_SEMI_PLANAR)), None, MediaCodecDirection::Decoder).is_err() {
                    // A failed configure may leave the component unusable.
                    drop(codec);
                    codec = create_codec()?;
                    codec.configure(&media(None), None, MediaCodecDirection::Decoder)
                        .map_err(|e| format!("configure {mime} for buffer output: {e:?}"))?;
                }
            }
        }
        codec.start().map_err(|e| format!("start {mime}: {e:?}"))?;
        info!("MediaCodec {} {w}x{h}@{fps} started ({output:?} output)", component.unwrap_or(mime));
        Ok(Running {
            codec,
            output,
            images,
            gpu_session,
            layout: None,
            width: w,
            height: h,
            pts_us: 0,
            frame_us: (1_000_000 / u64::from(fps.max(1))).max(1),
            pending: VecDeque::with_capacity(MAX_PENDING_FRAMES),
        })
    }

    fn media_format(mime: &str, w: i32, h: i32, fps: u32) -> MediaFormat {
        let mut media = MediaFormat::new();
        media.set_str("mime", mime);
        media.set_i32("width", w);
        media.set_i32("height", h);
        media.set_i32("max-input-size", (w * h).max(1 << 20));
        media.set_i32("frame-rate", fps.max(1) as i32);
        // Real-time decoding, no reordering delay (Android 11+ and vendors).
        media.set_i32("priority", 0);
        media.set_i32("operating-rate", i32::from(i16::MAX));
        media.set_i32("low-latency", 1);
        for key in [
            "vendor.qti-ext-dec-low-latency.enable",
            "vendor.qti-ext-dec-picture-order.enable",
            "vendor.rtc-ext-dec-low-latency.enable",
            "vendor.low-latency.enable",
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
        ] {
            media.set_i32(key, 1);
        }
        media
    }

    fn start_output(&mut self, outputs: &[Output], reason: &str) -> Result<(), String> {
        let (format, width, height, fps) = self.config.ok_or("decoder not configured")?;
        self.running = None; // Some devices allow only one decoder at a time.
        self.last_output = None;
        self.last_input = None;
        self.failures = 0;
        let mut error = reason.to_owned();
        if let Some((_, component)) = self.components.iter().find(|(codec, _)| *codec == format.codec) {
            for &output in outputs {
                match Self::start(format, width, height, fps, output, component.as_deref()) {
                    Ok(running) => {
                        self.running = Some(running);
                        return Ok(());
                    }
                    Err(e) => {
                        warn!("MediaCodec {output:?} output for {:?} failed: {e}", format.codec);
                        error = e;
                    }
                }
            }
        }
        #[cfg(feature = "openh264")]
        if format.codec == VideoCodec::H264 {
            match crate::software_h264::OpenH264Decoder::new(self.colorimetry) {
                Ok(decoder) => {
                    warn!("MediaCodec unavailable ({error}); decoding H.264 in software");
                    self.fallback = Some(decoder);
                    return Ok(());
                }
                Err(e) => error = e,
            }
        }
        let hint = if format.codec == VideoCodec::H264 { "" } else { " Choose H.264 to try another decoder." };
        let fatal = format!("{:?} decoder failed: {reason} ({error}).{hint}", format.codec);
        self.fatal = Some(fatal.clone());
        Err(fatal)
    }

    fn fall_back(&mut self, why: &str) -> Result<(), String> {
        let outputs: &[_] = match self.running.as_ref().map(|r| r.output) {
            Some(Output::Gpu) => &[Output::Images, Output::Buffers],
            Some(Output::Images) => &[Output::Buffers],
            Some(Output::Buffers) | None => &[],
        };
        warn!("MediaCodec output failed: {why}");
        self.start_output(outputs, why)
    }
}

impl Running {
    /// Queues one access unit; false when the codec had no free input buffer.
    fn feed(&mut self, data: &[u8], timing: FrameTiming, wait: Duration) -> Result<bool, String> {
        let mut input = match self.codec.dequeue_input_buffer(wait) {
            Ok(DequeuedInputBufferResult::Buffer(b)) => b,
            Ok(DequeuedInputBufferResult::TryAgainLater) => return Ok(false),
            Err(e) => return Err(format!("input buffer: {e:?}")),
        };
        let buffer = input.buffer_mut();
        if data.len() > buffer.len() {
            return Err(format!("frame of {} bytes > input buffer {}", data.len(), buffer.len()));
        }
        for (dst, &src) in buffer.iter_mut().zip(data) {
            dst.write(src);
        }
        self.pts_us += self.frame_us;
        self.codec
            .queue_input_buffer(input, 0, data.len(), self.pts_us, 0)
            .map_err(|e| format!("queue input: {e:?}"))?;
        if self.pending.len() == MAX_PENDING_FRAMES {
            self.pending.pop_front();
        }
        self.pending.push_back((self.pts_us, timing));
        Ok(true)
    }

    /// Buffer output: takes every finished picture, converts the newest
    /// into `out` (older ones go back unread) and returns its timing.
    fn drain_buffers(
        &mut self,
        wait: Duration,
        colorimetry: Colorimetry,
        out: &mut RgbaFrame,
    ) -> Result<Option<FrameTiming>, String> {
        let mut timeout = wait;
        let mut newest = None;
        loop {
            match self.codec.dequeue_output_buffer(timeout) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    timeout = Duration::ZERO;
                    // Codec configuration and empty buffers do not contain pictures.
                    if buffer.info().size() <= 0 || buffer.info().flags() & BUFFER_FLAG_CODEC_CONFIG != 0 {
                        self.codec
                            .release_output_buffer(buffer, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                        continue;
                    }
                    if let Some(older) = newest.replace(buffer) {
                        let pts = older.info().presentation_time_us() as u64;
                        self.pending.retain(|(at, _)| *at != pts);
                        self.codec
                            .release_output_buffer(older, false)
                            .map_err(|e| format!("release output: {e:?}"))?;
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    let format = self.codec.output_format();
                    match BufferLayout::from_format(|key| format.i32(key), (self.width, self.height)) {
                        Ok(layout) => {
                            info!("MediaCodec buffer output: {layout:?}");
                            self.layout = Some(layout);
                        }
                        Err(e) => {
                            warn!("MediaCodec buffer output: {e}");
                            self.layout = None;
                        }
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    if let Some(buffer) = newest.take() {
                        let _ = self.codec.release_output_buffer(buffer, false);
                    }
                    return Err(format!("output buffer: {e:?}"));
                }
            }
        }
        let Some(buffer) = newest else {
            return Ok(None);
        };
        let mut timing = self.pending.iter().position(|(at, _)| *at == buffer.info().presentation_time_us() as u64)
            .and_then(|index| self.pending.remove(index)).map(|(_, timing)| timing);
        if let Some(timing) = &mut timing {
            timing.codec_output_at = Instant::now();
        }
        let converted = if timing.is_some() {
            (|| {
                if self.layout.is_none() {
                    let format = self.codec.output_format();
                    self.layout = Some(BufferLayout::from_format(|key| format.i32(key), (self.width, self.height))?);
                }
                let layout = self.layout.as_ref().ok_or("no output buffer layout")?;
                // The NDK reports the amount of valid picture data in BufferInfo.
                let size = buffer.info().size() as usize;
                let data = buffer.buffer().get(..size).ok_or("incomplete decoder output buffer")?;
                layout.convert(data, colorimetry, out)
            })()
        } else {
            Ok(())
        };
        self.codec
            .release_output_buffer(buffer, false)
            .map_err(|e| format!("release output: {e:?}"))?;
        converted?;
        Ok(timing)
    }

    /// Releases every finished picture to the image reader; true if any.
    fn drain(&mut self, wait: Duration) -> Result<bool, String> {
        let mut rendered = false;
        let mut timeout = wait;
        loop {
            match self.codec.dequeue_output_buffer(timeout) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let pts = buffer.info().presentation_time_us() as u64;
                    if let Some((_, timing)) = self.pending.iter_mut().find(|(time, _)| *time == pts) {
                        timing.codec_output_at = Instant::now();
                    }
                    self.codec
                        .release_output_buffer_at_time(buffer, (pts * 1_000) as i64)
                        .map_err(|e| format!("release output: {e:?}"))?;
                    rendered = true;
                    timeout = Duration::ZERO;
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    let format = self.codec.output_format();
                    info!(
                        "MediaCodec output: {}x{} colour format {:?}",
                        format.i32("width").unwrap_or(0),
                        format.i32("height").unwrap_or(0),
                        format.i32("color-format")
                    );
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => return Ok(rendered),
                Err(e) => return Err(format!("output buffer: {e:?}")),
            }
        }
    }

    /// The newest rendered picture, once the image reader has it.
    fn latest_image(&self, wait: bool) -> Result<Option<Image>, String> {
        let Some((reader, _)) = &self.images else {
            return Ok(None);
        };
        let deadline = Instant::now() + if wait { IMAGE_WAIT } else { Duration::ZERO };
        loop {
            match reader.0.acquire_latest_image() {
                Ok(AcquireResult::Image(image)) => return Ok(Some(image)),
                Ok(AcquireResult::NoBufferAvailable) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_micros(500));
                }
                Ok(_) => return Ok(None),
                Err(e) => return Err(format!("acquire image: {e:?}")),
            }
        }
    }
}

fn convert(image: &Image, colorimetry: Colorimetry, fallback: (i32, i32), out: &mut RgbaFrame) -> Result<(), String> {
    let err = |what: &str, e: ndk::media_error::MediaError| format!("{what}: {e:?}");
    let (width, height) = match image.crop_rect() {
        Ok(r) if r.right > r.left && r.bottom > r.top => (r.right - r.left, r.bottom - r.top),
        _ => fallback,
    };
    let plane = |i| image.plane_data(i).map_err(|e| err("plane", e));
    let row = |i| {
        image
            .plane_row_stride(i)
            .map(|s| s as usize)
            .map_err(|e| err("row stride", e))
    };
    let src = Yuv420 {
        y: plane(0)?,
        u: plane(1)?,
        v: plane(2)?,
        y_stride: row(0)?,
        u_stride: row(1)?,
        v_stride: row(2)?,
        uv_pixel_stride: image
            .plane_pixel_stride(1)
            .map(|s| s as usize)
            .map_err(|e| err("pixel stride", e))?,
        width: width as usize,
        height: height as usize,
    };
    if src.y.len() < src.y_stride * (src.height - 1) + src.width {
        return Err("luma plane smaller than the picture".into());
    }
    out.reshape(width as u32, height as u32);
    yuv420_to_rgba(&src, colorimetry, &mut out.data);
    Ok(())
}

impl FrameDecoder for MediaCodecDecoder {
    fn name(&self) -> &'static str {
        #[cfg(feature = "openh264")]
        if self.fallback.is_some() {
            return "OpenH264 (software; MediaCodec failed)";
        }
        match self.running.as_ref().map(|r| r.output) {
            Some(Output::Gpu) => "MediaCodec (hardware, zero-copy to the GPU)",
            Some(Output::Buffers) => "MediaCodec (hardware, buffer output)",
            _ => "MediaCodec (hardware)",
        }
    }

    fn supports(&self, format: VideoFormat) -> bool {
        !format.ten_bit && !format.yuv444 && (
            self.components.iter().any(|(codec, _)| *codec == format.codec)
            || (cfg!(feature = "openh264") && format.codec == VideoCodec::H264)
        )
    }

    fn configure(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String> {
        self.config = Some((format, width, height, fps));
        self.fatal = None;
        let gpu = GpuVideo::global();
        let outputs: &[_] = if gpu.active_session().is_some() {
            &[Output::Gpu, Output::Images, Output::Buffers]
        } else {
            &[Output::Images, Output::Buffers]
        };
        self.start_output(outputs, "could not initialize video output")
    }

    fn terminal_error(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    fn decode(&mut self, frame: &EncodedFrame, out: &mut RgbaFrame) -> Result<bool, String> {
        let now = Instant::now();
        self.decode_timed(frame, out, FrameTiming::new(frame, now, now))
            .map(|t| t.is_some())
    }

    fn set_backlog(&mut self, frames: usize) {
        self.backlog = frames;
    }

    fn reset(&mut self) -> Result<(), String> {
        #[cfg(feature = "openh264")]
        if let Some(software) = &mut self.fallback {
            return software.reset();
        }
        let running = self.running.as_mut().ok_or("decoder not configured")?;
        if let Err(e) = running.codec.flush() {
            return self.fall_back(&format!("could not flush decoder: {e:?}"));
        }
        // Keep timestamps monotonic across resets so an image already handed
        // to the reader cannot be mistaken for an output from the new IDR.
        running.pending.clear();
        self.last_output = None;
        self.last_input = None;
        Ok(())
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut RgbaFrame,
        timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        #[cfg(feature = "openh264")]
        if let Some(software) = &mut self.fallback {
            return software.decode_timed(frame, out, timing);
        }
        if let Some(fatal) = &self.fatal {
            return Err(fatal.clone());
        }
        // The UI couldn't draw GPU pictures: decode for the CPU instead.
        if self.running.as_ref().is_some_and(|r| r.output == Output::Gpu)
            && let Some(why) = GpuVideo::global().failure()
        {
            self.fall_back(&format!("GPU renderer: {why}"))?;
            return Err("switched video output; waiting for a keyframe".into());
        }
        let now = Instant::now();
        if self.last_input.replace(now).is_none_or(|previous| now.duration_since(previous) >= OUTPUT_STALL) {
            self.last_output = Some(now);
        }
        let since = *self.last_output.get_or_insert(now);
        let result = self.running_decode(frame, out, timing, self.colorimetry);
        match &result {
            Ok(Some(_)) => {
                self.last_output = Some(Instant::now());
                self.failures = 0;
            }
            Ok(None) if since.elapsed() >= OUTPUT_STALL => {
                self.fall_back("packets arrive but the decoder produced no pictures for one second")?;
                return Err("switched video output; waiting for a keyframe".into());
            }
            Err(e) => {
                self.failures += 1;
                if self.failures >= FAILURES_BEFORE_FALLBACK {
                    self.fall_back(e)?;
                }
            }
            Ok(None) => {}
        }
        result
    }
}

impl MediaCodecDecoder {
    fn running_decode(
        &mut self,
        frame: &EncodedFrame,
        out: &mut RgbaFrame,
        timing: FrameTiming,
        colorimetry: Colorimetry,
    ) -> Result<Option<FrameTiming>, String> {
        // More frames waiting: queue the next one rather than wait for this
        // one's picture (the codec works on several at once).
        let busy = self.backlog > 0;
        let input_wait = if busy { Duration::ZERO } else { INPUT_WAIT };
        let output_wait = if busy { Duration::ZERO } else { OUTPUT_WAIT };
        let running = self.running.as_mut().ok_or("decoder not configured")?;
        if running.images.is_none() {
            let mut shown = None;
            if !running.feed(&frame.data, timing, input_wait)? {
                shown = running.drain_buffers(Duration::ZERO, colorimetry, out)?;
                if !running.feed(&frame.data, timing, INPUT_WAIT)? {
                    return Err("decoder input is full".into());
                }
            }
            let wait = if shown.is_some() { Duration::ZERO } else { output_wait };
            let newer = running.drain_buffers(wait, colorimetry, out)?;
            return Ok(newer.or(shown));
        }
        let mut rendered = false;
        if !running.feed(&frame.data, timing, input_wait)? {
            // Input full: collect what's done, then try once more.
            rendered = running.drain(Duration::ZERO)?;
            if !running.feed(&frame.data, timing, INPUT_WAIT)? {
                return Err("decoder input is full".into());
            }
        }
        rendered |= running.drain(if rendered { Duration::ZERO } else { output_wait })?;
        let Some(image) = running.latest_image(rendered && !busy)? else {
            return Ok(None);
        };
        let pts = image.timestamp().map_err(|e| format!("image timestamp: {e:?}"))? / 1_000;
        let Some(index) = running.pending.iter().position(|(at, _)| *at == pts as u64) else {
            return Ok(None);
        };
        // Keep unmatched input timings until their output arrives or the
        // bounded PTS history expires them.
        let Some((_, timing)) = running.pending.remove(index) else {
            return Ok(None);
        };
        if let Some(session) = &running.gpu_session {
            let reader = running
                .images
                .as_ref()
                .map(|(r, _)| r.clone())
                .ok_or("no image reader")?;
            let picture =
                GpuPicture::new(image, reader, session, timing)?;
            out.width = picture.width;
            out.height = picture.height;
            out.on_gpu = true;
            GpuVideo::global().put(picture);
            return Ok(Some(timing));
        }
        convert(&image, colorimetry, (running.width, running.height), out)?;
        Ok(Some(timing))
    }
}

/// How the codec lays out a picture in its output buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferLayout {
    /// Interleaved U/V (NV12) rather than separate planes (I420).
    semi_planar: bool,
    color_format: i32,
    stride: usize,
    slice_height: usize,
    /// The visible picture inside the buffer.
    left: usize,
    top: usize,
    width: usize,
    height: usize,
}

fn align(v: i32, to: i32) -> i32 {
    (v + to - 1) / to * to
}

impl BufferLayout {
    /// From the output format's keys (`get`), `size` when it lacks them.
    fn from_format(get: impl Fn(&str) -> Option<i32>, size: (i32, i32)) -> Result<Self, String> {
        let width = get("width").filter(|&w| w > 0).unwrap_or(size.0);
        let height = get("height").filter(|&h| h > 0).unwrap_or(size.1);
        let color_format = get("color-format").unwrap_or(COLOR_YUV420_SEMI_PLANAR);
        let semi_planar = match color_format {
            COLOR_YUV420_PLANAR | COLOR_YUV420_PACKED_PLANAR => false,
            // Flexible in a buffer is the codec's native layout; NV12 is
            // what hardware decoders use.
            COLOR_YUV420_SEMI_PLANAR
            | COLOR_YUV420_PACKED_SEMI_PLANAR
            | COLOR_YUV420_FLEXIBLE
            | COLOR_QCOM_YUV420_SEMI_PLANAR_32M => true,
            other => return Err(format!("output colour format {other:#x} isn't supported")),
        };
        let (aligned_w, aligned_h) = if color_format == COLOR_QCOM_YUV420_SEMI_PLANAR_32M {
            (align(width, 128), align(height, 32))
        } else {
            (width, height)
        };
        let stride = get("stride").filter(|&s| s >= width).unwrap_or(aligned_w);
        let slice_height = get("slice-height").filter(|&s| s >= height).unwrap_or(aligned_h);
        let left = get("crop-left").unwrap_or(0).clamp(0, width - 1);
        let top = get("crop-top").unwrap_or(0).clamp(0, height - 1);
        let right = get("crop-right")
            .filter(|&r| r > left)
            .unwrap_or(width - 1)
            .min(width - 1);
        let bottom = get("crop-bottom")
            .filter(|&b| b > top)
            .unwrap_or(height - 1)
            .min(height - 1);
        Ok(Self {
            semi_planar,
            color_format,
            stride: stride as usize,
            slice_height: slice_height as usize,
            left: left as usize,
            top: top as usize,
            width: (right - left + 1) as usize,
            height: (bottom - top + 1) as usize,
        })
    }

    /// The visible picture's planes in `data`.
    fn planes<'a>(&self, data: &'a [u8]) -> Result<Yuv420<'a>, String> {
        let (stride, left, top) = (self.stride, self.left, self.top);
        // Chroma right after the luma rows, at the reported slice height,
        // or (some decoders) right after the picture's own rows.
        let fits = |slice: usize| {
            let luma = stride * slice;
            let last_row = top + self.height - 1;
            let chroma_end = if self.semi_planar {
                luma + (last_row / 2) * stride + left + self.width
            } else {
                luma + (stride / 2) * (slice / 2) + (last_row / 2) * (stride / 2) + (left + self.width) / 2
            };
            last_row * stride + left + self.width <= data.len() && chroma_end <= data.len()
        };
        let slice = [self.slice_height, top + self.height]
            .into_iter()
            .find(|&s| fits(s))
            .ok_or_else(|| format!("output buffer of {} bytes is smaller than {self:?}", data.len()))?;
        let luma = stride * slice;
        let y = &data[top * stride + left..];
        Ok(if self.semi_planar {
            let uv = luma + (top / 2) * stride + (left & !1);
            Yuv420 {
                y,
                u: &data[uv..],
                v: &data[uv + 1..],
                y_stride: stride,
                u_stride: stride,
                v_stride: stride,
                uv_pixel_stride: 2,
                width: self.width,
                height: self.height,
            }
        } else {
            let chroma = stride / 2;
            let offset = (top / 2) * chroma + left / 2;
            let u = luma + offset;
            let v = luma + chroma * (slice / 2) + offset;
            Yuv420 {
                y,
                u: &data[u..],
                v: &data[v..],
                y_stride: stride,
                u_stride: chroma,
                v_stride: chroma,
                uv_pixel_stride: 1,
                width: self.width,
                height: self.height,
            }
        })
    }

    fn convert(&self, data: &[u8], colorimetry: Colorimetry, out: &mut RgbaFrame) -> Result<(), String> {
        let planes = self.planes(data)?;
        out.reshape(self.width as u32, self.height as u32);
        yuv420_to_rgba(&planes, colorimetry, &mut out.data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(keys: &[(&str, i32)]) -> Result<BufferLayout, String> {
        BufferLayout::from_format(|key| keys.iter().find(|(k, _)| *k == key).map(|&(_, v)| v), (8, 4))
    }

    #[test]
    fn nv12_with_padding() {
        let l = layout(&[("color-format", 21), ("stride", 16), ("slice-height", 6)]).unwrap();
        assert_eq!((l.stride, l.slice_height, l.width, l.height), (16, 6, 8, 4));
        let mut data = vec![0u8; 16 * 6 * 3 / 2];
        data[16 * 6] = 7; // first U
        data[16 * 6 + 1] = 9; // first V
        let p = l.planes(&data).unwrap();
        assert_eq!((p.u[0], p.v[0], p.uv_pixel_stride, p.u_stride), (7, 9, 2, 16));
    }

    #[test]
    fn i420_planes() {
        let l = layout(&[("color-format", 19)]).unwrap();
        let mut data = vec![0u8; 8 * 4 * 3 / 2];
        data[32] = 1; // U plane
        data[32 + 8] = 2; // V plane: 4x2 chroma after U
        let p = l.planes(&data).unwrap();
        assert_eq!((p.u[0], p.v[0], p.uv_pixel_stride, p.u_stride), (1, 2, 1, 4));
    }

    #[test]
    fn qualcomm_alignment_and_crop() {
        let l = layout(&[
            ("color-format", COLOR_QCOM_YUV420_SEMI_PLANAR_32M),
            ("width", 1280),
            ("height", 736),
            ("crop-right", 1279),
            ("crop-bottom", 719),
        ])
        .unwrap();
        assert_eq!((l.stride, l.slice_height, l.width, l.height), (1280, 736, 1280, 720));
        assert!(l.planes(&vec![0; 1280 * 736 * 3 / 2]).is_ok());
    }

    #[test]
    fn chroma_right_after_the_picture() {
        // Reports a 6-row slice but packs chroma after the 4 rows.
        let l = layout(&[("color-format", 21), ("slice-height", 6)]).unwrap();
        let data = vec![0u8; 8 * 4 * 3 / 2];
        assert!(l.planes(&data).is_ok());
        assert!(l.planes(&data[..20]).is_err());
    }

    #[test]
    fn tiled_formats_are_refused() {
        assert!(layout(&[("color-format", 0x7FC0_0002)]).is_err());
    }
}
