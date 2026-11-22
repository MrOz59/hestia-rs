//! Sinks that discard everything but keep statistics, like Moonlight's
//! "fake callbacks". Useful to validate the network pipeline on its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hestia_stream::sink::{AudioFormat, AudioSink, EncodedFrame, SubmitResult, VideoFormat, VideoSink};

#[derive(Debug, Default)]
pub struct Counters {
    pub video_frames: AtomicU64,
    pub video_bytes: AtomicU64,
    pub keyframes: AtomicU64,
    pub audio_packets: AtomicU64,
}

#[derive(Debug, Clone, Default)]
pub struct NullVideoSink {
    pub counters: Arc<Counters>,
}

impl VideoSink for NullVideoSink {
    fn setup(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String> {
        log::info!("null video sink: {format:?} {width}x{height}@{fps}");
        Ok(())
    }

    fn submit(&mut self, frame: EncodedFrame) -> SubmitResult {
        self.counters.video_frames.fetch_add(1, Ordering::Relaxed);
        self.counters
            .video_bytes
            .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        if frame.keyframe {
            self.counters.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        SubmitResult::Ok
    }
}

#[derive(Debug, Clone, Default)]
pub struct NullAudioSink {
    pub counters: Arc<Counters>,
}

impl AudioSink for NullAudioSink {
    fn setup(&mut self, format: &AudioFormat) -> Result<(), String> {
        log::info!("null audio sink: {} channels", format.channels);
        Ok(())
    }

    fn submit(&mut self, _opus_packet: &[u8]) {
        self.counters.audio_packets.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn counts() {
        let mut sink = NullVideoSink::default();
        let counters = sink.counters.clone();
        sink.submit(EncodedFrame {
            frame_number: 1,
            keyframe: true,
            data: vec![0; 10],
            received_at: Instant::now(),
            idr_requested_at: None,
            loss_detected_at: None,
            host_latency: None,
        });
        assert_eq!(counters.video_frames.load(Ordering::Relaxed), 1);
        assert_eq!(counters.video_bytes.load(Ordering::Relaxed), 10);
        assert_eq!(counters.keyframes.load(Ordering::Relaxed), 1);
    }
}
