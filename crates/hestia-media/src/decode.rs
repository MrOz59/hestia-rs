//! Decoding off the network thread.
//!
//! [`DecodingSink`] is the [`VideoSink`] handed to the session: `submit` only
//! queues the frame, a dedicated thread runs a [`FrameDecoder`] and gives each
//! picture to a [`FramePresenter`] (the UI, a test, a benchmark). When the
//! queue overflows or the decoder fails, frames are dropped until the next
//! keyframe and an IDR is requested from the host.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::Instant;

use hestia_stream::{EncodedFrame, KeyframeRequester, SubmitResult, VideoFormat, VideoSink};
use log::{info, warn};

/// A decoded picture in RGBA8, row-major, no padding.
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
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
        self.data.resize(width as usize * height as usize * 4, 0);
    }
}

/// Turns access units into RGBA pictures.
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
}

/// Receives decoded pictures on the decoder thread. Implementations that hand
/// the picture to another thread should copy what they need and return fast.
pub trait FramePresenter: Send {
    fn present(&mut self, frame: &RgbaFrame);
}

impl<F: FnMut(&RgbaFrame) + Send> FramePresenter for F {
    fn present(&mut self, frame: &RgbaFrame) {
        self(frame)
    }
}

#[derive(Debug, Default)]
pub struct DecodeStats {
    pub decoded: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
    /// Sum of decode + conversion time, microseconds.
    pub decode_time_us: AtomicU64,
}

impl DecodeStats {
    /// Mean decode time in milliseconds over everything decoded so far.
    pub fn mean_decode_ms(&self) -> f64 {
        let n = self.decoded.load(Ordering::Relaxed);
        if n == 0 {
            return 0.0;
        }
        self.decode_time_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0
    }
}

/// Frames waiting for the decoder; beyond this the decoder is too slow.
/// Deep enough to absorb a keyframe that takes several frame times to
/// decode (each overflow costs a new keyframe, which is slower still).
const QUEUE_DEPTH: usize = 6;

struct Shared {
    waiting_for_keyframe: AtomicBool,
    stats: Arc<DecodeStats>,
}

pub struct DecodingSink {
    decoder: Option<Box<dyn FrameDecoder>>,
    presenter: Option<Box<dyn FramePresenter>>,
    requester: Option<KeyframeRequester>,
    queue: Option<SyncSender<EncodedFrame>>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl DecodingSink {
    pub fn new(decoder: Box<dyn FrameDecoder>, presenter: Box<dyn FramePresenter>) -> Self {
        Self {
            decoder: Some(decoder),
            presenter: Some(presenter),
            requester: None,
            queue: None,
            thread: None,
            shared: Arc::new(Shared {
                waiting_for_keyframe: AtomicBool::new(true),
                stats: Arc::new(DecodeStats::default()),
            }),
        }
    }

    pub fn stats(&self) -> Arc<DecodeStats> {
        self.shared.stats.clone()
    }

    fn need_keyframe(&self) {
        self.shared.waiting_for_keyframe.store(true, Ordering::Relaxed);
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
        let presenter = self.presenter.take().ok_or("decoder already set up")?;
        let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
        let shared = self.shared.clone();
        let requester = self.requester.clone();
        self.thread = Some(
            std::thread::Builder::new()
                .name("hestia-decode".into())
                .spawn(move || decode_loop(rx, decoder, presenter, shared, requester))
                .map_err(|e| e.to_string())?,
        );
        self.queue = Some(tx);
        Ok(())
    }

    fn attach_keyframe_requester(&mut self, requester: KeyframeRequester) {
        self.requester = Some(requester);
    }

    fn submit(&mut self, frame: EncodedFrame) -> SubmitResult {
        let Some(queue) = &self.queue else {
            return SubmitResult::Ok;
        };
        if frame.keyframe {
            self.shared.waiting_for_keyframe.store(false, Ordering::Relaxed);
        } else if self.shared.waiting_for_keyframe.load(Ordering::Relaxed) {
            self.shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return SubmitResult::Ok;
        }
        match queue.try_send(frame) {
            Ok(()) => SubmitResult::Ok,
            Err(TrySendError::Full(_)) => {
                warn!("decoder is falling behind; dropping until the next keyframe");
                self.shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
                self.need_keyframe();
                SubmitResult::NeedKeyframe
            }
            Err(TrySendError::Disconnected(_)) => SubmitResult::Ok,
        }
    }

    fn cleanup(&mut self) {
        self.queue = None;
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
    rx: Receiver<EncodedFrame>,
    mut decoder: Box<dyn FrameDecoder>,
    mut presenter: Box<dyn FramePresenter>,
    shared: Arc<Shared>,
    requester: Option<KeyframeRequester>,
) {
    let mut picture = RgbaFrame::default();
    let mut logged_size = (0, 0);
    for frame in rx {
        // Frames queued before an error depend on the broken reference.
        if !frame.keyframe && shared.waiting_for_keyframe.load(Ordering::Relaxed) {
            shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let started = Instant::now();
        match decoder.decode(&frame, &mut picture) {
            Ok(true) => {
                picture.frame_number = frame.frame_number;
                picture.received_at = frame.received_at;
                picture.decoded_at = Instant::now();
                shared.stats.decoded.fetch_add(1, Ordering::Relaxed);
                shared
                    .stats
                    .decode_time_us
                    .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                if (picture.width, picture.height) != logged_size {
                    logged_size = (picture.width, picture.height);
                    info!("decoding {}x{}", picture.width, picture.height);
                }
                presenter.present(&picture);
            }
            Ok(false) => {}
            Err(e) => {
                warn!("decode error on frame {}: {e}", frame.frame_number);
                shared.stats.errors.fetch_add(1, Ordering::Relaxed);
                shared.waiting_for_keyframe.store(true, Ordering::Relaxed);
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
    use std::sync::Mutex;
    use std::time::Duration;

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
        let requests = Arc::new(AtomicU64::new(0));
        let s = shown.clone();
        let mut sink = DecodingSink::new(
            Box::new(Fake { fail: vec![3] }),
            Box::new(move |f: &RgbaFrame| s.lock().unwrap().push(f.data[0] as u32)),
        );
        let r = requests.clone();
        sink.attach_keyframe_requester(KeyframeRequester::new(move || {
            r.fetch_add(1, Ordering::Relaxed);
        }));
        sink.setup(H264, 1280, 720, 60).unwrap();

        // P-frame before the first keyframe is dropped.
        assert_eq!(sink.submit(frame(1, false)), SubmitResult::Ok);
        for n in 2..=6 {
            sink.submit(frame(n, n == 2));
            std::thread::sleep(Duration::from_millis(20));
        }
        sink.submit(frame(7, true));
        sink.submit(frame(8, false));
        sink.cleanup();

        // 3 fails → 4..6 are dropped until keyframe 7.
        assert_eq!(*shown.lock().unwrap(), vec![2, 7, 8]);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        let stats = sink.stats();
        assert_eq!(stats.errors.load(Ordering::Relaxed), 1);
        assert_eq!(stats.decoded.load(Ordering::Relaxed), 3);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 4);
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
