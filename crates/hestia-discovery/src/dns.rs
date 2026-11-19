//! Minimal DNS message encoding/decoding for mDNS service discovery.

use std::net::{Ipv4Addr, Ipv6Addr};

pub const TYPE_A: u16 = 1;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_AAAA: u16 = 28;
pub const TYPE_SRV: u16 = 33;
pub const CLASS_IN: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ptr(String),
    Srv {
        priority: u16,
        weight: u16,
        port: u16,
        target: String,
    },
    Other(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub ttl: u32,
    pub data: RecordData,
}

/// Builds a query for one or more (name, type) questions.
pub fn build_query(id: u16, questions: &[(&str, u16)]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(64);
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes()); // flags: standard query
    msg.extend_from_slice(&(questions.len() as u16).to_be_bytes());
    msg.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // an/ns/ar counts
    for (name, qtype) in questions {
        encode_name(&mut msg, name);
        msg.extend_from_slice(&qtype.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
    }
    msg
}

fn encode_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.').filter(|l| !l.is_empty()) {
        let bytes = &label.as_bytes()[..label.len().min(63)];
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
    out.push(0);
}

/// Parses every resource record (answers, authority and additional) of a
/// response. Returns `None` for malformed packets or queries.
pub fn parse_response(msg: &[u8]) -> Option<Vec<Record>> {
    if msg.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    if flags & 0x8000 == 0 {
        return None; // a query, not a response
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let rr = [6, 8, 10]
        .iter()
        .map(|&i| u16::from_be_bytes([msg[i], msg[i + 1]]) as usize)
        .sum::<usize>();

    let mut pos = 12;
    for _ in 0..qd {
        let (_, next) = read_name(msg, pos)?;
        pos = next + 4;
    }

    let mut records = Vec::with_capacity(rr);
    for _ in 0..rr {
        let (name, next) = read_name(msg, pos)?;
        pos = next;
        let header = msg.get(pos..pos + 10)?;
        let rtype = u16::from_be_bytes([header[0], header[1]]);
        let ttl = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let len = u16::from_be_bytes([header[8], header[9]]) as usize;
        pos += 10;
        let rdata = msg.get(pos..pos + len)?;
        let data = match rtype {
            TYPE_A if len == 4 => RecordData::A(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])),
            TYPE_AAAA if len == 16 => {
                let octets: [u8; 16] = rdata.try_into().ok()?;
                RecordData::Aaaa(Ipv6Addr::from(octets))
            }
            TYPE_PTR => RecordData::Ptr(read_name(msg, pos)?.0),
            TYPE_SRV if len >= 7 => RecordData::Srv {
                priority: u16::from_be_bytes([rdata[0], rdata[1]]),
                weight: u16::from_be_bytes([rdata[2], rdata[3]]),
                port: u16::from_be_bytes([rdata[4], rdata[5]]),
                target: read_name(msg, pos + 6)?.0,
            },
            other => RecordData::Other(other),
        };
        records.push(Record { name, ttl, data });
        pos += len;
    }
    Some(records)
}

/// Reads a possibly compressed name starting at `pos`. Returns the dotted
/// name and the offset just past it in the original stream.
fn read_name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xc0 == 0xc0 {
            let ptr = ((len & 0x3f) << 8) | *msg.get(pos + 1)? as usize;
            if end.is_none() {
                end = Some(pos + 2);
            }
            jumps += 1;
            if jumps > 32 || ptr >= msg.len() {
                return None;
            }
            pos = ptr;
            continue;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        let label = msg.get(pos + 1..pos + 1 + len)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
    }
    Some((labels.join("."), end.unwrap_or(pos)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rr(out: &mut Vec<u8>, name_bytes: &[u8], rtype: u16, rdata: &[u8]) {
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&0x8001u16.to_be_bytes());
        out.extend_from_slice(&120u32.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
    }

    #[test]
    fn query_layout() {
        let q = build_query(0x1234, &[("_nvstream._tcp.local", TYPE_PTR)]);
        assert_eq!(&q[..2], &[0x12, 0x34]);
        assert_eq!(&q[4..6], &[0, 1]);
        assert_eq!(&q[12..22], b"\x09_nvstream");
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0, 1]);
    }

    #[test]
    fn response_with_compression() {
        let mut msg = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3];
        // Answer: _nvstream._tcp.local PTR cachy._nvstream._tcp.local
        let service_off = msg.len();
        let mut service = Vec::new();
        encode_name(&mut service, "_nvstream._tcp.local");
        let mut ptr_rdata = vec![5];
        ptr_rdata.extend_from_slice(b"cachy");
        ptr_rdata.extend_from_slice(&[0xc0, service_off as u8]);
        rr(&mut msg, &service, TYPE_PTR, &ptr_rdata);
        let instance_off = msg.len() - ptr_rdata.len();
        // Additional: SRV instance -> cachy.local:47989
        let mut srv = vec![0, 0, 0, 0, 0xbb, 0x75, 5];
        srv.extend_from_slice(b"cachy");
        srv.extend_from_slice(&[5]);
        srv.extend_from_slice(b"local");
        srv.push(0);
        rr(&mut msg, &[0xc0, instance_off as u8], TYPE_SRV, &srv);
        let mut host = Vec::new();
        encode_name(&mut host, "cachy.local");
        rr(&mut msg, &host, TYPE_A, &[192, 168, 0, 10]);
        rr(&mut msg, &host, TYPE_TXT, &[0]);

        let records = parse_response(&msg).unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].data, RecordData::Ptr("cachy._nvstream._tcp.local".into()));
        assert_eq!(records[1].name, "cachy._nvstream._tcp.local");
        assert_eq!(
            records[1].data,
            RecordData::Srv {
                priority: 0,
                weight: 0,
                port: 47989,
                target: "cachy.local".into()
            }
        );
        assert_eq!(records[2].data, RecordData::A(Ipv4Addr::new(192, 168, 0, 10)));
        assert_eq!(records[3].data, RecordData::Other(TYPE_TXT));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_response(&[0; 5]).is_none());
        assert!(parse_response(&[0; 12]).is_none(), "queries are ignored");
        let mut loop_ptr = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        loop_ptr.extend_from_slice(&[0xc0, 12]);
        assert!(parse_response(&loop_ptr).is_none());
    }
}
