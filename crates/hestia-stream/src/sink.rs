//! What a streaming session produces and where it goes. Decoders and renderers
//! (in `hestia-media` or platform code) implement these traits, which keeps
//! the network stack independent of any codec or graphics API.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    pub codec: VideoCodec,
    pub ten_bit: bool,
    pub yuv444: bool,
}

/// One complete access unit, reassembled and FEC-recovered.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub frame_number: u32,
    pub keyframe: bool,
    /// Annex B bitstream (H.264/HEVC) or OBU stream (AV1).
    pub data: Vec<u8>,
    /// When the first packet of this frame arrived.
    pub received_at: Instant,
    /// Host-side capture→send latency, when the host reports it.
    pub host_latency: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitResult {
    Ok,
    /// The decoder lost its reference; ask the host for an IDR frame.
    NeedKeyframe,
}

pub trait VideoSink: Send {
    fn setup(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String>;
    fn submit(&mut self, frame: EncodedFrame) -> SubmitResult;
    fn cleanup(&mut self) {}
}

/// Opus multistream layout announced by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub streams: u8,
    pub coupled_streams: u8,
    pub samples_per_frame: u32,
    pub mapping: [u8; 8],
}

impl AudioFormat {
    pub fn stereo() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            streams: 1,
            coupled_streams: 1,
            samples_per_frame: 240,
            mapping: [0, 1, 0, 0, 0, 0, 0, 0],
        }
    }
}

pub trait AudioSink: Send {
    fn setup(&mut self, format: &AudioFormat) -> Result<(), String>;
    /// One Opus packet (already decrypted). An empty slice marks a lost
    /// packet so the decoder can run packet-loss concealment.
    fn submit(&mut self, opus_packet: &[u8]);
    fn cleanup(&mut self) {}
}
