//! Parsing of user-entered host addresses.

use hestia_nvhttp::DEFAULT_HTTP_PORT;

use crate::{CoreError, Result};

/// Splits `host[:port]` (with `[v6]:port` for IPv6) into host and HTTP port.
pub fn parse(input: &str) -> Result<(String, u16)> {
    let input = input.trim();
    let input = input
        .strip_prefix("http://")
        .or_else(|| input.strip_prefix("https://"))
        .unwrap_or(input)
        .trim_end_matches('/');
    if input.is_empty() {
        return Err(CoreError::InvalidAddress("empty address".into()));
    }

    let bad = || CoreError::InvalidAddress(input.to_owned());
    let parse_port = |p: &str| p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad);

    if let Some(rest) = input.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(bad)?;
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => DEFAULT_HTTP_PORT,
            None => return Err(bad()),
        };
        return Ok((host.to_owned(), port));
    }

    // Bare IPv6 (more than one colon) has no port.
    if input.matches(':').count() > 1 {
        return Ok((input.to_owned(), DEFAULT_HTTP_PORT));
    }

    match input.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Ok((host.to_owned(), parse_port(port)?)),
        Some(_) => Err(bad()),
        None => {
            if input.contains(|c: char| c.is_whitespace() || c == '/') {
                return Err(bad());
            }
            Ok((input.to_owned(), DEFAULT_HTTP_PORT))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn addresses() {
        assert_eq!(parse("192.168.0.10").unwrap(), ("192.168.0.10".into(), 47989));
        assert_eq!(parse(" pc.local:48000 ").unwrap(), ("pc.local".into(), 48000));
        assert_eq!(parse("[fe80::1]:47989").unwrap(), ("fe80::1".into(), 47989));
        assert_eq!(parse("[::1]").unwrap(), ("::1".into(), 47989));
        assert_eq!(parse("::1").unwrap(), ("::1".into(), 47989));
        assert_eq!(parse("http://10.0.0.2:47989/").unwrap(), ("10.0.0.2".into(), 47989));
        assert!(parse("").is_err());
        assert!(parse("host:0").is_err());
        assert!(parse("host:abc").is_err());
        assert!(parse(":47989").is_err());
        assert!(parse("bad host").is_err());
    }
}
