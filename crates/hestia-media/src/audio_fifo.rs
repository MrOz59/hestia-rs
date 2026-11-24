//! Bounded PCM FIFO between the network decoder and the audio device.
//! Latency is measured in frames, so it stays constant across channel layouts.
//!
//! The buffer aims to hold [`TARGET_LATENCY_MS`] before it plays. Each time
//! it runs dry while playing (a Wi-Fi stall, Bluetooth sharing the radio) it
//! aims for more, up to [`MAX_TARGET_LATENCY_MS`], and gives that back
//! slowly once the link has been steady. Lost packets are faded around
//! instead of cut, and skipping ahead after a burst crossfades.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use crate::audio_stats::AudioStats;

/// What the buffer aims to hold at first, and at least.
pub const TARGET_LATENCY_MS: usize = 30;
/// The most it aims for after repeated underruns.
pub const MAX_TARGET_LATENCY_MS: usize = 120;
/// Holding more than the target plus this skips ahead (a burst after a stall).
pub const HEADROOM_MS: usize = 50;
pub const SAMPLE_RATE: u32 = 48_000;
pub const MAX_CHANNELS: usize = 8;
const FRAMES_PER_MS: usize = SAMPLE_RATE as usize / 1_000;
const TARGET_FRAMES: usize = TARGET_LATENCY_MS * FRAMES_PER_MS;
const MAX_TARGET_FRAMES: usize = MAX_TARGET_LATENCY_MS * FRAMES_PER_MS;
const HEADROOM_FRAMES: usize = HEADROOM_MS * FRAMES_PER_MS;
/// Each underrun raises the target by this much...
const GROW_FRAMES: usize = 15 * FRAMES_PER_MS;
/// ...and each 10 s played without one lowers it by this much.
const SHRINK_FRAMES: usize = 5 * FRAMES_PER_MS;
const QUIET_FRAMES: u64 = 10 * SAMPLE_RATE as u64;
/// Fades around gaps and skips: 2 ms, short enough not to be heard as one.
const FADE_FRAMES: usize = 2 * FRAMES_PER_MS;
/// How long the output callback tries for the lock before playing silence.
const LOCK_SPINS: u32 = 2_000;

#[derive(Debug)]
pub struct Fifo {
    samples: VecDeque<f32>,
    channels: usize,
    priming: bool,
    /// Played at least once: running dry after that is an underrun.
    started: bool,
    /// Frames the buffer aims to hold before playing.
    target: usize,
    /// Frames played since the last underrun (or target reduction).
    steady: u64,
    /// The next decoded audio follows a gap and fades in.
    fade_in: bool,
    stats: Arc<AudioStats>,
}

pub fn lock(fifo: &Mutex<Fifo>) -> MutexGuard<'_, Fifo> {
    fifo.lock().unwrap_or_else(|p| p.into_inner())
}

impl Fifo {
    pub fn new(channels: usize, stats: Arc<AudioStats>) -> Self {
        stats.note_target(TARGET_FRAMES);
        Self {
            samples: VecDeque::with_capacity((MAX_TARGET_FRAMES + HEADROOM_FRAMES + 5_760) * channels),
            channels,
            priming: true,
            started: false,
            target: TARGET_FRAMES,
            steady: 0,
            fade_in: false,
            stats,
        }
    }

    pub fn stats(&self) -> &Arc<AudioStats> {
        &self.stats
    }

    /// Frames waiting to be played.
    pub fn queued_frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    /// Buffers complete frames in GameStream order (FL FR FC LFE BL BR SL SR).
    pub fn push(&mut self, samples: &[f32], channels: usize) {
        if !self.accept(channels) {
            return;
        }
        let len = samples.len() - samples.len() % channels;
        let start = self.samples.len();
        self.samples.extend(&samples[..len]);
        if self.fade_in && len > 0 {
            self.fade_in = false;
            self.ramp(start, len / channels, true);
        }
        self.trim();
    }

    /// Stands in for `frames` of lost audio: the audio before it fades out,
    /// silence fills its place, and the audio after it fades in.
    pub fn push_gap(&mut self, frames: usize, channels: usize) {
        if !self.accept(channels) {
            return;
        }
        if !self.fade_in {
            let queued = self.queued_frames();
            let fade = FADE_FRAMES.min(queued);
            self.ramp((queued - fade) * channels, fade, false);
        }
        self.samples.extend(std::iter::repeat_n(0.0, frames * channels));
        self.fade_in = true;
        self.trim();
    }

    fn accept(&mut self, channels: usize) -> bool {
        if !matches!(channels, 1 | 2 | 6 | 8) {
            return false;
        }
        if channels != self.channels {
            self.samples.clear();
            self.channels = channels;
            self.priming = true;
            self.fade_in = false;
        }
        true
    }

    /// Scales `frames` frames from sample `start` by a linear ramp, up
    /// (fade in) or down (fade out).
    fn ramp(&mut self, start: usize, frames: usize, up: bool) {
        let frames = frames.min(FADE_FRAMES);
        let channels = self.channels;
        for frame in 0..frames {
            let step = (frame + 1) as f32 / (frames + 1) as f32;
            let gain = if up { step } else { 1.0 - step };
            for channel in 0..channels {
                if let Some(sample) = self.samples.get_mut(start + frame * channels + channel) {
                    *sample *= gain;
                }
            }
        }
    }

    /// Too far ahead of the target (a burst after a stall): skip to the
    /// target, crossfading from what would have played into what will.
    fn trim(&mut self) {
        let queued = self.queued_frames();
        if queued <= self.target + HEADROOM_FRAMES {
            return;
        }
        let skip = queued - self.target;
        let channels = self.channels;
        let fade = FADE_FRAMES.min(self.target);
        for frame in 0..fade {
            let mix = (frame + 1) as f32 / (fade + 1) as f32;
            for channel in 0..channels {
                let old = self.samples[frame * channels + channel];
                let new = &mut self.samples[(skip + frame) * channels + channel];
                *new = old * (1.0 - mix) + *new * mix;
            }
        }
        self.samples.drain(..skip * channels);
        self.stats.trims.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.stats
            .trimmed_frames
            .fetch_add(skip as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// The buffer ran dry while playing: aim for more next time.
    fn underrun(&mut self) {
        self.stats.underruns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.set_target(self.target + GROW_FRAMES);
        self.steady = 0;
    }

    /// An output that asks for big blocks (Bluetooth) needs at least two of
    /// them buffered, or every callback runs dry.
    fn fit_callback(&mut self, frames: usize) {
        if self.target < frames * 2 {
            self.set_target(frames * 2);
        }
    }

    /// Played steadily for a while: give some latency back.
    fn relax(&mut self, played: usize) {
        self.steady += played as u64;
        if self.steady >= QUIET_FRAMES {
            self.steady = 0;
            if self.target > TARGET_FRAMES {
                self.set_target(self.target.saturating_sub(SHRINK_FRAMES).max(TARGET_FRAMES));
            }
        }
    }

    fn set_target(&mut self, frames: usize) {
        self.target = frames.clamp(TARGET_FRAMES, MAX_TARGET_FRAMES);
        self.stats.note_target(self.target);
    }
}

/// Takes the lock without making the output wait on the decoder side for
/// long: that side only holds it to append a packet's samples.
fn lock_briefly(fifo: &Mutex<Fifo>) -> Option<MutexGuard<'_, Fifo>> {
    for _ in 0..LOCK_SPINS {
        match fifo.try_lock() {
            Ok(fifo) => return Some(fifo),
            Err(TryLockError::Poisoned(error)) => return Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }
    None
}

/// Reads complete PCM frames without blocking playback on the producer.
pub fn fill<T: Copy>(
    fifo: &Mutex<Fifo>,
    stats: &AudioStats,
    out: &mut [T],
    stride: usize,
    channels: usize,
    convert: impl Fn(f32) -> T,
    silence: T,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(mut fifo) = lock_briefly(fifo) else {
        stats.lock_misses.fetch_add(1, Relaxed);
        out.fill(silence);
        return;
    };
    let source = fifo.channels;
    if stride < channels || stride == 0 || !matches!(channels, 1 | 2 | 6 | 8) {
        out.fill(silence);
        return;
    }
    let wanted = out.len() / stride;
    fifo.fit_callback(wanted);
    if fifo.priming && fifo.samples.len() < fifo.target * source {
        if fifo.started {
            fifo.stats.underrun_silence_frames.fetch_add(wanted as u64, Relaxed);
        }
        out.fill(silence);
        return;
    }
    fifo.priming = false;
    fifo.started = true;
    let mut played = 0;
    let mut frames = out.chunks_exact_mut(stride);
    for frame in &mut frames {
        if fifo.samples.len() < source {
            if !fifo.priming {
                fifo.priming = true;
                fifo.underrun();
            }
            fifo.stats.underrun_silence_frames.fetch_add(1, Relaxed);
            frame.fill(silence);
            continue;
        }
        played += 1;
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
    fifo.relax(played);
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
    use std::sync::atomic::Ordering::Relaxed;

    fn fifo(channels: usize) -> (Mutex<Fifo>, Arc<AudioStats>) {
        let stats = Arc::new(AudioStats::default());
        (Mutex::new(Fifo::new(channels, stats.clone())), stats)
    }

    fn read(fifo: &Mutex<Fifo>, stats: &AudioStats, frames: usize, channels: usize) -> Vec<f32> {
        let mut out = vec![9.0; frames * channels];
        fill(fifo, stats, &mut out, channels, channels, |s| s, 0.0);
        out
    }

    #[test]
    fn fifo_primes_then_preserves_all_surround_channels() {
        for channels in [6, 8] {
            let source = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
            let frame = &source[..channels];
            let (fifo, stats) = fifo(channels);
            let mut out = vec![1.0; channels + 1];
            fill(&fifo, &stats, &mut out, channels, channels, |sample| sample, 0.0);
            assert!(out.iter().all(|&sample| sample == 0.0));

            lock(&fifo).push(&frame.repeat(TARGET_FRAMES), channels);
            fill(&fifo, &stats, &mut out, channels, channels, |sample| sample, 0.0);
            assert_eq!(&out[..channels], frame);
            assert_eq!(out[channels], 0.0);
            fill(&fifo, &stats, &mut out, channels, channels, |sample| sample, 0.0);
            assert_eq!(
                &out[..channels],
                frame,
                "a partial device buffer cannot shift channel positions"
            );
        }
    }

    #[test]
    fn callback_does_not_wait_for_a_busy_producer() {
        let (fifo, stats) = fifo(2);
        let _producer = lock(&fifo);
        let mut out = [1.0f32; 4];
        fill(&fifo, &stats, &mut out, 2, 2, |sample| sample, 0.0);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(stats.lock_misses.load(Relaxed), 1);
    }

    #[test]
    fn push_trims_to_the_target_in_whole_frames() {
        let channels = 8;
        let (fifo, stats) = fifo(channels);
        let mut fifo = lock(&fifo);
        fifo.push(&[0.1; (TARGET_FRAMES + HEADROOM_FRAMES + 12) * 8 + 1], channels);
        assert_eq!(fifo.samples.len(), TARGET_FRAMES * channels);
        assert_eq!(stats.trims.load(Relaxed), 1);
        assert_eq!(stats.trimmed_frames.load(Relaxed), (HEADROOM_FRAMES + 12) as u64);
    }

    #[test]
    fn a_skip_crossfades_from_what_would_have_played() {
        let (fifo, _) = fifo(1);
        let mut fifo = lock(&fifo);
        // 1.0 now, -1.0 after the skip point: the first frames played after
        // the skip start near 1.0 and end at -1.0, with no jump.
        let mut samples = vec![1.0; HEADROOM_FRAMES + 1];
        samples.extend(vec![-1.0; TARGET_FRAMES]);
        fifo.push(&samples, 1);
        let head: Vec<f32> = fifo.samples.iter().take(FADE_FRAMES + 1).copied().collect();
        assert!(head[0] > 0.9, "{}", head[0]);
        assert!(head.windows(2).all(|w| w[1] <= w[0]));
        assert_eq!(head[FADE_FRAMES], -1.0);
    }

    #[test]
    fn an_underrun_raises_the_target_and_steady_play_lowers_it() {
        let (fifo, stats) = fifo(2);
        lock(&fifo).push(&vec![0.5; TARGET_FRAMES * 2], 2);
        for _ in 0..TARGET_FRAMES / 96 {
            assert!(read(&fifo, &stats, 96, 2).iter().all(|&s| s == 0.5));
        }
        // Dry: silence, an underrun, and 15 ms more to buffer next time.
        let out = read(&fifo, &stats, 96, 2);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(stats.underruns.load(Relaxed), 1);
        assert_eq!(lock(&fifo).target, TARGET_FRAMES + GROW_FRAMES);
        // Still priming: the old target isn't enough any more.
        lock(&fifo).push(&vec![0.5; TARGET_FRAMES * 2], 2);
        assert!(read(&fifo, &stats, 96, 2).iter().all(|&s| s == 0.0));
        assert_eq!(stats.underruns.load(Relaxed), 1, "one underrun, however long");
        lock(&fifo).push(&vec![0.5; GROW_FRAMES * 2], 2);
        assert!(read(&fifo, &stats, 96, 2).iter().all(|&s| s == 0.5));
        assert!(stats.underrun_silence_frames.load(Relaxed) >= 192);

        // Ten quiet seconds give 5 ms back, never below the start.
        for _ in 0..(QUIET_FRAMES as usize / 96) {
            lock(&fifo).push(&vec![0.5; 96 * 2], 2);
            read(&fifo, &stats, 96, 2);
        }
        assert_eq!(lock(&fifo).target, TARGET_FRAMES + GROW_FRAMES - SHRINK_FRAMES);
        assert_eq!(
            stats.max_target_frames.load(Relaxed),
            (TARGET_FRAMES + GROW_FRAMES) as u64
        );
        assert_eq!(stats.underruns.load(Relaxed), 1);
    }

    #[test]
    fn the_target_covers_two_large_callbacks_up_to_the_cap() {
        let (fifo, stats) = fifo(2);
        read(&fifo, &stats, 960, 2);
        assert_eq!(lock(&fifo).target, 1920, "20 ms callbacks need 40 ms buffered");
        read(&fifo, &stats, 9_600, 2);
        assert_eq!(lock(&fifo).target, MAX_TARGET_FRAMES);
        assert_eq!(stats.underruns.load(Relaxed), 0, "priming is not an underrun");
        assert_eq!(stats.underrun_silence_frames.load(Relaxed), 0);
    }

    #[test]
    fn a_gap_fades_out_then_in() {
        let (fifo, _) = fifo(1);
        let mut fifo = lock(&fifo);
        fifo.push(&[1.0; 240], 1);
        fifo.push_gap(240, 1);
        fifo.push(&[1.0; 240], 1);
        let s: Vec<f32> = fifo.samples.iter().copied().collect();
        assert_eq!(s[0], 1.0);
        assert!(s[239] < 0.05, "the audio before the gap fades out: {}", s[239]);
        assert!(s[240..480].iter().all(|&x| x == 0.0));
        assert!(s[480] < 0.05, "and the audio after it fades in: {}", s[480]);
        assert_eq!(s[480 + FADE_FRAMES], 1.0);
        // Back to back gaps fade once.
        fifo.push_gap(240, 1);
        fifo.push_gap(240, 1);
        assert!(fifo.fade_in);
    }

    #[test]
    fn surround_fallback_preserves_centre_and_combines_surrounds() {
        let (fifo, stats) = fifo(8);
        let source = [0.0, 0.0, 1.0, 0.5, 0.1, 0.2, 0.3, 0.4];
        lock(&fifo).push(&source.repeat(TARGET_FRAMES), 8);
        let mut surround = [0.0; 6];
        fill(&fifo, &stats, &mut surround, 6, 6, |sample| sample, 0.0);
        assert_eq!(&surround[..4], &source[..4]);
        assert!((0.1..0.3).contains(&surround[4]));
        assert!((0.2..0.4).contains(&surround[5]));
        let mut stereo = [0.0; 2];
        fill(&fifo, &stats, &mut stereo, 2, 2, |sample| sample, 0.0);
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
