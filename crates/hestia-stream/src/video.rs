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
const REORDER_WINDOW: Duration = Duration::from_millis(6);
const MAX_FRAME_LOOKAHEAD: u32 = 2;

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
    pub received_at: Instant,
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
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    received_data: usize,
}

impl Block {
    fn new(info: PacketInfo) -> Self {
        let data_shards = usize::from(info.data_shards);
        let parity_shards = usize::from(info.parity_shards());
        Self {
            lowest_seq: info.seq.wrapping_sub(info.fec_index),
            data_shards,
            parity_shards,
            shards: vec![None; data_shards + parity_shards],
            received: 0,
            received_data: 0,
        }
    }

    fn complete(&self) -> bool {
        self.received >= self.data_shards
    }

    fn add(&mut self, info: PacketInfo, packet: &[u8], shard_len: usize) -> Result<usize, fec::FecError> {
        let index = usize::from(info.seq.wrapping_sub(self.lowest_seq));
        if self.complete() || index >= self.shards.len() || self.shards[index].is_some() {
            return Ok(0);
        }
        let mut shard = Vec::with_capacity(shard_len);
        shard.extend_from_slice(packet);
        shard.resize(shard_len, 0);
        self.shards[index] = Some(shard);
        self.received += 1;
        if index < self.data_shards {
            self.received_data += 1;
        }
        if !self.complete() || self.received_data == self.data_shards {
            return Ok(0);
        }
        fec::reconstruct(&mut self.shards, self.data_shards, self.parity_shards, shard_len)?;
        Ok(self.data_shards - self.received_data)
    }
}

struct FrameState {
    number: u32,
    received_at: Instant,
    blocks: [Option<Block>; 4],
    last_block: usize,
    result: Option<Option<AssembledFrame>>,
}

/// Reassembles frames from shards. Pure logic, no I/O. Newer frames may finish
/// first but are emitted in sequence while the preceding frame can still arrive.
pub struct FrameAssembler {
    shard_len: usize,
    codec: VideoCodec,
    next_frame: Option<u32>,
    pending: Vec<FrameState>,
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
            pending: Vec::with_capacity(MAX_FRAME_LOOKAHEAD as usize + 2),
            waiting_for_idr: true,
            stats: AssemblerStats::default(),
        }
    }

    pub fn shard_len(&self) -> usize {
        self.shard_len
    }

    /// Suppresses delta frames until the decoder has a fresh reference.
    pub fn wait_for_keyframe(&mut self) {
        self.waiting_for_idr = true;
    }

    pub fn add(&mut self, packet: &[u8]) -> Vec<AssemblerOutput> {
        self.add_at(packet, Instant::now())
    }

    fn add_at(&mut self, packet: &[u8], now: Instant) -> Vec<AssemblerOutput> {
        let mut out = self.expire(now);
        let Some(info) = parse_packet(packet) else {
            return out;
        };
        let total_shards = usize::from(info.data_shards) + usize::from(info.parity_shards());
        if info.data_shards == 0
            || total_shards > fec::MAX_SHARDS
            || usize::from(info.fec_index) >= total_shards
            || info.block > info.last_block
            || packet.len() > self.shard_len
        {
            return out;
        }

        let next = *self.next_frame.get_or_insert(info.frame);
        if frame_before(info.frame, next) {
            return out;
        }
        let index = self
            .pending
            .binary_search_by_key(&info.frame.wrapping_sub(next), |f| f.number.wrapping_sub(next))
            .unwrap_or_else(|index| {
                self.pending.insert(
                    index,
                    FrameState {
                        number: info.frame,
                        received_at: now,
                        blocks: Default::default(),
                        last_block: usize::from(info.last_block),
                        result: None,
                    },
                );
                index
            });
        let frame = &mut self.pending[index];
        if frame.result.is_none() && frame.last_block == usize::from(info.last_block) {
            let block = frame.blocks[usize::from(info.block)].get_or_insert_with(|| Block::new(info));
            match block.add(info, packet, self.shard_len) {
                Ok(recovered) => {
                    self.stats.recovered_packets += recovered as u64;
                    if frame.blocks[..=frame.last_block]
                        .iter()
                        .all(|b| b.as_ref().is_some_and(Block::complete))
                    {
                        let payloads: Vec<_> = frame
                            .blocks
                            .iter()
                            .flatten()
                            .flat_map(|block| block.shards[..block.data_shards].iter().flatten())
                            .map(|shard| &shard[SHARD_HEADER_LEN..])
                            .collect();
                        frame.result = Some(depacketize_at(frame.number, &payloads, self.codec, frame.received_at));
                        frame.blocks = Default::default();
                    }
                }
                Err(_) => frame.result = Some(None),
            }
        }

        self.drain(now, &mut out);
        out
    }

    fn expire(&mut self, now: Instant) -> Vec<AssemblerOutput> {
        let mut out = Vec::new();
        self.drain(now, &mut out);
        out
    }

    fn drain(&mut self, now: Instant, out: &mut Vec<AssemblerOutput>) {
        while let Some(first) = self.pending.first_mut() {
            let next = self.next_frame.expect("buffered frames have a sequence start");
            let number = first.number;
            if number == next
                && let Some(result) = first.result.take()
            {
                self.pending.remove(0);
                self.next_frame = Some(next.wrapping_add(1));
                match result {
                    Some(frame) if !self.waiting_for_idr || frame.keyframe => {
                        self.waiting_for_idr = false;
                        self.stats.frames += 1;
                        out.push(AssemblerOutput::Frame(frame));
                    }
                    Some(_) => self.stats.dropped_waiting_idr += 1,
                    None => self.mark_lost(1, out),
                }
                continue;
            }

            // Evidence of a newer frame starts the deadline. A slow sender
            // alone is never treated as packet loss.
            let future_index = if number == next { 1 } else { 0 };
            let Some(future) = self.pending.get(future_index..) else {
                break;
            };
            let Some(oldest_arrival) = future.iter().map(|frame| frame.received_at).min() else {
                break;
            };
            let newest = self.pending.last().expect("we have at least one future frame");
            if newest.number.wrapping_sub(next) <= MAX_FRAME_LOOKAHEAD
                && now.saturating_duration_since(oldest_arrival) < REORDER_WINDOW
            {
                break;
            }
            let skipped = if number == next {
                self.pending.remove(0);
                1
            } else {
                number.wrapping_sub(next)
            };
            self.next_frame = Some(next.wrapping_add(skipped));
            self.mark_lost(skipped, out);
        }
    }

    fn mark_lost(&mut self, count: u32, out: &mut Vec<AssemblerOutput>) {
        self.stats.lost_frames += u64::from(count);
        self.waiting_for_idr = true;
        if !matches!(out.last(), Some(AssemblerOutput::NeedIdr)) {
            out.push(AssemblerOutput::NeedIdr);
        }
    }
}

/// Joins shard payloads into an access unit and strips the frame header.
pub fn depacketize(frame: u32, payloads: &[Vec<u8>], codec: VideoCodec) -> Option<AssembledFrame> {
    depacketize_at(frame, payloads, codec, Instant::now())
}

fn depacketize_at(
    frame: u32,
    payloads: &[impl AsRef<[u8]>],
    codec: VideoCodec,
    received_at: Instant,
) -> Option<AssembledFrame> {
    let first = payloads.first()?.as_ref();
    if first.len() < 8 {
        return None;
    }
    let header_len = if first[0] == 0x01 { 8 } else { 44 };
    let frame_type = first[3];
    let latency = u16::from_le_bytes([first[1], first[2]]);
    let last_len = u16::from_le_bytes([first[4], first[5]]) as usize;
    let capacity = first.len();

    let total_len: usize = payloads.iter().map(|part| part.as_ref().len()).sum();
    if total_len < header_len {
        return None;
    }
    let mut data = Vec::with_capacity(total_len - header_len);
    data.extend_from_slice(first.get(header_len..)?);
    for part in &payloads[1..] {
        data.extend_from_slice(part.as_ref());
    }
    // Exact length: (packets - 1) full payloads + the last packet's length.
    if header_len == 8 && last_len > 0 && last_len <= capacity {
        let exact = (payloads.len() - 1) * capacity + last_len;
        if exact <= total_len && exact >= header_len {
            data.truncate(exact - header_len);
        }
    } else if codec != VideoCodec::Av1 {
        while data.last() == Some(&0) {
            data.pop();
        }
    }
    let keyframe = frame_type == 2 || is_keyframe_bitstream(&data, codec);
    Some(AssembledFrame {
        frame,
        data,
        keyframe,
        host_latency_100us: latency,
        received_at,
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

/// Receive buffer for the video socket. A 1080p keyframe arrives as a
/// burst of a few hundred packets; the default (~200 KB on Android) can't
/// hold one while the receive thread is busy, and the kernel drops what
/// doesn't fit, more than FEC can repair. Moonlight asks for ~3 MB too.
pub const VIDEO_RECV_BUFFER: usize = 4 << 20;

/// Asks the kernel for a `bytes` receive buffer (it may grant less, up to
/// `net.core.rmem_max`); returns what it actually reserved.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn set_recv_buffer(socket: &UdpSocket, bytes: usize) -> io::Result<usize> {
    use std::os::fd::AsRawFd;

    const SOL_SOCKET: i32 = 1;
    const SO_RCVBUF: i32 = 8;
    unsafe extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const std::ffi::c_void, len: u32) -> i32;
        fn getsockopt(fd: i32, level: i32, name: i32, value: *mut std::ffi::c_void, len: *mut u32) -> i32;
    }
    let fd = socket.as_raw_fd();
    let wanted = i32::try_from(bytes).unwrap_or(i32::MAX);
    // SAFETY: plain setsockopt/getsockopt on a socket we own, with a
    // correctly sized i32 option value.
    unsafe {
        if setsockopt(fd, SOL_SOCKET, SO_RCVBUF, (&raw const wanted).cast(), 4) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut got: i32 = 0;
        let mut len: u32 = 4;
        if getsockopt(fd, SOL_SOCKET, SO_RCVBUF, (&raw mut got).cast(), &mut len) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(got.max(0) as usize)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn set_recv_buffer(_socket: &UdpSocket, _bytes: usize) -> io::Result<usize> {
    Ok(0)
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
        // The decoder's requests go through the receive loop, which keeps
        // one IDR request in flight (see `IdrRequests`).
        let decoder_wants_idr = Arc::new(AtomicBool::new(false));
        {
            let flag = decoder_wants_idr.clone();
            sink.attach_keyframe_requester(KeyframeRequester::new(move || {
                flag.store(true, Ordering::Relaxed);
            }));
        }
        sink.setup(cfg.format, cfg.width, cfg.height, cfg.fps)
            .map_err(|e| io::Error::other(format!("video decoder setup failed: {e}")))?;
        let socket = bind_udp(&cfg.remote)?;
        match set_recv_buffer(&socket, VIDEO_RECV_BUFFER) {
            // Linux reports twice the usable size (bookkeeping included).
            Ok(got) => info!("video receive buffer: {} KiB", got / 1024),
            Err(e) => warn!("could not enlarge the video receive buffer: {e}"),
        }
        socket.set_read_timeout(Some(REORDER_WINDOW))?;
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
                    .spawn(move || receive_loop(socket, cfg, sink, feedback, stop, counters, decoder_wants_idr))?,
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
    decoder_wants_idr: Arc<AtomicBool>,
) {
    let mut assembler = FrameAssembler::new(cfg.packet_size, cfg.format.codec);
    let shard_len = assembler.shard_len();
    let mut buf = vec![0u8; shard_len + ENC_PREFIX_LEN + 64];
    let started = Instant::now();
    let mut got_data = false;
    let mut first_frame = false;
    let mut idr = IdrRequests::default();

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if decoder_wants_idr.swap(false, Ordering::Relaxed) {
            assembler.wait_for_keyframe();
            if idr.want(now) {
                let _ = feedback.send(VideoFeedback::NeedIdr);
            }
        }
        if idr.retry_due(now) {
            debug!("no keyframe yet; asking again");
            let _ = feedback.send(VideoFeedback::NeedIdr);
        }
        let outputs = match socket.recv_from(&mut buf) {
            Ok((len, from)) if from.ip() == cfg.remote.ip() => {
                let received_at = Instant::now();
                if !got_data {
                    got_data = true;
                    info!("first video packet after {:?}", started.elapsed());
                }
                counters.packets.fetch_add(1, Ordering::Relaxed);
                counters.bytes.fetch_add(len as u64, Ordering::Relaxed);

                if let Some(key) = &cfg.decrypt_key {
                    if len < ENC_PREFIX_LEN + RTP_HEADER_LEN {
                        continue;
                    }
                    let iv: [u8; 12] = buf[..12].try_into().unwrap_or_default();
                    let Some(plain) = gcm_decrypt(key, &iv, &buf[ENC_PREFIX_LEN..len], &buf[16..32]) else {
                        continue;
                    };
                    assembler.add_at(&plain, received_at)
                } else {
                    assembler.add_at(&buf[..len], received_at)
                }
            }
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if !got_data && started.elapsed() > FIRST_DATA_TIMEOUT {
                    warn!("no video traffic from the host");
                    let _ = feedback.send(VideoFeedback::NoVideo);
                    return;
                }
                assembler.expire(Instant::now())
            }
            // ConnectionReset: ICMP port unreachable; Interrupted: a signal
            // (Android's freezer when the app goes to the background).
            Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted) => continue,
            Err(e) => {
                warn!("video socket error: {e}");
                return;
            }
        };
        for output in outputs {
            match output {
                AssemblerOutput::NeedIdr => {
                    if idr.want(Instant::now()) {
                        let _ = feedback.send(VideoFeedback::NeedIdr);
                    }
                }
                AssemblerOutput::Frame(frame) => {
                    if !first_frame {
                        first_frame = true;
                        let _ = feedback.send(VideoFeedback::FirstFrame);
                    }
                    counters.frames.fetch_add(1, Ordering::Relaxed);
                    if frame.keyframe {
                        idr.keyframe_arrived();
                    }
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
                        received_at: frame.received_at,
                        host_latency: Some(Duration::from_micros(frame.host_latency_100us as u64 * 100)),
                    };
                    if sink.submit(encoded) == SubmitResult::NeedKeyframe {
                        debug!("decoder asked for a keyframe");
                        assembler.wait_for_keyframe();
                        if idr.want(Instant::now()) {
                            let _ = feedback.send(VideoFeedback::NeedIdr);
                        }
                        break;
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

/// Keyframe requests: one in flight at a time. Losses while waiting don't
/// ask again (each IDR is large; asking per lost frame floods a lossy link
/// with keyframes and makes the loss worse); if none arrives within
/// [`IdrRequests::RETRY`], ask once more.
#[derive(Debug, Default)]
struct IdrRequests {
    pending_since: Option<Instant>,
}

impl IdrRequests {
    const RETRY: Duration = Duration::from_millis(1000);

    /// A keyframe is needed: true if a request should go out now.
    fn want(&mut self, now: Instant) -> bool {
        match self.pending_since {
            Some(at) if now.duration_since(at) < Self::RETRY => false,
            _ => {
                self.pending_since = Some(now);
                true
            }
        }
    }

    /// The pending request went unanswered for too long.
    fn retry_due(&mut self, now: Instant) -> bool {
        match self.pending_since {
            Some(at) if now.duration_since(at) >= Self::RETRY => {
                self.pending_since = Some(now);
                true
            }
            _ => false,
        }
    }

    fn keyframe_arrived(&mut self) {
        self.pending_since = None;
    }
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
        let start = Instant::now();
        let mut feed = |asm: &mut FrameAssembler, frame: u32, idr: bool, drop: usize| {
            let packets = packetize(frame, &h264_frame(4000, idr), PACKET_SIZE, 20, seq, idr);
            seq = seq.wrapping_add(packets.len() as u16);
            let now = start + Duration::from_millis(u64::from(frame) * 16);
            let mut out = Vec::new();
            for p in packets.iter().skip(drop) {
                out.extend(asm.add_at(p, now));
            }
            out
        };
        assert!(matches!(feed(&mut asm, 1, true, 0)[..], [AssemblerOutput::Frame(_)]));
        assert!(feed(&mut asm, 2, false, 3).is_empty());
        // The newer frame is complete, but the missing frame gets a short grace period.
        assert!(feed(&mut asm, 3, false, 0).is_empty());
        let out = asm.expire(start + Duration::from_millis(48) + REORDER_WINDOW);
        assert_eq!(out, vec![AssemblerOutput::NeedIdr]);
        assert_eq!(asm.stats.lost_frames, 1);
        assert_eq!(asm.stats.dropped_waiting_idr, 1);
        let out = feed(&mut asm, 4, true, 0);
        assert!(matches!(&out[..], [AssemblerOutput::Frame(f)] if f.keyframe && f.frame == 4));
    }

    #[test]
    fn accepts_late_frame_before_its_deadline_across_sequence_wrap() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let start = Instant::now();
        let older = packetize(u32::MAX, &h264_frame(4000, true), PACKET_SIZE, 20, 100, true);
        let newer = packetize(0, &h264_frame(500, false), PACKET_SIZE, 20, 200, false);
        assert!(asm.add_at(&older[0], start).is_empty());
        for p in &newer {
            assert!(asm.add_at(p, start + Duration::from_millis(1)).is_empty());
        }
        let mut out = Vec::new();
        for p in &older[1..] {
            out.extend(asm.add_at(p, start + Duration::from_millis(2)));
        }
        assert!(matches!(
            &out[..],
            [AssemblerOutput::Frame(a), AssemblerOutput::Frame(b)]
                if a.frame == u32::MAX && a.received_at == start && b.frame == 0
        ));
        assert_eq!(asm.stats.lost_frames, 0);
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
    fn one_keyframe_request_in_flight() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut idr = IdrRequests::default();
        assert!(idr.want(t0), "first loss asks");
        assert!(!idr.want(ms(250)), "more losses while waiting don't");
        assert!(!idr.retry_due(ms(900)));
        assert!(idr.retry_due(ms(1000)), "unanswered: ask again");
        assert!(!idr.retry_due(ms(1500)));
        idr.keyframe_arrived();
        assert!(!idr.retry_due(ms(5000)), "nothing pending");
        assert!(idr.want(ms(5001)), "a new loss asks right away");
    }

    #[test]
    fn waiting_for_a_keyframe_keeps_statistics() {
        let mut a = FrameAssembler::new(1024, VideoCodec::H264);
        a.stats.lost_frames = 7;
        a.wait_for_keyframe();
        assert_eq!(a.stats.lost_frames, 7);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn enlarges_the_receive_buffer() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let before = set_recv_buffer(&socket, 4096).unwrap();
        let after = set_recv_buffer(&socket, 1 << 20).unwrap();
        assert!(after > before, "{before} -> {after}");
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
