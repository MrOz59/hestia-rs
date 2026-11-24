//! Connects Opus decoding to a platform's PCM playback callback.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use hestia_stream::{AudioFormat, AudioSink};
use log::{info, warn};

use crate::audio_fifo::{Fifo, SAMPLE_RATE, fill, lock};
use crate::audio_stats::AudioStats;

/// Borrowed, interleaved 48 kHz PCM. An empty slice means no decoder output yet.
pub struct DecodedAudio<'a> {
    pub samples: &'a [f32],
    pub channels: usize,
    /// Frames of lost audio after `samples` that the decoder could not
    /// conceal: the playback buffer plays silence for them, faded in and out.
    pub gap_frames: usize,
}

pub trait AudioDecoder: Send {
    fn configure(&mut self, format: &AudioFormat) -> Result<(), String>;
    /// An empty packet signals a loss. The decoder supplies concealment, or
    /// reports the lost frames as [`DecodedAudio::gap_frames`].
    fn decode(&mut self, packet: &[u8]) -> Result<DecodedAudio<'_>, String>;
    /// Release decoder resources so the backend can be configured again.
    fn cleanup(&mut self);
    /// Where to count decoder-specific trouble (a busy hardware decoder).
    fn attach_stats(&mut self, _stats: Arc<AudioStats>) {}
}

/// The audio callback's side of the PCM buffer.
#[derive(Clone)]
pub struct PcmReader {
    fifo: Arc<Mutex<Fifo>>,
    channels: usize,
    stats: Arc<AudioStats>,
}

impl PcmReader {
    /// Number of channels negotiated with the host.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Where the output counts its own side (device, callbacks).
    pub fn stats(&self) -> &Arc<AudioStats> {
        &self.stats
    }

    /// Fill the device buffer in GameStream speaker order without waiting for decoding.
    pub fn read<T: Copy>(&self, out: &mut [T], channels: usize, convert: impl Fn(f32) -> T, silence: T) {
        fill(&self.fifo, &self.stats, out, channels, channels, convert, silence);
    }

    /// In a backend without speaker positions, only use the first two outputs.
    #[cfg(feature = "audio")]
    pub(crate) fn read_stereo<T: Copy>(&self, out: &mut [T], channels: usize, convert: impl Fn(f32) -> T, silence: T) {
        fill(
            &self.fifo,
            &self.stats,
            out,
            channels,
            channels.min(2),
            convert,
            silence,
        );
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
    stats: Arc<AudioStats>,
    /// Underruns already logged (they happen in the output callback, which
    /// must not log; this thread reports them, with their time).
    underruns_seen: u64,
    underrun_logs: u64,
}

impl<D: AudioDecoder, O: PcmOutput> PlaybackSink<D, O> {
    pub fn new(decoder: D, output: O) -> Self {
        let stats = Arc::new(AudioStats::default());
        let mut decoder = decoder;
        decoder.attach_stats(stats.clone());
        Self {
            decoder,
            output,
            reader: None,
            stats,
            underruns_seen: 0,
            underrun_logs: 0,
        }
    }

    /// Logs underruns since the last packet, the first ones and then every
    /// 25th, so the log shows when the audio stopped next to what the
    /// network did.
    fn log_underruns(&mut self) {
        let underruns = self.stats.underruns.load(Ordering::Relaxed);
        if underruns <= self.underruns_seen {
            return;
        }
        self.underruns_seen = underruns;
        self.underrun_logs += 1;
        if self.underrun_logs <= 20 || self.underrun_logs.is_multiple_of(25) {
            let target_ms = self.stats.target_frames.load(Ordering::Relaxed) * 1000 / u64::from(SAMPLE_RATE);
            info!("audio buffer ran dry ({underruns} so far); now keeping {target_ms} ms ahead");
        }
    }

    /// Counters for the whole playback path, kept across streams.
    pub fn stats(&self) -> Arc<AudioStats> {
        self.stats.clone()
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
            fifo: Arc::new(Mutex::new(Fifo::new(channels, self.stats.clone()))),
            channels,
            stats: self.stats.clone(),
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
        let counter = if packet.is_empty() {
            &self.stats.concealed
        } else {
            &self.stats.decoded
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let decoded = match self.decoder.decode(packet) {
            Ok(decoded) if !decoded.samples.is_empty() || decoded.gap_frames > 0 => decoded,
            Ok(_) => return,
            Err(e) => {
                let errors = self.stats.decode_errors.fetch_add(1, Ordering::Relaxed) + 1;
                if errors <= 5 || errors.is_multiple_of(500) {
                    warn!("opus decode ({errors} errors): {e}");
                }
                return;
            }
        };
        let mut fifo = lock(&reader.fifo);
        if !decoded.samples.is_empty() {
            fifo.push(decoded.samples, decoded.channels);
        }
        if decoded.gap_frames > 0 {
            fifo.push_gap(decoded.gap_frames, decoded.channels);
        }
        drop(fifo);
        self.log_underruns();
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
                gap_frames: 0,
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

    /// A decoder that can't conceal: a lost packet is a gap.
    #[derive(Default)]
    struct Gappy {
        silence: Vec<f32>,
        packet: Vec<f32>,
    }

    impl AudioDecoder for Gappy {
        fn configure(&mut self, _: &AudioFormat) -> Result<(), String> {
            self.packet = vec![1.0; 240 * 2];
            Ok(())
        }

        fn decode(&mut self, packet: &[u8]) -> Result<DecodedAudio<'_>, String> {
            if packet == [0xff] {
                return Err("bad packet".into());
            }
            Ok(DecodedAudio {
                samples: if packet.is_empty() { &self.silence } else { &self.packet },
                channels: 2,
                gap_frames: if packet.is_empty() { 240 } else { 0 },
            })
        }

        fn cleanup(&mut self) {}
    }

    #[test]
    fn losses_become_faded_gaps_and_everything_is_counted() {
        let mut sink = PlaybackSink::new(Gappy::default(), Output::default());
        sink.setup(&AudioFormat::stereo()).unwrap();
        for packet in [&[1u8][..], &[], &[1], &[0xff]] {
            sink.submit(packet);
        }
        let stats = sink.stats();
        let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        assert_eq!(
            (get(&stats.decoded), get(&stats.concealed), get(&stats.decode_errors)),
            (3, 1, 1)
        );
        let fifo = lock(&sink.reader.as_ref().unwrap().fifo);
        assert_eq!(fifo.queued_frames(), 720, "packet, gap, packet");
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
