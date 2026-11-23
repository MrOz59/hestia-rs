//! Wake-on-LAN: the "magic packet" (6 × 0xFF, then the MAC 16 times) sent
//! by UDP broadcast, as Moonlight does, to wake a sleeping host whose MAC
//! address an earlier `/serverinfo` told us. On IPv6 networks it also goes
//! to the all-nodes group, so IPv6-only LANs can wake hosts too.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};

/// The link-local all-nodes group.
const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

/// Ports Moonlight sends to: the classic WoL ports and GameStream's, which
/// some routers forward to the host.
pub const PORTS: [u16; 7] = [9, 7, 47998, 47999, 48000, 48002, 48010];

/// `aa:bb:cc:dd:ee:ff` (or with dashes) → bytes.
pub fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = mac.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (byte, part) in out.iter_mut().zip(parts) {
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    (out != [0; 6]).then_some(out)
}

pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&mac);
    }
    packet
}

/// Where to send: the global broadcast, plus the host's own address and
/// its /24 broadcast when it's an IPv4 address.
pub fn destinations(address: Option<IpAddr>) -> Vec<IpAddr> {
    let mut out = vec![IpAddr::V4(Ipv4Addr::BROADCAST)];
    if let Some(ip) = address {
        out.push(ip);
        if let IpAddr::V4(v4) = ip {
            let [a, b, c, _] = v4.octets();
            out.push(IpAddr::V4(Ipv4Addr::new(a, b, c, 255)));
        }
    }
    out.dedup();
    out
}

/// Sends the packet to every destination and port; returns how many went
/// out. `before_send` runs once per destination (Android uses it to pick
/// the Wi-Fi network for LAN addresses).
pub fn wake(mac: [u8; 6], address: Option<IpAddr>, before_send: impl Fn(&SocketAddr)) -> io::Result<usize> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let mut sent = 0;
    let mut last_err = None;
    for ip in destinations(address).into_iter().filter(IpAddr::is_ipv4) {
        before_send(&SocketAddr::new(ip, PORTS[0]));
        for port in PORTS {
            match socket.send_to(&packet, SocketAddr::new(ip, port)) {
                Ok(_) => sent += 1,
                Err(e) => last_err = Some(e),
            }
        }
    }
    match (sent, last_err) {
        (0, Some(e)) => Err(e),
        _ => Ok(sent),
    }
}

/// Sends the packet to the IPv6 all-nodes group on each interface (by
/// index). A sleeping network card matches the magic pattern in any frame,
/// and unlike unicast this needs no neighbour discovery, which a sleeping
/// host can't answer. Returns how many went out.
pub fn wake_ipv6(mac: [u8; 6], interfaces: &[u32]) -> io::Result<usize> {
    if interfaces.is_empty() {
        return Ok(0);
    }
    let socket = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))?;
    let packet = magic_packet(mac);
    let mut sent = 0;
    let mut last_err = None;
    for &interface in interfaces {
        for port in PORTS {
            match socket.send_to(&packet, SocketAddrV6::new(ALL_NODES, port, 0, interface)) {
                Ok(_) => sent += 1,
                Err(e) => last_err = Some(e),
            }
        }
    }
    match (sent, last_err) {
        (0, Some(e)) => Err(e),
        _ => Ok(sent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_ipv6_interface_sends_nothing() {
        assert_eq!(wake_ipv6([1, 2, 3, 4, 5, 6], &[]).unwrap(), 0);
    }

    #[test]
    fn macs() {
        assert_eq!(
            parse_mac("00:1A:2b:3c:4D:5e"),
            Some([0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e])
        );
        assert_eq!(
            parse_mac("00-1a-2b-3c-4d-5e"),
            Some([0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e])
        );
        assert_eq!(parse_mac("00:00:00:00:00:00"), None);
        assert_eq!(parse_mac("00:1a:2b:3c:4d"), None);
        assert_eq!(parse_mac("zz:1a:2b:3c:4d:5e"), None);
    }

    #[test]
    fn packet_layout() {
        let mac = [1, 2, 3, 4, 5, 6];
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xff; 6]);
        assert!(p[6..].chunks(6).all(|c| c == mac));
    }

    #[test]
    fn sends_to_broadcast_host_and_subnet() {
        let host: IpAddr = "192.168.1.205".parse().unwrap();
        let d = destinations(Some(host));
        assert_eq!(
            d,
            vec![
                IpAddr::V4(Ipv4Addr::BROADCAST),
                host,
                "192.168.1.255".parse::<IpAddr>().unwrap()
            ]
        );
        assert_eq!(destinations(None), vec![IpAddr::V4(Ipv4Addr::BROADCAST)]);
    }

    #[test]
    fn wakes_over_loopback() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = receiver.local_addr().unwrap().port();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        // Send straight to the receiver to check the bytes on the wire.
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mac = [0xde, 0xad, 0xbe, 0xef, 0, 1];
        sender.send_to(&magic_packet(mac), ("127.0.0.1", port)).unwrap();
        let mut buf = [0u8; 200];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &magic_packet(mac)[..]);
    }
}
