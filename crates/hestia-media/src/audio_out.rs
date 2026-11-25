//! Desktop PCM playback, with position-aware surround on Linux.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, warn};

use crate::audio_fifo::SAMPLE_RATE;
use crate::audio_playback::{PcmOutput, PcmReader};

#[derive(Default)]
pub struct CpalOutput {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PcmOutput for CpalOutput {
    fn start(&mut self, reader: PcmReader) -> Result<(), String> {
        self.stop();
        self.stop.store(false, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let stop = self.stop.clone();
        self.thread = Some(
            std::thread::Builder::new()
                .name("hestia-audio-out".into())
                .spawn(move || run_output(reader, stop, tx))
                .map_err(|e| e.to_string())?,
        );
        rx.recv().unwrap_or_else(|_| Err("audio thread exited".into()))
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl Drop for CpalOutput {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The device buffer asked for stereo output: 20 ms, the ALSA surround
/// output's four 5 ms periods. Left to its default, ALSA on PipeWire hands
/// out 2048-frame periods (43 ms each) and drives the whole audio graph at
/// a 1024-frame quantum, so every sound left the device 60 ms or more after
/// it was decoded, and the playback buffer, read 43 ms at a time, grew its
/// target to match.
const DEVICE_BUFFER_FRAMES: u32 = SAMPLE_RATE / 1000 * 20;

/// [`DEVICE_BUFFER_FRAMES`], within what the device takes.
fn device_buffer(supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    match *supported {
        cpal::SupportedBufferSize::Range { min, max } if min <= max => {
            cpal::BufferSize::Fixed(DEVICE_BUFFER_FRAMES.clamp(min, max))
        }
        _ => cpal::BufferSize::Fixed(DEVICE_BUFFER_FRAMES),
    }
}

/// The playback callback runs on the audio backend's own thread, which an
/// audio server may not have raised: raise it from its first call, on a
/// helper thread so the callback itself never waits.
fn raise_callback_once() {
    thread_local! {
        static RAISED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !RAISED.replace(true) {
        hestia_stream::thread_priority::raise_in_background(
            "audio playback callback",
            hestia_stream::thread_priority::Role::Audio,
        );
    }
}

fn run_output(reader: PcmReader, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>) {
    #[cfg(target_os = "linux")]
    if reader.channels() > 2 {
        match crate::alsa_surround::SurroundOutput::open(reader.channels()) {
            Ok(output) => {
                let _ = ready.send(Ok(()));
                match output.play(&reader, &stop) {
                    Ok(()) => return,
                    Err(e) => warn!("ALSA surround output stopped, trying stereo: {e}"),
                }
            }
            Err(e) => info!("surround unavailable on the default ALSA device, trying stereo: {e}"),
        }
    }

    if stop.load(Ordering::Relaxed) {
        return;
    }
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        let _ = ready.send(Err("no audio output device".into()));
        return;
    };
    let wanted = cpal::SampleRate(SAMPLE_RATE);
    let config = device
        .supported_output_configs()
        .ok()
        .and_then(|configs| {
            configs
                .filter(|c| {
                    c.min_sample_rate() <= wanted
                        && c.max_sample_rate() >= wanted
                        && matches!(c.sample_format(), cpal::SampleFormat::F32 | cpal::SampleFormat::I16)
                })
                .min_by_key(|c| (c.channels().abs_diff(2), c.sample_format() != cpal::SampleFormat::F32))
        })
        .map(|c| c.with_sample_rate(wanted));
    let Some(config) = config else {
        let _ = ready.send(Err("the audio device does not support 48 kHz output".into()));
        return;
    };
    let format = config.sample_format();
    let buffer = device_buffer(config.buffer_size());
    let mut stream_config: cpal::StreamConfig = config.into();
    stream_config.buffer_size = buffer;
    let channels = stream_config.channels as usize;
    let build = |stream_config: &cpal::StreamConfig| {
        let on_error = |e: cpal::StreamError| warn!("audio output error: {e}");
        let reader = reader.clone();
        match format {
            cpal::SampleFormat::I16 => device.build_output_stream(
                stream_config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    raise_callback_once();
                    reader.stats().note_callback((out.len() / channels.max(1)) as u64);
                    reader.read_stereo(out, channels, |s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16, 0)
                },
                on_error,
                None,
            ),
            _ => device.build_output_stream(
                stream_config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    raise_callback_once();
                    reader.stats().note_callback((out.len() / channels.max(1)) as u64);
                    reader.read_stereo(out, channels, |s| s, 0.0)
                },
                on_error,
                None,
            ),
        }
    };
    let stream = match build(&stream_config) {
        Err(e) if stream_config.buffer_size != cpal::BufferSize::Default => {
            info!("audio output: the device refused a {buffer:?} buffer ({e}); using its default");
            stream_config.buffer_size = cpal::BufferSize::Default;
            build(&stream_config)
        }
        built => built,
    };
    let stream = match stream
        .map_err(|e| e.to_string())
        .and_then(|s| s.play().map(|()| s).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(format!("audio output: {e}")));
            return;
        }
    };
    info!(
        "audio output: cpal {} (host {} ch, device {channels} ch, {format:?}, buffer {:?})",
        device.name().unwrap_or_else(|_| "default".into()),
        reader.channels(),
        stream_config.buffer_size
    );
    let _ = ready.send(Ok(()));
    // cpal streams are not Send on every platform: keep it on this thread.
    while !stop.load(Ordering::Relaxed) {
        std::thread::park_timeout(std::time::Duration::from_millis(100));
    }
    drop(stream);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_buffer_is_twenty_milliseconds_within_its_range() {
        let range = |min, max| cpal::SupportedBufferSize::Range { min, max };
        assert_eq!(device_buffer(&range(64, 8192)), cpal::BufferSize::Fixed(960));
        assert_eq!(device_buffer(&range(2048, 8192)), cpal::BufferSize::Fixed(2048));
        assert_eq!(device_buffer(&range(64, 512)), cpal::BufferSize::Fixed(512));
        assert_eq!(
            device_buffer(&cpal::SupportedBufferSize::Unknown),
            cpal::BufferSize::Fixed(960)
        );
    }
}
