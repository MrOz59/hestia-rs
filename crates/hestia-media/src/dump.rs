//! Writes the received elementary stream to disk for offline inspection.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use hestia_stream::sink::{EncodedFrame, SubmitResult, VideoCodec, VideoFormat, VideoSink};

pub struct DumpVideoSink {
    path: PathBuf,
    out: Option<BufWriter<File>>,
}

impl DumpVideoSink {
    /// `path` without extension; `.h264`, `.h265` or `.obu` is appended once
    /// the codec is known.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            out: None,
        }
    }
}

impl VideoSink for DumpVideoSink {
    fn setup(&mut self, format: VideoFormat, _width: u32, _height: u32, _fps: u32) -> Result<(), String> {
        let ext = match format.codec {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "h265",
            VideoCodec::Av1 => "obu",
            VideoCodec::PyroWave => "pyrowave",
        };
        let path = self.path.with_extension(ext);
        let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.out = Some(BufWriter::new(file));
        Ok(())
    }

    fn submit(&mut self, frame: EncodedFrame) -> SubmitResult {
        if let Some(out) = &mut self.out
            && let Err(e) = out.write_all(&frame.data)
        {
            log::warn!("dump write failed: {e}");
            self.out = None;
        }
        SubmitResult::Ok
    }

    fn cleanup(&mut self) {
        if let Some(mut out) = self.out.take() {
            let _ = out.flush();
        }
    }
}
