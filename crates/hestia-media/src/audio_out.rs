//! Opus decode + playback through the system audio API (cpal: ALSA /
//! PulseAudio / PipeWire on Linux). Feature `audio`.
//!
//! The network thread decodes each 5 ms packet straight into the sample
//! FIFO ([`crate::audio_fifo`]) that the audio callback drains.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hestia_stream::{AudioFormat, AudioSink};
use log::{info, warn};

use crate::audio_fifo::{CHANNELS, Fifo, SAMPLE_RATE, fill, lock};

pub struct OpusPlaybackSink {
    decoder: Option<opus::Decoder>,
    pcm: Vec<f32>,
    /// Samples per channel in one packet (240 for 5 ms).
    frame_samples: usize,
    fifo: Arc<Mutex<Fifo>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Default for OpusPlaybackSink {
    fn default() -> Self {
        Self::new()
    }
}

impl OpusPlaybackSink {
    pub fn new() -> Self {
        Self {
            decoder: None,
            pcm: vec![0.0; 5760 * CHANNELS],
            frame_samples: 240,
            fifo: Arc::new(Mutex::new(Fifo::new())),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }
}

fn run_output(fifo: Arc<Mutex<Fifo>>, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>) {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        let _ = ready.send(Err("no audio output device".into()));
        return;
    };
    let wanted = cpal::SampleRate(SAMPLE_RATE);
    let config = device
        .supported_output_configs()
        .ok()
        .and_then(|mut configs| {
            configs.find(|c| {
                c.channels() >= 1
                    && c.min_sample_rate() <= wanted
                    && c.max_sample_rate() >= wanted
                    && matches!(c.sample_format(), cpal::SampleFormat::F32 | cpal::SampleFormat::I16)
            })
        })
        .map(|c| c.with_sample_rate(wanted));
    let Some(config) = config else {
        let _ = ready.send(Err("the audio device does not support 48 kHz output".into()));
        return;
    };
    let format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();
    let channels = stream_config.channels as usize;
    let on_error = |e: cpal::StreamError| warn!("audio output error: {e}");
    let stream = match format {
        cpal::SampleFormat::I16 => {
            let fifo = fifo.clone();
            device.build_output_stream(
                &stream_config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    fill(
                        &fifo,
                        out,
                        channels,
                        |s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16,
                        0,
                    )
                },
                on_error,
                None,
            )
        }
        _ => {
            let fifo = fifo.clone();
            device.build_output_stream(
                &stream_config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| fill(&fifo, out, channels, |s| s, 0.0),
                on_error,
                None,
            )
        }
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
        "audio output: {} ({channels} ch, {format:?})",
        device.name().unwrap_or_else(|_| "default".into())
    );
    let _ = ready.send(Ok(()));
    // cpal streams are not Send on every platform: keep it on this thread.
    while !stop.load(Ordering::Relaxed) {
        std::thread::park_timeout(std::time::Duration::from_millis(100));
    }
    drop(stream);
}

impl AudioSink for OpusPlaybackSink {
    fn setup(&mut self, format: &AudioFormat) -> Result<(), String> {
        if format.channels != 2 || format.streams != 1 || format.sample_rate != SAMPLE_RATE {
            return Err(format!(
                "only 48 kHz stereo playback is implemented (host offers {} channels)",
                format.channels
            ));
        }
        self.decoder = Some(opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).map_err(|e| e.to_string())?);
        self.frame_samples = (format.samples_per_frame as usize).clamp(120, 5760);
        let (tx, rx) = mpsc::channel();
        let (fifo, stop) = (self.fifo.clone(), self.stop.clone());
        self.thread = Some(
            std::thread::Builder::new()
                .name("hestia-audio-out".into())
                .spawn(move || run_output(fifo, stop, tx))
                .map_err(|e| e.to_string())?,
        );
        rx.recv().unwrap_or_else(|_| Err("audio thread exited".into()))
    }

    fn submit(&mut self, opus_packet: &[u8]) {
        let Some(decoder) = &mut self.decoder else { return };
        // An empty packet runs packet-loss concealment, which must be asked
        // for exactly one packet's worth of samples.
        let out = if opus_packet.is_empty() {
            &mut self.pcm[..self.frame_samples * CHANNELS]
        } else {
            &mut self.pcm[..]
        };
        let samples = match decoder.decode_float(opus_packet, out, false) {
            Ok(n) => n,
            Err(e) => {
                warn!("opus decode: {e}");
                return;
            }
        };
        lock(&self.fifo).push(&self.pcm[..samples * CHANNELS]);
    }

    fn cleanup(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl Drop for OpusPlaybackSink {
    fn drop(&mut self) {
        self.cleanup();
    }
}
