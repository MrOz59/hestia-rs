//! Audio: UDP receive of RTP/Opus packets (payload type 97), with optional
//! AES-CBC decryption. FEC packets (type 127) are ignored for now. Packets
//! the network swapped are put back in order (waiting a few packets for a
//! gap to fill); the ones that never come are reported to the sink, so the
//! decoder can conceal them.

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hestia_crypto::stream::cbc_decrypt_pkcs7;
use log::{info, warn};

use crate::sink::{AudioFormat, AudioSink};
use crate::video::{bind_udp, ping_packet};

const RTP_HEADER_LEN: usize = 12;
const PT_OPUS: u8 = 97;
const PING_INTERVAL: Duration = Duration::from_millis(500);
/// A gap is given up on once this many later packets wait behind it
/// (20 ms of 5 ms packets)...
const REORDER_DEPTH: usize = 4;
/// ...or once the oldest of them has waited this long.
const REORDER_WAIT: Duration = Duration::from_millis(25);
/// A jump this far ahead is the stream starting over, not a gap.
const MAX_AHEAD: u16 = 64;
/// The most packets concealed for one gap. A longer outage is left to the
/// playback buffer, which plays silence and buffers more afterwards.
const MAX_CONCEALED: usize = 16;
/// How often waiting packets are checked while nothing arrives.
const POLL: Duration = Duration::from_millis(5);
/// A silence between packets long enough to log (packets come every 5 ms
/// while the host sends audio).
const QUIET_SPELL: Duration = Duration::from_millis(100);

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
    /// Packets that never came (each gap's first [`MAX_CONCEALED`] are concealed).
    pub lost: AtomicU64,
    /// Packets that came after their place was given up on, or repeats of
    /// ones already played.
    pub late: AtomicU64,
    /// Packets that came out of order, in time to be put back.
    pub reordered: AtomicU64,
    pub duplicates: AtomicU64,
    /// Silences between two packets: 50 ms or more, 100 ms or more, and the
    /// longest. Audio packets come every 5 ms, so these are the link going quiet.
    pub arrival_gaps_50ms: AtomicU64,
    pub arrival_gaps_100ms: AtomicU64,
    pub max_arrival_gap_us: AtomicU64,
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
        socket.set_read_timeout(Some(POLL))?;
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
    let mut reorder = Reorder::default();
    let mut released = Vec::new();
    let mut last_arrival: Option<Instant> = None;
    let mut quiet_spells = 0u64;
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        let received = match socket.recv_from(&mut buf) {
            Ok((len, from)) if from.ip() == cfg.remote.ip() => Some(len),
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => None,
            Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted) => continue,
            Err(e) => {
                warn!("audio socket error: {e}");
                break;
            }
        };
        let now = Instant::now();
        match received {
            Some(len) if len > RTP_HEADER_LEN && buf[1] & 0x7f == PT_OPUS => {
                if first {
                    first = false;
                    info!("first audio packet");
                }
                if let Some(previous) = last_arrival.replace(now) {
                    let gap = now.saturating_duration_since(previous);
                    note_arrival_gap(&counters, gap);
                    // Timestamped, to line up with video stalls in the log.
                    if gap >= QUIET_SPELL {
                        quiet_spells += 1;
                        if quiet_spells <= 20 || quiet_spells.is_multiple_of(25) {
                            info!(
                                "audio: no packets for {} ms ({quiet_spells} such gaps, {} packets lost so far)",
                                gap.as_millis(),
                                reorder.stats.lost
                            );
                        }
                    }
                }
                counters.packets.fetch_add(1, Ordering::Relaxed);
                let seq = u16::from_be_bytes([buf[2], buf[3]]);
                let payload = &buf[RTP_HEADER_LEN..len];
                // A packet that doesn't decrypt is played as a lost one.
                let opus = match &cfg.decrypt {
                    Some((key, key_id)) => decrypt_payload(key, *key_id, seq, payload).unwrap_or_default(),
                    None => payload.to_vec(),
                };
                reorder.push(seq, opus, now, &mut released);
            }
            Some(_) => continue,
            None => reorder.release(now, &mut released),
        }
        for packet in released.drain(..) {
            sink.submit(packet.as_deref().unwrap_or_default());
        }
        let s = &reorder.stats;
        counters.lost.store(s.lost, Ordering::Relaxed);
        counters.late.store(s.late, Ordering::Relaxed);
        counters.reordered.store(s.reordered, Ordering::Relaxed);
        counters.duplicates.store(s.duplicates, Ordering::Relaxed);
    }
    sink.cleanup();
}

fn note_arrival_gap(counters: &AudioCounters, gap: Duration) {
    if gap >= Duration::from_millis(50) {
        counters.arrival_gaps_50ms.fetch_add(1, Ordering::Relaxed);
    }
    if gap >= Duration::from_millis(100) {
        counters.arrival_gaps_100ms.fetch_add(1, Ordering::Relaxed);
    }
    counters
        .max_arrival_gap_us
        .fetch_max(gap.as_micros() as u64, Ordering::Relaxed);
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReorderStats {
    lost: u64,
    late: u64,
    reordered: u64,
    duplicates: u64,
}

/// Puts packets back in sequence order. Released packets go out as
/// `Some(opus)`; a packet given up on goes out as `None`, for the decoder
/// to conceal.
#[derive(Debug, Default)]
struct Reorder {
    /// Sequence number of `slots[0]`.
    next: Option<u16>,
    /// Packets from `next` on, with when they came; `None` is one not here (yet).
    slots: VecDeque<Option<(Instant, Vec<u8>)>>,
    /// How many slots hold a packet.
    held: usize,
    stats: ReorderStats,
}

impl Reorder {
    fn push(&mut self, seq: u16, opus: Vec<u8>, now: Instant, out: &mut Vec<Option<Vec<u8>>>) {
        let next = *self.next.get_or_insert(seq);
        let mut offset = seq.wrapping_sub(next);
        if offset >= 0x8000 {
            // Before `next`: given up on already, or played already.
            self.stats.late += 1;
            return;
        }
        if offset >= MAX_AHEAD {
            // The stream jumped ahead: play what is waiting, start over here.
            self.release_all(out);
            self.next = Some(seq);
            offset = 0;
        }
        let offset = usize::from(offset);
        if offset < self.slots.len() {
            if self.slots[offset].is_some() {
                self.stats.duplicates += 1;
                return;
            }
            // Later packets came first: this one fills a gap in time.
            self.stats.reordered += 1;
        } else {
            self.slots.resize(offset + 1, None);
        }
        self.slots[offset] = Some((now, opus));
        self.held += 1;
        self.release(now, out);
    }

    /// Releases the packets now in order, and gives up on a gap once enough
    /// packets, or time, have passed it.
    fn release(&mut self, now: Instant, out: &mut Vec<Option<Vec<u8>>>) {
        loop {
            match self.slots.front() {
                Some(Some(_)) => {
                    let (_, opus) = self.slots.pop_front().flatten().expect("front holds a packet");
                    self.held -= 1;
                    self.advance(1);
                    out.push(Some(opus));
                }
                Some(None) if self.held >= REORDER_DEPTH || self.oldest_wait(now) >= REORDER_WAIT => {
                    self.give_up_gap(out);
                }
                _ => break,
            }
        }
    }

    /// Everything waiting goes out now, the gaps between as losses.
    fn release_all(&mut self, out: &mut Vec<Option<Vec<u8>>>) {
        while let Some(front) = self.slots.front() {
            if front.is_some() {
                let (_, opus) = self.slots.pop_front().flatten().expect("front holds a packet");
                self.held -= 1;
                self.advance(1);
                out.push(Some(opus));
            } else {
                self.give_up_gap(out);
            }
        }
    }

    fn give_up_gap(&mut self, out: &mut Vec<Option<Vec<u8>>>) {
        let missing = self.slots.iter().take_while(|slot| slot.is_none()).count();
        self.slots.drain(..missing);
        self.advance(missing as u16);
        self.stats.lost += missing as u64;
        out.extend(std::iter::repeat_n(None, missing.min(MAX_CONCEALED)));
    }

    fn oldest_wait(&self, now: Instant) -> Duration {
        self.slots
            .iter()
            .flatten()
            .map(|(at, _)| now.saturating_duration_since(*at))
            .max()
            .unwrap_or_default()
    }

    fn advance(&mut self, by: u16) {
        self.next = self.next.map(|next| next.wrapping_add(by));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(r: &mut Reorder, seq: u16, at: Instant) -> Vec<Option<u8>> {
        let mut out = Vec::new();
        r.push(seq, vec![seq as u8], at, &mut out);
        out.into_iter().map(|p| p.map(|v| v[0])).collect()
    }

    #[test]
    fn in_order_packets_go_straight_through() {
        let mut r = Reorder::default();
        let t = Instant::now();
        for seq in 10..20u16 {
            assert_eq!(feed(&mut r, seq, t), vec![Some(seq as u8)]);
        }
        assert_eq!(r.stats, ReorderStats::default());
    }

    #[test]
    fn swapped_packets_are_put_back_in_order() {
        let mut r = Reorder::default();
        let t = Instant::now();
        assert_eq!(feed(&mut r, 1, t), vec![Some(1)]);
        assert!(feed(&mut r, 3, t).is_empty());
        assert_eq!(feed(&mut r, 2, t), vec![Some(2), Some(3)]);
        assert_eq!(r.stats.reordered, 1);
        assert_eq!(r.stats.lost, 0);
    }

    #[test]
    fn a_gap_is_given_up_after_four_packets_or_25_ms() {
        let mut r = Reorder::default();
        let t = Instant::now();
        feed(&mut r, 1, t);
        for seq in 3..=5 {
            assert!(feed(&mut r, seq, t).is_empty());
        }
        assert_eq!(feed(&mut r, 6, t), vec![None, Some(3), Some(4), Some(5), Some(6)]);
        assert_eq!(r.stats.lost, 1);
        // Too late now.
        assert!(feed(&mut r, 2, t).is_empty());
        assert_eq!(r.stats.late, 1);

        // A single packet past a gap waits 25 ms, then the gap goes.
        assert!(feed(&mut r, 9, t).is_empty());
        let mut out = Vec::new();
        r.release(t + Duration::from_millis(24), &mut out);
        assert!(out.is_empty());
        r.release(t + REORDER_WAIT, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[..2], [None, None]);
        assert_eq!(out[2].as_deref(), Some(&[9u8][..]));
        assert_eq!(r.stats.lost, 3);
    }

    #[test]
    fn long_gaps_conceal_only_so_much_and_jumps_start_over() {
        let mut r = Reorder::default();
        let t = Instant::now();
        feed(&mut r, 100, t);
        let mut out = Vec::new();
        r.push(140, vec![1], t, &mut out);
        r.release(t + REORDER_WAIT, &mut out);
        assert_eq!(out.iter().filter(|p| p.is_none()).count(), MAX_CONCEALED);
        assert_eq!(r.stats.lost, 39);
        // 1000 ahead: a restart, not 1000 losses.
        assert_eq!(feed(&mut r, 1141, t), vec![Some(1141u16 as u8)]);
        assert_eq!(r.stats.lost, 39);
        assert_eq!(feed(&mut r, 1142, t), vec![Some(1142u16 as u8)]);
    }

    #[test]
    fn duplicates_and_sequence_wrap() {
        let mut r = Reorder::default();
        let t = Instant::now();
        assert_eq!(feed(&mut r, 65534, t), vec![Some(254)]);
        assert!(feed(&mut r, 0, t).is_empty());
        assert!(feed(&mut r, 0, t).is_empty());
        assert_eq!(r.stats.duplicates, 1);
        assert_eq!(feed(&mut r, 65535, t), vec![Some(255), Some(0)]);
        assert!(feed(&mut r, 65535, t).is_empty());
        assert_eq!(r.stats.late, 1);
    }

    #[test]
    fn arrival_gaps_are_counted() {
        let c = AudioCounters::default();
        for ms in [5, 49, 50, 120] {
            note_arrival_gap(&c, Duration::from_millis(ms));
        }
        assert_eq!(c.arrival_gaps_50ms.load(Ordering::Relaxed), 2);
        assert_eq!(c.arrival_gaps_100ms.load(Ordering::Relaxed), 1);
        assert_eq!(c.max_arrival_gap_us.load(Ordering::Relaxed), 120_000);
    }
}
