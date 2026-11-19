//! Android hardware decoding with `AMediaCodec`.
//!
//! The codec decodes into an `AImageReader` surface in `YUV_420_888`, the
//! one output the NDK can read portably (ByteBuffer output uses vendor
//! layouts and the NDK has no `getOutputImage`). Each picture is converted
//! to RGBA like the software decoder's, so the rest of the pipeline is the
//! same. Low-latency keys (standard and vendor ones, as Moonlight sets)
//! keep the codec from holding frames back.

use std::time::{Duration, Instant};

use hestia_stream::sink::VideoCodec;
use log::{info, warn};
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

use crate::color::{Colorimetry, Yuv420, yuv420_to_rgba};
use crate::decode::{FrameDecoder, RgbaFrame};
use crate::{EncodedFrame, VideoFormat};

/// How long a call waits for an input buffer, and for its picture.
const INPUT_WAIT: Duration = Duration::from_millis(20);
const OUTPUT_WAIT: Duration = Duration::from_millis(12);
/// Rendering to the image reader is asynchronous; wait this long for it.
const IMAGE_WAIT: Duration = Duration::from_millis(20);

fn mime(codec: VideoCodec) -> Option<&'static str> {
    match codec {
        VideoCodec::H264 => Some("video/avc"),
        VideoCodec::Hevc => Some("video/hevc"),
        VideoCodec::Av1 => Some("video/av01"),
    }
}

/// The codec and the image reader it renders into. Neither is `Send` in
/// the ndk crate, but both may be used from any thread one at a time, and
/// only the decoder thread uses them after setup.
struct Running {
    codec: MediaCodec,
    reader: ImageReader,
    // Keeps the reader's surface referenced while the codec renders to it.
    _window: NativeWindow,
    width: i32,
    height: i32,
    pts_us: u64,
    frame_us: u64,
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
    h264: bool,
    hevc: bool,
    av1: bool,
    running: Option<Running>,
    /// Software decoder used when the hardware one can't be set up (H.264).
    #[cfg(feature = "openh264")]
    fallback: Option<crate::software_h264::OpenH264Decoder>,
}

impl MediaCodecDecoder {
    /// A decoder if this device has a hardware H.264 or HEVC decoder.
    pub fn probe(colorimetry: Colorimetry) -> Option<Self> {
        let h264 = MediaCodec::from_decoder_type("video/avc").is_some();
        let hevc = MediaCodec::from_decoder_type("video/hevc").is_some();
        let av1 = MediaCodec::from_decoder_type("video/av01").is_some();
        info!("MediaCodec decoders: H.264 {h264}, HEVC {hevc}, AV1 {av1}");
        (h264 || hevc).then_some(Self {
            colorimetry,
            h264,
            hevc,
            av1,
            running: None,
            #[cfg(feature = "openh264")]
            fallback: None,
        })
    }

    /// The formats to offer the host: HEVC first (same quality for less
    /// bandwidth), then H.264. AV1 comes last, used only when picked in
    /// the settings: the NDK can't tell (before Android 9) whether this
    /// device's AV1 decoder is hardware or a slow software one.
    pub fn formats(&self) -> Vec<VideoFormat> {
        let mut formats = Vec::new();
        for (ok, codec) in [
            (self.hevc, VideoCodec::Hevc),
            (self.h264, VideoCodec::H264),
            (self.av1, VideoCodec::Av1),
        ] {
            if ok {
                formats.push(VideoFormat {
                    codec,
                    ten_bit: false,
                    yuv444: false,
                });
            }
        }
        formats
    }

    fn start(format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<Running, String> {
        let mime = mime(format.codec).ok_or("unsupported codec")?;
        let codec = MediaCodec::from_decoder_type(mime).ok_or_else(|| format!("no decoder for {mime}"))?;
        let (w, h) = (width as i32, height as i32);
        let reader = ImageReader::new(w, h, ImageFormat::YUV_420_888, 4).map_err(|e| format!("image reader: {e:?}"))?;
        let window = reader.window().map_err(|e| format!("image reader surface: {e:?}"))?;

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
        codec
            .configure(&media, Some(&window), MediaCodecDirection::Decoder)
            .map_err(|e| format!("configure {mime}: {e:?}"))?;
        codec.start().map_err(|e| format!("start {mime}: {e:?}"))?;
        info!("MediaCodec {mime} {w}x{h}@{fps} started");
        Ok(Running {
            codec,
            reader,
            _window: window,
            width: w,
            height: h,
            pts_us: 0,
            frame_us: 1_000_000 / u64::from(fps.max(1)),
        })
    }
}

impl Running {
    /// Queues one access unit; false when the codec had no free input buffer.
    fn feed(&mut self, data: &[u8]) -> Result<bool, String> {
        let mut input = match self.codec.dequeue_input_buffer(INPUT_WAIT) {
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
        Ok(true)
    }

    /// Releases every finished picture to the image reader; true if any.
    fn drain(&mut self, wait: Duration) -> Result<bool, String> {
        let mut rendered = false;
        let mut timeout = wait;
        loop {
            match self.codec.dequeue_output_buffer(timeout) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    self.codec
                        .release_output_buffer(buffer, true)
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
    fn latest_image(&self) -> Result<Option<Image>, String> {
        let deadline = Instant::now() + IMAGE_WAIT;
        loop {
            match self.reader.acquire_latest_image() {
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
        "MediaCodec (hardware)"
    }

    fn supports(&self, format: VideoFormat) -> bool {
        !format.ten_bit
            && !format.yuv444
            && match format.codec {
                VideoCodec::H264 => self.h264,
                VideoCodec::Hevc => self.hevc,
                VideoCodec::Av1 => self.av1,
            }
    }

    fn configure(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String> {
        match Self::start(format, width, height, fps) {
            Ok(running) => {
                self.running = Some(running);
                Ok(())
            }
            #[cfg(feature = "openh264")]
            Err(e) if format.codec == VideoCodec::H264 => {
                warn!("MediaCodec unavailable ({e}); decoding in software");
                self.fallback = Some(crate::software_h264::OpenH264Decoder::new(self.colorimetry)?);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn decode(&mut self, frame: &EncodedFrame, out: &mut RgbaFrame) -> Result<bool, String> {
        #[cfg(feature = "openh264")]
        if let Some(software) = &mut self.fallback {
            return software.decode(frame, out);
        }
        let colorimetry = self.colorimetry;
        let running = self.running.as_mut().ok_or("decoder not configured")?;
        if !running.feed(&frame.data)? {
            // Input full: collect what's done, then try once more.
            running.drain(Duration::ZERO)?;
            if !running.feed(&frame.data)? {
                return Err("decoder input is full".into());
            }
        }
        if !running.drain(OUTPUT_WAIT)? {
            return Ok(false);
        }
        let Some(image) = running.latest_image()? else {
            return Ok(false);
        };
        convert(&image, colorimetry, (running.width, running.height), out)?;
        Ok(true)
    }
}
