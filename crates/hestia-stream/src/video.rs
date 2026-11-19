//! Video: UDP receive, (optional) decryption, FEC-block reassembly with
//! Reed–Solomon recovery and frame depacketization. Ported from
//! moonlight-common-c `VideoStream.c`, `RtpVideoQueue.c` and
//! `VideoDepacketizer.c`, simplified to "drop until the next IDR" on loss
//! (we don't use reference frame invalidation).
//!
//! Wire layout of one shard (`packet_size + 16` bytes):
//! `RTP(12) | reserved(4) | NV_VIDEO_PACKET(16) | payload`. The payload of the
//! first data shard starts with an 8-byte frame header. FEC covers whole
//! shards; after recovery only the payload bytes are trusted.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hestia_crypto::stream::gcm_decrypt;
use log::{debug, info, warn};

use crate::fec;
use crate::sink::{EncodedFrame, KeyframeRequester, SubmitResult, VideoCodec, VideoFormat, VideoSink};

pub const RTP_HEADER_LEN: usize = 12;
pub const MAX_RTP_HEADER_LEN: usize = 16;
pub const NV_HEADER_LEN: usize = 16;
pub const SHARD_HEADER_LEN: usize = MAX_RTP_HEADER_LEN + NV_HEADER_LEN;
const ENC_PREFIX_LEN: usize = 12 + 4 + 16;
const FLAG_EXTENSION: u8 = 0x10;
pub const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;

const PING_INTERVAL: Duration = Duration::from_millis(500);
const FIRST_DATA_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    pub seq: u16,
    pub timestamp: u32,
    pub frame: u32,
    pub flags: u8,
    pub fec_index: u16,
    pub data_shards: u16,
    pub fec_percent: u16,
    pub block: u8,
    pub last_block: u8,
}

impl PacketInfo {
    pub fn parity_shards(&self) -> u16 {
        ((self.data_shards as u32 * self.fec_percent as u32).div_ceil(100)) as u16
    }
}

pub fn parse_packet(p: &[u8]) -> Option<PacketInfo> {
    if p.len() < RTP_HEADER_LEN {
        return None;
    }
    let nv = if p[0] & FLAG_EXTENSION != 0 { 16 } else { 12 };
    if p.len() < nv + NV_HEADER_LEN {
        return None;
    }
    let le32 = |at: usize| u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]]);
    let fec_info = le32(nv + 12);
    let multi_blocks = p[nv + 11];
    Some(PacketInfo {
        seq: u16::from_be_bytes([p[2], p[3]]),
        timestamp: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        frame: le32(nv + 4),
        flags: p[nv + 8],
        fec_index: ((fec_info & 0x003f_f000) >> 12) as u16,
        data_shards: ((fec_info & 0xffc0_0000) >> 22) as u16,
        fec_percent: ((fec_info & 0x0000_0ff0) >> 4) as u16,
        block: (multi_blocks >> 4) & 0x3,
        last_block: (multi_blocks >> 6) & 0x3,
    })
}

fn frame_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// One reassembled access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledFrame {
    pub frame: u32,
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub host_latency_100us: u16,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AssemblerStats {
    pub frames: u64,
    pub recovered_packets: u64,
    pub lost_frames: u64,
    pub dropped_waiting_idr: u64,
}

struct Block {
    lowest_seq: u16,
    data_shards: usize,
    parity_shards: usize,
    last_block: u8,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    received_data: usize,
}

struct FrameState {
    frame: u32,
    /// Payload shards of completed FEC blocks, in order.
    payloads: Vec<Vec<u8>>,
    blocks_done: u8,
    block: Option<Block>,
}

/// Reassembles frames from shards. Pure logic, no I/O.
pub struct FrameAssembler {
    shard_len: usize,
    codec: VideoCodec,
    next_frame: Option<u32>,
    current: Option<FrameState>,
    waiting_for_idr: bool,
    pub stats: AssemblerStats,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssemblerOutput {
    Frame(AssembledFrame),
    /// A frame was lost; the decoder needs an IDR to recover.
    NeedIdr,
}

impl FrameAssembler {
    /// `packet_size` is the value announced as `x-nv-video[0].packetSize`.
    pub fn new(packet_size: u32, codec: VideoCodec) -> Self {
        Self {
            shard_len: packet_size as usize + MAX_RTP_HEADER_LEN,
            codec,
            next_frame: None,
            current: None,
            waiting_for_idr: true,
            stats: AssemblerStats::default(),
        }
    }

    pub fn shard_len(&self) -> usize {
        self.shard_len
    }

    fn lose(&mut self, out: &mut Vec<AssemblerOutput>, frame: u32) {
        self.stats.lost_frames += 1;
        self.current = None;
        self.next_frame = Some(frame.wrapping_add(1));
        if !self.waiting_for_idr {
            self.waiting_for_idr = true;
        }
        out.push(AssemblerOutput::NeedIdr);
    }

    pub fn add(&mut self, packet: &[u8]) -> Vec<AssemblerOutput> {
        let mut out = Vec::new();
        let Some(info) = parse_packet(packet) else {
            return out;
        };
        if let Some(next) = self.next_frame
            && frame_before(info.frame, next)
        {
            return out; // late shard of a finished or abandoned frame
        }
        if info.data_shards == 0 {
            return out;
        }

        // A packet for a newer frame means the current one can't complete.
        if let Some(cur) = &self.current
            && cur.frame != info.frame
        {
            let lost = cur.frame;
            self.lose(&mut out, lost);
        }
        if self.current.is_none() {
            if let Some(next) = self.next_frame
                && info.frame != next
            {
                // Whole frames went missing.
                self.stats.lost_frames += info.frame.wrapping_sub(next) as u64;
                self.waiting_for_idr = true;
                if !out.contains(&AssemblerOutput::NeedIdr) {
                    out.push(AssemblerOutput::NeedIdr);
                }
            }
            self.current = Some(FrameState {
                frame: info.frame,
                payloads: Vec::new(),
                blocks_done: 0,
                block: None,
            });
        }

        let shard_len = self.shard_len;
        let cur = self.current.as_mut().expect("current frame set above");
        if info.block < cur.blocks_done {
            return out; // extra parity for a block we already finished
        }
        if info.block > cur.blocks_done && cur.block.is_none() {
            // We missed an entire FEC block.
            let lost = cur.frame;
            self.lose(&mut out, lost);
            return out;
        }
        if cur.block.is_none() {
            let data_shards = info.data_shards as usize;
            let parity_shards = info.parity_shards() as usize;
            if data_shards + parity_shards > fec::MAX_SHARDS {
                let lost = cur.frame;
                self.lose(&mut out, lost);
                return out;
            }
            cur.block = Some(Block {
                lowest_seq: info.seq.wrapping_sub(info.fec_index),
                data_shards,
                parity_shards,
                last_block: info.last_block,
                shards: vec![None; data_shards + parity_shards],
                received: 0,
                received_data: 0,
            });
        }
        let block = cur.block.as_mut().expect("block set above");
        let index = info.seq.wrapping_sub(block.lowest_seq) as usize;
        if index >= block.shards.len() || block.shards[index].is_some() {
            return out;
        }
        let mut shard = packet.to_vec();
        shard.resize(shard_len, 0);
        block.shards[index] = Some(shard);
        block.received += 1;
        if index < block.data_shards {
            block.received_data += 1;
        }
        if block.received < block.data_shards {
            return out;
        }

        // Enough shards: recover what's missing and take the payloads.
        let recovered = block.data_shards - block.received_data;
        if recovered > 0 {
            if fec::reconstruct(&mut block.shards, block.data_shards, block.parity_shards, shard_len).is_err() {
                let lost = cur.frame;
                self.lose(&mut out, lost);
                return out;
            }
            self.stats.recovered_packets += recovered as u64;
        }
        let block = cur.block.take().expect("block present");
        let last_block = block.last_block;
        for shard in block.shards.into_iter().take(block.data_shards) {
            let shard = shard.unwrap_or_default();
            cur.payloads
                .push(shard.get(SHARD_HEADER_LEN..).unwrap_or_default().to_vec());
        }
        cur.blocks_done += 1;
        if cur.blocks_done <= last_block {
            return out; // more blocks to come
        }

        let state = self.current.take().expect("frame present");
        self.next_frame = Some(state.frame.wrapping_add(1));
        match depacketize(state.frame, &state.payloads, self.codec) {
            Some(frame) => {
                if self.waiting_for_idr && !frame.keyframe {
                    self.stats.dropped_waiting_idr += 1;
                } else {
                    self.waiting_for_idr = false;
                    self.stats.frames += 1;
                    out.push(AssemblerOutput::Frame(frame));
                }
            }
            None => self.lose(&mut out, state.frame),
        }
        out
    }
}

/// Joins shard payloads into an access unit and strips the frame header.
pub fn depacketize(frame: u32, payloads: &[Vec<u8>], codec: VideoCodec) -> Option<AssembledFrame> {
    let first = payloads.first()?;
    if first.len() < 8 {
        return None;
    }
    let header_len = if first[0] == 0x01 { 8 } else { 44 };
    let frame_type = first[3];
    let latency = u16::from_le_bytes([first[1], first[2]]);
    let last_len = u16::from_le_bytes([first[4], first[5]]) as usize;
    let capacity = first.len();

    let mut data: Vec<u8> = payloads.concat();
    // Exact length: (packets - 1) full payloads + the last packet's length.
    if header_len == 8 && last_len > 0 && last_len <= capacity {
        let exact = (payloads.len() - 1) * capacity + last_len;
        if exact <= data.len() && exact >= header_len {
            data.truncate(exact);
        }
    } else if codec != VideoCodec::Av1 {
        while data.last() == Some(&0) {
            data.pop();
        }
    }
    if data.len() < header_len {
        return None;
    }
    let data = data.split_off(header_len);
    let keyframe = frame_type == 2 || is_keyframe_bitstream(&data, codec);
    Some(AssembledFrame {
        frame,
        data,
        keyframe,
        host_latency_100us: latency,
    })
}

/// Scans Annex B NAL headers for IDR/parameter sets.
pub fn is_keyframe_bitstream(data: &[u8], codec: VideoCodec) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        let start = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            Some(i + 3)
        } else {
            None
        };
        if let Some(n) = start {
            if n < data.len() {
                let header = data[n];
                match codec {
                    VideoCodec::H264 => {
                        let t = header & 0x1f;
                        if t == 5 || t == 7 {
                            return true;
                        }
                        if t == 1 {
                            return false;
                        }
                    }
                    VideoCodec::Hevc => {
                        let t = (header >> 1) & 0x3f;
                        if (16..=21).contains(&t) || t == 32 || t == 33 {
                            return true;
                        }
                        if t < 16 {
                            return false;
                        }
                    }
                    VideoCodec::Av1 => return false,
                }
            }
            i = n;
        } else {
            i += 1;
        }
    }
    false
}

/// Configuration of the receive side.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub remote: SocketAddr,
    pub ping_payload: Option<[u8; 16]>,
    pub packet_size: u32,
    pub format: VideoFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `Some(key)` when the host encrypts video.
    pub decrypt_key: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoFeedback {
    NeedIdr,
    FirstFrame,
    NoVideo,
    SinkFailed(String),
}

#[derive(Debug, Default)]
pub struct VideoCounters {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub frames: AtomicU64,
    pub recovered: AtomicU64,
    pub lost_frames: AtomicU64,
    /// Sum of host-reported processing latency, and how many frames had it.
    pub host_latency_us: AtomicU64,
    pub host_latency_frames: AtomicU64,
}

pub struct VideoReceiver {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub counters: Arc<VideoCounters>,
}

pub fn bind_udp(remote: &SocketAddr) -> io::Result<UdpSocket> {
    let bind: SocketAddr = match remote.ip() {
        IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    UdpSocket::bind(bind)
}

/// `payload(16) | seq(be32)` or the legacy "PING".
pub fn ping_packet(payload: Option<&[u8; 16]>, seq: u32) -> Vec<u8> {
    match payload {
        Some(p) => {
            let mut v = p.to_vec();
            v.extend_from_slice(&seq.to_be_bytes());
            v
        }
        None => b"PING".to_vec(),
    }
}

impl VideoReceiver {
    pub fn start(cfg: VideoConfig, mut sink: Box<dyn VideoSink>, feedback: Sender<VideoFeedback>) -> io::Result<Self> {
        {
            let feedback = feedback.clone();
            sink.attach_keyframe_requester(KeyframeRequester::new(move || {
                let _ = feedback.send(VideoFeedback::NeedIdr);
            }));
        }
        sink.setup(cfg.format, cfg.width, cfg.height, cfg.fps)
            .map_err(|e| io::Error::other(format!("video decoder setup failed: {e}")))?;
        let socket = bind_udp(&cfg.remote)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(VideoCounters::default());
        let mut threads = Vec::new();

        {
            let socket = socket.try_clone()?;
            let stop = stop.clone();
            let remote = cfg.remote;
            let payload = cfg.ping_payload;
            threads.push(
                std::thread::Builder::new()
                    .name("hestia-video-ping".into())
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
                    .name("hestia-video-rx".into())
                    .spawn(move || receive_loop(socket, cfg, sink, feedback, stop, counters))?,
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

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn receive_loop(
    socket: UdpSocket,
    cfg: VideoConfig,
    mut sink: Box<dyn VideoSink>,
    feedback: Sender<VideoFeedback>,
    stop: Arc<AtomicBool>,
    counters: Arc<VideoCounters>,
) {
    let mut assembler = FrameAssembler::new(cfg.packet_size, cfg.format.codec);
    let shard_len = assembler.shard_len();
    let mut buf = vec![0u8; shard_len + ENC_PREFIX_LEN + 64];
    let started = Instant::now();
    let mut got_data = false;
    let mut first_frame = false;
    let mut frame_started: Option<Instant> = None;
    let mut last_idr_request = Instant::now() - Duration::from_secs(1);

    while !stop.load(Ordering::Relaxed) {
        let len = match socket.recv_from(&mut buf) {
            Ok((len, from)) if from.ip() == cfg.remote.ip() => len,
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if !got_data && started.elapsed() > FIRST_DATA_TIMEOUT {
                    warn!("no video traffic from the host");
                    let _ = feedback.send(VideoFeedback::NoVideo);
                    return;
                }
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                warn!("video socket error: {e}");
                return;
            }
        };
        if !got_data {
            got_data = true;
            info!("first video packet after {:?}", started.elapsed());
        }
        counters.packets.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(len as u64, Ordering::Relaxed);

        let plain;
        let packet: &[u8] = match &cfg.decrypt_key {
            Some(key) => {
                if len < ENC_PREFIX_LEN + RTP_HEADER_LEN {
                    continue;
                }
                let iv: [u8; 12] = buf[..12].try_into().unwrap_or_default();
                match gcm_decrypt(key, &iv, &buf[ENC_PREFIX_LEN..len], &buf[16..32]) {
                    Some(p) => {
                        plain = p;
                        &plain
                    }
                    None => continue,
                }
            }
            None => &buf[..len],
        };
        if frame_started.is_none() {
            frame_started = Some(Instant::now());
        }
        for output in assembler.add(packet) {
            match output {
                AssemblerOutput::NeedIdr => {
                    if last_idr_request.elapsed() > Duration::from_millis(250) {
                        last_idr_request = Instant::now();
                        let _ = feedback.send(VideoFeedback::NeedIdr);
                    }
                }
                AssemblerOutput::Frame(frame) => {
                    if !first_frame {
                        first_frame = true;
                        let _ = feedback.send(VideoFeedback::FirstFrame);
                    }
                    counters.frames.fetch_add(1, Ordering::Relaxed);
                    if frame.host_latency_100us > 0 {
                        counters
                            .host_latency_us
                            .fetch_add(u64::from(frame.host_latency_100us) * 100, Ordering::Relaxed);
                        counters.host_latency_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    let encoded = EncodedFrame {
                        frame_number: frame.frame,
                        keyframe: frame.keyframe,
                        data: frame.data,
                        received_at: frame_started.take().unwrap_or_else(Instant::now),
                        host_latency: Some(Duration::from_micros(frame.host_latency_100us as u64 * 100)),
                    };
                    if sink.submit(encoded) == SubmitResult::NeedKeyframe {
                        debug!("decoder asked for a keyframe");
                        assembler = FrameAssembler::new(cfg.packet_size, cfg.format.codec);
                        last_idr_request = Instant::now();
                        let _ = feedback.send(VideoFeedback::NeedIdr);
                    }
                }
            }
        }
        counters
            .recovered
            .store(assembler.stats.recovered_packets, Ordering::Relaxed);
        counters
            .lost_frames
            .store(assembler.stats.lost_frames, Ordering::Relaxed);
    }
    sink.cleanup();
}

/// Test helper: packetizes a frame the way Sunshine/Hermes do.
#[cfg(test)]
pub(crate) fn packetize(
    frame: u32,
    data: &[u8],
    packet_size: usize,
    fec_percent: usize,
    seq_start: u16,
    idr: bool,
) -> Vec<Vec<u8>> {
    let blocksize = packet_size + MAX_RTP_HEADER_LEN;
    let capacity = blocksize - SHARD_HEADER_LEN;
    let mut header = vec![0x01, 0, 0, if idr { 2 } else { 1 }, 0, 0, 0, 0];
    let total = data.len() + header.len();
    let mut last = total % (packet_size - NV_HEADER_LEN);
    if last == 0 {
        last = packet_size - NV_HEADER_LEN;
    }
    header[4..6].copy_from_slice(&(last as u16).to_le_bytes());
    let mut payload = header;
    payload.extend_from_slice(data);
    let chunks: Vec<&[u8]> = payload.chunks(capacity).collect();
    let ds = chunks.len();
    let ps = (ds * fec_percent).div_ceil(100).max(1);
    let pct = (ps * 100).div_ceil(ds);
    let ps = (ds * pct).div_ceil(100);
    let mut shards: Vec<Vec<u8>> = chunks
        .iter()
        .enumerate()
        .map(|(x, c)| {
            let mut s = vec![0u8; blocksize];
            let nv = MAX_RTP_HEADER_LEN;
            s[nv + 4..nv + 8].copy_from_slice(&frame.to_le_bytes());
            let mut flags = FLAG_CONTAINS_PIC_DATA;
            if x == 0 {
                flags |= FLAG_SOF;
            }
            if x == ds - 1 {
                flags |= FLAG_EOF;
            }
            s[nv + 8] = flags;
            s[nv + 10] = 0x10;
            s[SHARD_HEADER_LEN..SHARD_HEADER_LEN + c.len()].copy_from_slice(c);
            s
        })
        .collect();
    shards.extend((0..ps).map(|_| vec![0u8; blocksize]));
    fec::encode(&mut shards, ds, ps).unwrap();
    for (x, s) in shards.iter_mut().enumerate() {
        let nv = MAX_RTP_HEADER_LEN;
        let fec_info = (x << 12 | ds << 22 | pct << 4) as u32;
        s[nv + 12..nv + 16].copy_from_slice(&fec_info.to_le_bytes());
        s[0] = 0x80 | FLAG_EXTENSION;
        s[2..4].copy_from_slice(&seq_start.wrapping_add(x as u16).to_be_bytes());
        s[nv + 4..nv + 8].copy_from_slice(&frame.to_le_bytes());
    }
    shards
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKET_SIZE: usize = 1392;

    fn h264_frame(len: usize, idr: bool) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, if idr { 0x65 } else { 0x41 }];
        v.extend((0..len).map(|i| (i * 7 + 3) as u8 | 1));
        v
    }

    #[test]
    fn reassembles_with_losses() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let data = h264_frame(10_000, true);
        let mut packets = packetize(1, &data, PACKET_SIZE, 20, 100, true);
        // Drop two data packets; 20% FEC on 8 shards gives 2 parity.
        packets.remove(3);
        packets.remove(0);
        packets.reverse(); // and deliver out of order
        let mut frames = Vec::new();
        for p in &packets {
            for o in asm.add(p) {
                if let AssemblerOutput::Frame(f) = o {
                    frames.push(f);
                }
            }
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, data);
        assert!(frames[0].keyframe);
        assert_eq!(asm.stats.recovered_packets, 2);
    }

    #[test]
    fn waits_for_idr_after_loss() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let mut seq = 0u16;
        let mut feed = |asm: &mut FrameAssembler, frame: u32, idr: bool, drop: usize| {
            let packets = packetize(frame, &h264_frame(4000, idr), PACKET_SIZE, 20, seq, idr);
            seq = seq.wrapping_add(packets.len() as u16);
            let mut out = Vec::new();
            for p in packets.iter().skip(drop) {
                out.extend(asm.add(p));
            }
            out
        };
        let out = feed(&mut asm, 1, true, 0);
        assert!(matches!(out[..], [AssemblerOutput::Frame(_)]));
        // Frame 2 loses too much to recover.
        assert!(feed(&mut asm, 2, false, 3).is_empty());
        // Frame 3 arriving reveals the loss; it's a P-frame so it's dropped.
        let out = feed(&mut asm, 3, false, 0);
        assert_eq!(out, vec![AssemblerOutput::NeedIdr]);
        assert_eq!(asm.stats.dropped_waiting_idr, 1);
        // The IDR gets through again.
        let out = feed(&mut asm, 4, true, 0);
        assert!(matches!(&out[..], [AssemblerOutput::Frame(f)] if f.keyframe && f.frame == 4));
    }

    #[test]
    fn exact_length_without_padding() {
        let data = h264_frame(1300 - 5, false); // ends exactly at a packet boundary region
        let packets = packetize(7, &data, PACKET_SIZE, 20, 0, false);
        let payloads: Vec<Vec<u8>> = packets
            .iter()
            .take(packets.len() - 1)
            .map(|p| p[SHARD_HEADER_LEN..].to_vec())
            .collect();
        let frame = depacketize(7, &payloads, VideoCodec::H264).unwrap();
        assert_eq!(frame.data, data);
        assert!(!frame.keyframe);
    }

    #[test]
    fn keyframe_detection() {
        assert!(is_keyframe_bitstream(&[0, 0, 0, 1, 0x67, 1, 2], VideoCodec::H264));
        assert!(!is_keyframe_bitstream(&[0, 0, 1, 0x41, 1], VideoCodec::H264));
        assert!(is_keyframe_bitstream(&[0, 0, 0, 1, 0x40, 1], VideoCodec::Hevc)); // VPS
        assert_eq!(ping_packet(Some(b"0123456789abcdef"), 2).len(), 20);
        assert_eq!(ping_packet(None, 1), b"PING");
    }
}
