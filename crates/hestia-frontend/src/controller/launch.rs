//! Timing starts with the launch page. The first-picture deadline starts
//! separately, after the host has accepted the stream connection.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hestia_core::stream::video::FIRST_DATA_TIMEOUT;
use hestia_media::{AudioFormat, AudioSink};

const MINIMUM_DISPLAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub(super) struct Launch {
    pub seq: u64,
    pub shown_at: Instant,
}

#[derive(Debug, PartialEq)]
pub(super) enum LaunchStatus {
    Wait(Duration),
    Reveal,
    TimedOut,
}

impl Launch {
    pub fn status(self, now: Instant, connected: Instant, picture_ready: bool) -> LaunchStatus {
        let minimum_left = MINIMUM_DISPLAY.saturating_sub(now.duration_since(self.shown_at));
        if !minimum_left.is_zero() {
            return LaunchStatus::Wait(minimum_left);
        }
        if picture_ready {
            return LaunchStatus::Reveal;
        }
        let time_left = FIRST_DATA_TIMEOUT.saturating_sub(now.duration_since(connected));
        if time_left.is_zero() {
            LaunchStatus::TimedOut
        } else {
            // Only while waiting for the first presentation. Normal playback
            // already records its handoffs for CPU, native GPU and direct video.
            LaunchStatus::Wait(time_left.min(Duration::from_millis(16)))
        }
    }
}

/// Set up the audio device during negotiation; start it at live audio when
/// the picture is revealed. Startup packets are never queued for replay.
pub(super) struct LaunchAudio {
    pub output: Box<dyn AudioSink>,
    pub revealed: Arc<AtomicBool>,
}

impl AudioSink for LaunchAudio {
    fn setup(&mut self, format: &AudioFormat) -> Result<(), String> {
        self.output.setup(format)
    }

    fn submit(&mut self, packet: &[u8]) {
        if self.revealed.load(Ordering::Relaxed) {
            self.output.submit(packet);
        }
    }

    fn cleanup(&mut self) {
        self.output.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_and_slow_hosts_share_the_visible_minimum() {
        let start = Instant::now();
        let launch = Launch {
            seq: 1,
            shown_at: start,
        };
        let early = start + Duration::from_millis(450);
        assert_eq!(
            launch.status(early, early, true),
            LaunchStatus::Wait(Duration::from_millis(1550))
        );
        assert_eq!(
            launch.status(start + MINIMUM_DISPLAY, early, true),
            LaunchStatus::Reveal
        );
        let late = start + Duration::from_secs(9);
        assert!(matches!(launch.status(late, late, false), LaunchStatus::Wait(_)));
        assert_eq!(launch.status(late, late, true), LaunchStatus::Reveal);
        assert_eq!(
            launch.status(late + FIRST_DATA_TIMEOUT, late, false),
            LaunchStatus::TimedOut
        );
    }
}
