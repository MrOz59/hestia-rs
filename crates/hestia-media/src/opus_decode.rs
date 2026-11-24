//! GameStream Opus multistream decoding with libopus.

use hestia_stream::AudioFormat;

use crate::audio_fifo::{MAX_CHANNELS, SAMPLE_RATE};
use crate::audio_playback::{AudioDecoder, DecodedAudio};

const MAX_FRAME_SAMPLES: usize = 5760;

pub struct LibOpusDecoder {
    decoder: Option<opus::MSDecoder>,
    pcm: Vec<f32>,
    channels: usize,
    frame_samples: usize,
}

impl Default for LibOpusDecoder {
    fn default() -> Self {
        Self {
            decoder: None,
            pcm: vec![0.0; MAX_FRAME_SAMPLES * MAX_CHANNELS],
            channels: 2,
            frame_samples: 240,
        }
    }
}

impl AudioDecoder for LibOpusDecoder {
    fn configure(&mut self, format: &AudioFormat) -> Result<(), String> {
        self.cleanup();
        let channels = usize::from(format.channels);
        if !matches!(channels, 2 | 6 | 8) || format.sample_rate != SAMPLE_RATE {
            return Err(format!(
                "unsupported Opus format: {channels} ch, {} Hz",
                format.sample_rate
            ));
        }
        self.decoder = Some(
            opus::MSDecoder::new(
                SAMPLE_RATE,
                format.streams,
                format.coupled_streams,
                &format.mapping[..channels],
            )
            .map_err(|e| e.to_string())?,
        );
        self.channels = channels;
        self.frame_samples = (format.samples_per_frame as usize).clamp(120, MAX_FRAME_SAMPLES);
        Ok(())
    }

    fn decode(&mut self, packet: &[u8]) -> Result<DecodedAudio<'_>, String> {
        let decoder = self.decoder.as_mut().ok_or("Opus decoder is not configured")?;
        // Packet-loss concealment must be asked for exactly one packet's
        // worth of samples.
        let out = if packet.is_empty() {
            &mut self.pcm[..self.frame_samples * self.channels]
        } else {
            &mut self.pcm[..MAX_FRAME_SAMPLES * self.channels]
        };
        let samples = decoder.decode_float(packet, out, false).map_err(|e| e.to_string())?;
        Ok(DecodedAudio {
            samples: &self.pcm[..samples * self.channels],
            channels: self.channels,
            gap_frames: 0,
        })
    }

    fn cleanup(&mut self) {
        self.decoder = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_one_opus_packet_and_conceals_one_missing_packet() {
        let format = AudioFormat::stereo();
        let samples = format.samples_per_frame as usize * usize::from(format.channels);
        let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let mut packet = [0; 1500];
        let len = encoder.encode_float(&vec![0.0; samples], &mut packet).unwrap();

        let mut decoder = LibOpusDecoder::default();
        decoder.configure(&format).unwrap();
        let decoded = decoder.decode(&packet[..len]).unwrap();
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.samples.len(), samples);
        let concealed = decoder.decode(&[]).unwrap();
        assert_eq!(concealed.samples.len(), samples);
    }
    #[test]
    fn decodes_surround_in_the_negotiated_order_and_conceals_loss() {
        for (channels, streams, coupled_streams) in [(6, 4, 2), (8, 5, 3)] {
            let format = AudioFormat {
                channels,
                streams,
                coupled_streams,
                samples_per_frame: 960,
                // Exercise a nonidentity map, as used by some hosts.
                mapping: [0, 1, 3, 2, 5, 4, 7, 6],
                ..AudioFormat::stereo()
            };
            let mut decoder = LibOpusDecoder::default();
            decoder.configure(&format).unwrap();
            let channels = usize::from(channels);
            let frames = format.samples_per_frame as usize;
            let mut encoder = opus::MSEncoder::new(
                SAMPLE_RATE,
                streams,
                coupled_streams,
                &format.mapping[..channels],
                opus::Application::LowDelay,
            )
            .unwrap();
            let mut pcm = vec![0.0; frames * channels];
            for (i, frame) in pcm.chunks_exact_mut(channels).enumerate() {
                frame[channels - 1] = (i as f32 * 0.15).sin() * 0.7;
            }
            let mut packet = [0; 8192];
            let len = encoder.encode_float(&pcm, &mut packet).unwrap();
            let decoded = decoder.decode(&packet[..len]).unwrap();
            assert_eq!(decoded.channels, channels);
            assert_eq!(decoded.samples.len(), pcm.len());
            let mut energy = [0.0; MAX_CHANNELS];
            for frame in decoded.samples.chunks_exact(channels) {
                for (total, sample) in energy.iter_mut().zip(frame) {
                    *total += sample * sample;
                }
            }
            assert!(energy[channels - 1] > 10.0);
            assert!(energy[..channels - 1].iter().all(|&v| v < energy[channels - 1] * 0.1));
            assert_eq!(decoder.decode(&[]).unwrap().samples.len(), pcm.len());
        }
    }
}
