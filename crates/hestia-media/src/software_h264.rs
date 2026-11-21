//! Software H.264 decoding with Cisco's OpenH264 (built from source by the
//! `openh264` crate, so it cross-compiles for Android without system
//! libraries). The desktop's decoder, and the last fallback on Android.
//!
//! Each access unit goes through `DecodeFrameNoDelay`, so a picture comes out
//! of the same call that fed its data (no reordering delay; hosts send no
//! B-frames). The picture then leaves OpenH264's internal buffers one of two
//! ways:
//!
//! * **GPU colour** (desktop, when the window's GL renderer is ready): the
//!   4:2:0 planes are copied into pooled storage (1.5 bytes per pixel) and
//!   the renderer converts them to RGB while drawing.
//! * **CPU colour**: converted to RGBA here ([`crate::color`]), for the
//!   toolkit to upload as an image.
//!
//! The mode is chosen per frame, so the stream carries on (on the CPU) if
//! the GPU path fails or the window loses its GL context.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use hestia_stream::{EncodedFrame, VideoCodec, VideoFormat};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

use crate::color::{Colorimetry, I420, i420_to_rgba};
use crate::decode::{DecodedFrame, FrameBuffer, FrameDecoder, FrameTiming};
use crate::native_picture::{GpuOutput, NativePicture, PlanarPicture, PlanePool};

unsafe extern "C" {
    /// OpenH264's CPU feature probe (`codec/common/src/cpu.cpp`, C linkage).
    /// It returns 0 when the library was built without its SIMD code, which
    /// on x86 happens when `nasm` was missing at build time.
    fn WelsCPUFeatureDetect(logical_processors: *mut i32) -> u32;
}

/// Whether this build's OpenH264 uses SIMD code (checked once).
pub fn openh264_simd() -> bool {
    static SIMD: OnceLock<bool> = OnceLock::new();
    *SIMD.get_or_init(|| {
        // SAFETY: a pure query; a null pointer skips the core count.
        let features = unsafe { WelsCPUFeatureDetect(std::ptr::null_mut()) };
        if features == 0 {
            log::warn!(
                "OpenH264 was built without SIMD code (nasm missing at build time?): \
                 software decoding is about 25% slower"
            );
        }
        features != 0
    })
}

pub struct OpenH264Decoder {
    decoder: Decoder,
    colorimetry: Colorimetry,
    /// The window's GPU renderer and the plane storage it returns.
    gpu: Option<(Arc<GpuOutput>, Arc<PlanePool>)>,
    /// The last picture went out as planes for the GPU.
    gpu_colour: bool,
    simd: bool,
}

impl OpenH264Decoder {
    pub fn new(colorimetry: Colorimetry) -> Result<Self, String> {
        Self::with_gpu_output(colorimetry, None)
    }

    /// Hands pictures to `output`'s renderer as 4:2:0 planes whenever it is
    /// available, and converts to RGBA on the CPU otherwise.
    pub fn with_gpu_output(colorimetry: Colorimetry, output: Option<Arc<GpuOutput>>) -> Result<Self, String> {
        Ok(Self {
            decoder: Decoder::new().map_err(|e| format!("OpenH264: {e}"))?,
            colorimetry,
            gpu: output.map(|output| (output, Arc::new(PlanePool::default()))),
            gpu_colour: false,
            simd: openh264_simd(),
        })
    }

    /// The formats to offer the host when this decoder is in use.
    pub fn formats() -> Vec<VideoFormat> {
        vec![VideoFormat {
            codec: VideoCodec::H264,
            ten_bit: false,
            yuv444: false,
        }]
    }
}

impl FrameDecoder for OpenH264Decoder {
    fn name(&self) -> &'static str {
        match (self.simd, self.gpu_colour) {
            (true, false) => "OpenH264 (software)",
            (true, true) => "OpenH264 (software, GPU colour)",
            (false, false) => "OpenH264 (software, no SIMD)",
            (false, true) => "OpenH264 (software, no SIMD, GPU colour)",
        }
    }

    fn supports(&self, format: VideoFormat) -> bool {
        format.codec == VideoCodec::H264 && !format.ten_bit && !format.yuv444
    }

    fn decode(&mut self, frame: &EncodedFrame, out: &mut DecodedFrame) -> Result<bool, String> {
        let now = Instant::now();
        self.decode_timed(frame, out, FrameTiming::new(frame, now, now))
            .map(|t| t.is_some())
    }

    fn reset(&mut self) -> Result<(), String> {
        self.decoder = Decoder::new().map_err(|e| format!("OpenH264 reset: {e}"))?;
        Ok(())
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut DecodedFrame,
        mut timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        // One access unit (SPS/PPS + slices for IDRs) per call.
        let yuv = match self.decoder.decode(&frame.data) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        timing.codec_output_at = Instant::now();
        let (width, height) = yuv.dimensions();
        let (y_stride, u_stride, v_stride) = yuv.strides();
        if let Some((output, pool)) = &self.gpu
            && output.available()
        {
            let picture = PlanarPicture::copy_from(
                pool,
                (yuv.y(), yuv.u(), yuv.v()),
                (y_stride, u_stride, v_stride),
                width,
                height,
                self.colorimetry,
            )?;
            out.width = width as u32;
            out.height = height as u32;
            out.buffer = FrameBuffer::Native(NativePicture::Planar(picture));
            self.gpu_colour = true;
            return Ok(Some(timing));
        }
        self.gpu_colour = false;
        let pixels = out.reshape(width as u32, height as u32);
        i420_to_rgba(
            &I420 {
                y: yuv.y(),
                u: yuv.u(),
                v: yuv.v(),
                y_stride,
                uv_stride: u_stride,
                width,
                height,
            },
            self.colorimetry,
            pixels,
        );
        Ok(Some(timing))
    }
}
