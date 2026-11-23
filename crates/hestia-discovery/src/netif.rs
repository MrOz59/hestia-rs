//! This device's network interfaces: which addresses share a link with us,
//! which interfaces carry IPv6 (for link-local multicast), and the order of
//! preference between address families.

use std::net::IpAddr;

/// An address of this device and the length of its network's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalAddr {
    pub ip: IpAddr,
    pub prefix: u8,
    /// Interface index, the scope of IPv6 link-local addresses.
    pub interface: u32,
}

/// The addresses of every interface that is up, loopback aside. Empty where
/// the platform can't list them (then nothing counts as on-link).
pub fn local_addresses() -> Vec<LocalAddr> {
    sys::local_addresses()
}

/// Whether `ip` is on a network this device is attached to: the same subnet
/// as one of its addresses. VPNs such as Tailscale give the device a host
/// route (/32, /128), which covers no other address.
pub fn on_link(ip: IpAddr, locals: &[LocalAddr]) -> bool {
    let ip = canonical(ip);
    locals
        .iter()
        .any(|l| l.prefix > 0 && l.ip != ip && in_subnet(ip, l.ip, l.prefix))
}

/// Whether `ip` lies in `net/prefix`.
pub fn in_subnet(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (canonical(ip), canonical(net)) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let bits = u32::from(prefix.min(32));
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let bits = u32::from(prefix.min(128));
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// Indices of the interfaces with an IPv6 address, each once: where to send
/// link-local multicast (mDNS queries, Wake-on-LAN).
pub fn ipv6_interfaces(locals: &[LocalAddr]) -> Vec<u32> {
    let mut out = Vec::new();
    for l in locals {
        if l.ip.is_ipv6() && l.interface != 0 && !out.contains(&l.interface) {
            out.push(l.interface);
        }
    }
    out
}

/// Preference between address families, lower first: IPv6 unique-local,
/// other IPv6, then IPv4. RFC 6724's default table puts unique-local IPv6
/// below all of IPv4; this follows draft-ietf-6man-rfc6724-update instead,
/// where unique-local addresses on the local network come first.
pub fn family_rank(ip: IpAddr) -> u8 {
    match canonical(ip) {
        IpAddr::V6(v6) if (v6.segments()[0] & 0xfe00) == 0xfc00 => 0,
        IpAddr::V6(_) => 1,
        IpAddr::V4(_) => 2,
    }
}

/// IPv4-mapped IPv6 addresses as the IPv4 address they carry.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

#[cfg(unix)]
mod sys {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::LocalAddr;

    pub fn local_addresses() -> Vec<LocalAddr> {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: on success getifaddrs stores a list that is freed below.
        if unsafe { libc::getifaddrs(&mut list) } != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cursor = list;
        while !cursor.is_null() {
            // SAFETY: `cursor` walks the list getifaddrs returned.
            let entry = unsafe { &*cursor };
            cursor = entry.ifa_next;
            let flags = entry.ifa_flags as libc::c_int;
            if flags & libc::IFF_UP == 0 || flags & libc::IFF_LOOPBACK != 0 {
                continue;
            }
            // SAFETY: getifaddrs' address pointers are null or point at a
            // sockaddr of the family they name.
            let Some(ip) = (unsafe { ip_of(entry.ifa_addr) }) else {
                continue;
            };
            // SAFETY: as above, for the netmask of an address of `ip`'s family.
            let prefix = unsafe { prefix_of(entry.ifa_netmask, ip) };
            // SAFETY: `ifa_name` is the entry's NUL-terminated interface name.
            let interface = unsafe { libc::if_nametoindex(entry.ifa_name) };
            out.push(LocalAddr { ip, prefix, interface });
        }
        // SAFETY: `list` came from getifaddrs and is freed once.
        unsafe { libc::freeifaddrs(list) };
        out
    }

    unsafe fn ip_of(addr: *const libc::sockaddr) -> Option<IpAddr> {
        if addr.is_null() {
            return None;
        }
        // SAFETY: the caller passes a valid sockaddr; its family says which
        // larger struct it is.
        match unsafe { (*addr).sa_family } as libc::c_int {
            libc::AF_INET => {
                let sin = unsafe { &*addr.cast::<libc::sockaddr_in>() };
                Some(IpAddr::V4(Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes())))
            }
            libc::AF_INET6 => {
                let sin6 = unsafe { &*addr.cast::<libc::sockaddr_in6>() };
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }

    /// The netmask's prefix length, read as the address's own family: some
    /// platforms leave the netmask's family unset.
    unsafe fn prefix_of(mask: *const libc::sockaddr, ip: IpAddr) -> u8 {
        if mask.is_null() {
            return 0;
        }
        let bits = match ip {
            IpAddr::V4(_) => {
                // SAFETY: the caller passes the netmask of an IPv4 address.
                let sin = unsafe { &*mask.cast::<libc::sockaddr_in>() };
                sin.sin_addr.s_addr.count_ones()
            }
            IpAddr::V6(_) => {
                // SAFETY: the caller passes the netmask of an IPv6 address.
                let sin6 = unsafe { &*mask.cast::<libc::sockaddr_in6>() };
                sin6.sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum()
            }
        };
        bits as u8
    }
}

#[cfg(not(unix))]
mod sys {
    pub fn local_addresses() -> Vec<super::LocalAddr> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn local(addr: &str, prefix: u8, interface: u32) -> LocalAddr {
        LocalAddr {
            ip: ip(addr),
            prefix,
            interface,
        }
    }

    #[test]
    fn subnets() {
        assert!(in_subnet(ip("192.168.1.205"), ip("192.168.1.42"), 24));
        assert!(!in_subnet(ip("192.168.2.205"), ip("192.168.1.42"), 24));
        assert!(in_subnet(ip("10.9.8.7"), ip("10.0.0.1"), 8));
        assert!(in_subnet(ip("2001:db8:1:2::99"), ip("2001:db8:1:2::1"), 64));
        assert!(!in_subnet(ip("2001:db8:1:3::99"), ip("2001:db8:1:2::1"), 64));
        assert!(in_subnet(ip("::ffff:192.168.1.9"), ip("192.168.1.42"), 24));
        assert!(!in_subnet(ip("192.168.1.9"), ip("fd00::1"), 64));
        assert!(in_subnet(ip("8.8.8.8"), ip("1.1.1.1"), 0));
    }

    #[test]
    fn on_link_networks() {
        let locals = [
            local("192.168.1.42", 24, 2),
            local("2001:db8:1:2::42", 64, 2),
            local("fd12:3456::42", 64, 2),
            // Tailscale: host routes only.
            local("100.101.102.103", 32, 5),
            local("fd7a:115c:a1e0::1", 128, 5),
        ];
        for lan in ["192.168.1.10", "2001:db8:1:2::10", "fd12:3456::10"] {
            assert!(on_link(ip(lan), &locals), "{lan}");
        }
        for off in [
            "192.168.2.10",
            "100.101.102.104",
            "fd7a:115c:a1e0::2",
            "2001:db8:9::1",
            "8.8.8.8",
        ] {
            assert!(!on_link(ip(off), &locals), "{off}");
        }
        // Our own address isn't a host on the link.
        assert!(!on_link(ip("192.168.1.42"), &locals));
        assert!(!on_link(ip("192.168.1.10"), &[local("192.168.1.42", 0, 2)]));
    }

    #[test]
    fn interfaces_for_multicast() {
        let locals = [
            local("192.168.1.42", 24, 2),
            local("fe80::1", 64, 2),
            local("2001:db8::1", 64, 2),
            local("fe80::2", 64, 3),
            local("10.0.0.1", 8, 4),
        ];
        assert_eq!(ipv6_interfaces(&locals), [2, 3]);
    }

    #[test]
    fn families() {
        assert!(family_rank(ip("fd12::1")) < family_rank(ip("2001:db8::1")));
        assert!(family_rank(ip("2001:db8::1")) < family_rank(ip("192.168.1.1")));
        assert_eq!(family_rank(ip("::ffff:192.168.1.1")), family_rank(ip("192.168.1.1")));
    }

    #[test]
    fn lists_this_machine() {
        // Whatever the machine has, the list is consistent.
        for l in local_addresses() {
            assert!(!l.ip.is_loopback(), "{l:?}");
            assert!(l.prefix <= if l.ip.is_ipv4() { 32 } else { 128 }, "{l:?}");
        }
    }
}
