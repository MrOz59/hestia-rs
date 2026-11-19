//! Query-string building.

/// Percent-encodes a query component (RFC 3986 unreserved characters pass through).
pub fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Joins `key=value` pairs with `&`, encoding each component.
pub fn query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    #[test]
    fn encoding() {
        assert_eq!(super::encode_component("a b&c=d~"), "a%20b%26c%3Dd~");
        assert_eq!(
            super::query(&[("mode", "1920x1080x60".into()), ("x", "é".into())]),
            "mode=1920x1080x60&x=%C3%A9"
        );
    }
}
