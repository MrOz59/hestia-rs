//! A short offscreen decode trial during graphical Auto launches. The host
//! negotiates one format per RTSP session, so all candidates see the same
//! locally generated motion sequence before the live connection is opened.
//!
//! Each candidate decodes about half a second of a bundled clip fed at the
//! stream's frame rate. Codecs that keep up rank by their median latency
//! from scheduled input to picture, those that fall behind after them, and
//! those that fail or run out of time last. Small latency differences keep
//! the platform's order. The median, because the tail of 30 pictures is the
//! system's jitter (the same few spikes for every codec), not the codec's.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use log::info;

use crate::decode::FrameTiming;
use crate::{DecodedFrame, EncodedFrame, FrameDecoder, VideoCodec, VideoFormat};

/// Includes setup and teardown of all candidates. Never join a codec driver
/// on the launch thread: synchronous vendor calls can outlive their deadline.
pub const BUDGET: Duration = Duration::from_secs(3);
/// Left at the end of the budget for releasing the last hardware component.
const RELEASE_MARGIN: Duration = Duration::from_millis(75);
/// Frame intervals a decoder gets after the last input to deliver it.
const DRAIN_PERIODS: u32 = 3;
static PROBE_DEVICE: Mutex<()> = Mutex::new(());

struct Clip {
    width: u32,
    height: u32,
    // Length-prefixed access units, IDR first, no B-frames.
    data: &'static [u8],
}

impl Clip {
    fn access_units(&self) -> Result<Vec<&'static [u8]>, String> {
        let mut bytes = self.data;
        let mut units = Vec::new();
        while !bytes.is_empty() {
            let (size, rest) = bytes.split_first_chunk::<4>().ok_or("truncated sample")?;
            let size = u32::from_be_bytes(*size) as usize;
            if size == 0 || size > rest.len() {
                return Err("truncated sample".into());
            }
            let (unit, rest) = rest.split_at(size);
            units.push(unit);
            bytes = rest;
        }
        if units.is_empty() {
            return Err("empty sample".into());
        }
        Ok(units)
    }
}

fn clip(codec: VideoCodec, width: u32, height: u32) -> Option<Clip> {
    macro_rules! samples {
        ($height:literal) => {
            match codec {
                VideoCodec::H264 => include_bytes!(concat!("benchmark/samples/", $height, ".h264.au")).as_slice(),
                VideoCodec::Hevc => include_bytes!(concat!("benchmark/samples/", $height, ".hevc.au")).as_slice(),
                VideoCodec::Av1 => include_bytes!(concat!("benchmark/samples/", $height, ".av1.au")).as_slice(),
                VideoCodec::PyroWave => return None,
            }
        };
    }
    let pixels = u64::from(width) * u64::from(height);
    let (width, height, data) = if pixels >= 3840 * 2160 {
        (3840, 2160, samples!("2160"))
    } else if pixels >= 1920 * 1080 {
        (1920, 1080, samples!("1080"))
    } else {
        (1280, 720, samples!("720"))
    };
    Some(Clip { width, height, data })
}

#[derive(Clone, Copy, Debug)]
struct Score {
    codec: VideoCodec,
    outputs: usize,
    target: usize,
    p50: Duration,
    p90: Duration,
}

impl Score {
    fn missing(&self) -> usize {
        self.target.saturating_sub(self.outputs)
    }

    /// A hiccup, or a picture superseded by the next one in a burst, is not
    /// falling behind.
    fn kept_up(&self) -> bool {
        self.missing() * 10 <= self.target
    }
}

/// Latency differences this small are noise between runs, not a reason to
/// override the platform's order (Android lists the more efficient HEVC
/// before H.264).
fn tolerance(best: Duration) -> Duration {
    (best / 5).max(Duration::from_millis(1))
}

/// Stable: ties keep the platform's order, and formats without a score
/// (failed, out of time or not tried) follow every measured one.
fn order(formats: &mut [VideoFormat], scores: &[Score]) {
    let best = scores.iter().filter(|s| s.kept_up()).map(|s| s.p50).min();
    formats.sort_by_key(|f| {
        let Some(score) = scores.iter().find(|s| s.codec == f.codec) else {
            return (usize::MAX, Duration::MAX);
        };
        if !score.kept_up() {
            return (score.missing(), score.p50);
        }
        let close = best.is_some_and(|best| score.p50 <= best + tolerance(best));
        (0, if close { Duration::ZERO } else { score.p50 })
    });
}

struct Trial {
    codec: VideoCodec,
    result: Result<Score, String>,
}

pub struct Benchmark {
    trials: mpsc::Receiver<Trial>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    started: Instant,
    mode: (u32, u32),
    codecs: Vec<VideoCodec>,
}

impl Benchmark {
    /// Starts measuring on a thread of its own, or `None` when there is no
    /// choice to make (fewer than two codecs) or no time to make it.
    pub fn start(
        formats: &[VideoFormat],
        mode: (u32, u32, u32),
        surface_output: bool,
        deadline: Instant,
        make_decoder: impl Fn() -> Result<Box<dyn FrameDecoder>, String> + Send + 'static,
    ) -> Option<Self> {
        let (width, height, fps) = mode;
        let started = Instant::now();
        let deadline = deadline.min(started + BUDGET);
        // All current hardware backends offer 8-bit 4:2:0. Preserve format
        // preferences if a future backend offers modes our clips don't cover.
        if deadline <= started + RELEASE_MARGIN || fps == 0 || formats.iter().any(|f| f.ten_bit || f.yuv444) {
            return None;
        }
        let mut candidates = formats.to_vec();
        candidates.retain(|f| f.codec != VideoCodec::PyroWave);
        // Get a result for the broadly supported baseline early on slow devices.
        candidates.sort_by_key(|f| f.codec != VideoCodec::H264);
        candidates.dedup();
        let count = candidates.len();
        if count < 2 {
            return None;
        }
        let codecs = candidates.iter().map(|f| f.codec).collect();
        let sample = clip(candidates[0].codec, width, height)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let (send, trials) = mpsc::channel();
        thread::Builder::new()
            .name("hestia-codec-probe".into())
            .spawn(move || {
                // A previous probe stuck in a vendor call still holds its
                // decoder: skip rather than open a second one beside it.
                let _device = match PROBE_DEVICE.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => return,
                };
                let end = deadline - RELEASE_MARGIN;
                for (index, format) in candidates.into_iter().enumerate() {
                    let now = Instant::now();
                    if now >= end || stop.load(Ordering::Acquire) {
                        break;
                    }
                    let until = now + (end - now) / (count - index) as u32;
                    let result = make_decoder().and_then(|mut decoder| {
                        let sample = clip(format.codec, width, height).ok_or("no sample")?;
                        measure(decoder.as_mut(), format, sample, fps, surface_output, until, &stop)
                    });
                    if Instant::now() > deadline
                        || send
                            .send(Trial {
                                codec: format.codec,
                                result,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            trials,
            cancelled,
            deadline,
            started,
            mode: (sample.width, sample.height),
            codecs,
        })
    }

    /// Puts the best measured format first in `formats` and describes the
    /// measurement for the session report. Only the remaining part of the
    /// original budget may delay RTSP setup.
    pub fn finish(self, formats: &mut [VideoFormat]) -> String {
        let mut scores = Vec::new();
        let mut details = Vec::new();
        let mut reported = Vec::new();
        while let Ok(trial) = self
            .trials
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
        {
            reported.push(trial.codec);
            match trial.result {
                Ok(score) => {
                    details.push(format!(
                        "{:?} {}/{} pictures, p50 {:.1} / p90 {:.1} ms",
                        score.codec,
                        score.outputs,
                        score.target,
                        score.p50.as_secs_f64() * 1000.0,
                        score.p90.as_secs_f64() * 1000.0
                    ));
                    scores.push(score);
                }
                Err(why) => details.push(format!("{:?} failed ({why})", trial.codec)),
            }
        }
        details.extend(
            self.codecs
                .iter()
                .filter(|codec| !reported.contains(codec))
                .map(|codec| format!("{codec:?} not measured in time")),
        );
        order(formats, &scores);
        let first = formats
            .first()
            .map_or_else(|| "nothing".to_owned(), |f| format!("{:?}", f.codec));
        let summary = format!(
            "{}x{} offscreen: {}. Offering {first} first (measured in {} ms)",
            self.mode.0,
            self.mode.1,
            details.join(", "),
            self.deadline
                .min(Instant::now())
                .duration_since(self.started)
                .as_millis()
        );
        info!("auto codec benchmark: {summary}");
        summary
    }
}

impl Drop for Benchmark {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

fn measure(
    decoder: &mut dyn FrameDecoder,
    format: VideoFormat,
    sample: Clip,
    fps: u32,
    surface_output: bool,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Score, String> {
    let active = || Instant::now() < deadline && !cancelled.load(Ordering::Acquire);
    if !active() {
        return Err("out of time".into());
    }
    if !decoder.supports(format) {
        return Err(format!("{} cannot decode it", decoder.name()));
    }
    let packets = sample.access_units()?;
    decoder.configure_offscreen(format, sample.width, sample.height, fps, surface_output)?;

    let period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let start = Instant::now();
    // Fit the trial into its share of the budget, so that slow setup does not
    // count as pictures the decoder failed to deliver.
    let fits = (deadline.saturating_duration_since(start).as_secs_f64() / period.as_secs_f64()) as u32;
    let target = (fps / 2).clamp(3, 60).min(fits.saturating_sub(DRAIN_PERIODS));
    if target < 3 {
        return Err("out of time after setup".into());
    }
    let mut input = 0;
    let mut picture = DecodedFrame::default();
    let mut latencies = Vec::with_capacity(target as usize);
    let drain_until = (start + period * (target + DRAIN_PERIODS)).min(deadline);
    while active() && Instant::now() < drain_until && latencies.len() < target as usize {
        let due = start + period * input;
        let result = if input < target && Instant::now() >= due {
            let index = input as usize % packets.len();
            let frame = EncodedFrame {
                frame_number: input + 1,
                keyframe: index == 0,
                data: packets[index].to_vec(),
                received_at: due,
                idr_requested_at: None,
                loss_detected_at: None,
                host_latency: None,
            };
            input += 1;
            let timing = FrameTiming::new(&frame, due, Instant::now());
            decoder.decode_timed(&frame, &mut picture, timing)?
        } else {
            let drained = decoder.drain_output(&mut picture)?;
            if drained.is_none() {
                // Poll pending output, but feed the next input on time.
                let next = if input < target { due } else { drain_until };
                thread::sleep(
                    next.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                );
            }
            drained
        };
        if let Some(reason) = decoder.terminal_error() {
            return Err(reason.into());
        }
        if let Some(timing) = result {
            latencies.push(timing.received_at.elapsed());
            picture.discard_native();
        }
    }
    if latencies.len() < 3 {
        return Err(format!("{} of {target} pictures before the deadline", latencies.len()));
    }
    latencies.sort_unstable();
    let percentile = |p: usize| latencies[(latencies.len() * p).div_ceil(100) - 1];
    Ok(Score {
        codec: format.codec,
        outputs: latencies.len(),
        target: target as usize,
        p50: percentile(50),
        p90: percentile(90),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn format(codec: VideoCodec) -> VideoFormat {
        VideoFormat {
            codec,
            ten_bit: false,
            yuv444: false,
        }
    }
    const H264: VideoFormat = format(VideoCodec::H264);
    const HEVC: VideoFormat = format(VideoCodec::Hevc);
    const AV1: VideoFormat = format(VideoCodec::Av1);

    fn codecs(formats: &[VideoFormat]) -> Vec<VideoCodec> {
        formats.iter().map(|f| f.codec).collect()
    }

    /// Whether an access unit starts a coded video sequence.
    fn keyframe(codec: VideoCodec, unit: &[u8]) -> bool {
        let nal_types = || unit.windows(4).filter(|w| w[..3] == [0, 0, 1]).map(|w| w[3]);
        match codec {
            VideoCodec::H264 => nal_types().any(|nal| nal & 0x1f == 5),
            VideoCodec::Hevc => nal_types().any(|nal| matches!((nal >> 1) & 0x3f, 19 | 20)),
            // Low-overhead OBUs, each with its size: look for a sequence header.
            VideoCodec::Av1 => {
                let mut rest = unit;
                while let Some((&header, tail)) = rest.split_first() {
                    if (header >> 3) & 0xf == 1 {
                        return true;
                    }
                    let mut tail = &tail[usize::from((header >> 2) & 1)..];
                    let (mut size, mut shift) = (0usize, 0);
                    while let Some((&byte, more)) = tail.split_first() {
                        size |= usize::from(byte & 0x7f) << shift;
                        shift += 7;
                        tail = more;
                        if byte & 0x80 == 0 {
                            break;
                        }
                    }
                    rest = tail.get(size..).unwrap_or_default();
                }
                false
            }
            VideoCodec::PyroWave => false,
        }
    }

    #[test]
    fn samples_are_one_idr_and_eleven_predicted_pictures() {
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            for (width, height) in [(1280, 720), (1920, 1080), (3840, 2160)] {
                let sample = clip(codec, width, height).unwrap();
                assert_eq!((sample.width, sample.height), (width, height));
                let units = sample.access_units().unwrap();
                assert_eq!(units.len(), 12, "{codec:?} {height}p");
                assert!(keyframe(codec, units[0]), "{codec:?} {height}p starts with a keyframe");
                assert!(
                    units[1..].iter().all(|unit| !keyframe(codec, unit)),
                    "{codec:?} {height}p has one keyframe"
                );
            }
        }
        // Modes between the clips use the next smaller one.
        let between = clip(VideoCodec::Hevc, 2560, 1440).unwrap();
        assert_eq!((between.width, between.height), (1920, 1080));
        assert!(clip(VideoCodec::PyroWave, 1920, 1080).is_none());
    }

    #[test]
    fn truncated_samples_are_refused() {
        let units = |data: &'static [u8]| {
            Clip {
                width: 1,
                height: 1,
                data,
            }
            .access_units()
        };
        assert!(units(&[0, 0, 0, 2, 9]).is_err());
        assert!(units(&[0, 0]).is_err());
        assert!(units(&[0, 0, 0, 0]).is_err());
        assert!(units(&[]).is_err());
        assert_eq!(units(&[0, 0, 0, 1, 9]).unwrap(), [&[9][..]]);
    }

    fn score(codec: VideoCodec, outputs: usize, p50_ms: f64) -> Score {
        let p50 = Duration::from_secs_f64(p50_ms / 1000.0);
        Score {
            codec,
            outputs,
            target: 30,
            p50,
            p90: p50 * 2,
        }
    }

    #[test]
    fn noise_keeps_the_platform_order_and_clear_wins_lead() {
        use VideoCodec::{Av1, H264 as Avc, Hevc};
        let mut formats = [HEVC, H264, AV1];
        order(
            &mut formats,
            &[score(Avc, 30, 4.0), score(Hevc, 30, 4.6), score(Av1, 30, 9.0)],
        );
        assert_eq!(codecs(&formats), [Hevc, Avc, Av1]);

        // Unmeasured formats go last.
        order(&mut formats, &[score(Avc, 30, 3.0), score(Hevc, 30, 6.0)]);
        assert_eq!(codecs(&formats), [Avc, Hevc, Av1]);

        // Falling behind costs more than any latency.
        order(
            &mut formats,
            &[score(Avc, 25, 1.0), score(Hevc, 30, 12.0), score(Av1, 30, 30.0)],
        );
        assert_eq!(codecs(&formats), [Hevc, Av1, Avc]);

        // A picture or three short of 30 is still keeping up.
        order(
            &mut formats,
            &[score(Avc, 30, 4.5), score(Hevc, 29, 4.0), score(Av1, 26, 2.0)],
        );
        assert_eq!(codecs(&formats), [Hevc, Avc, Av1]);

        // With no measurement the order is kept.
        order(&mut formats, &[]);
        assert_eq!(codecs(&formats), [Hevc, Avc, Av1]);
    }

    #[test]
    fn no_trial_without_a_choice_or_time() {
        let decoder = || -> Result<Box<dyn FrameDecoder>, String> { panic!("no trial expected") };
        let later = Instant::now() + BUDGET;
        let mode = (1920, 1080, 60);
        assert!(Benchmark::start(&[H264], mode, false, later, decoder).is_none());
        assert!(Benchmark::start(&[H264, H264], mode, false, later, decoder).is_none());
        assert!(Benchmark::start(&[H264, HEVC], (1920, 1080, 0), false, later, decoder).is_none());
        assert!(Benchmark::start(&[H264, HEVC], mode, false, Instant::now(), decoder).is_none());
        let main10 = VideoFormat { ten_bit: true, ..HEVC };
        assert!(Benchmark::start(&[main10, H264, HEVC], mode, false, later, decoder).is_none());
    }

    /// H.264 decodes too slowly for the frame rate, HEVC instantly, and AV1
    /// cannot be configured.
    #[derive(Default)]
    struct Fake(Option<VideoCodec>);

    impl FrameDecoder for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn supports(&self, _: VideoFormat) -> bool {
            true
        }
        fn configure(&mut self, format: VideoFormat, _: u32, _: u32, _: u32) -> Result<(), String> {
            if format.codec == VideoCodec::Av1 {
                return Err("no AV1 here".into());
            }
            self.0 = Some(format.codec);
            Ok(())
        }
        fn decode(&mut self, _: &EncodedFrame, _: &mut DecodedFrame) -> Result<bool, String> {
            if self.0 == Some(VideoCodec::H264) {
                thread::sleep(Duration::from_millis(40));
            }
            Ok(true)
        }
    }

    // The only test that goes through `Benchmark::start`'s probe thread (one
    // probe runs at a time, others give up).
    #[test]
    fn keeping_up_beats_falling_behind_beats_failing() {
        let mut formats = [H264, HEVC, AV1];
        let benchmark = Benchmark::start(&formats, (1280, 720, 60), true, Instant::now() + BUDGET, || {
            Ok(Box::new(Fake::default()))
        })
        .unwrap();
        let report = benchmark.finish(&mut formats);
        assert_eq!(
            codecs(&formats),
            [VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Av1],
            "{report}"
        );
        assert!(report.starts_with("1280x720 offscreen: H264 "), "{report}");
        assert!(report.contains("Hevc 30/30 pictures"), "{report}");
        assert!(report.contains("Av1 failed (no AV1 here)"), "{report}");
        assert!(report.contains("Offering Hevc first"), "{report}");
    }

    #[cfg(feature = "openh264")]
    #[test]
    fn openh264_decodes_the_looped_sample() {
        use crate::Colorimetry;
        use crate::software_h264::OpenH264Decoder;

        let mut decoder = OpenH264Decoder::new(Colorimetry::default()).unwrap();
        // 15 pictures at 30 fps: the clip's IDR comes round again.
        let sample = clip(VideoCodec::H264, 1280, 720).unwrap();
        let later = Instant::now() + Duration::from_secs(10);
        let score = measure(&mut decoder, H264, sample, 30, false, later, &AtomicBool::new(false)).unwrap();
        assert_eq!(score.target, 15);
        assert!(score.outputs >= 3, "{score:?}");
        let sample = clip(VideoCodec::Hevc, 1280, 720).unwrap();
        assert!(measure(&mut decoder, HEVC, sample, 30, false, later, &AtomicBool::new(false)).is_err());
    }

    #[cfg(all(feature = "vaapi", target_os = "linux"))]
    #[test]
    #[ignore = "requires a VA-API render node"]
    fn measures_every_vaapi_codec() {
        use crate::native_picture::{GpuOutput, GpuRendererStatus};

        // Zero-copy, as with the app's GPU video: pictures stay DMA-BUFs.
        let renderer = Arc::new(GpuRendererStatus::default());
        renderer.set_dmabuf(None);
        renderer.ready();
        let probe = move || {
            let output = Arc::new(GpuOutput::new(renderer.clone()));
            crate::vaapi::probe(crate::Colorimetry::default(), Some(output))
                .map(|(decoder, _)| decoder)
                .ok_or_else(|| "no VA-API render node".to_owned())
        };
        let (_, mut formats) = crate::vaapi::probe(crate::Colorimetry::default(), None).expect("no VA-API render node");
        for (width, height) in [(1280, 720), (1920, 1080), (3840, 2160)] {
            let benchmark = Benchmark::start(
                &formats,
                (width, height, 60),
                true,
                Instant::now() + BUDGET,
                probe.clone(),
            )
            .expect("VA-API offers more than one codec");
            let report = benchmark.finish(&mut formats);
            println!("{report}");
            assert!(!report.contains("failed"), "{report}");
            assert!(!report.contains("not measured"), "{report}");
        }
    }
}
