//! Media backends that consume a [`hestia_stream`] session.
//!
//! Each backend implements [`VideoSink`] / [`AudioSink`], so the streaming
//! code never knows which decoder or audio API is in use. Planned backends:
//!
//! | Backend       | Platform | Notes                                              |
//! |---------------|----------|----------------------------------------------------|
//! | `mediacodec`  | Android  | `AMediaCodec` straight into a `Surface`, zero-copy |
//! | `vaapi`       | Linux    | via FFmpeg/libva, DMA-BUF into the GL/Vulkan view  |
//! | `software`    | any      | FFmpeg software decode, fallback                   |
//! | `opus` + AAudio / PipeWire | both | Opus multistream decode and playback     |
//!
//! Available today:
//! * [`null`]: counts frames and packets; for exercising the pipeline.
//! * [`dump`]: writes the elementary video stream to a file (`ffplay out.h265`).

pub mod dump;
pub mod null;

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
            name: "Null (testing)",
            hardware: false,
            available: true,
        },
    ]
}
