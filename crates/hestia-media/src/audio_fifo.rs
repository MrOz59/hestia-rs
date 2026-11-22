//! Bounded PCM FIFO between the network decoder and the audio device.
//! Latency is measured in frames, so it stays constant across channel layouts.

use std::collections::VecDeque;
use std::sync::{Mutex, TryLockError};

pub const TARGET_LATENCY_MS: usize = 30;
pub const MAX_LATENCY_MS: usize = 80;
pub const SAMPLE_RATE: u32 = 48_000;
pub const MAX_CHANNELS: usize = 8;
const TARGET_FRAMES: usize = TARGET_LATENCY_MS * SAMPLE_RATE as usize / 1_000;
const MAX_FRAMES: usize = MAX_LATENCY_MS * SAMPLE_RATE as usize / 1_000;

#[derive(Debug)]
pub struct Fifo {
    samples: VecDeque<f32>,
    channels: usize,
    priming: bool,
}

pub fn lock(fifo: &Mutex<Fifo>) -> std::sync::MutexGuard<'_, Fifo> {
    fifo.lock().unwrap_or_else(|p| p.into_inner())
}

impl Fifo {
    pub fn new(channels: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity((MAX_FRAMES + 5_760) * channels),
            channels,
            priming: true,
        }
    }

    /// Buffers complete frames in GameStream order (FL FR FC LFE BL BR SL SR).
    pub fn push(&mut self, samples: &[f32], channels: usize) {
        if !matches!(channels, 1 | 2 | 6 | 8) {
            return;
        }
        if channels != self.channels {
            self.samples.clear();
            self.channels = channels;
            self.priming = true;
        }
        let len = samples.len() - samples.len() % channels;
        self.samples.extend(&samples[..len]);
        if self.samples.len() > MAX_FRAMES * channels {
            self.samples.drain(..self.samples.len() - TARGET_FRAMES * channels);
        }
    }
}

/// Reads complete PCM frames without blocking playback on the producer.
pub fn fill<T: Copy>(
    fifo: &Mutex<Fifo>,
    out: &mut [T],
    stride: usize,
    channels: usize,
    convert: impl Fn(f32) -> T,
    silence: T,
) {
    let mut fifo = match fifo.try_lock() {
        Ok(fifo) => fifo,
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
        Err(TryLockError::WouldBlock) => {
            out.fill(silence);
            return;
        }
    };
    let source = fifo.channels;
    if stride < channels
        || !matches!(channels, 1 | 2 | 6 | 8)
        || (fifo.priming && fifo.samples.len() < TARGET_FRAMES * source)
    {
        out.fill(silence);
        return;
    }
    fifo.priming = false;
    let mut frames = out.chunks_exact_mut(stride);
    for frame in &mut frames {
        if fifo.samples.len() < source {
            fifo.priming = true;
            frame.fill(silence);
            continue;
        }
        let (frame, unused) = frame.split_at_mut(channels);
        unused.fill(silence);
        if source == channels {
            for dst in frame {
                *dst = convert(fifo.samples.pop_front().unwrap());
            }
            continue;
        }
        let mut input = [0.0; MAX_CHANNELS];
        for sample in &mut input[..source] {
            *sample = fifo.samples.pop_front().unwrap();
        }
        mix_frame(input, source, frame, &convert, silence);
    }
    frames.into_remainder().fill(silence);
}

fn mix_frame<T: Copy>(
    input: [f32; MAX_CHANNELS],
    source: usize,
    out: &mut [T],
    convert: &impl Fn(f32) -> T,
    silence: T,
) {
    const SURROUND_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let [fl, fr, centre, lfe, bl, br, sl, sr] = input;
    out.fill(silence);
    if out.len() <= 2 {
        let (left, right) = match source {
            1 => (fl, fl),
            2 => (fl, fr),
            _ => {
                // Keep centre and surround content without clipping. Stereo
                // bass management is left to the playback system.
                let sum = 1.0 + SURROUND_GAIN * if source == 8 { 3.0 } else { 2.0 };
                (
                    (fl + SURROUND_GAIN * (centre + bl + sl)) / sum,
                    (fr + SURROUND_GAIN * (centre + br + sr)) / sum,
                )
            }
        };
        if out.len() == 1 {
            out[0] = convert((left + right) * 0.5);
        } else {
            out[0] = convert(left);
            out[1] = convert(right);
        }
    } else if source == 8 {
        // On a 5.1 route, combine the rear and side pairs.
        let left = (bl + SURROUND_GAIN * sl) / (1.0 + SURROUND_GAIN);
        let right = (br + SURROUND_GAIN * sr) / (1.0 + SURROUND_GAIN);
        for (dst, sample) in out.iter_mut().zip([fl, fr, centre, lfe, left, right]) {
            *dst = convert(sample);
        }
    } else {
        for (dst, sample) in out
            .iter_mut()
            .zip([fl, if source == 1 { fl } else { fr }, centre, lfe, bl, br])
        {
            *dst = convert(sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_primes_then_preserves_all_surround_channels() {
        for channels in [6, 8] {
            let source = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
            let frame = &source[..channels];
            let fifo = Mutex::new(Fifo::new(channels));
            let mut out = vec![1.0; channels + 1];
            fill(&fifo, &mut out, channels, channels, |sample| sample, 0.0);
            assert!(out.iter().all(|&sample| sample == 0.0));

            lock(&fifo).push(&frame.repeat(TARGET_FRAMES), channels);
            fill(&fifo, &mut out, channels, channels, |sample| sample, 0.0);
            assert_eq!(&out[..channels], frame);
            assert_eq!(out[channels], 0.0);
            fill(&fifo, &mut out, channels, channels, |sample| sample, 0.0);
            assert_eq!(
                &out[..channels],
                frame,
                "a partial device buffer cannot shift channel positions"
            );
        }
    }

    #[test]
    fn callback_does_not_wait_for_a_busy_producer() {
        let fifo = Mutex::new(Fifo::new(2));
        let _producer = lock(&fifo);
        let mut out = [1.0f32; 4];
        fill(&fifo, &mut out, 2, 2, |sample| sample, 0.0);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn push_trims_to_the_target_in_whole_frames() {
        let channels = 8;
        let mut fifo = Fifo::new(channels);
        fifo.push(&[0.1; (MAX_FRAMES + 12) * 8 + 1], channels);
        assert_eq!(fifo.samples.len(), TARGET_FRAMES * channels);
    }

    #[test]
    fn surround_fallback_preserves_centre_and_combines_surrounds() {
        let fifo = Mutex::new(Fifo::new(8));
        let source = [0.0, 0.0, 1.0, 0.5, 0.1, 0.2, 0.3, 0.4];
        lock(&fifo).push(&source.repeat(TARGET_FRAMES), 8);
        let mut surround = [0.0; 6];
        fill(&fifo, &mut surround, 6, 6, |sample| sample, 0.0);
        assert_eq!(&surround[..4], &source[..4]);
        assert!((0.1..0.3).contains(&surround[4]));
        assert!((0.2..0.4).contains(&surround[5]));
        let mut stereo = [0.0; 2];
        fill(&fifo, &mut stereo, 2, 2, |sample| sample, 0.0);
        assert!(stereo[1] > stereo[0] && stereo[0] > 0.0);
        for channels in [6, 8] {
            let mut stereo = [0.0; 2];
            let mut full_scale = [0.0; MAX_CHANNELS];
            full_scale[..channels].fill(1.0);
            mix_frame(full_scale, channels, &mut stereo, &|sample| sample, 0.0);
            assert!(stereo.iter().all(|&sample| sample <= 1.0));
        }
    }
}
