//! A small ENet 1.3 client, wire-compatible with the ENet fork Sunshine,
//! Hermes and moonlight-common-c use for the control stream.
//!
//! Only what a single-peer client needs is implemented: connect/verify,
//! reliable and unsequenced sends with acknowledgements, retransmission with
//! ENet's RTT estimator, pings, in-order delivery per channel (including
//! reassembly of fragmented reliable packets) and disconnects. Bandwidth
//! throttling and compression are not used by these hosts.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

// Commands
const CMD_ACKNOWLEDGE: u8 = 1;
const CMD_CONNECT: u8 = 2;
const CMD_VERIFY_CONNECT: u8 = 3;
const CMD_DISCONNECT: u8 = 4;
const CMD_PING: u8 = 5;
const CMD_SEND_RELIABLE: u8 = 6;
const CMD_SEND_UNRELIABLE: u8 = 7;
const CMD_SEND_FRAGMENT: u8 = 8;
const CMD_SEND_UNSEQUENCED: u8 = 9;
const CMD_BANDWIDTH_LIMIT: u8 = 10;
const CMD_THROTTLE_CONFIGURE: u8 = 11;
const CMD_SEND_UNRELIABLE_FRAGMENT: u8 = 12;
const CMD_MASK: u8 = 0x0f;
const FLAG_ACKNOWLEDGE: u8 = 0x80;
const FLAG_UNSEQUENCED: u8 = 0x40;

// Header
const HEADER_FLAG_COMPRESSED: u16 = 1 << 14;
const HEADER_FLAG_SENT_TIME: u16 = 1 << 15;
const HEADER_SESSION_SHIFT: u16 = 12;
const HEADER_SESSION_MASK: u16 = 3 << 12;
const MAX_PEER_ID: u16 = 0xfff;

const SYSTEM_CHANNEL: u8 = 0xff;
/// Default MTU of the ENet fork used by Moonlight/Sunshine.
const DEFAULT_MTU: u32 = 900;
const MAX_WINDOW_SIZE: u32 = 65536;
const THROTTLE_INTERVAL: u32 = 5000;
const THROTTLE_ACCELERATION: u32 = 2;
const THROTTLE_DECELERATION: u32 = 2;
const DEFAULT_RTT: u32 = 500;
const PING_INTERVAL: u32 = 500;
/// Moonlight sets the peer timeout to 10 s (`enet_peer_timeout(peer, 2, 10000, 10000)`).
const TIMEOUT_LIMIT: u32 = 2;
const TIMEOUT_MINIMUM: u32 = 10_000;
const TIMEOUT_MAXIMUM: u32 = 10_000;
const MAX_PACKET_SIZE: usize = 32 * 1024 * 1024;
const COMMAND_HEADER_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Connected,
    Receive { channel: u8, data: Vec<u8> },
    Disconnected { data: u32 },
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Connecting,
    Connected,
    Disconnecting,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Reliable,
    Unsequenced,
}

struct SentCommand {
    channel: u8,
    seq: u16,
    command: u8,
    bytes: Vec<u8>,
    sent_time: u32,
    rto: u32,
    attempts: u32,
}

enum Incoming {
    Packet(Vec<u8>),
    Fragment {
        start: u16,
        number: u32,
        count: u32,
        offset: u32,
        total: u32,
        data: Vec<u8>,
    },
}

#[derive(Default)]
struct Channel {
    out_reliable: u16,
    out_unreliable: u16,
    in_reliable: u16,
    /// Out-of-order reliable commands keyed by sequence number.
    pending: BTreeMap<u16, Incoming>,
    /// Fragmented packet being reassembled: (start sequence, buffer, received count).
    assembling: Option<(u16, Vec<u8>, u32)>,
}

pub struct EnetClient {
    socket: UdpSocket,
    remote: SocketAddr,
    state: State,
    epoch: Instant,
    connect_id: u32,
    outgoing_peer_id: u16,
    incoming_session: u8,
    outgoing_session: u8,
    mtu: u32,
    channels: Vec<Channel>,
    system_out_seq: u16,
    unsequenced_group: u16,
    sent: Vec<SentCommand>,
    acks: Vec<(u8, u16, u16)>,
    rtt: u32,
    rtt_var: u32,
    have_rtt: bool,
    last_receive: u32,
    last_send: u32,
    earliest_timeout: u32,
    events: VecDeque<Event>,
    recv_buf: Vec<u8>,
}

impl std::fmt::Debug for EnetClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnetClient")
            .field("remote", &self.remote)
            .field("state", &self.state)
            .field("rtt", &self.rtt)
            .finish()
    }
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn command_size(cmd: u8) -> usize {
    match cmd {
        CMD_ACKNOWLEDGE => 8,
        CMD_CONNECT => 48,
        CMD_VERIFY_CONNECT => 44,
        CMD_DISCONNECT => 8,
        CMD_PING => 4,
        CMD_SEND_RELIABLE => 6,
        CMD_SEND_UNRELIABLE => 8,
        CMD_SEND_FRAGMENT => 24,
        CMD_SEND_UNSEQUENCED => 8,
        CMD_BANDWIDTH_LIMIT => 12,
        CMD_THROTTLE_CONFIGURE => 16,
        CMD_SEND_UNRELIABLE_FRAGMENT => 24,
        _ => 0,
    }
}

/// `a` comes before `b` in 16-bit sequence space.
fn seq_before(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) < 0
}

impl EnetClient {
    /// Connects and waits for the handshake to complete.
    pub fn connect(remote: SocketAddr, channel_count: usize, data: u32, timeout: Duration) -> io::Result<Self> {
        let bind: SocketAddr = match remote.ip() {
            IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;
        let channel_count = channel_count.clamp(1, 255);
        let mut connect_id = [0u8; 4];
        fill_random(&mut connect_id);

        let mut client = Self {
            socket,
            remote,
            state: State::Connecting,
            epoch: Instant::now(),
            connect_id: u32::from_ne_bytes(connect_id),
            outgoing_peer_id: MAX_PEER_ID,
            incoming_session: 0xff,
            outgoing_session: 0xff,
            mtu: DEFAULT_MTU,
            channels: (0..channel_count).map(|_| Channel::default()).collect(),
            system_out_seq: 0,
            unsequenced_group: 0,
            sent: Vec::new(),
            acks: Vec::new(),
            rtt: DEFAULT_RTT,
            rtt_var: 0,
            have_rtt: false,
            last_receive: 0,
            last_send: 0,
            earliest_timeout: 0,
            events: VecDeque::new(),
            recv_buf: vec![0u8; 4096],
        };

        let mut cmd = Vec::with_capacity(48);
        cmd.extend_from_slice(&[CMD_CONNECT | FLAG_ACKNOWLEDGE, SYSTEM_CHANNEL, 0, 0]);
        cmd.extend_from_slice(&0u16.to_be_bytes()); // our peer id
        cmd.push(0xff); // incoming session id
        cmd.push(0xff); // outgoing session id
        for v in [
            client.mtu,
            MAX_WINDOW_SIZE,
            channel_count as u32,
            0, // incoming bandwidth (unlimited)
            0, // outgoing bandwidth
            THROTTLE_INTERVAL,
            THROTTLE_ACCELERATION,
            THROTTLE_DECELERATION,
        ] {
            cmd.extend_from_slice(&v.to_be_bytes());
        }
        cmd.extend_from_slice(&client.connect_id.to_ne_bytes());
        cmd.extend_from_slice(&data.to_be_bytes());
        client.send_command(SYSTEM_CHANNEL, cmd, true)?;

        let deadline = Instant::now() + timeout;
        loop {
            for event in client.poll()? {
                match event {
                    Event::Connected => return Ok(client),
                    Event::Disconnected { .. } | Event::TimedOut => {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "ENet connect rejected",
                        ));
                    }
                    other => client.events.push_back(other),
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "ENet connect timed out"));
            }
            client.wait((deadline - now).min(Duration::from_millis(20)));
        }
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    pub fn round_trip(&self) -> (u32, u32) {
        (self.rtt, self.rtt_var)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// A handle to the socket for waiting on readability without holding
    /// the client (see [`wait_readable`]).
    pub fn socket_handle(&self) -> io::Result<UdpSocket> {
        let s = self.socket.try_clone()?;
        Ok(s)
    }

    fn now(&self) -> u32 {
        (self.epoch.elapsed().as_millis() as u32).max(1)
    }

    /// Sends one packet on `channel`.
    pub fn send(&mut self, channel: u8, data: &[u8], delivery: Delivery) -> io::Result<()> {
        if self.state != State::Connected {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "ENet peer not connected"));
        }
        let channel = if (channel as usize) < self.channels.len() {
            channel
        } else {
            0
        };
        if data.len() > u16::MAX as usize || data.len() + 8 + 4 > self.mtu as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control packet larger than the ENet MTU",
            ));
        }
        let mut cmd = Vec::with_capacity(8 + data.len());
        match delivery {
            Delivery::Reliable => {
                cmd.extend_from_slice(&[CMD_SEND_RELIABLE | FLAG_ACKNOWLEDGE, channel, 0, 0]);
                cmd.extend_from_slice(&(data.len() as u16).to_be_bytes());
            }
            Delivery::Unsequenced => {
                self.unsequenced_group = self.unsequenced_group.wrapping_add(1);
                cmd.extend_from_slice(&[CMD_SEND_UNSEQUENCED | FLAG_UNSEQUENCED, channel, 0, 0]);
                cmd.extend_from_slice(&self.unsequenced_group.to_be_bytes());
                cmd.extend_from_slice(&(data.len() as u16).to_be_bytes());
            }
        }
        cmd.extend_from_slice(data);
        self.send_command(channel, cmd, delivery == Delivery::Reliable)
    }

    /// Assigns sequence numbers, sends immediately (with pending acks) and
    /// tracks reliable commands for retransmission.
    fn send_command(&mut self, channel: u8, mut cmd: Vec<u8>, reliable: bool) -> io::Result<()> {
        let seq = if channel == SYSTEM_CHANNEL {
            self.system_out_seq = self.system_out_seq.wrapping_add(1);
            self.system_out_seq
        } else if reliable {
            let ch = &mut self.channels[channel as usize];
            ch.out_reliable = ch.out_reliable.wrapping_add(1);
            ch.out_unreliable = 0;
            ch.out_reliable
        } else {
            0
        };
        cmd[2..4].copy_from_slice(&seq.to_be_bytes());
        let now = self.now();
        if reliable {
            let rto = self.rtt + 4 * self.rtt_var;
            self.sent.push(SentCommand {
                channel,
                seq,
                command: cmd[0] & CMD_MASK,
                bytes: cmd.clone(),
                sent_time: now,
                rto,
                attempts: 1,
            });
        }
        self.transmit(&[cmd], reliable)
    }

    fn transmit(&mut self, commands: &[Vec<u8>], with_sent_time: bool) -> io::Result<()> {
        let now = self.now();
        let mut packet = Vec::with_capacity(self.mtu as usize);
        let mut peer_field = self.outgoing_peer_id;
        if self.outgoing_peer_id < MAX_PEER_ID {
            peer_field |= (self.outgoing_session as u16) << HEADER_SESSION_SHIFT;
        }
        if with_sent_time {
            peer_field |= HEADER_FLAG_SENT_TIME;
        }
        packet.extend_from_slice(&peer_field.to_be_bytes());
        if with_sent_time {
            packet.extend_from_slice(&((now & 0xffff) as u16).to_be_bytes());
        }
        // Piggyback pending acknowledgements.
        let mut acks = std::mem::take(&mut self.acks);
        let budget = (self.mtu as usize)
            .saturating_sub(commands.iter().map(Vec::len).sum::<usize>())
            .saturating_sub(packet.len());
        let fit = (budget / 8).min(acks.len());
        for (channel, seq, sent) in acks.drain(..fit) {
            packet.extend_from_slice(&ack_command(channel, seq, sent));
        }
        self.acks = acks;
        for c in commands {
            packet.extend_from_slice(c);
        }
        self.socket.send_to(&packet, self.remote)?;
        self.last_send = now;
        // Anything that didn't fit goes in its own datagrams.
        while !self.acks.is_empty() && self.outgoing_peer_id < MAX_PEER_ID {
            self.flush_acks()?;
        }
        Ok(())
    }

    fn flush_acks(&mut self) -> io::Result<()> {
        if self.acks.is_empty() || self.outgoing_peer_id == MAX_PEER_ID {
            return Ok(());
        }
        let mut packet = Vec::with_capacity(self.mtu as usize);
        let peer_field = self.outgoing_peer_id | ((self.outgoing_session as u16) << HEADER_SESSION_SHIFT);
        packet.extend_from_slice(&peer_field.to_be_bytes());
        let fit = ((self.mtu as usize - 2) / 8).min(self.acks.len());
        for (channel, seq, sent) in self.acks.drain(..fit) {
            packet.extend_from_slice(&ack_command(channel, seq, sent));
        }
        self.socket.send_to(&packet, self.remote)?;
        self.last_send = self.now();
        Ok(())
    }

    /// Receives everything pending, sends acks, retransmits, pings. Never blocks.
    pub fn poll(&mut self) -> io::Result<Vec<Event>> {
        loop {
            match self.socket.recv_from(&mut self.recv_buf) {
                Ok((len, from)) => {
                    if from != self.remote && !(from.ip() == self.remote.ip() && from.port() == self.remote.port()) {
                        continue;
                    }
                    let datagram = self.recv_buf[..len].to_vec();
                    self.handle_datagram(&datagram);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted) => continue,
                Err(e) => return Err(e),
            }
        }
        self.flush_acks()?;
        self.check_timeouts()?;
        if self.state == State::Connected && self.sent.is_empty() {
            let now = self.now();
            if now.wrapping_sub(self.last_receive) >= PING_INTERVAL && now.wrapping_sub(self.last_send) >= PING_INTERVAL
            {
                self.send_command(
                    SYSTEM_CHANNEL,
                    vec![CMD_PING | FLAG_ACKNOWLEDGE, SYSTEM_CHANNEL, 0, 0],
                    true,
                )?;
            }
        }
        Ok(self.events.drain(..).collect())
    }

    /// How long a waiting thread may sleep before [`poll`](Self::poll) has
    /// work to do (retransmission or ping).
    pub fn next_deadline(&self) -> Duration {
        let now = self.now();
        let mut wait = PING_INTERVAL.saturating_sub(now.wrapping_sub(self.last_send.max(self.last_receive)));
        for s in &self.sent {
            let due = s.sent_time.wrapping_add(s.rto);
            wait = wait.min(due.saturating_sub(now));
        }
        Duration::from_millis(wait.clamp(1, 100) as u64)
    }

    /// Blocks until data arrives or `timeout` elapses.
    pub fn wait(&self, timeout: Duration) {
        let _ = wait_readable(&self.socket, timeout);
    }

    fn check_timeouts(&mut self) -> io::Result<()> {
        let now = self.now();
        let mut resend = Vec::new();
        for s in self.sent.iter_mut() {
            if now.wrapping_sub(s.sent_time) < s.rto {
                continue;
            }
            if self.earliest_timeout == 0 || seq_time_before(s.sent_time, self.earliest_timeout) {
                self.earliest_timeout = s.sent_time;
            }
            let waited = now.wrapping_sub(self.earliest_timeout);
            if waited >= TIMEOUT_MAXIMUM
                || ((1u32 << (s.attempts - 1).min(31)) >= TIMEOUT_LIMIT && waited >= TIMEOUT_MINIMUM)
            {
                self.state = State::Disconnected;
                self.events.push_back(Event::TimedOut);
                return Ok(());
            }
            let base = self.rtt + self.rtt.min(4 * self.rtt_var.max(1));
            let base = base.min(TIMEOUT_MAXIMUM / 5);
            s.rto = base * s.attempts.clamp(1, TIMEOUT_LIMIT);
            s.attempts += 1;
            s.sent_time = now;
            resend.push(s.bytes.clone());
        }
        for cmd in resend {
            self.transmit(&[cmd], true)?;
        }
        Ok(())
    }

    fn handle_datagram(&mut self, d: &[u8]) {
        if d.len() < 2 {
            return;
        }
        let raw = be16(d, 0);
        let flags = raw & (HEADER_FLAG_COMPRESSED | HEADER_FLAG_SENT_TIME);
        let session = ((raw & HEADER_SESSION_MASK) >> HEADER_SESSION_SHIFT) as u8;
        let peer_id = raw & !(HEADER_FLAG_COMPRESSED | HEADER_FLAG_SENT_TIME | HEADER_SESSION_MASK);
        if flags & HEADER_FLAG_COMPRESSED != 0 {
            return; // never negotiated
        }
        // Packets for us carry our peer id (0) or MAX before verification.
        if peer_id != 0 && peer_id != MAX_PEER_ID {
            return;
        }
        if self.outgoing_peer_id < MAX_PEER_ID && self.incoming_session != 0xff && session != self.incoming_session {
            return;
        }
        let (sent_time, mut pos) = if flags & HEADER_FLAG_SENT_TIME != 0 {
            if d.len() < 4 {
                return;
            }
            (Some(be16(d, 2)), 4)
        } else {
            (None, 2)
        };

        while pos + COMMAND_HEADER_LEN <= d.len() {
            let cmd_byte = d[pos];
            let cmd = cmd_byte & CMD_MASK;
            let size = command_size(cmd);
            if size == 0 || pos + size > d.len() {
                break;
            }
            let channel = d[pos + 1];
            let seq = be16(d, pos + 2);
            let body = &d[pos..pos + size];
            let mut next = pos + size;
            let payload_len = match cmd {
                CMD_SEND_RELIABLE => be16(body, 4) as usize,
                CMD_SEND_UNRELIABLE | CMD_SEND_UNSEQUENCED => be16(body, 6) as usize,
                CMD_SEND_FRAGMENT | CMD_SEND_UNRELIABLE_FRAGMENT => be16(body, 6) as usize,
                _ => 0,
            };
            if next + payload_len > d.len() || payload_len > MAX_PACKET_SIZE {
                break;
            }
            let payload = &d[next..next + payload_len];
            next += payload_len;

            let was_connecting = self.state == State::Connecting;
            let ok = self.handle_command(cmd, channel, seq, body, payload);
            if !ok {
                break;
            }
            // Acknowledge reliable commands (including the host's DISCONNECT,
            // which it waits on). Before VERIFY_CONNECT we have no peer id.
            let ackable = !(was_connecting && cmd != CMD_VERIFY_CONNECT) && self.outgoing_peer_id < MAX_PEER_ID;
            if cmd_byte & FLAG_ACKNOWLEDGE != 0
                && ackable
                && let Some(sent) = sent_time
            {
                self.acks.push((channel, seq, sent));
            }
            pos = next;
        }
        self.last_receive = self.now();
    }

    fn handle_command(&mut self, cmd: u8, channel: u8, seq: u16, body: &[u8], payload: &[u8]) -> bool {
        match cmd {
            CMD_VERIFY_CONNECT => {
                if self.state != State::Connecting {
                    return true; // duplicate; still acked
                }
                let interval = be32(body, 28);
                let accel = be32(body, 32);
                let decel = be32(body, 36);
                let connect_id = u32::from_ne_bytes([body[40], body[41], body[42], body[43]]);
                if interval != THROTTLE_INTERVAL
                    || accel != THROTTLE_ACCELERATION
                    || decel != THROTTLE_DECELERATION
                    || connect_id != self.connect_id
                {
                    self.state = State::Disconnected;
                    self.events.push_back(Event::Disconnected { data: 0 });
                    return false;
                }
                self.remove_sent(SYSTEM_CHANNEL, 1);
                let channels = be32(body, 16) as usize;
                if channels >= 1 && channels < self.channels.len() {
                    self.channels.truncate(channels);
                }
                self.outgoing_peer_id = be16(body, 4);
                self.incoming_session = body[6];
                self.outgoing_session = body[7];
                let mtu = be32(body, 8).clamp(576, 4096);
                self.mtu = self.mtu.min(mtu);
                self.state = State::Connected;
                self.events.push_back(Event::Connected);
                true
            }
            CMD_ACKNOWLEDGE => {
                self.handle_ack(channel, body);
                true
            }
            CMD_DISCONNECT => {
                if self.state != State::Disconnected {
                    let data = be32(body, 4);
                    self.state = State::Disconnected;
                    self.events.push_back(Event::Disconnected { data });
                }
                true
            }
            CMD_PING | CMD_BANDWIDTH_LIMIT | CMD_THROTTLE_CONFIGURE => true,
            CMD_SEND_RELIABLE => {
                self.queue_reliable(channel, seq, Incoming::Packet(payload.to_vec()));
                true
            }
            CMD_SEND_FRAGMENT => {
                let fragment = Incoming::Fragment {
                    start: be16(body, 4),
                    count: be32(body, 8),
                    number: be32(body, 12),
                    total: be32(body, 16),
                    offset: be32(body, 20),
                    data: payload.to_vec(),
                };
                self.queue_reliable(channel, seq, fragment);
                true
            }
            CMD_SEND_UNRELIABLE | CMD_SEND_UNSEQUENCED => {
                if self.state == State::Connected && (channel as usize) < self.channels.len() {
                    self.events.push_back(Event::Receive {
                        channel,
                        data: payload.to_vec(),
                    });
                }
                true
            }
            // Unreliable fragments are not used by these hosts; drop them.
            CMD_SEND_UNRELIABLE_FRAGMENT => true,
            _ => false,
        }
    }

    fn handle_ack(&mut self, channel: u8, body: &[u8]) {
        let now = self.now();
        let received_seq = be16(body, 4);
        let sent16 = be16(body, 6) as u32;
        let mut sent = sent16 | (now & 0xffff_0000);
        if (sent & 0x8000) > (now & 0x8000) {
            sent = sent.wrapping_sub(0x10000);
        }
        if !seq_time_before(now, sent) {
            let rtt = now.wrapping_sub(sent).max(1);
            if self.have_rtt {
                self.rtt_var -= self.rtt_var.div_ceil(4);
                if rtt >= self.rtt {
                    let diff = rtt - self.rtt;
                    self.rtt_var += diff.div_ceil(4);
                    self.rtt += diff.div_ceil(8);
                } else {
                    let diff = self.rtt - rtt;
                    self.rtt_var += diff.div_ceil(4);
                    self.rtt -= diff.div_ceil(8);
                }
            } else {
                self.rtt = rtt;
                self.rtt_var = rtt.div_ceil(2);
                self.have_rtt = true;
            }
        }
        self.earliest_timeout = 0;
        if let Some(command) = self.remove_sent(channel, received_seq)
            && command == CMD_DISCONNECT
            && self.state == State::Disconnecting
        {
            self.state = State::Disconnected;
            self.events.push_back(Event::Disconnected { data: 0 });
        }
    }

    fn remove_sent(&mut self, channel: u8, seq: u16) -> Option<u8> {
        let idx = self.sent.iter().position(|s| s.channel == channel && s.seq == seq)?;
        Some(self.sent.remove(idx).command)
    }

    fn queue_reliable(&mut self, channel: u8, seq: u16, item: Incoming) {
        if self.state != State::Connected || channel as usize >= self.channels.len() {
            return;
        }
        let ch = &mut self.channels[channel as usize];
        let expected = ch.in_reliable.wrapping_add(1);
        if seq_before(seq, expected) {
            return; // duplicate
        }
        if seq.wrapping_sub(expected) > 0x1000 * 7 {
            return; // far outside the reliable window
        }
        ch.pending.entry(seq).or_insert(item);
        // Deliver everything that is now contiguous.
        loop {
            let next = ch.in_reliable.wrapping_add(1);
            let Some(item) = ch.pending.remove(&next) else { break };
            ch.in_reliable = next;
            match item {
                Incoming::Packet(data) => self.events.push_back(Event::Receive { channel, data }),
                Incoming::Fragment {
                    start,
                    number,
                    count,
                    offset,
                    total,
                    data,
                } => {
                    if total as usize > MAX_PACKET_SIZE || count == 0 || number >= count {
                        ch.assembling = None;
                        continue;
                    }
                    let fresh = !matches!(ch.assembling, Some((s, _, _)) if s == start);
                    if fresh {
                        ch.assembling = Some((start, vec![0u8; total as usize], 0));
                    }
                    if let Some((_, buf, got)) = ch.assembling.as_mut() {
                        let off = offset as usize;
                        if off < buf.len() {
                            let n = data.len().min(buf.len() - off);
                            buf[off..off + n].copy_from_slice(&data[..n]);
                        }
                        *got += 1;
                        if *got >= count {
                            let (_, buf, _) = ch.assembling.take().unwrap_or_default();
                            self.events.push_back(Event::Receive { channel, data: buf });
                        }
                    }
                }
            }
        }
    }

    /// Graceful disconnect: sends DISCONNECT and waits up to `linger` for the ack.
    pub fn disconnect(&mut self, linger: Duration) {
        if self.state != State::Connected {
            return;
        }
        let mut cmd = vec![CMD_DISCONNECT | FLAG_ACKNOWLEDGE, SYSTEM_CHANNEL, 0, 0];
        cmd.extend_from_slice(&0u32.to_be_bytes());
        self.state = State::Disconnecting;
        if self.send_command(SYSTEM_CHANNEL, cmd, true).is_err() {
            self.state = State::Disconnected;
            return;
        }
        let deadline = Instant::now() + linger;
        while self.state == State::Disconnecting && Instant::now() < deadline {
            if self.poll().is_err() {
                break;
            }
            self.wait(Duration::from_millis(10));
        }
        self.state = State::Disconnected;
    }
}

fn seq_time_before(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) >= 86_400_000
}

fn ack_command(channel: u8, seq: u16, sent_time: u16) -> [u8; 8] {
    let s = seq.to_be_bytes();
    let t = sent_time.to_be_bytes();
    [CMD_ACKNOWLEDGE, channel, s[0], s[1], s[0], s[1], t[0], t[1]]
}

/// Waits until `socket` is readable or `timeout` passes, without consuming data.
pub fn wait_readable(socket: &UdpSocket, timeout: Duration) -> io::Result<bool> {
    let probe = socket.try_clone()?;
    probe.set_nonblocking(false)?;
    probe.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
    let mut byte = [0u8; 1];
    let result = match probe.peek_from(&mut byte) {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(false),
        // A message larger than the probe buffer still means "readable".
        Err(e) if e.raw_os_error().is_some() && e.kind() == io::ErrorKind::Other => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(true),
        Err(e) => Err(e),
    };
    // try_clone shares the file description, so restore non-blocking mode.
    socket.set_nonblocking(true)?;
    result
}

/// Best-effort randomness without pulling in a dependency: hashes time and
/// addresses through the std SipHash-based RandomState.
pub(crate) fn fill_random(out: &mut [u8]) {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(Instant::now().elapsed().as_nanos());
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_ordering() {
        assert!(seq_before(1, 2));
        assert!(seq_before(0xffff, 0));
        assert!(!seq_before(2, 1));
        assert_eq!(ack_command(3, 0x0102, 0x0a0b), [1, 3, 1, 2, 1, 2, 0x0a, 0x0b]);
        assert_eq!(command_size(CMD_CONNECT), 48);
    }
}
