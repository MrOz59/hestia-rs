//! Media backends that consume a [`hestia_stream`] session.
//!
//! Each backend implements [`VideoSink`] / [`AudioSink`], so the streaming
//! code never knows which decoder or audio API is in use.
//!
//! Available today:
//! * [`decode`]: [`DecodingSink`], which decodes on its own thread with any
//!   [`FrameDecoder`] and hands RGBA pictures to a [`FramePresenter`].
//! * `software_h264` (feature `openh264`, on by default): OpenH264 software
//!   H.264 decoding, on every platform.
//! * [`null`]: counts frames and packets; for exercising the pipeline.
//! * [`dump`]: writes the elementary video stream to a file (`ffplay out.h264`).
//!
//! Planned: `mediacodec` (Android, `AMediaCodec` into a `Surface`), `vaapi`
//! (Linux, DMA-BUF into the GL view), Opus decode + AAudio/PipeWire playback.

pub mod color;
pub mod decode;
pub mod dump;
pub mod null;
#[cfg(feature = "openh264")]
pub mod software_h264;

pub use color::Colorimetry;
pub use decode::{DecodeStats, DecodingSink, FrameDecoder, FramePresenter, RgbaFrame};
pub use hestia_stream::sink::{AudioFormat, AudioSink, EncodedFrame, SubmitResult, VideoFormat, VideoSink};

/// Description of a decoder backend, for settings screens and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    pub name: &'static str,
    pub hardware: bool,
    pub available: bool,
}

/// Backends compiled into this build, best first.
pub fn video_backends() -> Vec<BackendInfo> {
    vec![
        BackendInfo {
            name: if cfg!(target_os = "android") {
                "MediaCodec"
            } else {
                "VA-API"
            },
            hardware: true,
            available: false,
        },
        BackendInfo {
            name: "OpenH264 (software H.264)",
            hardware: false,
            available: cfg!(feature = "openh264"),
        },
        BackendInfo {
            name: "Null (testing)",
            hardware: false,
            available: true,
        },
    ]
}

/// A decoder plus the formats to announce to the host for it.
pub type DecoderChoice = (Box<dyn FrameDecoder>, Vec<VideoFormat>);

/// The best video decoder this build offers.
pub fn default_decoder(colorimetry: Colorimetry) -> Result<DecoderChoice, String> {
    #[cfg(feature = "openh264")]
    {
        let decoder = software_h264::OpenH264Decoder::new(colorimetry)?;
        Ok((Box::new(decoder), software_h264::OpenH264Decoder::formats()))
    }
    #[cfg(not(feature = "openh264"))]
    {
        let _ = colorimetry;
        Err("this build has no video decoder (enable the `openh264` feature)".into())
    }
}
