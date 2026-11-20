//! Stereo Opus decoding with libopus.

use hestia_stream::AudioFormat;

use crate::audio_fifo::{CHANNELS, SAMPLE_RATE};
use crate::audio_playback::{AudioDecoder, DecodedAudio};

const MAX_FRAME_SAMPLES: usize = 5760;

pub struct LibOpusDecoder {
    decoder: Option<opus::Decoder>,
    pcm: Vec<f32>,
    frame_samples: usize,
}

impl Default for LibOpusDecoder {
    fn default() -> Self {
        Self {
            decoder: None,
            pcm: vec![0.0; MAX_FRAME_SAMPLES * CHANNELS],
            frame_samples: 240,
        }
    }
}

impl AudioDecoder for LibOpusDecoder {
    fn configure(&mut self, format: &AudioFormat) -> Result<(), String> {
        if format.channels != 2 || format.streams != 1 || format.sample_rate != SAMPLE_RATE {
            return Err(format!(
                "only 48 kHz stereo playback is implemented (host offers {} channels)",
                format.channels
            ));
        }
        self.decoder = Some(opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).map_err(|e| e.to_string())?);
        self.frame_samples = (format.samples_per_frame as usize).clamp(120, MAX_FRAME_SAMPLES);
        Ok(())
    }

    fn decode(&mut self, packet: &[u8]) -> Result<DecodedAudio<'_>, String> {
        let decoder = self.decoder.as_mut().ok_or("Opus decoder is not configured")?;
        // Packet-loss concealment must be asked for exactly one packet's
        // worth of samples.
        let out = if packet.is_empty() {
            &mut self.pcm[..self.frame_samples * CHANNELS]
        } else {
            &mut self.pcm[..]
        };
        let samples = decoder.decode_float(packet, out, false).map_err(|e| e.to_string())?;
        Ok(DecodedAudio {
            samples: &self.pcm[..samples * CHANNELS],
            channels: CHANNELS,
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
        let samples = format.samples_per_frame as usize * CHANNELS;
        let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let mut packet = [0; 1500];
        let len = encoder.encode_float(&vec![0.0; samples], &mut packet).unwrap();

        let mut decoder = LibOpusDecoder::default();
        decoder.configure(&format).unwrap();
        let decoded = decoder.decode(&packet[..len]).unwrap();
        assert_eq!(decoded.channels, CHANNELS);
        assert_eq!(decoded.samples.len(), samples);
        let concealed = decoder.decode(&[]).unwrap();
        assert_eq!(concealed.samples.len(), samples);
    }
}
