//! The small sample FIFO between a network thread that decodes audio
//! packets and the audio API's callback that plays them.
//!
//! The FIFO is kept short: when it grows past [`MAX_LATENCY_MS`] the oldest
//! samples are dropped, so clock drift between host and device never turns
//! into growing delay; after an underrun it refills to [`TARGET_LATENCY_MS`]
//! before playing again.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Target and maximum buffered audio.
pub const TARGET_LATENCY_MS: usize = 30;
pub const MAX_LATENCY_MS: usize = 80;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

#[derive(Debug, Default)]
pub struct Fifo {
    pub samples: VecDeque<f32>,
    /// Waiting to refill to the target after an underrun.
    pub priming: bool,
}

pub fn lock(fifo: &Mutex<Fifo>) -> std::sync::MutexGuard<'_, Fifo> {
    fifo.lock().unwrap_or_else(|p| p.into_inner())
}

/// Fills `out` (interleaved, `channels` per frame) from the stereo FIFO.
pub fn fill<T: Copy>(fifo: &Mutex<Fifo>, out: &mut [T], channels: usize, convert: impl Fn(f32) -> T, silence: T) {
    let mut f = lock(fifo);
    let target = TARGET_LATENCY_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
    if f.priming && f.samples.len() < target {
        out.fill(silence);
        return;
    }
    f.priming = false;
    for frame in out.chunks_mut(channels) {
        if f.samples.len() < CHANNELS {
            f.priming = true;
            frame.fill(silence);
            continue;
        }
        let (l, r) = (
            f.samples.pop_front().unwrap_or(0.0),
            f.samples.pop_front().unwrap_or(0.0),
        );
        match frame.len() {
            1 => frame[0] = convert((l + r) * 0.5),
            _ => {
                frame[0] = convert(l);
                frame[1] = convert(r);
                for extra in &mut frame[2..] {
                    *extra = silence;
                }
            }
        }
    }
}

impl Fifo {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            priming: true,
        }
    }

    /// Appends interleaved stereo samples, trimming back to the target
    /// latency when the buffer has grown too long.
    pub fn push(&mut self, stereo: &[f32]) {
        self.samples.extend(stereo);
        let max = MAX_LATENCY_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
        if self.samples.len() > max {
            let excess = self.samples.len() - TARGET_LATENCY_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
            self.samples.drain(..excess - excess % CHANNELS);
        }
    }
}

/// Mixes `channels`-channel interleaved samples down to stereo, in the
/// GameStream channel order (FL FR FC LFE BL BR SL SR). Stereo passes
/// through; mono is duplicated.
pub fn downmix_to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    match channels {
        0 => {}
        1 => input.iter().for_each(|&s| out.extend([s, s])),
        2 => out.extend_from_slice(input),
        n => {
            const SIDE: f32 = std::f32::consts::FRAC_1_SQRT_2;
            // Centre and the surrounds at -3 dB; scaled so a full-scale
            // signal on every channel can't clip. LFE is left out.
            let gain = 1.0 / (1.0 + SIDE * if n >= 8 { 3.0 } else { 2.0 });
            for frame in input.chunks_exact(n) {
                let at = |i: usize| frame.get(i).copied().unwrap_or(0.0);
                let (centre, back_l, back_r) = (at(2), at(4), at(5));
                let (side_l, side_r) = (at(6), at(7));
                let l = at(0) + SIDE * (centre + back_l + side_l);
                let r = at(1) + SIDE * (centre + back_r + side_r);
                out.extend([l * gain, r * gain]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_primes_then_plays_and_handles_mono() {
        let fifo = Mutex::new(Fifo::new());
        let mut out = [9.0f32; 8];
        fill(&fifo, &mut out, 2, |s| s, 0.0);
        assert_eq!(out, [0.0; 8], "silence while priming");

        let target = TARGET_LATENCY_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
        lock(&fifo)
            .samples
            .extend((0..target).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }));
        fill(&fifo, &mut out, 2, |s| s, 0.0);
        assert_eq!(out, [0.5, -0.5, 0.5, -0.5, 0.5, -0.5, 0.5, -0.5]);

        let mut mono = [1.0f32; 2];
        fill(&fifo, &mut mono, 1, |s| s, 0.0);
        assert_eq!(mono, [0.0, 0.0]);
    }

    #[test]
    fn push_trims_to_the_target() {
        let mut f = Fifo::new();
        let max = MAX_LATENCY_MS * SAMPLE_RATE as usize / 1000 * CHANNELS;
        f.push(&vec![0.1; max + 100]);
        assert_eq!(
            f.samples.len(),
            TARGET_LATENCY_MS * SAMPLE_RATE as usize / 1000 * CHANNELS
        );
    }

    #[test]
    fn downmixes_surround() {
        let mut out = Vec::new();
        downmix_to_stereo(&[0.5, -0.5], 2, &mut out);
        assert_eq!(out, [0.5, -0.5]);
        downmix_to_stereo(&[0.25], 1, &mut out);
        assert_eq!(out, [0.25, 0.25]);
        // 5.1: only the centre speaks; it lands equally left and right.
        downmix_to_stereo(&[0.0, 0.0, 1.0, 1.0, 0.0, 0.0], 6, &mut out);
        assert!(out[0] > 0.0 && (out[0] - out[1]).abs() < 1e-6);
        // Full scale everywhere never clips.
        downmix_to_stereo(&[1.0; 6], 6, &mut out);
        assert!(out.iter().all(|&s| s <= 1.0), "{out:?}");
        downmix_to_stereo(&[1.0; 8], 8, &mut out);
        assert!(out.iter().all(|&s| s <= 1.0), "{out:?}");
    }
}
