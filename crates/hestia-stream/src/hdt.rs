//! HDT/1 WAN receiver feedback. See docs/hdt-feedback.md for the wire contract.

use crate::StreamStats;

pub(crate) const ATTRIBUTE: &str = "x-hermes-hdt.feedback";
pub(crate) const MESSAGE_TYPE: u16 = 0x4844;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackMode {
    Off,
    Unsupported,
    ControlEncryptionRequired,
    Active,
}

impl FeedbackMode {
    pub(crate) fn negotiate(enabled: bool, sdp: &str, encrypted_control: bool) -> Self {
        if !enabled {
            return Self::Off;
        }
        let mut versions = sdp.lines().filter_map(|line| {
            let (name, value) = line.trim().strip_prefix("a=")?.split_once(':')?;
            (name == ATTRIBUTE).then(|| value.trim())
        });
        if versions.next() != Some("1") || versions.next().is_some() {
            return Self::Unsupported;
        }
        if encrypted_control {
            Self::Active
        } else {
            Self::ControlEncryptionRequired
        }
    }
}

/// One self-contained sample. Counters are cumulative so gaps in the
/// feedback path cost resolution, without losing the intervening totals.
pub(crate) fn encode(elapsed_ms: u64, stats: &StreamStats, late_packets: u64, rtt: Option<(u32, u32)>) -> [u8; 64] {
    let mut out = [0; 64];
    out[..4].copy_from_slice(b"HDT1");
    let counts = [
        elapsed_ms,
        stats.packets,
        stats.bytes,
        stats.frames,
        stats.recovered_packets,
        stats.lost_frames,
        late_packets,
    ];
    for (field, value) in out[4..60].chunks_exact_mut(8).zip(counts) {
        field.copy_from_slice(&value.to_be_bytes());
    }
    let measured = |ms: u32| ms.min(u16::MAX as u32 - 1) as u16;
    let (rtt, variance) = rtt
        .map(|(r, v)| (measured(r), measured(v)))
        .unwrap_or((u16::MAX, u16::MAX));
    out[60..62].copy_from_slice(&rtt.to_be_bytes());
    out[62..64].copy_from_slice(&variance.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_an_unambiguous_host_offer_opt_in_and_encrypted_control() {
        let offer = "v=0\r\na=x-hermes-hdt.feedback:1 \r\n";
        assert_eq!(FeedbackMode::negotiate(true, offer, true), FeedbackMode::Active);
        assert_eq!(FeedbackMode::negotiate(false, offer, true), FeedbackMode::Off);
        assert_eq!(
            FeedbackMode::negotiate(true, offer, false),
            FeedbackMode::ControlEncryptionRequired
        );
        for invalid in [
            "",
            "a=x-hermes-hdt.feedback:10",
            "a=x-hermes-hdt.feedback:1x",
            "a=x-other:x-hermes-hdt.feedback:1",
            "a=x-hermes-hdt.feedback:2\na=x-hermes-hdt.feedback:1",
        ] {
            assert_eq!(FeedbackMode::negotiate(true, invalid, true), FeedbackMode::Unsupported);
        }
    }

    #[test]
    fn wire_vector_and_rtt_sentinels() {
        let stats = StreamStats {
            packets: 2,
            bytes: 3,
            frames: 4,
            recovered_packets: 5,
            lost_frames: 6,
            ..Default::default()
        };
        let payload = encode(1, &stats, 7, Some((0x1234, 0x5678)));
        assert_eq!(&payload[..4], b"HDT1");
        for (index, field) in payload[4..60].chunks_exact(8).enumerate() {
            assert_eq!(field, &[0, 0, 0, 0, 0, 0, 0, index as u8 + 1]);
        }
        assert_eq!(&payload[60..], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(&encode(0, &stats, 0, None)[60..], &[0xff; 4]);
        assert_eq!(&encode(0, &stats, 0, Some((u32::MAX, 0)))[60..], &[0xff, 0xfe, 0, 0]);
    }
}
