//! Which network should carry traffic to a host.
//!
//! Android sends an app's traffic over the *default* network. When the
//! Wi-Fi has no internet access, the default network becomes mobile data,
//! and connections to a LAN host (192.168.x.x) go out over 4G/5G and time
//! out, even though the phone is on the host's Wi-Fi. So before connecting
//! to a local address we look for the Wi-Fi/Ethernet network whose subnet
//! holds it and, if that isn't the default network, route the app through it.
//! When the default is a VPN, Android handles routing and split-tunnel rules.

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A connected network, as Android's ConnectivityManager reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetInfo {
    /// Wi-Fi, Ethernet… (not mobile data, not a VPN).
    pub local_transport: bool,
    /// The system's default network.
    pub active: bool,
    /// Interface addresses with their prefix lengths.
    pub addresses: Vec<(IpAddr, u8)>,
}

/// Private, link-local or unique-local: only reachable on a local network.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Whether `ip` lies in `net/prefix`.
pub fn in_subnet(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    fn masked(bytes: &[u8], prefix: usize) -> Vec<u8> {
        bytes
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let keep = prefix.saturating_sub(i * 8).min(8);
                if keep == 0 { 0 } else { b & (0xffu8 << (8 - keep)) }
            })
            .collect()
    }
    let (a, b) = match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => (a.octets().to_vec(), b.octets().to_vec()),
        (IpAddr::V6(a), IpAddr::V6(b)) => (a.octets().to_vec(), b.octets().to_vec()),
        _ => return false,
    };
    let prefix = usize::from(prefix).min(a.len() * 8);
    masked(&a, prefix) == masked(&b, prefix)
}

/// The address in Java's `InetAddress.getAddress()` bytes.
pub fn ip_from_bytes(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

/// The network to bind the app to for `target`: `Some(index)` when a local
/// network that isn't the default one holds it, `None` to follow the
/// system default (including a VPN whose local subnet overlaps Wi-Fi).
pub fn choose(target: IpAddr, networks: &[NetInfo], default_is_vpn: bool) -> Option<usize> {
    if default_is_vpn || !is_local(target) {
        return None;
    }
    let index = networks
        .iter()
        .position(|n| n.local_transport && n.addresses.iter().any(|&(net, prefix)| in_subnet(target, net, prefix)))?;
    (!networks[index].active).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn wifi(active: bool) -> NetInfo {
        NetInfo {
            local_transport: true,
            active,
            addresses: vec![(ip("192.168.1.42"), 24), (ip("fe80::1"), 64)],
        }
    }

    fn mobile(active: bool) -> NetInfo {
        NetInfo {
            local_transport: false,
            active,
            addresses: vec![(ip("10.120.3.7"), 30)],
        }
    }

    #[test]
    fn subnets() {
        assert!(in_subnet(ip("192.168.1.205"), ip("192.168.1.42"), 24));
        assert!(!in_subnet(ip("192.168.2.205"), ip("192.168.1.42"), 24));
        assert!(in_subnet(ip("10.1.2.3"), ip("10.200.0.1"), 8));
        assert!(in_subnet(ip("172.16.5.9"), ip("172.16.4.1"), 23));
        assert!(!in_subnet(ip("172.16.6.9"), ip("172.16.4.1"), 23));
        assert!(in_subnet(ip("fe80::abcd"), ip("fe80::1"), 64));
        assert!(!in_subnet(ip("192.168.1.205"), ip("fe80::1"), 64));
        assert!(in_subnet(ip("8.8.8.8"), ip("1.2.3.4"), 0));
    }

    #[test]
    fn local_addresses() {
        for local in [
            "192.168.1.205",
            "10.0.0.2",
            "172.20.1.1",
            "169.254.3.4",
            "fd12::1",
            "fe80::2",
        ] {
            assert!(is_local(ip(local)), "{local}");
        }
        for public in ["8.8.8.8", "100.101.102.103", "2001:db8::1"] {
            assert!(!is_local(ip(public)), "{public}");
        }
    }

    #[test]
    fn routes_lan_hosts_through_wifi_when_mobile_data_is_default() {
        // Wi-Fi without internet: mobile data is the default network.
        let nets = [mobile(true), wifi(false)];
        assert_eq!(choose(ip("192.168.1.205"), &nets, false), Some(1));
        // Public hosts keep the default route.
        assert_eq!(choose(ip("203.0.113.9"), &nets, false), None);
        // A LAN address no local network holds (e.g. behind a VPN): default.
        assert_eq!(choose(ip("10.8.0.1"), &nets, false), None);
        // Wi-Fi already default: nothing to do.
        assert_eq!(choose(ip("192.168.1.205"), &[mobile(false), wifi(true)], false), None);
        // Mobile data's own subnet never wins.
        assert_eq!(choose(ip("10.120.3.5"), &nets, false), None);
    }

    #[test]
    fn defers_to_default_vpn_even_when_wifi_matches_the_host_subnet() {
        // Android may omit the VPN from getAllNetworks. A matching Wi-Fi
        // subnet alone cannot tell the home network from an away network.
        let nets = [wifi(false)];
        assert_eq!(choose(ip("192.168.1.102"), &nets, true), None);
        assert_eq!(choose(ip("fd12::1"), &nets, true), None);
        assert_eq!(choose(ip("fe80::205"), &nets, true), None);
        assert_eq!(choose(ip("192.168.1.102"), &nets, false), Some(0));
    }

    #[test]
    fn java_address_bytes() {
        assert_eq!(ip_from_bytes(&[192, 168, 1, 205]), Some(ip("192.168.1.205")));
        assert_eq!(ip_from_bytes(&[0; 16]), Some(ip("::")));
        assert_eq!(ip_from_bytes(&[1, 2, 3]), None);
    }
}
