//! Audio on Android: Opus decoded by the system's own MediaCodec decoder
//! (`audio/opus`, present since Android 5, so no native libopus to build)
//! and played through AAudio in low-latency mode.
//!
//! The network thread feeds each 5 ms packet to the decoder and moves the
//! PCM it produces into the sample FIFO ([`crate::audio_fifo`]) that the
//! AAudio callback drains. Surround streams are mixed down to stereo.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hestia_stream::{AudioFormat, AudioSink};
use log::{info, warn};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat as PcmFormat, AudioPerformanceMode, AudioSharingMode, AudioStream,
    AudioStreamBuilder,
};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;

use crate::audio_fifo::{CHANNELS, Fifo, SAMPLE_RATE, downmix_to_stereo, fill, lock};

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

/// The AAudio output stream (not `Send` in the ndk crate; it's only
/// started and stopped from the network thread).
struct Output(AudioStream);

// SAFETY: AAudio streams may be controlled from any thread.
unsafe impl Send for Output {}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.0.request_stop();
    }
}

fn open_output(fifo: Arc<Mutex<Fifo>>, lost: Arc<AtomicBool>) -> Result<Output, String> {
    let stream = AudioStreamBuilder::new()
        .map_err(|e| format!("AAudio: {e:?}"))?
        .direction(AudioDirection::Output)
        .format(PcmFormat::PCM_Float)
        .channel_count(CHANNELS as i32)
        .sample_rate(SAMPLE_RATE as i32)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(AudioSharingMode::Shared)
        .data_callback(Box::new(move |stream: &AudioStream, data: *mut c_void, frames: i32| {
            let channels = stream.channel_count().max(1) as usize;
            let len = frames.max(0) as usize * channels;
            // SAFETY: AAudio hands us `frames` frames of `channels` floats.
            let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), len) };
            fill(&fifo, out, channels, |s| s.clamp(-1.0, 1.0), 0.0);
            AudioCallbackResult::Continue
        }))
        .error_callback(Box::new(move |_stream: &AudioStream, error| {
            // Headphones unplugged, Bluetooth switched…: reopen from the
            // network thread (not allowed from this callback).
            warn!("audio output lost: {error:?}");
            lost.store(true, Ordering::Relaxed);
        }))
        .open_stream()
        .map_err(|e| format!("AAudio open: {e:?}"))?;
    stream.request_start().map_err(|e| format!("AAudio start: {e:?}"))?;
    info!(
        "audio output: AAudio {} Hz, {} ch, burst {} frames",
        stream.sample_rate(),
        stream.channel_count(),
        stream.frames_per_burst()
    );
    Ok(Output(stream))
}

pub struct AndroidAudioSink {
    decoder: Option<Decoder>,
    output: Option<Output>,
    fifo: Arc<Mutex<Fifo>>,
    output_lost: Arc<AtomicBool>,
    pcm: Vec<f32>,
    stereo: Vec<f32>,
    /// Samples per channel in one packet (240 for 5 ms), for lost packets.
    frame_samples: usize,
    pts_us: u64,
    frame_us: u64,
    errors: u32,
}

impl Default for AndroidAudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AndroidAudioSink {
    pub fn new() -> Self {
        Self {
            decoder: None,
            output: None,
            fifo: Arc::new(Mutex::new(Fifo::new())),
            output_lost: Arc::new(AtomicBool::new(false)),
            pcm: Vec::with_capacity(5760 * 8),
            stereo: Vec::with_capacity(5760 * 2),
            frame_samples: 240,
            pts_us: 0,
            frame_us: 5_000,
            errors: 0,
        }
    }

    fn reopen_output_if_lost(&mut self) {
        if self.output_lost.swap(false, Ordering::Relaxed) {
            self.output = None;
            match open_output(self.fifo.clone(), self.output_lost.clone()) {
                Ok(output) => self.output = Some(output),
                Err(e) => warn!("reopening audio output failed: {e}"),
            }
        }
    }
}

impl AudioSink for AndroidAudioSink {
    fn setup(&mut self, format: &AudioFormat) -> Result<(), String> {
        if format.sample_rate != SAMPLE_RATE {
            return Err(format!("unexpected audio rate {} Hz", format.sample_rate));
        }
        self.frame_samples = (format.samples_per_frame as usize).clamp(120, 5760);
        self.frame_us = self.frame_samples as u64 * 1_000_000 / u64::from(SAMPLE_RATE);
        self.decoder = Some(Decoder::new(format)?);
        self.output = Some(open_output(self.fifo.clone(), self.output_lost.clone())?);
        info!(
            "Opus via MediaCodec: {} ch ({} streams), {} samples per packet",
            format.channels, format.streams, self.frame_samples
        );
        Ok(())
    }

    fn submit(&mut self, opus_packet: &[u8]) {
        self.reopen_output_if_lost();
        let Some(decoder) = &mut self.decoder else { return };
        self.pts_us += self.frame_us;
        if opus_packet.is_empty() {
            // Lost packet: a packet's worth of silence keeps the timing.
            lock(&self.fifo).push(&vec![0.0; self.frame_samples * CHANNELS]);
            return;
        }
        self.pcm.clear();
        let result = decoder
            .feed(opus_packet, self.pts_us)
            .and_then(|()| decoder.drain(&mut self.pcm));
        if let Err(e) = result {
            self.errors += 1;
            if self.errors <= 5 || self.errors % 500 == 0 {
                warn!("audio decode ({} errors): {e}", self.errors);
            }
        }
        if self.pcm.is_empty() {
            return;
        }
        downmix_to_stereo(&self.pcm, decoder.channels, &mut self.stereo);
        lock(&self.fifo).push(&self.stereo);
    }

    fn cleanup(&mut self) {
        self.output = None;
        self.decoder = None;
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
