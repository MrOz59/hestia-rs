//! Hex helpers. GameStream puts every binary blob in query strings and XML as hex.

pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn encode_upper(bytes: &[u8]) -> String {
    encode(bytes).to_ascii_uppercase()
}

/// Decodes hex, ignoring surrounding whitespace. Returns `None` on odd length
/// or non-hex characters.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let data = [0x00, 0x7f, 0x80, 0xff, 0x12];
        assert_eq!(super::encode(&data), "007f80ff12");
        assert_eq!(super::decode("007F80ff12").unwrap(), data);
        assert_eq!(super::decode(" abc "), None);
        assert_eq!(super::decode("zz"), None);
    }
}
