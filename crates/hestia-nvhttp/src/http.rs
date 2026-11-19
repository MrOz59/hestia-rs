//! Minimal blocking HTTP/1.1 client.
//!
//! GameStream hosts speak plain, short-lived HTTP/1.1 (one request per
//! connection, `Connection: close`), so a few hundred lines over `std::net`
//! (plus rustls for HTTPS) cover everything and keep the dependency tree small
//! for Android. HTTP/2, keep-alive and redirects are deliberately absent.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Upper bound for a response body. Box art is the largest thing we fetch.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub method: &'a str,
    /// Path including the query string, e.g. `/serverinfo?uniqueid=...`.
    pub target: &'a str,
    pub body: Option<Body<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Body<'a> {
    pub content_type: &'a str,
    pub data: &'a [u8],
}

impl<'a> Request<'a> {
    pub fn get(target: &'a str) -> Self {
        Self {
            method: "GET",
            target,
            body: None,
        }
    }

    pub fn post_json(target: &'a str, json: &'a [u8]) -> Self {
        Self {
            method: "POST",
            target,
            body: Some(Body {
                content_type: "application/json",
                data: json,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Formats `host:port` for the Host header and for display, bracketing IPv6.
pub fn authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Opens a TCP connection, trying every resolved address in turn.
pub fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, format!("{host} did not resolve"));
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                return Ok(stream);
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Writes the request and reads the full response from `stream`.
pub fn exchange<S: Read + Write>(stream: &mut S, host_header: &str, request: &Request<'_>) -> io::Result<Response> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: hestia-rs/{}\r\nAccept: */*\r\nConnection: close\r\n",
        request.method,
        request.target,
        host_header,
        env!("CARGO_PKG_VERSION"),
    );
    if let Some(body) = &request.body {
        head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            body.content_type,
            body.data.len()
        ));
    } else if request.method != "GET" {
        head.push_str("Content-Length: 0\r\n");
    }
    head.push_str("\r\n");

    let mut out = head.into_bytes();
    if let Some(body) = &request.body {
        out.extend_from_slice(body.data);
    }
    stream.write_all(&out)?;
    stream.flush()?;
    read_response(stream)
}

/// Reads one response. Tolerates peers that drop the connection (or skip TLS
/// close_notify) once the body is complete.
pub fn read_response<R: Read>(reader: &mut R) -> io::Result<Response> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];

    // Headers
    let header_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(invalid("response headers too large"));
        }
        let n = read_tolerant(reader, &mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before response headers",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| invalid("non UTF-8 headers"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(invalid("not an HTTP response"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("bad status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();

    let mut response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    let mut body = buf[header_end + 4..].to_vec();

    let chunked = response
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let content_length = response.header("content-length").and_then(|v| v.parse::<usize>().ok());

    if chunked {
        loop {
            if let Some(decoded) = decode_chunked(&body)? {
                response.body = decoded;
                return Ok(response);
            }
            let n = read_tolerant(reader, &mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated chunked body"));
            }
            body.extend_from_slice(&chunk[..n]);
            if body.len() > MAX_BODY_BYTES {
                return Err(invalid("response body too large"));
            }
        }
    } else if let Some(len) = content_length {
        if len > MAX_BODY_BYTES {
            return Err(invalid("response body too large"));
        }
        while body.len() < len {
            let n = read_tolerant(reader, &mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated body"));
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
    } else {
        loop {
            let n = read_tolerant(reader, &mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
            if body.len() > MAX_BODY_BYTES {
                return Err(invalid("response body too large"));
            }
        }
    }
    response.body = body;
    Ok(response)
}

/// `read` that maps "peer went away" into a clean EOF.
fn read_tolerant<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset) => {
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Returns the decoded body once the terminating chunk is present.
fn decode_chunked(data: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let Some(line_end) = find(&data[pos..], b"\r\n") else {
            return Ok(None);
        };
        let line = std::str::from_utf8(&data[pos..pos + line_end]).map_err(|_| invalid("bad chunk header"))?;
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid("bad chunk size"))?;
        pos += line_end + 2;
        if size == 0 {
            return Ok(Some(out));
        }
        if data.len() < pos + size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&data[pos..pos + size]);
        pos += size + 2;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        input: io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Hand data out in small pieces to exercise the buffering.
            let max = buf.len().min(7);
            self.input.read(&mut buf[..max])
        }
    }

    impl Write for Fake {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn fake(response: &str) -> Fake {
        Fake {
            input: io::Cursor::new(response.as_bytes().to_vec()),
            written: Vec::new(),
        }
    }

    #[test]
    fn content_length() {
        let mut s = fake("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/xml\r\n\r\nhelloEXTRA");
        let r = exchange(&mut s, "h:1", &Request::get("/serverinfo?x=1")).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(r.header("content-type"), Some("text/xml"));
        let sent = String::from_utf8(s.written).unwrap();
        assert!(sent.starts_with("GET /serverinfo?x=1 HTTP/1.1\r\nHost: h:1\r\n"));
        assert!(sent.ends_with("\r\n\r\n"));
    }

    #[test]
    fn chunked_and_eof() {
        let mut s = fake("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n");
        assert_eq!(read_response(&mut s.input).unwrap().body, b"abcde");
        let mut s = fake("HTTP/1.0 404 Not Found\r\n\r\nnope");
        let r = read_response(&mut s.input).unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.text(), "nope");
    }

    #[test]
    fn post_body() {
        let mut s = fake("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        exchange(&mut s, "h:1", &Request::post_json("/api", b"{\"a\":1}")).unwrap();
        let sent = String::from_utf8(s.written).unwrap();
        assert!(sent.contains("Content-Length: 7\r\n"));
        assert!(sent.ends_with("\r\n\r\n{\"a\":1}"));
    }

    #[test]
    fn truncated() {
        let mut s = fake("HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort");
        assert!(read_response(&mut s.input).is_err());
        assert_eq!(authority("::1", 5), "[::1]:5");
        assert_eq!(authority("10.0.0.2", 5), "10.0.0.2:5");
    }
}
