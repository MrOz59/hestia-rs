//! Audio: UDP receive of RTP/Opus packets (payload type 97), with optional
//! AES-CBC decryption. FEC packets (type 127) are ignored for now; lost
//! packets are reported to the sink so the decoder can conceal them.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use hestia_crypto::stream::cbc_decrypt_pkcs7;
use log::{info, warn};

use crate::sink::{AudioFormat, AudioSink};
use crate::video::{bind_udp, ping_packet};

const RTP_HEADER_LEN: usize = 12;
const PT_OPUS: u8 = 97;
const PING_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub remote: SocketAddr,
    pub ping_payload: Option<[u8; 16]>,
    pub format: AudioFormat,
    /// `(rikey, rikeyid)` when the host encrypts audio.
    pub decrypt: Option<([u8; 16], u32)>,
}

#[derive(Debug, Default)]
pub struct AudioCounters {
    pub packets: AtomicU64,
    pub lost: AtomicU64,
}

pub struct AudioReceiver {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub counters: Arc<AudioCounters>,
}

/// Decrypts one Opus payload. The IV is `be32(rikeyid + seq)` followed by zeros.
pub fn decrypt_payload(key: &[u8; 16], key_id: u32, seq: u16, payload: &[u8]) -> Option<Vec<u8>> {
    let mut iv = [0u8; 16];
    iv[..4].copy_from_slice(&key_id.wrapping_add(seq as u32).to_be_bytes());
    cbc_decrypt_pkcs7(key, &iv, payload)
}

impl AudioReceiver {
    pub fn start(cfg: AudioConfig, mut sink: Box<dyn AudioSink>) -> io::Result<Self> {
        sink.setup(&cfg.format)
            .map_err(|e| io::Error::other(format!("audio renderer setup failed: {e}")))?;
        let socket = bind_udp(&cfg.remote)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(AudioCounters::default());
        let mut threads = Vec::new();
        {
            let socket = socket.try_clone()?;
            let stop = stop.clone();
            let (remote, payload) = (cfg.remote, cfg.ping_payload);
            threads.push(
                std::thread::Builder::new()
                    .name("hestia-audio-ping".into())
                    .spawn(move || {
                        let mut seq = 0u32;
                        while !stop.load(Ordering::Relaxed) {
                            seq += 1;
                            let _ = socket.send_to(&ping_packet(payload.as_ref(), seq), remote);
                            std::thread::sleep(PING_INTERVAL);
                        }
                    })?,
            );
        }
        {
            let stop = stop.clone();
            let counters = counters.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("hestia-audio-rx".into())
                    .spawn(move || receive_loop(socket, cfg, sink, stop, counters))?,
            );
        }
        Ok(Self {
            stop,
            threads,
            counters,
        })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn receive_loop(
    socket: UdpSocket,
    cfg: AudioConfig,
    mut sink: Box<dyn AudioSink>,
    stop: Arc<AtomicBool>,
    counters: Arc<AudioCounters>,
) {
    let mut buf = [0u8; 2048];
    let mut last_seq: Option<u16> = None;
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        let len = match socket.recv_from(&mut buf) {
            Ok((len, from)) if from.ip() == cfg.remote.ip() => len,
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                warn!("audio socket error: {e}");
                break;
            }
        };
        if len <= RTP_HEADER_LEN || buf[1] & 0x7f != PT_OPUS {
            continue;
        }
        if first {
            first = false;
            info!("first audio packet");
        }
        let seq = u16::from_be_bytes([buf[2], buf[3]]);
        if let Some(prev) = last_seq {
            let gap = seq.wrapping_sub(prev);
            if gap == 0 || gap > 0x8000 {
                continue; // duplicate or late
            }
            if gap > 1 {
                let missing = (gap - 1).min(10);
                counters.lost.fetch_add(missing as u64, Ordering::Relaxed);
                for _ in 0..missing {
                    sink.submit(&[]);
                }
            }
        }
        last_seq = Some(seq);
        counters.packets.fetch_add(1, Ordering::Relaxed);
        let payload = &buf[RTP_HEADER_LEN..len];
        match &cfg.decrypt {
            Some((key, key_id)) => match decrypt_payload(key, *key_id, seq, payload) {
                Some(opus) => sink.submit(&opus),
                None => sink.submit(&[]),
            },
            None => sink.submit(payload),
        }
    }
    sink.cleanup();
}
