//! Just enough DER to build a self-signed X.509 certificate and to pull the
//! public key and signature back out of one.
//!
//! GameStream pairing needs two things from certificates: the raw signature
//! bytes (they are mixed into the challenge hashes) and the RSA public key (to
//! verify the host's pairing secret). Doing this by hand keeps the crate free of
//! a full X.509 stack and its API churn.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerError(pub &'static str);

impl fmt::Display for DerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed DER: {}", self.0)
    }
}

impl std::error::Error for DerError {}

pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_BIT_STRING: u8 = 0x03;
pub const TAG_NULL: u8 = 0x05;
pub const TAG_OID: u8 = 0x06;
pub const TAG_UTF8_STRING: u8 = 0x0c;
pub const TAG_UTC_TIME: u8 = 0x17;
pub const TAG_GENERALIZED_TIME: u8 = 0x18;
pub const TAG_SEQUENCE: u8 = 0x30;
pub const TAG_SET: u8 = 0x31;

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

pub fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, &parts.concat())
}

pub fn set(parts: &[&[u8]]) -> Vec<u8> {
    tlv(TAG_SET, &parts.concat())
}

/// Explicitly tagged, constructed, context-specific field (`[n] EXPLICIT`).
pub fn explicit(n: u8, content: &[u8]) -> Vec<u8> {
    tlv(0xa0 | n, content)
}

/// Unsigned big-endian integer.
pub fn integer(value: &[u8]) -> Vec<u8> {
    let skip = value.iter().take_while(|b| **b == 0).count();
    let mut body: Vec<u8> = value[skip..].to_vec();
    if body.is_empty() {
        body.push(0);
    } else if body[0] & 0x80 != 0 {
        body.insert(0, 0);
    }
    tlv(TAG_INTEGER, &body)
}

pub fn null() -> Vec<u8> {
    vec![TAG_NULL, 0]
}

pub fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 1);
    body.push(0); // no unused bits
    body.extend_from_slice(bytes);
    tlv(TAG_BIT_STRING, &body)
}

pub fn utf8_string(s: &str) -> Vec<u8> {
    tlv(TAG_UTF8_STRING, s.as_bytes())
}

pub fn oid(arcs: &[u64]) -> Vec<u8> {
    assert!(arcs.len() >= 2, "an OID needs at least two arcs");
    let mut body = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        let mut chunk = Vec::new();
        let mut v = arc;
        chunk.push((v & 0x7f) as u8);
        v >>= 7;
        while v > 0 {
            chunk.push(0x80 | (v & 0x7f) as u8);
            v >>= 7;
        }
        chunk.reverse();
        body.extend_from_slice(&chunk);
    }
    tlv(TAG_OID, &body)
}

/// X.509 `Time`: UTCTime before 2050, GeneralizedTime from 2050 on (RFC 5280 §4.1.2.5).
pub fn x509_time(unix_secs: i64) -> Vec<u8> {
    let (y, mo, d, h, mi, s) = civil_from_unix(unix_secs);
    if (1950..2050).contains(&y) {
        let text = format!("{:02}{:02}{:02}{:02}{:02}{:02}Z", y % 100, mo, d, h, mi, s);
        tlv(TAG_UTC_TIME, text.as_bytes())
    } else {
        let text = format!("{:04}{:02}{:02}{:02}{:02}{:02}Z", y, mo, d, h, mi, s);
        tlv(TAG_GENERALIZED_TIME, text.as_bytes())
    }
}

/// Converts a Unix timestamp to (year, month, day, hour, minute, second) in UTC.
pub fn civil_from_unix(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400);
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// One parsed TLV element. `raw` covers the whole element (header + content).
#[derive(Debug, Clone, Copy)]
pub struct Element<'a> {
    pub tag: u8,
    pub content: &'a [u8],
    pub raw: &'a [u8],
}

/// Reads the element at the start of `input` and returns it with the rest.
pub fn read(input: &[u8]) -> Result<(Element<'_>, &[u8]), DerError> {
    if input.len() < 2 {
        return Err(DerError("truncated header"));
    }
    let tag = input[0];
    if tag & 0x1f == 0x1f {
        return Err(DerError("multi-byte tags are not supported"));
    }
    let first = input[1];
    let (len, header) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > std::mem::size_of::<usize>() {
            return Err(DerError("unsupported length encoding"));
        }
        if input.len() < 2 + n {
            return Err(DerError("truncated length"));
        }
        let mut len = 0usize;
        for b in &input[2..2 + n] {
            len = (len << 8) | *b as usize;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len).ok_or(DerError("length overflow"))?;
    if input.len() < end {
        return Err(DerError("truncated content"));
    }
    Ok((
        Element {
            tag,
            content: &input[header..end],
            raw: &input[..end],
        },
        &input[end..],
    ))
}

fn expect<'a>(input: &'a [u8], tag: u8, what: &'static str) -> Result<(Element<'a>, &'a [u8]), DerError> {
    let (el, rest) = read(input)?;
    if el.tag != tag {
        return Err(DerError(what));
    }
    Ok((el, rest))
}

/// The pieces of a certificate pairing cares about.
#[derive(Debug, Clone, Copy)]
pub struct CertParts<'a> {
    /// DER of the TBSCertificate (what the signature covers).
    pub tbs: &'a [u8],
    /// DER of the SubjectPublicKeyInfo.
    pub spki: &'a [u8],
    /// Raw signature bytes (BIT STRING payload without the unused-bits byte).
    pub signature: &'a [u8],
}

pub fn parse_certificate(der: &[u8]) -> Result<CertParts<'_>, DerError> {
    let (cert, _) = expect(der, TAG_SEQUENCE, "certificate is not a SEQUENCE")?;
    let (tbs, rest) = expect(cert.content, TAG_SEQUENCE, "tbsCertificate is not a SEQUENCE")?;
    let (_sig_alg, rest) = expect(rest, TAG_SEQUENCE, "signatureAlgorithm is not a SEQUENCE")?;
    let (sig, _) = expect(rest, TAG_BIT_STRING, "signatureValue is not a BIT STRING")?;
    if sig.content.is_empty() || sig.content[0] != 0 {
        return Err(DerError("signature has unused bits"));
    }

    // Walk the TBSCertificate: [0] version (optional), serial, signature,
    // issuer, validity, subject, subjectPublicKeyInfo, ...
    let mut fields = tbs.content;
    let (first, rest) = read(fields)?;
    fields = if first.tag == 0xa0 { rest } else { fields };
    let (_serial, rest) = expect(fields, TAG_INTEGER, "serialNumber is not an INTEGER")?;
    let (_alg, rest) = expect(rest, TAG_SEQUENCE, "signature is not a SEQUENCE")?;
    let (_issuer, rest) = expect(rest, TAG_SEQUENCE, "issuer is not a SEQUENCE")?;
    let (_validity, rest) = expect(rest, TAG_SEQUENCE, "validity is not a SEQUENCE")?;
    let (_subject, rest) = expect(rest, TAG_SEQUENCE, "subject is not a SEQUENCE")?;
    let (spki, _) = expect(rest, TAG_SEQUENCE, "subjectPublicKeyInfo is not a SEQUENCE")?;

    Ok(CertParts {
        tbs: tbs.raw,
        spki: spki.raw,
        signature: &sig.content[1..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths() {
        assert_eq!(tlv(0x04, &[1, 2]), vec![0x04, 2, 1, 2]);
        let long = vec![0u8; 300];
        let enc = tlv(0x04, &long);
        assert_eq!(&enc[..4], &[0x04, 0x82, 0x01, 0x2c]);
        let (el, rest) = read(&enc).unwrap();
        assert_eq!(el.content.len(), 300);
        assert!(rest.is_empty());
    }

    #[test]
    fn integers() {
        assert_eq!(integer(&[0, 0, 5]), vec![0x02, 1, 5]);
        assert_eq!(integer(&[0x80]), vec![0x02, 2, 0, 0x80]);
        assert_eq!(integer(&[]), vec![0x02, 1, 0]);
    }

    #[test]
    fn oids() {
        // sha256WithRSAEncryption
        assert_eq!(
            oid(&[1, 2, 840, 113549, 1, 1, 11]),
            vec![0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]
        );
        // commonName
        assert_eq!(oid(&[2, 5, 4, 3]), vec![0x06, 0x03, 0x55, 0x04, 0x03]);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_unix(1_763_510_400 + 3661), (2025, 11, 19, 1, 1, 1));
        assert_eq!(civil_from_unix(2_524_608_000), (2050, 1, 1, 0, 0, 0));
    }

    #[test]
    fn times() {
        assert_eq!(x509_time(0), tlv(TAG_UTC_TIME, b"700101000000Z"));
        assert_eq!(x509_time(2_524_608_000), tlv(TAG_GENERALIZED_TIME, b"20500101000000Z"));
    }

    #[test]
    fn roundtrip_fake_certificate() {
        let spki = sequence(&[&sequence(&[&oid(&[1, 2, 3]), &null()]), &bit_string(&[9, 9])]);
        let name = sequence(&[&set(&[&sequence(&[&oid(&[2, 5, 4, 3]), &utf8_string("x")])])]);
        let alg = sequence(&[&oid(&[1, 2, 840, 113549, 1, 1, 11]), &null()]);
        let tbs = sequence(&[
            &explicit(0, &integer(&[2])),
            &integer(&[1]),
            &alg,
            &name,
            &sequence(&[&x509_time(0), &x509_time(1)]),
            &name,
            &spki,
        ]);
        let cert = sequence(&[&tbs, &alg, &bit_string(&[0xde, 0xad])]);
        let parts = parse_certificate(&cert).unwrap();
        assert_eq!(parts.tbs, &tbs[..]);
        assert_eq!(parts.spki, &spki[..]);
        assert_eq!(parts.signature, &[0xde, 0xad]);
    }
}
