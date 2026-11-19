//! Brings the pieces up in moonlight-common-c's order — RTSP handshake,
//! control stream connect + START, video, audio, input — and supervises them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{info, warn};

use crate::audio::{AudioConfig, AudioReceiver};
use crate::control::{ControlEvent, ControlStream, TERMINATION_GRACEFUL};
use crate::input::{self, InputEvent};
use crate::rtsp::{self, HandshakeConfig, Negotiated};
use crate::sdp;
use crate::sink::{AudioSink, VideoSink};
use crate::video::{VideoConfig, VideoFeedback, VideoReceiver};
use crate::{SessionListener, StreamConfig, StreamError, StreamEvent, StreamStage, StreamStats};

pub struct StreamSession {
    negotiated: Negotiated,
    control: Arc<ControlStream>,
    video: Option<VideoReceiver>,
    audio: Option<AudioReceiver>,
    stop: Arc<AtomicBool>,
    supervisor: Option<JoinHandle<()>>,
}

impl StreamSession {
    pub fn start(
        config: StreamConfig,
        video_sink: Box<dyn VideoSink>,
        audio_sink: Box<dyn AudioSink>,
        listener: Arc<dyn SessionListener>,
    ) -> Result<Self, StreamError> {
        let stage = |s: StreamStage| listener.on_event(StreamEvent::StageStarting(s));
        let fail = |s: StreamStage, e: String| {
            listener.on_event(StreamEvent::StageFailed(s, e.clone()));
            e
        };

        stage(StreamStage::Rtsp);
        let negotiated = rtsp::handshake(&HandshakeConfig {
            session_url: config.session_url.clone(),
            server_codec_modes: config.server_codec_modes,
            remote_input_key: config.remote_input_key,
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            packet_size: config.packet_size,
            formats: config.video_formats.clone(),
            audio_channels: config.audio_channels,
            audio_channel_mask: config.audio_channel_mask,
            streaming_remotely: config.streaming_remotely,
        })
        .map_err(|e| StreamError::Rtsp(fail(StreamStage::Rtsp, e.to_string())))?;

        let ip = negotiated.remote.ip();
        stage(StreamStage::Control);
        let encrypted_control = negotiated.encryption_enabled & sdp::SS_ENC_CONTROL_V2 != 0;
        let mut control = ControlStream::connect(
            SocketAddr::new(ip, negotiated.control_port),
            negotiated.control_connect_data,
            config.remote_input_key,
            encrypted_control,
        )
        .map_err(|e| StreamError::Io(fail(StreamStage::Control, format!("control stream: {e}"))))?;
        let (control_tx, control_rx) = mpsc::channel();
        control
            .run(control_tx)
            .map_err(|e| StreamError::Io(fail(StreamStage::Control, e.to_string())))?;
        control
            .start()
            .map_err(|e| StreamError::Io(fail(StreamStage::Control, format!("START: {e}"))))?;
        let control = Arc::new(control);

        stage(StreamStage::Video);
        let announced_packet_size = if negotiated.encryption_enabled & sdp::SS_ENC_VIDEO != 0 {
            negotiated.packet_size - sdp::ENC_VIDEO_HEADER_LEN
        } else {
            negotiated.packet_size
        };
        let (video_tx, video_rx) = mpsc::channel();
        let video = VideoReceiver::start(
            VideoConfig {
                remote: SocketAddr::new(ip, negotiated.video_port),
                ping_payload: negotiated.video_ping_payload,
                packet_size: announced_packet_size,
                format: negotiated.video_format,
                width: config.width,
                height: config.height,
                fps: config.fps,
                decrypt_key: (negotiated.encryption_enabled & sdp::SS_ENC_VIDEO != 0)
                    .then_some(config.remote_input_key),
            },
            video_sink,
            video_tx,
        )
        .map_err(|e| StreamError::Io(fail(StreamStage::Video, e.to_string())))?;

        stage(StreamStage::Audio);
        let audio = match AudioReceiver::start(
            AudioConfig {
                remote: SocketAddr::new(ip, negotiated.audio_port),
                ping_payload: negotiated.audio_ping_payload,
                format: negotiated.audio_format,
                decrypt: negotiated
                    .audio_encrypted
                    .then_some((config.remote_input_key, config.remote_input_key_id)),
            },
            audio_sink,
        ) {
            Ok(a) => Some(a),
            Err(e) => {
                // Audio failing shouldn't kill an otherwise working stream.
                warn!("audio disabled: {e}");
                listener.on_event(StreamEvent::StageFailed(StreamStage::Audio, e.to_string()));
                None
            }
        };

        stage(StreamStage::Input);
        // Wiggle the mouse to wake the host display, like Moonlight does.
        for (dx, dy) in [(1, 1), (-1, -1)] {
            for p in input::encode(&InputEvent::MouseMove { dx, dy }) {
                let _ = control.send_input(&p.bytes, p.channel);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let supervisor = {
            let control = control.clone();
            let stop = stop.clone();
            let listener = listener.clone();
            let counters = video.counters.clone();
            std::thread::Builder::new()
                .name("hestia-session".into())
                .spawn(move || {
                    let mut last_stats = std::time::Instant::now();
                    while !stop.load(Ordering::Relaxed) {
                        while let Ok(fb) = video_rx.try_recv() {
                            match fb {
                                // The video loop already keeps one request in flight.
                                VideoFeedback::NeedIdr => {
                                    let _ = control.request_idr();
                                }
                                VideoFeedback::FirstFrame => listener.on_event(StreamEvent::FirstFrame),
                                VideoFeedback::NoVideo => {
                                    listener.on_event(StreamEvent::Terminated {
                                        reason: "no video received from the host".into(),
                                        graceful: false,
                                    });
                                    return;
                                }
                                VideoFeedback::SinkFailed(reason) => {
                                    listener.on_event(StreamEvent::Terminated { reason, graceful: false });
                                    return;
                                }
                            }
                        }
                        match control_rx.recv_timeout(Duration::from_millis(20)) {
                            Ok(ControlEvent::Terminated { code }) => {
                                let graceful = code == TERMINATION_GRACEFUL || code == 0;
                                info!("host ended the stream (code {code:#x})");
                                listener.on_event(StreamEvent::Terminated {
                                    reason: if graceful {
                                        "the app was closed on the host".into()
                                    } else {
                                        format!("the host ended the stream (error {code:#010x})")
                                    },
                                    graceful,
                                });
                                return;
                            }
                            Ok(ControlEvent::ConnectionLost) => {
                                listener.on_event(StreamEvent::Terminated {
                                    reason: "lost the connection to the host".into(),
                                    graceful: false,
                                });
                                return;
                            }
                            Ok(ControlEvent::Rumble { controller, low, high }) => {
                                listener.on_event(StreamEvent::Rumble {
                                    gamepad: controller as u8,
                                    low,
                                    high,
                                });
                            }
                            Ok(ControlEvent::HdrMode { enabled }) => listener.on_event(StreamEvent::HdrMode(enabled)),
                            Ok(ControlEvent::TriggerRumble { .. }) => {}
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                        if last_stats.elapsed() >= Duration::from_secs(1) {
                            last_stats = std::time::Instant::now();
                            let (rtt, rtt_var) = control.round_trip();
                            listener.on_event(StreamEvent::Stats(StreamStats {
                                frames: counters.frames.load(Ordering::Relaxed),
                                packets: counters.packets.load(Ordering::Relaxed),
                                bytes: counters.bytes.load(Ordering::Relaxed),
                                recovered_packets: counters.recovered.load(Ordering::Relaxed),
                                lost_frames: counters.lost_frames.load(Ordering::Relaxed),
                                rtt_ms: rtt,
                                rtt_variance_ms: rtt_var,
                                host_latency_us: counters.host_latency_us.load(Ordering::Relaxed),
                                host_latency_frames: counters.host_latency_frames.load(Ordering::Relaxed),
                            }));
                        }
                    }
                })
                .map_err(|e| StreamError::Io(e.to_string()))?
        };

        listener.on_event(StreamEvent::Connected);
        Ok(Self {
            negotiated,
            control,
            video: Some(video),
            audio,
            stop,
            supervisor: Some(supervisor),
        })
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    pub fn send_input(&self, event: &InputEvent) {
        for p in input::encode(event) {
            if let Err(e) = self.control.send_input(&p.bytes, p.channel) {
                warn!("input send failed: {e}");
                break;
            }
        }
    }

    pub fn request_keyframe(&self) {
        let _ = self.control.request_idr();
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.supervisor.take() {
            let _ = s.join();
        }
        if let Some(v) = self.video.take() {
            v.stop();
        }
        if let Some(a) = self.audio.take() {
            a.stop();
        }
        // The control stream disconnects when its last handle drops.
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_host::MockHost;
    use crate::sink::{AudioFormat, EncodedFrame, SubmitResult, VideoCodec, VideoFormat};
    use std::sync::Mutex;

    struct Collect(Arc<Mutex<Vec<EncodedFrame>>>);
    impl VideoSink for Collect {
        fn setup(&mut self, _: VideoFormat, _: u32, _: u32, _: u32) -> Result<(), String> {
            Ok(())
        }
        fn submit(&mut self, frame: EncodedFrame) -> SubmitResult {
            self.0.lock().unwrap().push(frame);
            SubmitResult::Ok
        }
    }
    struct NoAudio;
    impl AudioSink for NoAudio {
        fn setup(&mut self, _: &AudioFormat) -> Result<(), String> {
            Ok(())
        }
        fn submit(&mut self, _: &[u8]) {}
    }
    struct Events(Mutex<Vec<StreamEvent>>);
    impl SessionListener for Events {
        fn on_event(&self, event: StreamEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn frame(i: usize, len: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, if i == 0 { 0x65 } else { 0x41 }];
        v.extend((0..len).map(|b| ((b * 13 + i) as u8) | 1));
        v
    }

    #[test]
    fn end_to_end_against_mock_host() {
        let key = [0x42u8; 16];
        let frames: Vec<Vec<u8>> = (0..5).map(|i| frame(i, 3000 + i * 2500)).collect();
        let host = MockHost::start(key, frames.clone(), 1392);
        let got = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Events(Mutex::new(Vec::new())));
        let config = StreamConfig {
            session_url: crate::SessionUrl::parse(&format!("rtsp://127.0.0.1:{}", host.rtsp_port), "127.0.0.1")
                .unwrap(),
            server_codec_modes: 1,
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 10_000,
            packet_size: 1392,
            video_formats: vec![VideoFormat {
                codec: VideoCodec::H264,
                ten_bit: false,
                yuv444: false,
            }],
            audio_channels: 2,
            audio_channel_mask: 3,
            remote_input_key: key,
            remote_input_key_id: 7,
            streaming_remotely: false,
        };
        let session = StreamSession::start(
            config,
            Box::new(Collect(got.clone())),
            Box::new(NoAudio),
            events.clone(),
        )
        .expect("session starts");
        session.send_input(&InputEvent::Key {
            key: 0x41,
            pressed: true,
            modifiers: Default::default(),
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if events
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, StreamEvent::Terminated { .. }))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        session.stop();

        let evs = events.0.lock().unwrap().clone();
        assert!(evs.contains(&StreamEvent::Connected), "{evs:?}");
        assert!(evs.contains(&StreamEvent::FirstFrame), "{evs:?}");
        assert!(
            evs.iter()
                .any(|e| matches!(e, StreamEvent::Terminated { graceful: true, .. })),
            "{evs:?}"
        );
        let received = got.lock().unwrap();
        assert_eq!(received.len(), frames.len(), "every frame recovered through FEC");
        for (r, f) in received.iter().zip(&frames) {
            assert_eq!(&r.data, f);
        }
        assert!(received[0].keyframe);

        let announce = host.announce.lock().unwrap().clone().expect("ANNOUNCE received");
        assert!(announce.contains("a=x-nv-video[0].clientViewportWd:1280 \r\n"));
        assert!(announce.contains("a=x-ss-general.encryptionEnabled:1 \r\n"));
        let messages = host.control_messages.lock().unwrap();
        let types: Vec<u16> = messages.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            &types[..2],
            &[crate::control::PT_REQUEST_IDR, crate::control::PT_START_B],
            "START A/B first"
        );
        assert!(types.contains(&crate::control::PT_PERIODIC_PING));
        assert!(
            messages
                .iter()
                .any(|(t, p)| *t == crate::control::PT_INPUT && p.len() == 14),
            "keyboard input arrives decrypted"
        );
    }
}
