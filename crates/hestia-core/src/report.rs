//! Stream session reports: what was asked, what was negotiated, how the
//! stream behaved second by second, how it ended, and every log line from
//! launch to end, in one text file that is easy to share while the client is
//! a prototype.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Reports kept on disk; older ones are deleted.
pub const KEEP_REPORTS: usize = 20;
/// Per-second rows written in full; longer sessions keep the first and last
/// rows.
const MAX_STAT_ROWS: usize = 600;

/// One second of stream statistics.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StatsSample {
    pub fps: f64,
    pub mbps: f64,
    pub rtt_ms: u32,
    pub rtt_variance_ms: u32,
    /// Running totals.
    pub fec_recovered: u64,
    pub lost_frames: u64,
    /// Decoder (when the client decodes).
    pub decoded_fps: Option<f64>,
    pub decode_ms: Option<f64>,
    pub dropped: Option<u64>,
    /// Host-reported processing latency for this second.
    pub host_ms: Option<f64>,
    /// Bottleneck verdict for this second ("network", "decode"…), None
    /// when healthy or not judged.
    pub verdict: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct StreamReport {
    title: String,
    started: SystemTime,
    started_at: Instant,
    log_mark: u64,
    sections: Vec<(String, Vec<(String, String)>)>,
    events: Vec<(Duration, String)>,
    samples: Vec<(Duration, StatsSample)>,
    first_frame: Option<Duration>,
    outcome: Option<(Duration, String)>,
}

/// Days since 1970-01-01 → (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn split(ts: SystemTime) -> (i64, u32, u32, u64, u32) {
    let d = ts.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    (y, m, day, secs % 86_400, d.subsec_millis())
}

/// `2026-11-19 05:24:10 UTC`
pub fn utc(ts: SystemTime) -> String {
    let (y, m, d, s, _) = split(ts);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `05:24:10.123` (UTC), for log lines.
pub fn clock(ts: SystemTime) -> String {
    let (_, _, _, s, ms) = split(ts);
    format!("{:02}:{:02}:{:02}.{ms:03}", s / 3600, s / 60 % 60, s % 60)
}

/// `20261119-052410`, for file names.
fn stamp(ts: SystemTime) -> String {
    let (y, m, d, s, _) = split(ts);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    format!("+{}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

fn stats_row(at: Duration, s: &StatsSample) -> String {
    let decode = match (s.decoded_fps, s.decode_ms, s.dropped) {
        (Some(f), Some(ms), Some(dr)) => format!(" | dec {f:5.1} fps {ms:5.1} ms drop {dr}"),
        _ => String::new(),
    };
    let host = s.host_ms.map(|h| format!(" | host {h:4.1} ms")).unwrap_or_default();
    let verdict = s
        .verdict
        .map(|v| format!(" | <- {}", v.to_uppercase()))
        .unwrap_or_default();
    format!(
        "{} | {:5.1} fps | {:6.2} Mbps | rtt {:3}±{:<3} | fec {:5} | lost {:4}{decode}{host}{verdict}",
        elapsed(at),
        s.fps,
        s.mbps,
        s.rtt_ms,
        s.rtt_variance_ms,
        s.fec_recovered,
        s.lost_frames
    )
}

impl StreamReport {
    /// Starts a report now; `log_mark` is [`crate::logcapture::mark`] (or 0).
    pub fn new(title: impl Into<String>, log_mark: u64) -> Self {
        Self {
            title: title.into(),
            started: SystemTime::now(),
            started_at: Instant::now(),
            log_mark,
            sections: Vec::new(),
            events: Vec::new(),
            samples: Vec::new(),
            first_frame: None,
            outcome: None,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Adds `key: value` to a section (created on first use).
    pub fn field(&mut self, section: &str, key: &str, value: impl ToString) {
        let value = value.to_string();
        match self.sections.iter_mut().find(|(name, _)| name == section) {
            Some((_, fields)) => fields.push((key.to_owned(), value)),
            None => self.sections.push((section.to_owned(), vec![(key.to_owned(), value)])),
        }
    }

    pub fn event(&mut self, text: impl Into<String>) {
        self.events.push((self.started_at.elapsed(), text.into()));
    }

    pub fn first_frame(&mut self) {
        if self.first_frame.is_none() {
            let at = self.started_at.elapsed();
            self.first_frame = Some(at);
            self.events.push((at, "first video frame".into()));
        }
    }

    pub fn sample(&mut self, sample: StatsSample) {
        self.samples.push((self.started_at.elapsed(), sample));
    }

    /// Records how the session ended (the first call wins).
    pub fn finish(&mut self, outcome: impl Into<String>) {
        if self.outcome.is_none() {
            let at = self.started_at.elapsed();
            let outcome = outcome.into();
            self.events.push((at, format!("ended: {outcome}")));
            self.outcome = Some((at, outcome));
        }
    }

    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    fn duration(&self) -> Duration {
        self.outcome
            .as_ref()
            .map_or_else(|| self.started_at.elapsed(), |(at, _)| *at)
    }

    /// A few lines for the report screen.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut out = vec![
            format!(
                "Result: {}",
                self.outcome.as_ref().map_or("still running", |(_, o)| o.as_str())
            ),
            format!("Duration: {}", human(self.duration())),
        ];
        match self.first_frame {
            Some(at) => out.push(format!("First frame after {}", human(at))),
            None => out.push("No video frame was received".into()),
        }
        // Skip the first second (ramp-up) when there is more.
        let steady: Vec<&StatsSample> = self
            .samples
            .iter()
            .skip(usize::from(self.samples.len() > 3))
            .map(|(_, s)| s)
            .collect();
        if !steady.is_empty() {
            let n = steady.len() as f64;
            let avg = |f: fn(&StatsSample) -> f64| steady.iter().map(|s| f(s)).sum::<f64>() / n;
            let min_fps = steady.iter().map(|s| s.fps).fold(f64::MAX, f64::min);
            out.push(format!(
                "Video: {:.1} fps average (lowest {:.1}), {:.1} Mbps",
                avg(|s| s.fps),
                min_fps,
                avg(|s| s.mbps)
            ));
            out.push(format!(
                "Network: RTT {:.0} ms average, max {} ms",
                avg(|s| s.rtt_ms as f64),
                steady.iter().map(|s| s.rtt_ms).max().unwrap_or(0)
            ));
            // Seconds the per-second diagnosis flagged, by cause.
            let mut causes: Vec<(&str, usize)> = Vec::new();
            for v in steady.iter().filter_map(|s| s.verdict) {
                match causes.iter_mut().find(|(c, _)| *c == v) {
                    Some((_, n)) => *n += 1,
                    None => causes.push((v, 1)),
                }
            }
            causes.sort_by(|a, b| b.1.cmp(&a.1));
            out.push(if causes.is_empty() {
                "Diagnosis: no bottleneck detected".to_owned()
            } else {
                let parts: Vec<String> = causes.iter().map(|(c, n)| format!("{n} s {c}")).collect();
                format!("Diagnosis: problems in {} (of {} s)", parts.join(", "), steady.len())
            });
            if let Some(last) = steady.last() {
                out.push(format!(
                    "Losses: {} frames lost, {} packets recovered by FEC",
                    last.lost_frames, last.fec_recovered
                ));
                if let (Some(ms), Some(dropped)) = (last.decode_ms, last.dropped) {
                    out.push(format!("Decoder: {ms:.1} ms per frame, {dropped} frames dropped"));
                }
            }
        }
        out
    }

    /// The full text, with `logs` (normally
    /// [`crate::logcapture::lines_since`] this report's mark).
    pub fn render_with_logs(&self, logs: &[String]) -> String {
        let mut t = String::new();
        let _ = writeln!(t, "Hestia RS stream report — {}", self.title);
        let _ = writeln!(t, "Started {}", utc(self.started));
        let _ = writeln!(t);
        let _ = writeln!(t, "== Summary");
        for line in self.summary_lines() {
            let _ = writeln!(t, "  {line}");
        }
        for (name, fields) in &self.sections {
            let _ = writeln!(t, "\n== {name}");
            let width = fields.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
            for (k, v) in fields {
                let _ = writeln!(t, "  {k:<width$}  {v}");
            }
        }
        let _ = writeln!(t, "\n== Events");
        for (at, text) in &self.events {
            let _ = writeln!(t, "  {} {text}", elapsed(*at));
        }
        let _ = writeln!(t, "\n== Statistics (per second)");
        if self.samples.is_empty() {
            let _ = writeln!(t, "  (none)");
        }
        let n = self.samples.len();
        for (i, (at, s)) in self.samples.iter().enumerate() {
            if n > MAX_STAT_ROWS && i == MAX_STAT_ROWS / 2 {
                let _ = writeln!(t, "  … {} rows omitted …", n - MAX_STAT_ROWS);
            }
            if n <= MAX_STAT_ROWS || i < MAX_STAT_ROWS / 2 || i >= n - MAX_STAT_ROWS / 2 {
                let _ = writeln!(t, "  {}", stats_row(*at, s));
            }
        }
        let _ = writeln!(t, "\n== Log ({} lines, times in UTC)", logs.len());
        for line in logs {
            let _ = writeln!(t, "{line}");
        }
        t
    }

    pub fn render(&self) -> String {
        self.render_with_logs(&crate::logcapture::lines_since(self.log_mark))
    }

    /// Writes the report to `dir` and deletes the oldest beyond
    /// [`KEEP_REPORTS`].
    pub fn save(&self, dir: &Path) -> io::Result<PathBuf> {
        self.save_text(dir, &self.render())
    }

    /// Like [`save`](Self::save) with an already rendered text.
    pub fn save_text(&self, dir: &Path, text: &str) -> io::Result<PathBuf> {
        save_named(dir, "stream", self.started, text)
    }
}

/// Writes `text` to `dir/<prefix>-<stamp>.txt` and keeps the newest
/// [`KEEP_REPORTS`] files with that prefix (e.g. Hermes' own reports next
/// to stream reports).
pub fn save_named(dir: &Path, prefix: &str, at: SystemTime, text: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{prefix}-{}.txt", stamp(at)));
    std::fs::write(&path, text)?;
    prune(dir, prefix, KEEP_REPORTS);
    Ok(path)
}

fn prune(dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut reports: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{prefix}-")) && n.ends_with(".txt"))
        })
        .collect();
    reports.sort();
    let excess = reports.len().saturating_sub(keep);
    for old in &reports[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

/// Shortens `text` to at most `max_bytes` by cutting whole lines out of the
/// middle: the summary at the top and the end of the log matter most.
pub fn clip_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let note_len = 96;
    let budget = max_bytes.saturating_sub(note_len);
    let head = budget / 3;
    let tail = budget - head;
    let mut head_end = head.min(text.len());
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    head_end = text[..head_end].rfind('\n').map_or(head_end, |i| i + 1);
    let mut tail_start = text.len() - tail.min(text.len());
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    tail_start = text[tail_start..].find('\n').map_or(tail_start, |i| tail_start + i + 1);
    let cut = tail_start.saturating_sub(head_end);
    format!(
        "{}[… {cut} bytes cut here; the saved report file has everything …]\n{}",
        &text[..head_end],
        &text[tail_start.max(head_end)..]
    )
}

/// The newest saved stream report in `dir`, if any.
pub fn latest(dir: &Path) -> Option<PathBuf> {
    let mut reports: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("stream-") && n.ends_with(".txt"))
        })
        .collect();
    reports.sort();
    reports.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let t = UNIX_EPOCH + Duration::from_secs(1_795_065_850) + Duration::from_millis(42);
        assert_eq!(utc(t), "2026-11-19 05:24:10 UTC");
        assert_eq!(clock(t), "05:24:10.042");
        assert_eq!(stamp(t), "20261119-052410");
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01 00:00:00 UTC");
        // Leap day.
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29 00:00:00 UTC"
        );
    }

    fn sample(fps: f64, rtt: u32, lost: u64) -> StatsSample {
        StatsSample {
            fps,
            mbps: 20.0,
            rtt_ms: rtt,
            rtt_variance_ms: 1,
            fec_recovered: 7,
            lost_frames: lost,
            decoded_fps: Some(fps),
            decode_ms: Some(4.0),
            dropped: Some(0),
            host_ms: Some(3.0),
            verdict: if lost > 1 { Some("network") } else { None },
        }
    }

    #[test]
    fn renders_everything() {
        let mut r = StreamReport::new("Desktop on cachy", 0);
        r.field("Host", "name", "cachy");
        r.field("Host", "address", "192.168.1.20");
        r.field("Negotiated", "video", "H264 1920x1080@60");
        r.event("RTSP handshake");
        r.first_frame();
        for (fps, rtt) in [(30.0, 9), (60.0, 3), (58.0, 4), (60.0, 5)] {
            r.sample(sample(fps, rtt, 1));
        }
        r.finish("the app was closed on the host");
        r.finish("ignored: first outcome wins");
        let text = r.render_with_logs(&["05:24:10.000 INFO  hestia_stream: hello".into()]);
        for needle in [
            "Hestia RS stream report — Desktop on cachy",
            "== Host",
            "  name     cachy",
            "  video  H264 1920x1080@60",
            "RTSP handshake",
            "first video frame",
            "ended: the app was closed on the host",
            "Video: 59.3 fps average (lowest 58.0), 20.0 Mbps",
            "Network: RTT 4 ms average, max 5 ms",
            "Losses: 1 frames lost, 7 packets recovered by FEC",
            "Diagnosis: no bottleneck detected",
            "== Log (1 lines, times in UTC)",
            "hestia_stream: hello",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        assert!(!text.contains("ignored"));
        assert!(r.summary_lines()[0].starts_with("Result: the app was closed"));
    }

    #[test]
    fn counts_flagged_seconds() {
        let mut r = StreamReport::new("x", 0);
        for lost in [0, 0, 2, 3, 0] {
            r.sample(sample(60.0, 3, lost));
        }
        let summary = r.summary_lines();
        assert!(
            summary.contains(&"Diagnosis: problems in 2 s network (of 4 s)".to_string()),
            "{summary:?}"
        );
        assert!(r.render_with_logs(&[]).contains("| <- NETWORK"));
    }

    #[test]
    fn long_sessions_keep_first_and_last_rows() {
        let mut r = StreamReport::new("long", 0);
        for i in 0..1000 {
            r.sample(sample(60.0, i, 0));
        }
        let text = r.render_with_logs(&[]);
        assert!(text.contains("400 rows omitted"));
        assert_eq!(text.matches(" fps | ").count(), MAX_STAT_ROWS);
    }

    #[test]
    fn clips_the_middle_on_line_boundaries() {
        let text: String = (0..2000).map(|i| format!("line {i:04} ção\n")).collect();
        assert_eq!(clip_middle("short", 100), "short");
        let clipped = clip_middle(&text, 4000);
        assert!(clipped.len() <= 4000, "{}", clipped.len());
        assert!(clipped.starts_with("line 0000 ção\n"));
        assert!(clipped.ends_with("line 1999 ção\n"));
        assert!(clipped.contains("bytes cut here"));
        // Only whole lines survive around the cut.
        for line in clipped.lines() {
            assert!(line.starts_with("line ") || line.starts_with("[…"), "{line:?}");
        }
    }

    #[test]
    fn saves_and_prunes() {
        let dir = std::env::temp_dir().join(format!("hestia-report-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(KEEP_REPORTS + 3) {
            std::fs::write(dir.join(format!("stream-20250101-0000{i:02}.txt")), "old").unwrap();
        }
        let hermes = save_named(&dir, "hermes", SystemTime::now(), "host report").unwrap();
        let r = StreamReport::new("now", 0);
        let path = r.save(&dir).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Hestia RS stream report — now")
        );
        let count = std::fs::read_dir(&dir).unwrap().count();
        // 20 stream reports + the Hermes one (pruned separately).
        assert_eq!(count, KEEP_REPORTS + 1);
        assert!(hermes.exists());
        assert_eq!(latest(&dir).as_deref(), Some(path.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
