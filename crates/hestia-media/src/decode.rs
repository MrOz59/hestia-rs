//! Decoding off the network thread.
//!
//! [`DecodingSink`] is the [`VideoSink`] handed to the session: `submit` only
//! queues the frame, a dedicated thread runs a [`FrameDecoder`] and gives each
//! picture to a [`FramePresenter`] (the UI, a test, a benchmark). When the
//! queue overflows or the decoder fails, frames are dropped until the next
//! keyframe and an IDR is requested from the host.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hestia_stream::{EncodedFrame, KeyframeRequester, SubmitResult, VideoFormat, VideoSink};
use log::{info, warn};

/// A decoded picture in RGBA8, row-major, no padding.
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// The picture went to the GPU renderer (Android zero-copy, see
    /// `gpu_video`); `data` doesn't hold it.
    pub on_gpu: bool,
    pub frame_number: u32,
    /// When the first packet of the frame arrived.
    pub received_at: Instant,
    pub decoded_at: Instant,
}

impl Default for RgbaFrame {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            width: 0,
            height: 0,
            data: Vec::new(),
            on_gpu: false,
            frame_number: 0,
            received_at: now,
            decoded_at: now,
        }
    }
}

impl RgbaFrame {
    /// Resizes the buffer for a `width` × `height` picture.
    pub fn reshape(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.on_gpu = false;
        self.data.resize(width as usize * height as usize * 4, 0);
    }
}

/// Timing and identity carried from input to its corresponding decoder output.
#[derive(Debug, Clone, Copy)]
pub struct FrameTiming {
    pub frame_number: u32,
    pub received_at: Instant,
    pub queued_at: Instant,
    pub decode_started_at: Instant,
    pub codec_output_at: Instant,
}

impl FrameTiming {
    pub(crate) fn new(frame: &EncodedFrame, queued_at: Instant, started_at: Instant) -> Self {
        Self {
            frame_number: frame.frame_number,
            received_at: frame.received_at,
            queued_at,
            decode_started_at: started_at,
            codec_output_at: started_at,
        }
    }
}

/// Consumes compressed access units. Decoders with asynchronous output can
/// return the timing of a previous input when that picture becomes available.
pub trait FrameDecoder: Send {
    fn name(&self) -> &'static str;
    fn supports(&self, format: VideoFormat) -> bool;
    /// Called once the stream's format and size are known, before the
    /// first frame (hardware decoders are created here).
    fn configure(&mut self, _format: VideoFormat, _width: u32, _height: u32, _fps: u32) -> Result<(), String> {
        Ok(())
    }
    /// Decodes one access unit; `Ok(true)` when `out` now holds a picture.
    fn decode(&mut self, frame: &EncodedFrame, out: &mut RgbaFrame) -> Result<bool, String>;

    /// Discards pending output and references before decoding a recovery IDR.
    fn reset(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// How many frames are queued behind the next one: a pipelined decoder
    /// can take the next input instead of waiting for this one's picture.
    fn set_backlog(&mut self, _frames: usize) {}

    /// A failure that requesting a new keyframe cannot recover from.
    fn terminal_error(&self) -> Option<&str> {
        None
    }

    fn decode_timed(
        &mut self,
        frame: &EncodedFrame,
        out: &mut RgbaFrame,
        mut timing: FrameTiming,
    ) -> Result<Option<FrameTiming>, String> {
        if !self.decode(frame, out)? {
            return Ok(None);
        }
        timing.codec_output_at = Instant::now();
        Ok(Some(timing))
    }
}

/// Receives pictures on the decoder thread. A presenter handing a picture to
/// another thread can take the buffer and return an older one for reuse.
pub trait FramePresenter: Send {
    fn present(&mut self, frame: &mut RgbaFrame);
}

impl<F: FnMut(&RgbaFrame) + Send> FramePresenter for F {
    fn present(&mut self, frame: &mut RgbaFrame) {
        self(frame)
    }
}

/// Buckets use 0.25 ms resolution up to 64 ms and 4 ms up to 1,024 ms.
const FINE_BUCKETS: usize = 256;
const COARSE_BUCKETS: usize = 240;
const LATENCY_OVERFLOW: usize = FINE_BUCKETS + COARSE_BUCKETS + 1;

#[derive(Debug)]
pub struct LatencyHistogram {
    buckets: [AtomicU64; LATENCY_OVERFLOW + 1],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl LatencyHistogram {
    pub fn record(&self, elapsed: Duration) {
        let us = elapsed.as_micros();
        let bucket = if us <= 64_000 {
            us.div_ceil(250) as usize
        } else {
            FINE_BUCKETS + ((us - 64_000).div_ceil(4_000)).min(COARSE_BUCKETS as u128 + 1) as usize
        };
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    pub fn summary(&self) -> Option<String> {
        let counts = self.buckets.each_ref().map(|n| n.load(Ordering::Relaxed));
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let percentile = |p: u64| {
            let target = (total * p).div_ceil(100);
            let mut cumulative = 0;
            let bucket = counts
                .iter()
                .position(|n| {
                    cumulative += *n;
                    cumulative >= target
                })
                .unwrap_or(LATENCY_OVERFLOW);
            if bucket <= FINE_BUCKETS {
                format!("{:.2}", bucket as f64 * 0.25)
            } else if bucket < LATENCY_OVERFLOW {
                format!("{}.00", 64 + (bucket - FINE_BUCKETS) * 4)
            } else {
                ">1024".to_owned()
            }
        };
        Some(format!(
            "p50 {} / p95 {} / p99 {} ms ({total} samples)",
            percentile(50),
            percentile(95),
            percentile(99)
        ))
    }
}

#[derive(Debug, Default)]
pub struct DecodeStats {
    /// The decoder in use, as it calls itself now (fallbacks change it).
    pub decoder: Mutex<&'static str>,
    pub decoded: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
    /// Sum of decode + conversion time, microseconds.
    pub decode_time_us: AtomicU64,
    pub dropped_backlog: AtomicU64,
    pub receive_to_queue: LatencyHistogram,
    pub queue_wait: LatencyHistogram,
    pub codec: LatencyHistogram,
    pub frame_conversion: LatencyHistogram,
}

impl DecodeStats {
    pub fn latency_stages(&self) -> [(&'static str, &LatencyHistogram); 4] {
        [
            ("first packet to decode queue", &self.receive_to_queue),
            ("decoder queue", &self.queue_wait),
            ("decoder output", &self.codec),
            ("codec output to frame ready", &self.frame_conversion),
        ]
    }

    /// Mean decode time in milliseconds over everything decoded so far.
    pub fn mean_decode_ms(&self) -> f64 {
        let n = self.decoded.load(Ordering::Relaxed);
        if n == 0 {
            return 0.0;
        }
        self.decode_time_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0
    }
}

/// Compressed frames are never dropped for being late: each one is a
/// reference for the next, so dropping one costs a keyframe (large, slow on
/// WAN links, and a stall until it arrives). The decoder catches up and only
/// the newest picture is shown. Only a backlog this deep or this old (the
/// decoder can't keep up at all) is flushed for a keyframe.
const QUEUE_DEPTH: usize = 48;
const MIN_BACKLOG_AGE: Duration = Duration::from_millis(300);
/// Frames of backlog allowed at low frame rates (more than the age above).
const MIN_BACKLOG_FRAMES: u64 = 8;

struct QueuedFrame {
    frame: EncodedFrame,
    queued_at: Instant,
    generation: u64,
}

struct Queue {
    frames: VecDeque<QueuedFrame>,
    waiting_for_keyframe: bool,
    generation: u64,
    max_age: Duration,
    closed: bool,
}

impl Queue {
    fn invalidate(&mut self) -> u64 {
        let dropped = self.frames.len() as u64;
        self.frames.clear();
        self.waiting_for_keyframe = true;
        self.generation += 1;
        dropped
    }
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    stats: Arc<DecodeStats>,
}

impl Shared {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dropped_backlog(&self, count: u64) {
        self.stats.dropped.fetch_add(count, Ordering::Relaxed);
        self.stats.dropped_backlog.fetch_add(count, Ordering::Relaxed);
    }
}

pub struct DecodingSink {
    decoder: Option<Box<dyn FrameDecoder>>,
    presenter: Option<Box<dyn FramePresenter>>,
    requester: Option<KeyframeRequester>,
    failure_handler: Option<Box<dyn Fn(String) + Send>>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl DecodingSink {
    pub fn new(decoder: Box<dyn FrameDecoder>, presenter: Box<dyn FramePresenter>) -> Self {
        Self {
            decoder: Some(decoder),
            presenter: Some(presenter),
            requester: None,
            failure_handler: None,
            thread: None,
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    frames: VecDeque::with_capacity(QUEUE_DEPTH),
                    waiting_for_keyframe: true,
                    generation: 0,
                    max_age: Duration::ZERO,
                    closed: false,
                }),
                ready: Condvar::new(),
                stats: Arc::new(DecodeStats::default()),
            }),
        }
    }

    pub fn stats(&self) -> Arc<DecodeStats> {
        self.shared.stats.clone()
    }
}

impl VideoSink for DecodingSink {
    fn setup(&mut self, format: VideoFormat, width: u32, height: u32, fps: u32) -> Result<(), String> {
        let mut decoder = self.decoder.take().ok_or("decoder already set up")?;
        if !decoder.supports(format) {
            return Err(format!("{} cannot decode {format:?}", decoder.name()));
        }
        decoder.configure(format, width, height, fps)?;
        info!(
            "video decoder: {} for {format:?} {width}x{height}@{fps}",
            decoder.name()
        );
        *self.shared.stats.decoder.lock().unwrap_or_else(|p| p.into_inner()) = decoder.name();
        let presenter = self.presenter.take().ok_or("decoder already set up")?;
        // Bursts (Wi-Fi, VPNs, keyframes) are decoded through; only a
        // decoder that can't keep up at all is reset (see QUEUE_DEPTH).
        self.shared.queue().max_age =
            Duration::from_micros(MIN_BACKLOG_FRAMES * 1_000_000 / u64::from(fps.max(1))).max(MIN_BACKLOG_AGE);
        let shared = self.shared.clone();
        let requester = self.requester.clone();
        let failure_handler = self.failure_handler.take();
        self.thread = Some(
            std::thread::Builder::new()
                .name("hestia-decode".into())
                .spawn(move || decode_loop(decoder, presenter, shared, requester, failure_handler))
                .map_err(|e| e.to_string())?,
        );
        Ok(())
    }

    fn attach_keyframe_requester(&mut self, requester: KeyframeRequester) {
        self.requester = Some(requester);
    }

    fn attach_failure_handler(&mut self, handler: Box<dyn Fn(String) + Send>) {
        self.failure_handler = Some(handler);
    }

    fn submit(&mut self, frame: EncodedFrame) -> SubmitResult {
        if self.thread.is_none() {
            return SubmitResult::Ok;
        }
        let now = Instant::now();
        let mut queue = self.shared.queue();
        if queue.closed {
            return SubmitResult::Ok;
        }
        let overloaded = queue.frames.len() == QUEUE_DEPTH
            || queue
                .frames
                .front()
                .is_some_and(|f| now.duration_since(f.queued_at) > queue.max_age);
        if overloaded {
            let dropped = queue.invalidate();
            self.shared.dropped_backlog(dropped);
        }
        if queue.waiting_for_keyframe && !frame.keyframe {
            if overloaded {
                self.shared.dropped_backlog(1);
                return SubmitResult::NeedKeyframe;
            }
            self.shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return SubmitResult::Ok;
        }
        queue.waiting_for_keyframe = false;
        let generation = queue.generation;
        self.shared
            .stats
            .receive_to_queue
            .record(now.saturating_duration_since(frame.received_at));
        queue.frames.push_back(QueuedFrame {
            frame,
            queued_at: now,
            generation,
        });
        drop(queue);
        self.shared.ready.notify_one();
        SubmitResult::Ok
    }

    fn cleanup(&mut self) {
        self.shared.queue().closed = true;
        self.shared.ready.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for DecodingSink {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn decode_loop(
    mut decoder: Box<dyn FrameDecoder>,
    mut presenter: Box<dyn FramePresenter>,
    shared: Arc<Shared>,
    requester: Option<KeyframeRequester>,
    failure_handler: Option<Box<dyn Fn(String) + Send>>,
) {
    let mut picture = RgbaFrame::default();
    let mut logged_size = (0, 0);
    let mut decoder_generation = 0;
    loop {
        let mut queue = shared
            .ready
            .wait_while(shared.queue(), |q| q.frames.is_empty() && !q.closed)
            .unwrap_or_else(|e| e.into_inner());
        let Some(QueuedFrame {
            frame,
            queued_at,
            generation,
        }) = queue.frames.pop_front()
        else {
            break;
        };
        let started = Instant::now();
        let queue_age = started.duration_since(queued_at);
        shared.stats.queue_wait.record(queue_age);
        if queue_age > queue.max_age {
            let dropped = queue.invalidate();
            drop(queue);
            warn!(
                "decoder {} ms behind: dropping {} frames for a keyframe",
                queue_age.as_millis(),
                dropped + 1
            );
            shared.dropped_backlog(dropped + 1);
            if let Some(r) = &requester {
                r.request();
            }
            continue;
        }
        let backlog = queue.frames.len();
        drop(queue);
        decoder.set_backlog(backlog);
        let timing = FrameTiming::new(&frame, queued_at, started);
        let result = if decoder_generation != generation {
            let reset = decoder.reset();
            if reset.is_ok() {
                decoder_generation = generation;
            }
            reset.and_then(|()| decoder.decode_timed(&frame, &mut picture, timing))
        } else {
            decoder.decode_timed(&frame, &mut picture, timing)
        };

        let name = decoder.name();
        let mut current = shared.stats.decoder.lock().unwrap_or_else(|p| p.into_inner());
        if *current != name {
            info!("video decoder now: {name}");
            *current = name;
        }
        drop(current);
        let mut queue = shared.queue();
        if let Some(reason) = decoder.terminal_error() {
            let dropped = queue.invalidate();
            queue.closed = true;
            drop(queue);
            shared.stats.errors.fetch_add(1, Ordering::Relaxed);
            shared.stats.dropped.fetch_add(dropped, Ordering::Relaxed);
            if let Some(report) = &failure_handler {
                report(reason.to_owned());
            } else {
                warn!("video decoder stopped: {reason}");
            }
            return;
        }
        if queue.generation != generation {
            // A newer keyframe may already be queued. Its generation owns recovery.
            shared.dropped_backlog(1);
            continue;
        }
        match result {
            Ok(Some(timing)) => {
                drop(queue);
                picture.frame_number = timing.frame_number;
                picture.received_at = timing.received_at;
                picture.decoded_at = Instant::now();
                let stats = &shared.stats;
                let decode_time = picture.decoded_at.duration_since(timing.decode_started_at);
                stats
                    .decode_time_us
                    .fetch_add(decode_time.as_micros() as u64, Ordering::Relaxed);
                stats
                    .codec
                    .record(timing.codec_output_at.duration_since(timing.decode_started_at));
                stats
                    .frame_conversion
                    .record(picture.decoded_at.duration_since(timing.codec_output_at));
                stats.decoded.fetch_add(1, Ordering::Relaxed);
                if (picture.width, picture.height) != logged_size {
                    logged_size = (picture.width, picture.height);
                    info!("decoding {}x{}", picture.width, picture.height);
                }
                presenter.present(&mut picture);
            }
            Ok(None) => {}
            Err(e) => {
                let dropped = queue.invalidate();
                drop(queue);
                // The first few in full, then a reminder now and then: a
                // decoder failing every frame would bury the log.
                let errors = shared.stats.errors.fetch_add(1, Ordering::Relaxed) + 1;
                if errors <= 5 || errors % 100 == 0 {
                    warn!("decode error on frame {} ({errors} so far): {e}", frame.frame_number);
                }
                shared.stats.dropped.fetch_add(dropped, Ordering::Relaxed);
                if let Some(r) = &requester {
                    r.request();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hestia_stream::VideoCodec;
    use std::sync::mpsc;

    /// "Decodes" a frame into a 2x1 picture whose red channel is the frame
    /// number; fails on frames listed in `fail`.
    struct Fake {
        fail: Vec<u32>,
    }

    impl FrameDecoder for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn supports(&self, f: VideoFormat) -> bool {
            f.codec == VideoCodec::H264
        }
        fn decode(&mut self, frame: &EncodedFrame, out: &mut RgbaFrame) -> Result<bool, String> {
            if self.fail.contains(&frame.frame_number) {
                return Err("corrupt".into());
            }
            out.reshape(2, 1);
            out.data[0] = frame.frame_number as u8;
            Ok(true)
        }
    }

    fn frame(n: u32, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            frame_number: n,
            keyframe,
            data: vec![0, 0, 0, 1],
            received_at: Instant::now(),
            host_latency: None,
        }
    }

    const H264: VideoFormat = VideoFormat {
        codec: VideoCodec::H264,
        ten_bit: false,
        yuv444: false,
    };

    #[test]
    fn decodes_and_recovers_from_errors() {
        let shown = Arc::new(Mutex::new(Vec::new()));
        let (requested, request) = mpsc::channel();
        let s = shown.clone();
        let mut sink = DecodingSink::new(
            Box::new(Fake { fail: vec![3] }),
            Box::new(move |f: &RgbaFrame| s.lock().unwrap().push(f.data[0] as u32)),
        );
        sink.attach_keyframe_requester(KeyframeRequester::new(move || {
            requested.send(()).unwrap();
        }));
        sink.setup(H264, 1280, 720, 60).unwrap();

        // P-frame before the first keyframe is dropped.
        assert_eq!(sink.submit(frame(1, false)), SubmitResult::Ok);
        sink.submit(frame(2, true));
        sink.submit(frame(3, false));
        request.recv_timeout(Duration::from_secs(1)).unwrap();
        for n in 4..=6 {
            sink.submit(frame(n, false));
        }
        sink.submit(frame(7, true));
        sink.submit(frame(8, false));
        sink.cleanup();

        // 3 fails → 4..6 are dropped until keyframe 7.
        assert_eq!(*shown.lock().unwrap(), vec![2, 7, 8]);
        assert!(request.try_recv().is_err());
        let stats = sink.stats();
        assert_eq!(stats.errors.load(Ordering::Relaxed), 1);
        assert_eq!(stats.decoded.load(Ordering::Relaxed), 3);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 4);
    }

    /// A decoder slower than real time for a while (a burst arrives at
    /// once): every frame is still decoded, nothing is dropped, no keyframe
    /// is asked for.
    #[test]
    fn bursts_are_decoded_through() {
        struct Slow;
        impl FrameDecoder for Slow {
            fn name(&self) -> &'static str {
                "slow"
            }
            fn supports(&self, _f: VideoFormat) -> bool {
                true
            }
            fn decode(&mut self, frame: &EncodedFrame, out: &mut RgbaFrame) -> Result<bool, String> {
                std::thread::sleep(Duration::from_millis(5));
                out.reshape(2, 1);
                out.data[0] = frame.frame_number as u8;
                Ok(true)
            }
        }
        let shown = Arc::new(Mutex::new(Vec::new()));
        let (requested, request) = mpsc::channel();
        let s = shown.clone();
        let mut sink = DecodingSink::new(
            Box::new(Slow),
            Box::new(move |f: &RgbaFrame| s.lock().unwrap().push(f.data[0] as u32)),
        );
        sink.attach_keyframe_requester(KeyframeRequester::new(move || {
            requested.send(()).unwrap();
        }));
        sink.setup(H264, 1280, 720, 60).unwrap();
        // 20 frames at once: 100 ms of decoding, six frame times behind.
        sink.submit(frame(1, true));
        for n in 2..=20 {
            assert_eq!(sink.submit(frame(n, false)), SubmitResult::Ok);
        }
        sink.cleanup();
        assert_eq!(*shown.lock().unwrap(), (1..=20).collect::<Vec<_>>());
        assert!(request.try_recv().is_err(), "no keyframe needed");
        assert_eq!(sink.stats().dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reports_backlog_over_64_ms() {
        let histogram = LatencyHistogram::default();
        histogram.record(Duration::from_millis(300));
        assert_eq!(
            histogram.summary().as_deref(),
            Some("p50 300.00 / p95 300.00 / p99 300.00 ms (1 samples)")
        );
    }

    #[test]
    fn terminal_failure_reports_once_without_requesting_an_idr() {
        struct Broken(bool);
        impl FrameDecoder for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn supports(&self, _format: VideoFormat) -> bool {
                true
            }
            fn decode(&mut self, _frame: &EncodedFrame, _out: &mut RgbaFrame) -> Result<bool, String> {
                self.0 = true;
                Err("no usable video output".into())
            }
            fn terminal_error(&self) -> Option<&str> {
                self.0.then_some("no usable video output")
            }
        }

        let (send_error, errors) = mpsc::channel();
        let (send_idr, idrs) = mpsc::channel();
        let mut sink = DecodingSink::new(Box::new(Broken(false)), Box::new(|_: &RgbaFrame| {}));
        sink.attach_failure_handler(Box::new(move |reason| send_error.send(reason).unwrap()));
        sink.attach_keyframe_requester(KeyframeRequester::new(move || send_idr.send(()).unwrap()));
        sink.setup(H264, 1280, 720, 60).unwrap();
        sink.submit(frame(1, true));
        assert_eq!(errors.recv_timeout(Duration::from_secs(1)).unwrap(), "no usable video output");
        sink.submit(frame(2, true));
        sink.cleanup();
        assert!(errors.try_recv().is_err());
        assert!(idrs.try_recv().is_err());
    }

    #[test]
    fn rejects_unsupported_codec() {
        let mut sink = DecodingSink::new(Box::new(Fake { fail: vec![] }), Box::new(|_: &RgbaFrame| {}));
        let hevc = VideoFormat {
            codec: VideoCodec::Hevc,
            ..H264
        };
        assert!(sink.setup(hevc, 1920, 1080, 60).is_err());
    }
}
