#[path = "../../crates/hestia-stream/src/enet.rs"]
mod enet;
use enet::{Delivery, EnetClient, Event};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

// UDP relay on 127.0.0.1:proxy that drops every 3rd datagram in each direction.
fn proxy(listen: u16, target: u16) {
    let sock = UdpSocket::bind(("127.0.0.1", listen)).unwrap();
    let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
    upstream.connect(("127.0.0.1", target)).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
    upstream.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
    let mut client = None;
    let (mut n_up, mut n_down) = (0u32, 0u32);
    let mut buf = [0u8; 9000];
    loop {
        if let Ok((len, from)) = sock.recv_from(&mut buf) {
            client = Some(from);
            n_up += 1;
            if n_up % 3 != 0 { let _ = upstream.send(&buf[..len]); }
        }
        if let Ok(len) = upstream.recv(&mut buf) {
            n_down += 1;
            if let Some(c) = client { if n_down % 3 != 0 { let _ = sock.send_to(&buf[..len], c); } }
        }
    }
}

fn main() {
    let target: u16 = std::env::args().nth(1).unwrap().parse().unwrap();
    std::thread::spawn(move || proxy(41555, target));
    std::thread::sleep(Duration::from_millis(100));
    let mut c = EnetClient::connect("127.0.0.1:41555".parse().unwrap(), 0x30, 7, Duration::from_secs(10)).expect("connect through lossy proxy");
    println!("rust: connected through lossy link");
    for i in 0..20 { c.send(2, format!("m{i:02}").as_bytes(), Delivery::Reliable).unwrap(); }
    let mut echoes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && echoes.len() < 20 {
        for e in c.poll().unwrap() {
            if let Event::Receive { channel: 2, data } = e {
                if data.starts_with(b"echo:") { echoes.push(String::from_utf8(data).unwrap()); }
            }
        }
        c.wait(c.next_deadline());
    }
    let expected: Vec<String> = (0..20).map(|i| format!("echo:m{i:02}")).collect();
    assert_eq!(echoes, expected, "reliable messages must arrive once and in order");
    println!("rust: 20/20 reliable echoes in order, rtt={:?}", c.round_trip());
    c.disconnect(Duration::from_secs(3));
    println!("rust: LOSSY OK");
}
