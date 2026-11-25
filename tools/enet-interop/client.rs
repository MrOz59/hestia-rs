#[path = "../../crates/hestia-stream/src/enet.rs"]
mod enet;
use enet::{Delivery, EnetClient, Event};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

fn main() {
    let port: u16 = std::env::args().nth(1).unwrap().parse().unwrap();
    let ip: IpAddr = std::env::args().nth(2).unwrap_or_else(|| "127.0.0.1".into()).parse().unwrap();
    let mut c = EnetClient::connect(SocketAddr::new(ip, port), 0x30, 0xDEADBEEF, Duration::from_secs(5)).expect("connect");
    println!("rust: connected");
    c.send(2, b"hello", Delivery::Reliable).unwrap();
    c.send(3, b"uns", Delivery::Unsequenced).unwrap();
    c.send(1, b"big", Delivery::Reliable).unwrap();
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut sent_bye = false;
    let mut disconnected = None;
    while Instant::now() < deadline && disconnected.is_none() {
        for e in c.poll().unwrap() {
            match e {
                Event::Receive { channel, data } => {
                    let shown = if data.len() > 40 { format!("<{} bytes>", data.len()) } else { String::from_utf8_lossy(&data).into_owned() };
                    println!("rust: recv ch={channel} {shown}");
                    if data.len() == 5000 {
                        assert!(data.iter().enumerate().all(|(i, &b)| b == b'a' + (i % 26) as u8), "fragment reassembly corrupt");
                    }
                    got.push((channel, data));
                }
                Event::Disconnected { data } => { disconnected = Some(data); }
                other => println!("rust: event {other:?}"),
            }
        }
        if got.len() >= 5 && !sent_bye {
            c.send(0, b"bye", Delivery::Reliable).unwrap();
            sent_bye = true;
        }
        c.wait(c.next_deadline());
    }
    println!("rust: rtt={:?} got={} disconnected={:?}", c.round_trip(), got.len(), disconnected);
    assert_eq!(disconnected, Some(42));
    assert!(got.iter().any(|(ch, d)| *ch == 2 && d == b"echo:hello"));
    assert!(got.iter().any(|(ch, d)| *ch == 3 && d == b"echo:uns"));
    assert!(got.iter().any(|(_, d)| d.len() == 5000));
    println!("rust: ALL OK");
}
