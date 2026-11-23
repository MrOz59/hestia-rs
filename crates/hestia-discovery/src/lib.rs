//! LAN discovery of GameStream hosts (`_nvstream._tcp`) over mDNS.
//!
//! Uses "legacy unicast" one-shot queries (RFC 6762 §6.7): the query goes to
//! the mDNS multicast group from an ordinary ephemeral port, and responders
//! (Avahi, which Hermes/Sunshine publish through) answer by unicast. That
//! avoids binding port 5353 next to the system's own responder and, on
//! Android, receiving multicast — which would need a `WifiManager` multicast
//! lock. Queries go out over IPv4 and over IPv6 on every interface that has
//! it, so IPv6-only networks are browsed too.

pub mod dns;
pub mod netif;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dns::{RecordData, TYPE_A, TYPE_AAAA, TYPE_PTR, TYPE_SRV};
use log::{debug, warn};

pub const SERVICE_TYPE: &str = "_nvstream._tcp.local";
const MDNS_V4: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 251)), 5353);
const MDNS_V6_GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MDNS_PORT: u16 = 5353;

/// A host seen on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    /// Service instance name, usually the host's configured name.
    pub name: String,
    /// mDNS hostname (e.g. `cachy.local`).
    pub hostname: String,
    pub addresses: Vec<IpAddr>,
    /// GameStream HTTP port.
    pub port: u16,
}

/// Browses until dropped (or [`Browser::stop`]), sending each newly resolved
/// or changed host on the channel.
pub struct Browser {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Browser {
    pub fn start() -> io::Result<(Self, Receiver<DiscoveredHost>)> {
        let v4 = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        v4.set_multicast_ttl_v4(255).ok();
        // Without IPv6 (disabled, or no stack) browse over IPv4 alone.
        let v6 = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
            .inspect_err(|e| debug!("mDNS over IPv6 unavailable: {e}"))
            .ok();
        let sockets = Sockets { v4, v6 };
        sockets.set_read_timeout(Duration::from_millis(250))?;
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("hestia-mdns".into())
                .spawn(move || run(sockets, tx, stop))?
        };
        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            rx,
        ))
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One-off scan: browses for `duration` and returns everything found.
pub fn scan(duration: Duration) -> io::Result<Vec<DiscoveredHost>> {
    let (browser, rx) = Browser::start()?;
    let deadline = Instant::now() + duration;
    let mut found: HashMap<String, DiscoveredHost> = HashMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(host) => {
                found.insert(host.name.clone(), host);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    browser.stop();
    let mut hosts: Vec<_> = found.into_values().collect();
    hosts.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(hosts)
}

#[derive(Default)]
struct Cache {
    /// instance → (target host, port)
    srv: HashMap<String, (String, u16)>,
    /// hostname → addresses
    addrs: HashMap<String, Vec<IpAddr>>,
    instances: Vec<String>,
    reported: HashMap<String, DiscoveredHost>,
}

/// The query sockets: IPv4, and IPv6 when the system has it.
struct Sockets {
    v4: UdpSocket,
    v6: Option<UdpSocket>,
}

impl Sockets {
    /// Each socket waits a share of `total`, so a pass over them all takes
    /// about as long as one socket would.
    fn set_read_timeout(&self, total: Duration) -> io::Result<()> {
        let share = total / if self.v6.is_some() { 2 } else { 1 };
        self.v4.set_read_timeout(Some(share))?;
        if let Some(v6) = &self.v6 {
            v6.set_read_timeout(Some(share))?;
        }
        Ok(())
    }

    fn iter(&self) -> impl Iterator<Item = &UdpSocket> {
        std::iter::once(&self.v4).chain(&self.v6)
    }

    /// Sends a query to the IPv4 group and, on every interface with IPv6,
    /// to the link-local IPv6 group (the scope id picks the interface).
    fn send(&self, packet: &[u8]) {
        if let Err(e) = self.v4.send_to(packet, MDNS_V4) {
            debug!("mDNS query send failed: {e}");
        }
        let Some(v6) = &self.v6 else { return };
        for interface in netif::ipv6_interfaces(&netif::local_addresses()) {
            let group = SocketAddrV6::new(MDNS_V6_GROUP, MDNS_PORT, 0, interface);
            if let Err(e) = v6.send_to(packet, group) {
                debug!("mDNS query send failed on interface {interface}: {e}");
            }
        }
    }
}

fn run(sockets: Sockets, tx: Sender<DiscoveredHost>, stop: Arc<AtomicBool>) {
    // Query quickly at first, then back off.
    let schedule = [0u64, 1, 3, 7];
    let started = Instant::now();
    let mut next_query = 0usize;
    let mut last_periodic = Instant::now();
    let mut cache = Cache::default();
    let mut buf = [0u8; 9000];
    let mut id: u16 = 0x4853; // "HS"

    while !stop.load(Ordering::Relaxed) {
        let elapsed = started.elapsed().as_secs();
        let due = if next_query < schedule.len() {
            elapsed >= schedule[next_query]
        } else {
            last_periodic.elapsed() >= Duration::from_secs(15)
        };
        if due {
            id = id.wrapping_add(1);
            sockets.send(&dns::build_query(id, &[(SERVICE_TYPE, TYPE_PTR)]));
            // Ask again for anything we know about but couldn't resolve yet.
            for instance in &cache.instances {
                match cache.srv.get(instance) {
                    None => sockets.send(&dns::build_query(id, &[(instance.as_str(), TYPE_SRV)])),
                    Some((target, _)) if !cache.addrs.contains_key(target) => sockets.send(&dns::build_query(
                        id,
                        &[(target.as_str(), TYPE_A), (target.as_str(), TYPE_AAAA)],
                    )),
                    _ => {}
                }
            }
            if next_query < schedule.len() {
                next_query += 1;
            }
            last_periodic = Instant::now();
        }

        let mut received = false;
        for socket in sockets.iter() {
            let (len, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    warn!("mDNS receive failed: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            };
            let Some(records) = dns::parse_response(&buf[..len]) else {
                continue;
            };
            debug!("mDNS: {} records from {from}", records.len());
            absorb(&mut cache, records, from.ip());
            received = true;
        }
        if !received {
            continue;
        }

        for instance in cache.instances.clone() {
            let Some((target, port)) = cache.srv.get(&instance).cloned() else {
                continue;
            };
            let Some(addresses) = cache.addrs.get(&target).cloned() else {
                continue;
            };
            let host = DiscoveredHost {
                name: instance_label(&instance),
                hostname: target.clone(),
                addresses,
                port,
            };
            if cache.reported.get(&instance) != Some(&host) {
                cache.reported.insert(instance.clone(), host.clone());
                if tx.send(host).is_err() {
                    return; // receiver gone
                }
            }
        }
    }
}

fn absorb(cache: &mut Cache, records: Vec<dns::Record>, source: IpAddr) {
    let service = SERVICE_TYPE.to_ascii_lowercase();
    for record in records {
        let name = record.name.to_ascii_lowercase();
        match record.data {
            RecordData::Ptr(instance) if name == service => {
                let key = instance.to_ascii_lowercase();
                if !cache.instances.contains(&key) {
                    cache.instances.push(key);
                }
            }
            RecordData::Srv { port, target, .. } => {
                cache.srv.insert(name.clone(), (target.to_ascii_lowercase(), port));
                if !cache.instances.contains(&name) && name.ends_with(&service) {
                    cache.instances.push(name);
                }
            }
            RecordData::A(ip) => push_addr(cache, name, IpAddr::V4(ip)),
            // Link-local IPv6 needs a scope id we don't track; skip it.
            RecordData::Aaaa(ip) if !is_link_local_v6(IpAddr::V6(ip)) => push_addr(cache, name, IpAddr::V6(ip)),
            _ => {}
        }
    }
    // A responder that only answered SRV still told us where it lives
    // (unless that's an IPv6 link-local address, useless without its scope).
    if source.is_unspecified() || is_link_local_v6(source) {
        return;
    }
    for (target, _) in cache.srv.values() {
        if !cache.addrs.contains_key(target) {
            cache.addrs.entry(target.clone()).or_default();
        }
    }
    for (target, addrs) in cache.addrs.iter_mut() {
        if addrs.is_empty() && cache.srv.values().any(|(t, _)| t == target) {
            addrs.push(source);
        }
    }
}

fn push_addr(cache: &mut Cache, name: String, ip: IpAddr) {
    let list = cache.addrs.entry(name).or_default();
    if !list.contains(&ip) {
        // Preferred family first (IPv6 unique-local, other IPv6, IPv4).
        let at = list
            .iter()
            .position(|known| netif::family_rank(*known) > netif::family_rank(ip))
            .unwrap_or(list.len());
        list.insert(at, ip);
    }
}

fn is_link_local_v6(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
}

fn instance_label(instance: &str) -> String {
    instance
        .strip_suffix(&format!(".{}", SERVICE_TYPE.to_ascii_lowercase()))
        .unwrap_or(instance)
        .replace("\\032", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(instance_label("cachy._nvstream._tcp.local"), "cachy");
        assert_eq!(instance_label("other"), "other");
    }

    #[test]
    fn srv_without_address_falls_back_to_source() {
        let mut cache = Cache::default();
        absorb(
            &mut cache,
            vec![dns::Record {
                name: "pc._nvstream._tcp.local".into(),
                ttl: 120,
                data: RecordData::Srv {
                    priority: 0,
                    weight: 0,
                    port: 47989,
                    target: "pc.local".into(),
                },
            }],
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
        );
        assert_eq!(cache.instances, vec!["pc._nvstream._tcp.local".to_string()]);
        assert_eq!(cache.addrs["pc.local"], vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))]);
    }

    fn srv(target: &str) -> dns::Record {
        dns::Record {
            name: "pc._nvstream._tcp.local".into(),
            ttl: 120,
            data: RecordData::Srv {
                priority: 0,
                weight: 0,
                port: 47989,
                target: target.into(),
            },
        }
    }

    fn record(name: &str, data: RecordData) -> dns::Record {
        dns::Record {
            name: name.into(),
            ttl: 120,
            data,
        }
    }

    #[test]
    fn link_local_ipv6_source_is_not_an_address() {
        let mut cache = Cache::default();
        absorb(&mut cache, vec![srv("pc.local")], "fe80::1".parse().unwrap());
        assert!(!cache.addrs.contains_key("pc.local"));
        absorb(&mut cache, vec![srv("pc.local")], "fd00::5".parse().unwrap());
        assert_eq!(cache.addrs["pc.local"], vec!["fd00::5".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn ipv6_first() {
        let mut cache = Cache::default();
        absorb(
            &mut cache,
            vec![
                srv("pc.local"),
                record("pc.local", RecordData::A(Ipv4Addr::new(192, 168, 1, 20))),
                record("pc.local", RecordData::Aaaa("2001:db8::20".parse().unwrap())),
                record("pc.local", RecordData::Aaaa("fe80::20".parse().unwrap())),
                record("pc.local", RecordData::Aaaa("fd00::20".parse().unwrap())),
            ],
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
        );
        let expected: Vec<IpAddr> = ["fd00::20", "2001:db8::20", "192.168.1.20"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(cache.addrs["pc.local"], expected);
    }
}
