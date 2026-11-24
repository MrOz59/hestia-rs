//! What happened to the audio between the network and the speaker, for the
//! session report. Updated from the network thread (decoding, buffering)
//! and from the output's callback (playing), so everything is atomic.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::audio_fifo::SAMPLE_RATE;

#[derive(Debug, Default)]
pub struct AudioStats {
    /// Packets decoded, and lost ones concealed.
    pub decoded: AtomicU64,
    pub concealed: AtomicU64,
    pub decode_errors: AtomicU64,
    /// The decoder had no free input buffer at first, and how many packets
    /// it still had none for after waiting (those are played as gaps).
    pub decoder_busy_waits: AtomicU64,
    pub decoder_busy_drops: AtomicU64,

    /// The playback buffer ran dry while playing, and the silence that
    /// followed until it had refilled (in frames).
    pub underruns: AtomicU64,
    pub underrun_silence_frames: AtomicU64,
    /// The buffer held too much (a burst after a stall) and skipped ahead.
    pub trims: AtomicU64,
    pub trimmed_frames: AtomicU64,
    /// How much the buffer aims to hold (frames), now and at most.
    pub target_frames: AtomicU64,
    pub max_target_frames: AtomicU64,
    /// Output callbacks that found the buffer locked by the decoder side
    /// for too long and played silence instead.
    pub lock_misses: AtomicU64,

    /// The output device: its id (AAudio; 0 unknown), burst and buffer
    /// sizes, the underruns it counted itself, and callback sizes seen.
    pub device_id: AtomicI64,
    pub burst_frames: AtomicU64,
    pub buffer_frames: AtomicU64,
    pub device_xruns: AtomicU64,
    pub min_callback_frames: AtomicU64,
    pub max_callback_frames: AtomicU64,
    /// The output was opened again (route change, headphones, Bluetooth).
    pub reopened: AtomicU64,
}

fn ms(frames: u64) -> f64 {
    frames as f64 * 1000.0 / f64::from(SAMPLE_RATE)
}

impl AudioStats {
    /// Called by the output for each callback of `frames` frames.
    pub fn note_callback(&self, frames: u64) {
        let _ = self
            .min_callback_frames
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |min| {
                (min == 0 || frames < min).then_some(frames)
            });
        self.max_callback_frames.fetch_max(frames, Ordering::Relaxed);
    }

    pub(crate) fn note_target(&self, frames: usize) {
        self.target_frames.store(frames as u64, Ordering::Relaxed);
        self.max_target_frames.fetch_max(frames as u64, Ordering::Relaxed);
    }

    /// The report's lines for the playback side, in order.
    pub fn report(&self) -> Vec<(&'static str, String)> {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut lines = vec![
            ("packets decoded", get(&self.decoded).to_string()),
            ("lost packets concealed", get(&self.concealed).to_string()),
            (
                "decoder busy (waited / dropped)",
                format!("{} / {}", get(&self.decoder_busy_waits), get(&self.decoder_busy_drops)),
            ),
            ("decode errors", get(&self.decode_errors).to_string()),
            (
                "buffer underruns",
                format!(
                    "{} ({:.0} ms of silence)",
                    get(&self.underruns),
                    ms(get(&self.underrun_silence_frames))
                ),
            ),
            (
                "buffer skips after bursts",
                format!("{} ({:.0} ms skipped)", get(&self.trims), ms(get(&self.trimmed_frames))),
            ),
            (
                "buffer target",
                format!(
                    "{:.0} ms at the end / {:.0} ms largest",
                    ms(get(&self.target_frames)),
                    ms(get(&self.max_target_frames))
                ),
            ),
            (
                "output callbacks that found the buffer busy",
                get(&self.lock_misses).to_string(),
            ),
        ];
        let burst = get(&self.burst_frames);
        if burst > 0 {
            let buffer = get(&self.buffer_frames);
            lines.push((
                "output burst / buffer",
                format!("{burst} / {buffer} frames ({:.1} / {:.1} ms)", ms(burst), ms(buffer)),
            ));
            lines.push(("output underruns (device)", get(&self.device_xruns).to_string()));
        }
        let max_callback = get(&self.max_callback_frames);
        if max_callback > 0 {
            lines.push((
                "output callback sizes",
                format!(
                    "{}-{max_callback} frames ({:.1}-{:.1} ms)",
                    get(&self.min_callback_frames),
                    ms(get(&self.min_callback_frames)),
                    ms(max_callback)
                ),
            ));
        }
        lines.push(("output reopened", get(&self.reopened).to_string()));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_sizes_keep_the_smallest_and_largest() {
        let s = AudioStats::default();
        for frames in [192, 96, 960] {
            s.note_callback(frames);
        }
        assert_eq!(s.min_callback_frames.load(Ordering::Relaxed), 96);
        assert_eq!(s.max_callback_frames.load(Ordering::Relaxed), 960);
        let report = s.report();
        let sizes = report.iter().find(|(k, _)| *k == "output callback sizes").unwrap();
        assert_eq!(sizes.1, "96-960 frames (2.0-20.0 ms)");
    }
}
