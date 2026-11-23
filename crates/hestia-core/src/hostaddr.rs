//! The addresses a host is known by.
//!
//! One PC is often reachable several ways: its LAN address, a VPN such as
//! Tailscale, a DNS name. Its LAN address can also change with a new DHCP
//! lease. The pairing (the pinned certificate) belongs to the host, which is
//! identified by its GameStream unique id, never to an address. So a host
//! record keeps every address the host has been reached at, and each check
//! asks all of them at once and keeps the best answer.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// How many addresses a host record keeps; the least recently used go first.
pub const MAX_ADDRESSES: usize = 8;

/// One way to reach a host's GameStream HTTP port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAddress {
    /// IP address (IPv6 without brackets) or DNS name.
    pub address: String,
    pub http_port: u16,
}

/// Where an address leads, best first: when several of a host's addresses
/// answer, the local network wins over a VPN, which wins over the internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddressKind {
    /// Private, link-local, unique-local or loopback IPs, `.local` names.
    Lan,
    /// Tailscale: the 100.64.0.0/10 shared range, its IPv6 prefix
    /// fd7a:115c:a1e0::/48 and MagicDNS `.ts.net` names.
    Vpn,
    /// Public IPs and other DNS names.
    Other,
}

impl HostAddress {
    pub fn new(address: impl Into<String>, http_port: u16) -> Self {
        Self {
            address: address.into(),
            http_port,
        }
    }

    /// Same address, whatever the port: a record has one entry per address.
    pub fn is(&self, address: &str) -> bool {
        self.address.eq_ignore_ascii_case(address)
    }

    pub fn kind(&self) -> AddressKind {
        let bare = self.address.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = bare.parse::<IpAddr>() {
            return ip_kind(ip);
        }
        let name = bare.trim_end_matches('.').to_ascii_lowercase();
        if name == "localhost" || name.ends_with(".local") {
            AddressKind::Lan
        } else if name.ends_with(".ts.net") {
            AddressKind::Vpn
        } else {
            AddressKind::Other
        }
    }
}

fn ip_kind(ip: IpAddr) -> AddressKind {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if v4.is_private() || v4.is_loopback() || v4.is_link_local() {
                AddressKind::Lan
            } else if a == 100 && (b & 0xc0) == 64 {
                AddressKind::Vpn
            } else {
                AddressKind::Other
            }
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_kind(IpAddr::V4(v4));
            }
            let s = v6.segments();
            if s[..3] == [0xfd7a, 0x115c, 0xa1e0] {
                AddressKind::Vpn
            } else if v6.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 {
                AddressKind::Lan
            } else {
                AddressKind::Other
            }
        }
    }
}

/// Whether an address learned from the network (mDNS) is worth remembering:
/// not unspecified or loopback, and not IPv6 link-local, which can't be
/// reached without the interface it was seen on.
pub fn worth_keeping(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_loopback() && !v4.is_broadcast(),
        IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

/// Puts `address` first in `list` (the address in use), adding it or
/// updating its port.
pub fn promote(list: &mut Vec<HostAddress>, address: HostAddress) {
    list.retain(|a| !a.is(&address.address));
    list.insert(0, address);
    list.truncate(MAX_ADDRESSES);
}

/// Adds `address` right after the address in use, or updates the port of the
/// entry it already has. Returns whether `list` changed.
pub fn remember(list: &mut Vec<HostAddress>, address: HostAddress) -> bool {
    if let Some(known) = list.iter_mut().find(|a| a.is(&address.address)) {
        let changed = known.http_port != address.http_port;
        known.http_port = address.http_port;
        return changed;
    }
    list.insert(list.len().min(1), address);
    list.truncate(MAX_ADDRESSES);
    true
}

/// Drops `address` (whatever its port).
pub fn forget(list: &mut Vec<HostAddress>, address: &str) {
    list.retain(|a| !a.is(address));
}

/// Appends the addresses of `other` that `list` lacks, after its own.
pub fn union(list: &mut Vec<HostAddress>, other: &[HostAddress]) {
    for a in other {
        if !list.iter().any(|k| k.is(&a.address)) {
            list.push(a.clone());
        }
    }
    list.truncate(MAX_ADDRESSES);
}

/// One address's answer to a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// Position in the record's address list (0 = the address in use).
    pub index: usize,
    pub kind: AddressKind,
    /// Came over HTTPS with the pinned certificate, so its pair status is real.
    pub verified: bool,
    /// Answered with the record's unique id (always, before the id is known).
    pub same_host: bool,
}

impl Answer {
    fn key(&self) -> (bool, bool, AddressKind, usize) {
        (!self.same_host, !self.verified, self.kind, self.index)
    }

    /// Nothing else is worth waiting for: this host, on the LAN, with a real
    /// pair status when we have one to check.
    pub fn is_ideal(&self, pinned: bool) -> bool {
        self.same_host && self.kind == AddressKind::Lan && (self.verified || !pinned)
    }
}

/// Which answer to use (an index into `answers`): one from this host,
/// verified first, then by [`AddressKind`], then in the record's order. An
/// answer with another unique id is only taken from the address in use, when
/// no other address answered as this host: the host was reinstalled and got a
/// new id, and the user is asked to pair again, as before.
pub fn pick(answers: &[Answer]) -> Option<usize> {
    answers
        .iter()
        .enumerate()
        .filter(|(_, a)| a.same_host || a.index == 0)
        .min_by_key(|(_, a)| a.key())
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(address: &str) -> HostAddress {
        HostAddress::new(address, 47989)
    }

    fn names(list: &[HostAddress]) -> Vec<&str> {
        list.iter().map(|a| a.address.as_str()).collect()
    }

    #[test]
    fn kinds() {
        for lan in [
            "192.168.0.10",
            "10.1.2.3",
            "172.20.0.5",
            "169.254.1.1",
            "fe80::1",
            "fd00::5",
            "cachy.local",
            "::ffff:192.168.0.10",
        ] {
            assert_eq!(a(lan).kind(), AddressKind::Lan, "{lan}");
        }
        for vpn in [
            "100.101.102.103",
            "100.64.0.1",
            "fd7a:115c:a1e0::1",
            "cachy.tail1234.ts.net",
            "[fd7a:115c:a1e0::2]",
        ] {
            assert_eq!(a(vpn).kind(), AddressKind::Vpn, "{vpn}");
        }
        for other in [
            "8.8.8.8",
            "100.128.0.1",
            "100.63.255.255",
            "2001:db8::1",
            "pc.example.com",
            "cachy",
        ] {
            assert_eq!(a(other).kind(), AddressKind::Other, "{other}");
        }
    }

    #[test]
    fn keeping() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(worth_keeping(ip("192.168.0.10")));
        assert!(worth_keeping(ip("100.101.102.103")));
        assert!(worth_keeping(ip("fd00::5")));
        assert!(!worth_keeping(ip("fe80::1")));
        assert!(!worth_keeping(ip("127.0.0.1")));
        assert!(!worth_keeping(ip("0.0.0.0")));
        assert!(!worth_keeping(ip("::")));
    }

    #[test]
    fn list_operations() {
        let mut list = vec![a("192.168.0.10")];
        assert!(remember(&mut list, a("100.101.102.103")));
        assert!(!remember(&mut list, a("100.101.102.103")));
        assert!(remember(&mut list, HostAddress::new("100.101.102.103", 48000)));
        assert_eq!(list[1].http_port, 48000);
        // New addresses go right after the one in use.
        assert!(remember(&mut list, a("PC.local")));
        assert_eq!(names(&list), ["192.168.0.10", "PC.local", "100.101.102.103"]);
        assert!(!remember(&mut list, a("pc.LOCAL")));

        promote(&mut list, a("100.101.102.103"));
        assert_eq!(names(&list), ["100.101.102.103", "192.168.0.10", "PC.local"]);
        assert_eq!(list[0].http_port, 47989);

        forget(&mut list, "pc.local");
        assert_eq!(names(&list), ["100.101.102.103", "192.168.0.10"]);

        union(&mut list, &[a("192.168.0.10"), a("192.168.0.11")]);
        assert_eq!(names(&list), ["100.101.102.103", "192.168.0.10", "192.168.0.11"]);

        let mut empty = Vec::new();
        assert!(remember(&mut empty, a("192.168.0.10")));
        assert_eq!(names(&empty), ["192.168.0.10"]);
    }

    #[test]
    fn bounded() {
        let mut list = vec![a("10.0.0.0")];
        for i in 1..20 {
            remember(&mut list, a(&format!("10.0.0.{i}")));
            promote(&mut list, a("10.0.0.0"));
        }
        assert_eq!(list.len(), MAX_ADDRESSES);
        assert_eq!(list[0].address, "10.0.0.0");
        // The newest stay; the oldest fell off the end.
        assert_eq!(list[1].address, "10.0.0.19");
        union(&mut list, &[a("10.0.1.1")]);
        assert_eq!(list.len(), MAX_ADDRESSES);
    }

    fn answer(index: usize, kind: AddressKind, verified: bool, same_host: bool) -> Answer {
        Answer {
            index,
            kind,
            verified,
            same_host,
        }
    }

    #[test]
    fn picking() {
        use AddressKind::*;
        assert_eq!(pick(&[]), None);

        // At home: the LAN beats Tailscale even when Tailscale is in use.
        let home = [answer(0, Vpn, true, true), answer(1, Lan, true, true)];
        assert_eq!(pick(&home), Some(1));

        // A verified answer beats one that fell back to plain HTTP.
        let busy = [answer(0, Lan, false, true), answer(1, Vpn, true, true)];
        assert_eq!(pick(&busy), Some(1));

        // Same kind: the record's order (the address in use first).
        let two_lans = [answer(2, Lan, true, true), answer(1, Lan, true, true)];
        assert_eq!(pick(&two_lans), Some(1));

        // An old LAN address now held by another PC never wins over this host.
        let reused = [answer(1, Lan, true, false), answer(2, Other, true, true)];
        assert_eq!(pick(&reused), Some(1));
        assert_eq!(pick(&[answer(1, Lan, false, false)]), None);

        // A new id on the address in use, and nothing else: reinstalled host.
        assert_eq!(pick(&[answer(0, Lan, false, false)]), Some(0));
        let reinstalled_but_seen = [answer(0, Lan, false, false), answer(1, Vpn, true, true)];
        assert_eq!(pick(&reinstalled_but_seen), Some(1));
    }

    #[test]
    fn ideal() {
        use AddressKind::*;
        assert!(answer(1, Lan, true, true).is_ideal(true));
        assert!(answer(1, Lan, false, true).is_ideal(false));
        assert!(!answer(1, Lan, false, true).is_ideal(true));
        assert!(!answer(0, Vpn, true, true).is_ideal(true));
        assert!(!answer(0, Lan, true, false).is_ideal(true));
    }
}
