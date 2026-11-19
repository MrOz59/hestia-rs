//! Keeps recent log lines in memory so a stream report can include them.
//!
//! [`install`] wraps the platform logger (env_logger, android_logger…): every
//! record still goes there, and records at Info and above, plus Debug from
//! Hestia's own crates, are also kept in a bounded ring buffer. A report
//! takes a [`mark`] when a session starts and collects [`lines_since`] it.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use log::{LevelFilter, Log, Metadata, Record};

/// Lines kept in memory (about 2–3 MB at most).
pub const CAPACITY: usize = 20_000;

#[derive(Debug, Default)]
pub struct LineBuffer {
    lines: VecDeque<String>,
    /// Sequence number of the next line.
    next: u64,
    capacity: usize,
}

impl LineBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            next: 0,
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, line: String) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
        self.next += 1;
    }

    pub fn mark(&self) -> u64 {
        self.next
    }

    /// Lines pushed since `mark`; if some were already dropped, the first
    /// line says how many.
    pub fn since(&self, mark: u64) -> Vec<String> {
        let first = self.next - self.lines.len() as u64;
        let mut out = Vec::new();
        if mark < first {
            out.push(format!(
                "[… {} earlier lines dropped from the log buffer]",
                first - mark
            ));
        }
        let skip = mark.saturating_sub(first) as usize;
        out.extend(self.lines.iter().skip(skip).cloned());
        out
    }
}

static BUFFER: Mutex<LineBuffer> = Mutex::new(LineBuffer {
    lines: VecDeque::new(),
    next: 0,
    capacity: CAPACITY,
});

fn buffer() -> std::sync::MutexGuard<'static, LineBuffer> {
    BUFFER.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn mark() -> u64 {
    buffer().mark()
}

pub fn lines_since(mark: u64) -> Vec<String> {
    buffer().since(mark)
}

/// Adds a line directly (for code that wants something in the report
/// without going through `log`).
pub fn note(line: impl Into<String>) {
    push_line(line.into());
}

/// This run's log on disk, and the one before it.
pub const CURRENT_LOG: &str = "app-current.log";
pub const PREVIOUS_LOG: &str = "app-previous.log";
/// The file stops growing past this (the in-memory buffer goes on).
const MAX_FILE_BYTES: u64 = 16 << 20;

struct LogFile {
    file: File,
    written: u64,
}

static FILE: Mutex<Option<LogFile>> = Mutex::new(None);

fn push_line(line: String) {
    if let Some(log) = FILE.lock().unwrap_or_else(|p| p.into_inner()).as_mut()
        && log.written < MAX_FILE_BYTES
    {
        let text = if log.written + line.len() as u64 + 1 >= MAX_FILE_BYTES {
            "[log file full; later lines are only in the app]\n".to_owned()
        } else {
            format!("{line}\n")
        };
        if log.file.write_all(text.as_bytes()).is_ok() {
            log.written += text.len() as u64;
        }
    }
    buffer().push(line);
}

/// Mirrors the log into `dir/app-current.log` as it is written, so it
/// survives the app being killed; the previous run's file is kept as
/// `app-previous.log`. Lines logged before this call are written first.
pub fn persist(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let current = dir.join(CURRENT_LOG);
    if current.exists() {
        fs::rename(&current, dir.join(PREVIOUS_LOG))?;
    }
    let mut file = File::create(&current)?;
    let mut written = 0;
    for line in lines_since(0) {
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        written += line.len() as u64 + 1;
    }
    *FILE.lock().unwrap_or_else(|p| p.into_inner()) = Some(LogFile { file, written });
    Ok(())
}

/// The previous run's log, if one was kept.
pub fn previous_run(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(PREVIOUS_LOG)).ok()
}

fn captured(meta: &Metadata<'_>) -> bool {
    meta.level() <= log::Level::Info || (meta.level() <= log::Level::Debug && meta.target().starts_with("hestia"))
}

struct Tee {
    inner: Box<dyn Log>,
}

impl Log for Tee {
    fn enabled(&self, meta: &Metadata<'_>) -> bool {
        self.inner.enabled(meta) || captured(meta)
    }

    fn log(&self, record: &Record<'_>) {
        if self.inner.enabled(record.metadata()) {
            self.inner.log(record);
        }
        if captured(record.metadata()) {
            let line = format!(
                "{} {:<5} {}: {}",
                crate::report::clock(SystemTime::now()),
                record.level(),
                record.target(),
                record.args()
            );
            push_line(line);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Installs the capturing logger around `inner`. `inner_level` is the most
/// verbose level `inner` wants (the capture itself needs Debug).
pub fn install(inner: Box<dyn Log>, inner_level: LevelFilter) -> Result<(), log::SetLoggerError> {
    log::set_boxed_logger(Box::new(Tee { inner }))?;
    log::set_max_level(inner_level.max(LevelFilter::Debug));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_marks_and_drops() {
        let mut b = LineBuffer::new(3);
        b.push("a".into());
        let m = b.mark();
        b.push("b".into());
        b.push("c".into());
        assert_eq!(b.since(m), ["b", "c"]);
        b.push("d".into());
        b.push("e".into());
        // "b" fell out of the buffer.
        assert_eq!(
            b.since(m),
            ["[… 1 earlier lines dropped from the log buffer]", "c", "d", "e"]
        );
        assert!(b.since(b.mark()).is_empty());
    }

    #[test]
    fn persists_and_keeps_the_previous_run() {
        let dir = std::env::temp_dir().join(format!("hestia-logfile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        note("first run line");
        persist(&dir).unwrap();
        note("written live");
        let current = fs::read_to_string(dir.join(CURRENT_LOG)).unwrap();
        assert!(
            current.contains("first run line") && current.contains("written live"),
            "{current}"
        );
        persist(&dir).unwrap();
        assert!(previous_run(&dir).unwrap().contains("written live"));
        *FILE.lock().unwrap() = None;
        let _ = fs::remove_dir_all(&dir);
    }
}
