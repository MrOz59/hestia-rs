//! The streaming data plane, in pure Rust.
//!
//! Once `/launch` succeeds, a GameStream client:
//!
//! 1. **RTSP handshake** (TCP 48010, AES-GCM framed when `rtspenc://`):
//!    OPTIONS, DESCRIBE (host SDP: codecs, audio layout), SETUP for audio,
//!    video and control (learns ports, session id, ping payload), ANNOUNCE
//!    (our SDP: resolution, fps, bitrate, packet size, FEC, encryption) and PLAY.
//! 2. **Control stream** (ENet over UDP 47999, AES-GCM): start packets,
//!    periodic loss stats, IDR requests, input, rumble/HDR/termination.
//! 3. **Video** (RTP over UDP 47998): pings to open the path, packet
//!    reassembly, Reed–Solomon FEC recovery, frame depacketization.
//! 4. **Audio** (RTP over UDP 48000): AES-CBC decryption, RS FEC, Opus.
//! 5. **Input**: events encoded into input packets on the control stream.
//!
//! Status of this crate:
//!
//! | Piece                      | State            |
//! |----------------------------|------------------|
//! | Session URL / RTSP messages| done (`rtsp`)    |
//! | Encrypted RTSP framing     | done (`rtsp`)    |
//! | Input event model          | done (`input`)   |
//! | Sink traits (decoder API)  | done (`sink`)    |
//! | RTSP handshake + SDP       | next             |
//! | ENet control stream        | planned          |
//! | Video RTP + FEC            | planned          |
//! | Audio RTP + FEC            | planned          |
//!
//! [`StreamSession::start`] reports [`StreamError::Unimplemented`] until the
//! handshake lands, so front-ends can already wire the session lifecycle.

pub mod input;
pub mod rtsp;
pub mod sink;

use std::fmt;

pub use input::InputEvent;
pub use rtsp::SessionUrl;
pub use sink::{AudioFormat, AudioSink, EncodedFrame, SubmitResult, VideoCodec, VideoFormat, VideoSink};

/// Everything needed to open the stream, taken from the launch request and
/// the host's `/launch` answer.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub host: String,
    pub session_url: SessionUrl,
    /// Host app version quad (`7.1.431.-1`), which gates protocol features.
    pub app_version: Vec<i32>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Codecs we can decode, best first.
    pub video_formats: Vec<VideoFormat>,
    pub audio_channels: u8,
    /// `rikey` / `rikeyid` from `/launch`.
    pub remote_input_key: [u8; 16],
    pub remote_input_key_id: u32,
    /// Max UDP payload for video packets.
    pub packet_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStage {
    Rtsp,
    Control,
    Video,
    Audio,
    Input,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StageStarting(StreamStage),
    StageFailed(StreamStage, String),
    Connected,
    /// Packet loss / latency summary, roughly once a second.
    Quality {
        loss_percent: f32,
        rtt_ms: u32,
    },
    Rumble {
        gamepad: u8,
        low: u16,
        high: u16,
    },
    HdrMode(bool),
    Terminated {
        reason: String,
    },
}

pub trait SessionListener: Send + Sync {
    fn on_event(&self, event: StreamEvent);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    Unimplemented(&'static str),
    Rtsp(String),
    Io(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unimplemented(what) => write!(f, "{what} is not implemented yet"),
            Self::Rtsp(e) => write!(f, "RTSP: {e}"),
            Self::Io(e) => write!(f, "network: {e}"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<rtsp::RtspError> for StreamError {
    fn from(e: rtsp::RtspError) -> Self {
        Self::Rtsp(e.to_string())
    }
}

/// A running stream. Dropping it tears the connection down.
pub struct StreamSession {
    config: StreamConfig,
}

impl StreamSession {
    pub fn start(
        config: StreamConfig,
        _video: Box<dyn VideoSink>,
        _audio: Box<dyn AudioSink>,
        listener: Box<dyn SessionListener>,
    ) -> Result<Self, StreamError> {
        listener.on_event(StreamEvent::StageStarting(StreamStage::Rtsp));
        let err = StreamError::Unimplemented("the RTSP handshake");
        listener.on_event(StreamEvent::StageFailed(StreamStage::Rtsp, err.to_string()));
        let _ = config;
        Err(err)
    }

    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    pub fn send_input(&self, _event: InputEvent) {}

    pub fn request_keyframe(&self) {}

    pub fn stop(self) {}
}
