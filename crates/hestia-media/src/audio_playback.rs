//! Connects Opus decoding to a platform's PCM playback callback.

use std::sync::{Arc, Mutex};

use hestia_stream::{AudioFormat, AudioSink};
use log::warn;

use crate::audio_fifo::{Fifo, SAMPLE_RATE, fill, lock};

/// Borrowed, interleaved 48 kHz PCM. An empty slice means no decoder output yet.
pub struct DecodedAudio<'a> {
    pub samples: &'a [f32],
    pub channels: usize,
}

pub trait AudioDecoder: Send {
    fn configure(&mut self, format: &AudioFormat) -> Result<(), String>;
    /// An empty packet signals a loss. The decoder supplies concealment or silence.
    fn decode(&mut self, packet: &[u8]) -> Result<DecodedAudio<'_>, String>;
    /// Release decoder resources so the backend can be configured again.
    fn cleanup(&mut self);
}

/// The audio callback's side of the PCM buffer.
#[derive(Clone)]
pub struct PcmReader {
    fifo: Arc<Mutex<Fifo>>,
    channels: usize,
}

impl PcmReader {
    /// Number of channels negotiated with the host.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Fill the device buffer in GameStream speaker order without waiting for decoding.
    pub fn read<T: Copy>(&self, out: &mut [T], channels: usize, convert: impl Fn(f32) -> T, silence: T) {
        fill(&self.fifo, out, channels, channels, convert, silence);
    }

    /// In a backend without speaker positions, only use the first two outputs.
    #[cfg(feature = "audio")]
    pub(crate) fn read_stereo<T: Copy>(&self, out: &mut [T], channels: usize, convert: impl Fn(f32) -> T, silence: T) {
        fill(&self.fifo, out, channels, channels.min(2), convert, silence);
    }
}

pub trait PcmOutput: Send {
    fn start(&mut self, reader: PcmReader) -> Result<(), String>;
    /// Called on the producer thread, for device changes that must be handled
    /// outside the output callback.
    fn maintain(&mut self) {}
    /// Release all resources. Also called after a partially failed start.
    fn stop(&mut self);
}

/// A decoder and PCM output connected to the same bounded playback FIFO.
pub struct PlaybackSink<D: AudioDecoder, O: PcmOutput> {
    decoder: D,
    output: O,
    reader: Option<PcmReader>,
}

impl<D: AudioDecoder, O: PcmOutput> PlaybackSink<D, O> {
    pub fn new(decoder: D, output: O) -> Self {
        Self {
            decoder,
            output,
            reader: None,
        }
    }
}

impl<D: AudioDecoder, O: PcmOutput> AudioSink for PlaybackSink<D, O> {
    fn setup(&mut self, format: &AudioFormat) -> Result<(), String> {
        self.cleanup();
        if format.sample_rate != SAMPLE_RATE {
            return Err(format!("unexpected audio rate {} Hz", format.sample_rate));
        }
        if !matches!(format.channels, 2 | 6 | 8) {
            return Err(format!("unexpected audio layout: {} channels", format.channels));
        }
        if let Err(e) = self.decoder.configure(format) {
            self.decoder.cleanup();
            return Err(e);
        }
        let channels = usize::from(format.channels);
        let reader = PcmReader {
            fifo: Arc::new(Mutex::new(Fifo::new(channels))),
            channels,
        };
        if let Err(e) = self.output.start(reader.clone()) {
            self.cleanup();
            return Err(e);
        }
        self.reader = Some(reader);
        Ok(())
    }

    fn submit(&mut self, packet: &[u8]) {
        let Some(reader) = &self.reader else {
            return;
        };
        self.output.maintain();
        let decoded = match self.decoder.decode(packet) {
            Ok(decoded) if !decoded.samples.is_empty() => decoded,
            Ok(_) => return,
            Err(e) => {
                warn!("opus decode: {e}");
                return;
            }
        };
        lock(&reader.fifo).push(decoded.samples, decoded.channels);
    }

    fn cleanup(&mut self) {
        self.reader = None;
        self.output.stop();
        self.decoder.cleanup();
    }
}

impl<D: AudioDecoder, O: PcmOutput> Drop for PlaybackSink<D, O> {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_fifo::TARGET_LATENCY_MS;

    #[derive(Default)]
    struct Decoder {
        configured: bool,
        loss_seen: bool,
        packet: Vec<f32>,
    }

    impl AudioDecoder for Decoder {
        fn configure(&mut self, _: &AudioFormat) -> Result<(), String> {
            self.configured = true;
            self.packet.resize(TARGET_LATENCY_MS * SAMPLE_RATE as usize / 1000, 0.0);
            Ok(())
        }

        fn decode(&mut self, packet: &[u8]) -> Result<DecodedAudio<'_>, String> {
            assert!(self.configured);
            self.loss_seen |= packet.is_empty();
            self.packet.fill(if packet.is_empty() { 0.0 } else { 0.25 });
            Ok(DecodedAudio {
                samples: &self.packet,
                channels: 1,
            })
        }

        fn cleanup(&mut self) {
            self.configured = false;
        }
    }

    #[derive(Default)]
    struct Output {
        reader: Option<PcmReader>,
        fail_start: bool,
    }

    impl PcmOutput for Output {
        fn start(&mut self, reader: PcmReader) -> Result<(), String> {
            self.reader = Some(reader);
            if self.fail_start {
                Err("mock output failed".into())
            } else {
                Ok(())
            }
        }

        fn stop(&mut self) {
            self.reader = None;
        }
    }

    #[test]
    fn playback_connects_the_decoder_and_output_and_resets_between_streams() {
        let mut sink = PlaybackSink::new(Decoder::default(), Output::default());
        sink.setup(&AudioFormat::stereo()).unwrap();
        sink.submit(&[1]);
        let mut out = [0.0; 4];
        sink.output.reader.as_ref().unwrap().read(&mut out, 2, |s| s, 0.0);
        assert_eq!(out, [0.25; 4], "mono output is duplicated");
        sink.submit(&[]);
        assert!(sink.decoder.loss_seen);

        sink.setup(&AudioFormat::stereo()).unwrap();
        sink.output.reader.as_ref().unwrap().read(&mut out, 2, |s| s, 0.0);
        assert_eq!(out, [0.0; 4], "no leftovers from the previous stream");
    }

    #[test]
    fn a_failed_output_start_releases_both_backends() {
        let output = Output {
            fail_start: true,
            ..Output::default()
        };
        let mut sink = PlaybackSink::new(Decoder::default(), output);
        assert_eq!(sink.setup(&AudioFormat::stereo()), Err("mock output failed".into()));
        assert!(!sink.decoder.configured);
        assert!(sink.output.reader.is_none());
        sink.submit(&[]);
        assert!(!sink.decoder.loss_seen);
    }
}
