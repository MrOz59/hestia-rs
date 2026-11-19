//! `hestia stream`: launch an app and run the full streaming pipeline
//! headless, printing per-second statistics. Video is decoded (default),
//! dumped to a file or just counted; audio is counted.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use hestia_core::stream::{SessionListener, StreamEvent, StreamSession, VideoCodec, VideoFormat, VideoSink};
use hestia_core::{DeviceInfo, HestiaClient};
use hestia_media::null::{NullAudioSink, NullVideoSink};
use hestia_media::{Colorimetry, DecodeStats, DecodingSink, RgbaFrame, dump::DumpVideoSink};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMode {
    Decode,
    Dump,
    Count,
}

pub struct Options {
    pub app: String,
    pub settings: hestia_core::StreamSettings,
    pub seconds: u64,
    pub mode: VideoMode,
    pub dump: Option<PathBuf>,
    pub snapshot: Option<PathBuf>,
    pub quit_after: bool,
}

enum Msg {
    Event(StreamEvent),
    Stop,
}

struct Forward(Sender<Msg>);

impl SessionListener for Forward {
    fn on_event(&self, event: StreamEvent) {
        let _ = self.0.send(Msg::Event(event));
    }
}

fn all_formats() -> Vec<VideoFormat> {
    [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264]
        .into_iter()
        .map(|codec| VideoFormat {
            codec,
            ten_bit: false,
            yuv444: false,
        })
        .collect()
}

/// Saves one decoded picture as a binary PPM (any image viewer opens it).
fn write_ppm(path: &Path, frame: &RgbaFrame) -> std::io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "P6\n{} {}\n255\n", frame.width, frame.height)?;
    for px in frame.data.chunks_exact(4) {
        out.write_all(&px[..3])?;
    }
    out.flush()
}

pub fn run(client: &HestiaClient, host_id: &str, opts: Options) -> Result<()> {
    let apps = client.apps(host_id)?;
    let app = apps
        .iter()
        .find(|a| a.id.to_string() == opts.app || a.title.eq_ignore_ascii_case(&opts.app))
        .ok_or_else(|| format!("no app matching '{}'", opts.app))?;

    // Pick the video sink before launching so a missing decoder fails early.
    let mut decode_stats: Option<Arc<DecodeStats>> = None;
    let (formats, video_sink): (Vec<VideoFormat>, Box<dyn VideoSink>) = match opts.mode {
        VideoMode::Decode => {
            let (decoder, formats) = hestia_media::default_decoder(Colorimetry::default())?;
            let snapshot = opts.snapshot.clone();
            let started = Instant::now();
            let mut saved = false;
            let presenter = move |frame: &RgbaFrame| {
                if let Some(path) = snapshot.as_deref().filter(|_| !saved) {
                    if started.elapsed() > Duration::from_secs(2) {
                        saved = true;
                        match write_ppm(path, frame) {
                            Ok(()) => eprintln!("saved a {}x{} frame to {}", frame.width, frame.height, path.display()),
                            Err(e) => eprintln!("snapshot failed: {e}"),
                        }
                    }
                }
            };
            let sink = DecodingSink::new(decoder, Box::new(presenter));
            decode_stats = Some(sink.stats());
            (formats, Box::new(sink))
        }
        VideoMode::Dump => {
            let path = opts.dump.clone().unwrap_or_else(|| PathBuf::from("hestia-stream"));
            (all_formats(), Box::new(DumpVideoSink::new(path)))
        }
        VideoMode::Count => (all_formats(), Box::new(NullVideoSink::default())),
    };

    let launched = client.launch_with(host_id, app.id, &DeviceInfo::default(), opts.settings.clone())?;
    let config = launched.stream_config(&formats)?;
    eprintln!(
        "{} '{}' on {} — {}x{}@{} {} kbps via {}",
        if launched.response.resumed {
            "resumed"
        } else {
            "launched"
        },
        app.title,
        launched.host.name,
        config.width,
        config.height,
        config.fps,
        config.bitrate_kbps,
        config.session_url.raw
    );

    let (tx, rx) = mpsc::channel();
    let session = StreamSession::start(
        config,
        video_sink,
        Box::new(NullAudioSink::default()),
        Arc::new(Forward(tx.clone())),
    )?;
    let n = session.negotiated();
    eprintln!(
        "streaming {:?} video, {} audio channels, encryption {:#x}{}",
        n.video_format.codec,
        n.audio_format.channels,
        n.encryption_enabled,
        if opts.seconds > 0 {
            format!("; stopping after {} s", opts.seconds)
        } else {
            "; press Enter to stop".into()
        }
    );

    // Enter stops the stream; EOF (no terminal) is ignored.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            if matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
                let _ = tx.send(Msg::Stop);
            }
        });
    }

    let deadline = (opts.seconds > 0).then(|| Instant::now() + Duration::from_secs(opts.seconds));
    let mut last: Option<(Instant, hestia_core::stream::StreamStats, u64)> = None;
    let outcome = loop {
        let wait = deadline.map_or(Duration::from_millis(500), |d| {
            d.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500))
        });
        match rx.recv_timeout(wait) {
            Ok(Msg::Stop) => break None,
            Ok(Msg::Event(StreamEvent::Stats(stats))) => {
                let decoded = decode_stats.as_ref().map_or(0, |d| d.decoded.load(Ordering::Relaxed));
                if let Some((at, prev, prev_decoded)) = &last {
                    let secs = at.elapsed().as_secs_f64().max(0.001);
                    let mut line = format!(
                        "{:5.1} fps  {:6.2} Mbps  rtt {:3} ms (±{})  fec-recovered {}  lost frames {}",
                        (stats.frames - prev.frames) as f64 / secs,
                        (stats.bytes - prev.bytes) as f64 * 8.0 / secs / 1e6,
                        stats.rtt_ms,
                        stats.rtt_variance_ms,
                        stats.recovered_packets,
                        stats.lost_frames
                    );
                    if let Some(d) = &decode_stats {
                        line += &format!(
                            "  decoded {:5.1} fps ({:.1} ms, {} dropped, {} errors)",
                            (decoded - prev_decoded) as f64 / secs,
                            d.mean_decode_ms(),
                            d.dropped.load(Ordering::Relaxed),
                            d.errors.load(Ordering::Relaxed)
                        );
                    }
                    eprintln!("{line}");
                }
                last = Some((Instant::now(), stats, decoded));
            }
            Ok(Msg::Event(StreamEvent::Terminated { reason, graceful })) => break Some((reason, graceful)),
            Ok(Msg::Event(StreamEvent::StageFailed(stage, e))) => eprintln!("{stage:?} failed: {e}"),
            Ok(Msg::Event(StreamEvent::FirstFrame)) => eprintln!("first video frame received"),
            Ok(Msg::Event(StreamEvent::HdrMode(on))) => eprintln!("host HDR mode: {on}"),
            Ok(Msg::Event(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break None,
        }
    };
    session.stop();

    if opts.quit_after {
        client.quit(host_id)?;
        eprintln!("quit the app on the host");
    }
    match outcome {
        Some((reason, false)) => Err(reason.into()),
        Some((reason, true)) => {
            eprintln!("{reason}");
            Ok(())
        }
        None => Ok(()),
    }
}
