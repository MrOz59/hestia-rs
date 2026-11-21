//! Media backends that consume a [`hestia_stream`] session.
//!
//! Each backend implements [`VideoSink`] / [`AudioSink`], so the streaming
//! code never knows which decoder or audio API is in use.
//!
//! Available today:
//! * [`decode`]: [`DecodingSink`], which decodes on its own thread with any
//!   [`FrameDecoder`] and hands decoded frames to a [`FramePresenter`].
//! * `software_h264` (feature `openh264`, on by default): OpenH264 software
//!   H.264 decoding, on every platform.
//! * [`audio_playback`]: independently selected Opus decoders and PCM outputs,
//!   connected by the low-latency playback FIFO.
//! * [`null`]: counts frames and packets; for exercising the pipeline.
//! * [`dump`]: writes the elementary video stream to a file (`ffplay out.h264`).
//!
//! * `mediacodec` (Android): hardware H.264/HEVC/AV1 through `AMediaCodec`,
//!   rendered into GPU buffers the UI draws directly (`gpu_video`), or read
//!   back on the CPU; falls back to OpenH264 for H.264.
//!
//! Planned: `vaapi` (Linux, DMA-BUF into the GL view), surround audio.

#[cfg(target_os = "android")]
pub mod aaudio;
#[cfg(target_os = "android")]
pub mod android_audio;
pub mod audio_fifo;
#[cfg(feature = "audio")]
pub mod audio_out;
pub mod audio_playback;
pub mod color;
pub mod decode;
pub mod dump;
#[cfg(target_os = "android")]
pub mod gpu_video;
#[cfg(target_os = "android")]
pub mod mediacodec;
pub mod null;
#[cfg(feature = "audio")]
pub mod opus_decode;
pub mod picture_mailbox;
#[cfg(feature = "openh264")]
pub mod software_h264;

pub use color::Colorimetry;
pub use decode::{
    DecodeStats, DecodedFrame, DecodingSink, FrameBuffer, FrameDecoder, FramePresenter, LatencyHistogram,
    RecoveryLatency,
};
pub use hestia_stream::sink::{AudioFormat, AudioSink, EncodedFrame, SubmitResult, VideoCodec, VideoFormat, VideoSink};

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
            available: cfg!(target_os = "android"),
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
    decoder_with(colorimetry, true)
}

/// Like [`default_decoder`]; `hardware: false` skips hardware decoders.
pub fn decoder_with(colorimetry: Colorimetry, hardware: bool) -> Result<DecoderChoice, String> {
    #[cfg(target_os = "android")]
    if hardware && let Some(decoder) = mediacodec::MediaCodecDecoder::probe(colorimetry) {
        let formats = decoder.formats();
        return Ok((Box::new(decoder), formats));
    }
    #[cfg(not(target_os = "android"))]
    let _ = hardware;
    software_decoder(colorimetry)
}

/// Chooses a component already matched by Android to the requested stream mode.
#[cfg(target_os = "android")]
pub fn decoder_with_android_capabilities(
    colorimetry: Colorimetry,
    candidates: Vec<(VideoCodec, String)>,
    output: Option<std::sync::Arc<gpu_video::GpuOutput>>,
) -> Result<DecoderChoice, String> {
    if let Some(decoder) = mediacodec::MediaCodecDecoder::with_components(colorimetry, candidates, output) {
        let formats = decoder.formats();
        return Ok((Box::new(decoder), formats));
    }
    software_decoder(colorimetry)
}

fn software_decoder(colorimetry: Colorimetry) -> Result<DecoderChoice, String> {
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

/// Audio playback when this build has it, otherwise a sink that only counts.
pub fn default_audio_sink() -> Box<dyn AudioSink> {
    #[cfg(target_os = "android")]
    {
        Box::new(audio_playback::PlaybackSink::new(
            android_audio::MediaCodecOpusDecoder::default(),
            aaudio::AAudioOutput::default(),
        ))
    }
    #[cfg(all(feature = "audio", not(target_os = "android")))]
    {
        Box::new(audio_playback::PlaybackSink::new(
            opus_decode::LibOpusDecoder::default(),
            audio_out::CpalOutput::default(),
        ))
    }
    #[cfg(not(any(feature = "audio", target_os = "android")))]
    {
        Box::new(null::NullAudioSink::default())
    }
}
