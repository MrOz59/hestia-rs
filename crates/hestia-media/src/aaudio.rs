//! Low-latency PCM output through Android AAudio.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{info, warn};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat as PcmFormat, AudioPerformanceMode, AudioSharingMode, AudioStream,
    AudioStreamBuilder,
};

use crate::audio_fifo::{CHANNELS, SAMPLE_RATE};
use crate::audio_playback::{PcmOutput, PcmReader};

#[derive(Default)]
pub struct AAudioOutput {
    stream: Option<Output>,
    reader: Option<PcmReader>,
    lost: Arc<AtomicBool>,
}

impl PcmOutput for AAudioOutput {
    fn start(&mut self, reader: PcmReader) -> Result<(), String> {
        self.stop();
        self.reader = Some(reader.clone());
        self.stream = Some(open_output(reader, self.lost.clone())?);
        Ok(())
    }

    fn maintain(&mut self) {
        if self.lost.swap(false, Ordering::Relaxed) && let Some(reader) = &self.reader {
            self.stream = None;
            match open_output(reader.clone(), self.lost.clone()) {
                Ok(output) => self.stream = Some(output),
                Err(e) => warn!("reopening audio output failed: {e}"),
            }
        }
    }

    fn stop(&mut self) {
        self.stream = None;
        self.reader = None;
        self.lost.store(false, Ordering::Relaxed);
    }
}

/// The AAudio output stream (not `Send` in the ndk crate; it's only
/// started and stopped from the network thread).
struct Output(AudioStream);

// SAFETY: AAudio streams may be controlled from any thread.
unsafe impl Send for Output {}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.0.request_stop();
    }
}

fn open_output(reader: PcmReader, lost: Arc<AtomicBool>) -> Result<Output, String> {
    let stream = AudioStreamBuilder::new()
        .map_err(|e| format!("AAudio: {e:?}"))?
        .direction(AudioDirection::Output)
        .format(PcmFormat::PCM_Float)
        .channel_count(CHANNELS as i32)
        .sample_rate(SAMPLE_RATE as i32)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(AudioSharingMode::Shared)
        .data_callback(Box::new(move |stream: &AudioStream, data: *mut c_void, frames: i32| {
            let channels = stream.channel_count().max(1) as usize;
            let len = frames.max(0) as usize * channels;
            // SAFETY: AAudio hands us `frames` frames of `channels` floats.
            let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), len) };
            reader.read(out, channels, |s| s.clamp(-1.0, 1.0), 0.0);
            AudioCallbackResult::Continue
        }))
        .error_callback(Box::new(move |_stream: &AudioStream, error| {
            // Headphones unplugged, Bluetooth switched…: reopen from the
            // network thread (not allowed from this callback).
            warn!("audio output lost: {error:?}");
            lost.store(true, Ordering::Relaxed);
        }))
        .open_stream()
        .map_err(|e| format!("AAudio open: {e:?}"))?;
    stream.request_start().map_err(|e| format!("AAudio start: {e:?}"))?;
    info!(
        "audio output: AAudio {} Hz, {} ch, burst {} frames",
        stream.sample_rate(),
        stream.channel_count(),
        stream.frames_per_burst()
    );
    Ok(Output(stream))
}
