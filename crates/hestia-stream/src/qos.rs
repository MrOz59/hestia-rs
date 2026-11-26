//! Mark the packets the client sends ahead of bulk traffic on the local
//! network.
//!
//! Input and control messages are small and each one is a wait somebody
//! feels; on Wi-Fi they queue behind whatever else the device uploads
//! (a sync, a backup, a video call). A Wi-Fi access point with WMM, the
//! device's own Wi-Fi driver and managed switches all pick a queue by the
//! packet's DSCP. Expedited Forwarding (46) is the one RFC 8325 maps to the
//! voice queue, which Wi-Fi serves first; Linux and Android follow it for
//! the device's own uplink. `SO_PRIORITY` puts the packets in the first band
//! of the device's own queueing too.
//!
//! Only toward private, link-local and loopback addresses: routers on the
//! internet reset the mark at best and drop marked packets at worst, and a
//! VPN does not carry it to the host.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};

/// Expedited Forwarding, in the TOS byte (DSCP 46 << 2).
pub const DSCP_EXPEDITED_FORWARDING: u8 = 46 << 2;

/// Marks `socket`'s packets toward `remote` for interactive traffic, when
/// `remote` is on the local network. Returns whether it marked them.
pub fn mark_interactive(socket: &UdpSocket, remote: &SocketAddr) -> io::Result<bool> {
    let ip = remote.ip().to_canonical();
    if !crate::is_private_address(ip) {
        return Ok(false);
    }
    sys::mark(socket, ip, DSCP_EXPEDITED_FORWARDING)?;
    Ok(true)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod sys {
    use std::os::fd::AsRawFd;

    use super::*;

    const SOL_SOCKET: i32 = 1;
    const SO_PRIORITY: i32 = 12;
    const IPPROTO_IP: i32 = 0;
    const IP_TOS: i32 = 1;
    const IPPROTO_IPV6: i32 = 41;
    const IPV6_TCLASS: i32 = 67;
    /// `TC_PRIO_INTERACTIVE`, the highest an unprivileged socket may ask.
    const PRIORITY_INTERACTIVE: i32 = 6;

    unsafe extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const std::ffi::c_void, len: u32) -> i32;
    }

    fn set(socket: &UdpSocket, level: i32, name: i32, value: i32) -> io::Result<()> {
        // SAFETY: setsockopt on a socket we own, with an i32 option value.
        let result = unsafe { setsockopt(socket.as_raw_fd(), level, name, (&raw const value).cast(), 4) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn mark(socket: &UdpSocket, remote: IpAddr, tos: u8) -> io::Result<()> {
        // The socket's family, not the remote's: an IPv6 socket talking to
        // an IPv4-mapped address takes the IPv4 option as well.
        match socket.local_addr()? {
            SocketAddr::V4(_) => set(socket, IPPROTO_IP, IP_TOS, i32::from(tos))?,
            SocketAddr::V6(_) => {
                set(socket, IPPROTO_IPV6, IPV6_TCLASS, i32::from(tos))?;
                if remote.is_ipv4() {
                    set(socket, IPPROTO_IP, IP_TOS, i32::from(tos))?;
                }
            }
        }
        // Best effort: the mark above is what the network reads.
        let _ = set(socket, SOL_SOCKET, SO_PRIORITY, PRIORITY_INTERACTIVE);
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod sys {
    use super::*;

    pub(super) fn mark(_socket: &UdpSocket, _remote: IpAddr, _tos: u8) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    fn option(socket: &UdpSocket, level: i32, name: i32) -> i32 {
        unsafe extern "C" {
            fn getsockopt(fd: i32, level: i32, name: i32, value: *mut std::ffi::c_void, len: *mut u32) -> i32;
        }
        let mut value: i32 = -1;
        let mut len: u32 = 4;
        // SAFETY: getsockopt on a socket we own, into an i32.
        assert_eq!(
            unsafe { getsockopt(socket.as_raw_fd(), level, name, (&raw mut value).cast(), &mut len) },
            0
        );
        value
    }

    #[test]
    fn packets_to_the_local_network_are_marked() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host: SocketAddr = "192.168.1.20:47999".parse().unwrap();
        assert!(mark_interactive(&socket, &host).unwrap());
        assert_eq!(option(&socket, 0, 1), i32::from(DSCP_EXPEDITED_FORWARDING));
        assert_eq!(option(&socket, 1, 12), 6);
    }

    #[test]
    fn packets_to_the_internet_are_not() {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        for host in ["203.0.113.7:47999", "100.64.0.9:47999", "[2001:db8::1]:47999"] {
            assert!(!mark_interactive(&socket, &host.parse().unwrap()).unwrap(), "{host}");
        }
        assert_eq!(option(&socket, 0, 1), 0);
    }

    #[test]
    fn ipv6_sockets_mark_their_traffic_class() {
        let Ok(socket) = UdpSocket::bind("[::1]:0") else {
            return; // no IPv6 on this machine
        };
        assert!(mark_interactive(&socket, &"[fe80::1]:47999".parse().unwrap()).unwrap());
        assert_eq!(option(&socket, 41, 67), i32::from(DSCP_EXPEDITED_FORWARDING));
    }
}
