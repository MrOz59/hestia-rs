//! Session-local bitrate advice. It uses the existing receiver counters, so
//! collecting it adds no work to video packet reception.

use std::time::Duration;

use crate::StreamStats;

const MAX_WINDOW: Duration = Duration::from_millis(2500);
const LOSS_HOLD: Duration = Duration::from_secs(3);
const RECOVERY_HOLD: Duration = Duration::from_secs(8);

pub(crate) struct RateAdvisor {
    ceiling: u32,
    target: u32,
    previous: Option<(Duration, StreamStats)>,
    damaged: Duration,
    clean: Duration,
}

impl RateAdvisor {
    pub(crate) fn new(ceiling_kbps: u32) -> Self {
        Self {
            ceiling: ceiling_kbps,
            target: ceiling_kbps,
            previous: None,
            damaged: Duration::ZERO,
            clean: Duration::ZERO,
        }
    }

    pub(crate) fn target(&self) -> u32 {
        self.target
    }

    /// Returns the first recommendation and subsequent changes. Repeating the
    /// absolute target on the wire is independent of recording these events.
    pub(crate) fn observe(&mut self, now: Duration, stats: &StreamStats, rtt: Option<(u32, u32)>) -> Option<u32> {
        let Some((then, previous)) = self.previous.replace((now, *stats)) else {
            return Some(self.target);
        };
        let window = now.saturating_sub(then);
        let packets = stats.packets.saturating_sub(previous.packets);
        let frames = stats.frames.saturating_sub(previous.frames);
        let losses = stats.lost_frames.saturating_sub(previous.lost_frames);
        let repairs = stats.recovered_packets.saturating_sub(previous.recovered_packets);
        let active = !window.is_zero() && window <= MAX_WINDOW && packets > 0;
        let old = self.target;
        if active && losses > 0 && losses.saturating_mul(100) >= frames.saturating_add(losses) {
            self.clean = Duration::ZERO;
            self.damaged += window;
            if self.damaged >= LOSS_HOLD {
                let floor = self.ceiling.div_ceil(4);
                self.target = self.target.saturating_sub(self.target.div_ceil(5)).max(floor);
                self.damaged = Duration::ZERO;
            }
        } else if active
            && frames >= 4
            && losses == 0
            && repairs <= packets / 100
            && rtt.is_some_and(|(rtt, variance)| variance <= 12.max(rtt / 8))
        {
            self.damaged = Duration::ZERO;
            self.clean += window;
            if self.clean >= RECOVERY_HOLD {
                self.target = self.target.saturating_add(self.ceiling.div_ceil(16)).min(self.ceiling);
                self.clean = Duration::ZERO;
            }
        } else {
            self.damaged = Duration::ZERO;
            self.clean = Duration::ZERO;
        }
        (old != self.target).then_some(self.target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Replay {
        advisor: RateAdvisor,
        stats: StreamStats,
        now: Duration,
    }

    impl Replay {
        fn new(cap: u32) -> Self {
            let mut advisor = RateAdvisor::new(cap);
            let stats = StreamStats::default();
            assert_eq!(advisor.observe(Duration::ZERO, &stats, None), Some(cap));
            Self {
                advisor,
                stats,
                now: Duration::ZERO,
            }
        }

        fn tick(&mut self, ms: u64, frames: u64, loss: u64, repairs: u64, rtt: Option<(u32, u32)>) -> Option<u32> {
            self.now += Duration::from_millis(ms);
            self.stats.packets += frames * 8 + loss + repairs;
            self.stats.frames += frames;
            self.stats.lost_frames += loss;
            self.stats.recovered_packets += repairs;
            self.advisor.observe(self.now, &self.stats, rtt)
        }
    }

    #[test]
    fn sustained_damage_then_slow_recovery_on_a_long_rtt_path() {
        let mut trace = Replay::new(20_000);
        let long_rtt = Some((300, 8));
        for _ in 0..2 {
            assert_eq!(trace.tick(1000, 58, 2, 4, long_rtt), None);
        }
        assert_eq!(trace.tick(1000, 58, 2, 4, long_rtt), Some(16_000));
        for _ in 0..7 {
            assert_eq!(trace.tick(1000, 60, 0, 0, long_rtt), None);
        }
        assert_eq!(trace.tick(1000, 60, 0, 0, long_rtt), Some(17_250));
        for _ in 0..80 {
            trace.tick(1000, 60, 0, 0, long_rtt);
        }
        assert_eq!(trace.advisor.target(), 20_000);
    }

    #[test]
    fn bursts_fec_and_scheduler_gaps_do_not_build_a_loss_streak() {
        let mut trace = Replay::new(10_000);
        for _ in 0..10 {
            for (ms, frames, lost, repaired) in [
                (1000, 55, 5, 0),
                (1000, 60, 0, 16),
                (1000, 55, 5, 0),
                (4000, 120, 10, 0),
                (1000, 0, 0, 0),
            ] {
                assert_eq!(trace.tick(ms, frames, lost, repaired, None), None);
            }
        }
    }

    #[test]
    fn pauses_repairs_and_unstable_rtt_hold_a_reduced_target() {
        let mut trace = Replay::new(4000);
        for _ in 0..60 {
            trace.tick(1000, 0, 6, 0, None);
        }
        assert_eq!(trace.advisor.target(), 1000);
        for (frames, repairs, rtt) in [
            (0, 0, Some((10, 1))),
            (60, 0, None),
            (60, 0, Some((100, 55))),
            (60, 15, Some((100, 2))),
        ] {
            for _ in 0..16 {
                assert_eq!(trace.tick(1000, frames, 0, repairs, rtt), None);
            }
        }
        assert_eq!(trace.advisor.target(), 1000);
    }
}
