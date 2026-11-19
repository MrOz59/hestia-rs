//! Software H.264 decoding with Cisco's OpenH264 (built from source by the
//! `openh264` crate, so it cross-compiles for Android without system
//! libraries). The prototype's portable decoder: hardware backends
//! (MediaCodec, VA-API) slot in behind the same [`FrameDecoder`] trait.

use std::time::Instant;

use hestia_stream::{EncodedFrame, VideoCodec, VideoFormat};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

use crate::color::{Colorimetry, I420, i420_to_rgba};
use crate::decode::{FrameDecoder, FrameTiming, RgbaFrame};

pub struct OpenH264Decoder {
    decoder: Decoder,
    colorimetry: Colorimetry,
}

impl OpenH264Decoder {
    pub fn new(colorimetry: Colorimetry) -> Result<Self, String> {
        Ok(Self {
            decoder: Decoder::new().map_err(|e| format!("OpenH264: {e}"))?,
            colorimetry,
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
        "OpenH264 (software)"
    }

    fn supports(&self, format: VideoFormat) -> bool {
        format.codec == VideoCodec::H264 && !format.ten_bit && !format.yuv444
    }

    fn decode(&mut self, frame: &EncodedFrame, out: &mut RgbaFrame) -> Result<bool, String> {
        let now = Instant::now();
        self.decode_timed(frame, out, FrameTiming::new(frame, now, now)).map(|t| t.is_some())
    }

    fn reset(&mut self) -> Result<(), String> {
        self.decoder = Decoder::new().map_err(|e| format!("OpenH264 reset: {e}"))?;
        Ok(())
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut RgbaFrame,
        mut timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        // One access unit (SPS/PPS + slice for IDRs) per call; the host sends
        // a single slice per frame.
        let yuv = match self.decoder.decode(&frame.data) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        timing.codec_output_at = Instant::now();
        let (width, height) = yuv.dimensions();
        let (y_stride, uv_stride, _) = yuv.strides();
        out.reshape(width as u32, height as u32);
        i420_to_rgba(
            &I420 {
                y: yuv.y(),
                u: yuv.u(),
                v: yuv.v(),
                y_stride,
                uv_stride,
                width,
                height,
            },
            self.colorimetry,
            &mut out.data,
        );
        Ok(Some(timing))
    }
}
