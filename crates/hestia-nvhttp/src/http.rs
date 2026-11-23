//! Minimal blocking HTTP/1.1 client.
//!
//! GameStream hosts speak plain, short-lived HTTP/1.1 (one request per
//! connection, `Connection: close`), so a few hundred lines over `std::net`
//! (plus rustls for HTTPS) cover everything and keep the dependency tree small
//! for Android. HTTP/2, keep-alive and redirects are deliberately absent.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
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

static ROUTE_HOOK: std::sync::OnceLock<fn(&SocketAddr)> = std::sync::OnceLock::new();

/// Registers a function called with every address before connecting to it.
/// The Android app uses it to send LAN traffic over the Wi-Fi network when
/// mobile data is the default network.
pub fn set_route_hook(hook: fn(&SocketAddr)) {
    let _ = ROUTE_HOOK.set(hook);
}

/// Runs the route hook for `addr` (for traffic that doesn't go through
/// [`connect`], such as Wake-on-LAN broadcasts).
pub fn route(addr: &SocketAddr) {
    if let Some(hook) = ROUTE_HOOK.get() {
        hook(addr);
    }
}

/// How long a connection attempt runs alone before the next address gets
/// one too (RFC 8305's recommended "Connection Attempt Delay").
pub const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Opens a TCP connection to `host` (an IP or a name). With several resolved
/// addresses this is Happy Eyeballs (RFC 8305): IPv6 first, families
/// alternating, a new attempt every [`ATTEMPT_DELAY`] (or as soon as one
/// fails), and the first connection wins. A dead IPv6 path then costs a
/// quarter of a second instead of a full timeout. Errors name the address.
pub fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs = connection_order((host, port).to_socket_addrs()?.collect());
    match addrs.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{host} did not resolve"),
        )),
        1 => connect_one(addrs[0], timeout),
        _ => race(addrs, timeout),
    }
}

/// The order to try addresses in: IPv6 before IPv4, unique-local IPv6 first,
/// then the families alternate (RFC 8305 §4). The resolver's own order
/// (RFC 6724) would put unique-local IPv6 below every IPv4 address.
pub fn connection_order(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut v6 = Vec::new();
    let mut v4 = Vec::new();
    for addr in addrs {
        let list = if addr.is_ipv6() { &mut v6 } else { &mut v4 };
        if !list.contains(&addr) {
            list.push(addr);
        }
    }
    // Stable: unique-local first, the resolver's order otherwise.
    v6.sort_by_key(|a| !matches!(a.ip(), IpAddr::V6(ip) if (ip.segments()[0] & 0xfe00) == 0xfc00));
    let mut out = Vec::with_capacity(v6.len() + v4.len());
    let (mut v6, mut v4) = (v6.into_iter(), v4.into_iter());
    loop {
        match (v6.next(), v4.next()) {
            (None, None) => break,
            (a, b) => out.extend(a.into_iter().chain(b)),
        }
    }
    out
}

fn connect_one(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    route(&addr);
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => {
            stream.set_nodelay(true).ok();
            Ok(stream)
        }
        Err(e) => Err(io::Error::new(e.kind(), format!("{addr}: {e}"))),
    }
}

/// Staggered attempts, first connection wins. Attempts still running when one
/// wins finish on their own and their connections are dropped.
///
/// On Android the route hook binds the whole process to a network right
/// before each attempt; staggering them lets each one create its socket
/// before the next can rebind.
fn race(addrs: Vec<SocketAddr>, timeout: Duration) -> io::Result<TcpStream> {
    let (tx, rx) = mpsc::channel();
    let mut tx = Some(tx);
    let mut queue = addrs.into_iter();
    let mut running = 0usize;
    let mut last_err = None;
    loop {
        // Start the next attempt, if any is left.
        if let Some(sender) = &tx {
            match queue.next() {
                Some(addr) => {
                    let sender = sender.clone();
                    let spawned = std::thread::Builder::new()
                        .name("hestia-connect".into())
                        .spawn(move || {
                            // The receiver is gone once another attempt won.
                            let _ = sender.send(connect_one(addr, timeout));
                        });
                    match spawned {
                        Ok(_) => running += 1,
                        Err(e) => last_err = Some(e),
                    }
                }
                // Nothing left to start: once every attempt has reported,
                // the channel closes and the loop ends.
                None => tx = None,
            }
        }
        if running == 0 && tx.is_none() {
            break;
        }
        let result = if tx.is_some() {
            match rx.recv_timeout(ATTEMPT_DELAY) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(result) => result,
                Err(_) => break,
            }
        };
        running -= 1;
        match result {
            Ok(stream) => return Ok(stream),
            // A failed attempt starts the next one right away.
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no address to connect to")))
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

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ipv6_first_then_alternating() {
        let resolved = vec![
            sa("192.168.1.20:47989"),
            sa("[2001:db8::20]:47989"),
            sa("10.0.0.20:47989"),
            sa("[fd00::20]:47989"),
            sa("[2001:db8::20]:47989"),
            sa("[2001:db8::21]:47989"),
        ];
        assert_eq!(
            connection_order(resolved),
            [
                sa("[fd00::20]:47989"),
                sa("192.168.1.20:47989"),
                sa("[2001:db8::20]:47989"),
                sa("10.0.0.20:47989"),
                sa("[2001:db8::21]:47989"),
            ]
        );
        let v4_only = vec![sa("10.0.0.1:1"), sa("10.0.0.2:1")];
        assert_eq!(connection_order(v4_only.clone()), v4_only);
    }

    /// A port nothing listens on (bound, then released).
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn a_failed_attempt_hands_over_at_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        let refused = SocketAddr::new(open.ip(), closed_port());
        let started = std::time::Instant::now();
        let stream = race(vec![refused, open], Duration::from_secs(5)).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open);
        assert!(started.elapsed() < ATTEMPT_DELAY, "{:?}", started.elapsed());
    }

    #[test]
    fn a_silent_address_costs_one_attempt_delay() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        // TEST-NET-1 (RFC 5737): routed nowhere, so the attempt usually hangs.
        let silent = sa("192.0.2.1:9");
        let started = std::time::Instant::now();
        let stream = race(vec![silent, open], Duration::from_secs(10)).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open);
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    }

    #[test]
    fn every_attempt_failing_reports_one() {
        let a = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), closed_port());
        let b = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), closed_port());
        let err = race(vec![a, b], Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().starts_with("127.0.0.1:"), "{err}");
    }

    #[test]
    fn connects_by_name() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // `localhost` may also resolve to ::1, where nothing listens.
        let stream = connect("localhost", port, Duration::from_secs(5)).unwrap();
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        assert!(connect("[127.0.0.1]", port, Duration::from_secs(5)).is_ok());
    }
}
