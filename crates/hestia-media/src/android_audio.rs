//! Opus decoding via Android MediaCodec.

use std::time::Duration;

use hestia_stream::AudioFormat;
use log::{info, warn};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;

use crate::audio_fifo::{CHANNELS, SAMPLE_RATE};
use crate::audio_playback::{AudioDecoder, DecodedAudio};

/// How long a packet may wait for a free decoder input buffer.
const INPUT_WAIT: Duration = Duration::from_millis(5);

/// `OpusHead` (RFC 7845 §5.1), which Android's decoder takes as `csd-0`.
/// Family 0 for plain stereo, family 1 with the stream layout otherwise.
pub fn opus_head(format: &AudioFormat) -> Vec<u8> {
    let channels = format.channels.max(1);
    let mut head = b"OpusHead".to_vec();
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip: none in GameStream
    head.extend_from_slice(&format.sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    if channels <= 2 && format.streams <= 1 {
        head.push(0);
    } else {
        head.push(1);
        head.push(format.streams);
        head.push(format.coupled_streams);
        head.extend_from_slice(&format.mapping[..usize::from(channels).min(8)]);
    }
    head
}

/// The decoder, which the ndk crate doesn't mark `Send`; only the network
/// thread uses it after setup.
struct Decoder {
    codec: MediaCodec,
    channels: usize,
}

// SAFETY: AMediaCodec may be used from any thread, one at a time.
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.codec.stop();
    }
}

impl Decoder {
    fn new(format: &AudioFormat) -> Result<Self, String> {
        let codec = MediaCodec::from_decoder_type("audio/opus").ok_or("this device has no Opus decoder")?;
        let mut media = MediaFormat::new();
        media.set_str("mime", "audio/opus");
        media.set_i32("sample-rate", format.sample_rate as i32);
        media.set_i32("channel-count", i32::from(format.channels));
        media.set_buffer("csd-0", &opus_head(format));
        media.set_buffer("csd-1", &0u64.to_le_bytes()); // pre-skip, ns
        media.set_buffer("csd-2", &80_000_000u64.to_le_bytes()); // seek pre-roll, ns
        codec
            .configure(&media, None, MediaCodecDirection::Decoder)
            .map_err(|e| format!("Opus decoder: {e:?}"))?;
        codec.start().map_err(|e| format!("Opus decoder start: {e:?}"))?;
        Ok(Self {
            codec,
            channels: usize::from(format.channels.max(1)),
        })
    }

    fn feed(&mut self, packet: &[u8], pts_us: u64) -> Result<(), String> {
        let mut input = match self.codec.dequeue_input_buffer(INPUT_WAIT) {
            Ok(DequeuedInputBufferResult::Buffer(b)) => b,
            Ok(DequeuedInputBufferResult::TryAgainLater) => return Err("decoder busy; packet dropped".into()),
            Err(e) => return Err(format!("input buffer: {e:?}")),
        };
        let buffer = input.buffer_mut();
        let n = packet.len().min(buffer.len());
        for (dst, &src) in buffer.iter_mut().zip(&packet[..n]) {
            dst.write(src);
        }
        self.codec
            .queue_input_buffer(input, 0, n, pts_us, 0)
            .map_err(|e| format!("queue input: {e:?}"))
    }

    /// Moves every finished block of PCM (16-bit) into `out` as floats.
    fn drain(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
        loop {
            match self.codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let info = *buffer.info();
                    let start = info.offset().max(0) as usize;
                    let len = info.size().max(0) as usize;
                    if let Some(bytes) = buffer.buffer().get(start..start + len) {
                        out.extend(
                            bytes
                                .chunks_exact(2)
                                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0),
                        );
                    }
                    self.codec
                        .release_output_buffer(buffer, false)
                        .map_err(|e| format!("release output: {e:?}"))?;
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    let format = self.codec.output_format();
                    if let Some(channels) = format.i32("channel-count").filter(|&c| c > 0) {
                        self.channels = channels as usize;
                    }
                    info!(
                        "Opus decoder output: {} ch, {} Hz",
                        self.channels,
                        format.i32("sample-rate").unwrap_or(0)
                    );
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => return Ok(()),
                Err(e) => return Err(format!("output buffer: {e:?}")),
            }
        }
    }
}


pub struct MediaCodecOpusDecoder {
    decoder: Option<Decoder>,
    pcm: Vec<f32>,
    frame_samples: usize,
    pts_us: u64,
    frame_us: u64,
    errors: u32,
}

impl Default for MediaCodecOpusDecoder {
    fn default() -> Self {
        Self {
            decoder: None,
            pcm: Vec::with_capacity(5760 * 8),
            frame_samples: 240,
            pts_us: 0,
            frame_us: 5_000,
            errors: 0,
        }
    }
}

impl AudioDecoder for MediaCodecOpusDecoder {
    fn configure(&mut self, format: &AudioFormat) -> Result<(), String> {
        self.cleanup();
        self.decoder = Some(Decoder::new(format)?);
        self.frame_samples = (format.samples_per_frame as usize).clamp(120, 5760);
        self.frame_us = self.frame_samples as u64 * 1_000_000 / u64::from(SAMPLE_RATE);
        info!(
            "Opus via MediaCodec: {} ch ({} streams), {} samples per packet",
            format.channels, format.streams, self.frame_samples
        );
        Ok(())
    }

    fn decode(&mut self, packet: &[u8]) -> Result<DecodedAudio<'_>, String> {
        let decoder = self.decoder.as_mut().ok_or("Opus decoder is not configured")?;
        self.pts_us += self.frame_us;
        self.pcm.clear();
        if packet.is_empty() {
            // Lost packet: a packet's worth of silence keeps the timing.
            self.pcm.resize(self.frame_samples * CHANNELS, 0.0);
            return Ok(DecodedAudio {
                samples: &self.pcm,
                channels: CHANNELS,
            });
        }
        let result = decoder
            .feed(packet, self.pts_us)
            .and_then(|()| decoder.drain(&mut self.pcm));
        if let Err(e) = result {
            self.errors += 1;
            if self.errors <= 5 || self.errors % 500 == 0 {
                warn!("audio decode ({} errors): {e}", self.errors);
            }
        }
        Ok(DecodedAudio {
            samples: &self.pcm,
            channels: decoder.channels,
        })
    }

    fn cleanup(&mut self) {
        self.decoder = None;
        self.pts_us = 0;
        self.errors = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_head_for_stereo_and_surround() {
        let stereo = opus_head(&AudioFormat::stereo());
        assert_eq!(&stereo[..8], b"OpusHead");
        assert_eq!(stereo.len(), 19);
        assert_eq!(stereo[9], 2);
        assert_eq!(&stereo[12..16], &48_000u32.to_le_bytes());
        assert_eq!(stereo[18], 0, "mapping family 0");

        let surround = AudioFormat {
            sample_rate: 48_000,
            channels: 6,
            streams: 4,
            coupled_streams: 2,
            samples_per_frame: 240,
            mapping: [0, 4, 1, 5, 2, 3, 0, 0],
        };
        let head = opus_head(&surround);
        assert_eq!(head[18], 1, "mapping family 1");
        assert_eq!(&head[19..21], &[4, 2]);
        assert_eq!(&head[21..], &[0, 4, 1, 5, 2, 3]);
    }
}
