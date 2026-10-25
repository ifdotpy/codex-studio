//! Bounded HTTP/1 requests and responses. Each connection carries one request.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct Limits {
    pub request_line: usize,
    pub header_count: usize,
    pub header_bytes: usize,
    pub body_bytes: usize,
    pub header_timeout: Duration,
    pub body_timeout: Duration,
    pub write_timeout: Duration,
}

impl Limits {
    pub fn configure(&self, stream: &TcpStream) -> io::Result<()> {
        stream.set_read_timeout(Some(self.header_timeout.min(self.body_timeout)))?;
        stream.set_write_timeout(Some(self.write_timeout))
    }
}

/// Socket deadlines also bound loops inside a TLS adapter.
pub struct Socket {
    stream: TcpStream,
    read_until: Instant,
    write_until: Instant,
    read_wait: Duration,
}

impl Socket {
    pub fn new(stream: TcpStream, timeout: Duration) -> Self {
        Self { stream, read_until: Instant::now() + timeout, write_until: Instant::now() + timeout, read_wait: timeout }
    }
    pub fn begin_read(&mut self, total: Duration, wait: Duration) {
        self.read_until = Instant::now() + total;
        self.read_wait = wait;
    }
    pub fn begin_write(&mut self, timeout: Duration) {
        self.write_until = Instant::now() + timeout;
    }
    fn left(until: Instant) -> io::Result<Duration> {
        until
            .checked_duration_since(Instant::now())
            .filter(|v| !v.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "HTTP deadline reached"))
    }
}

impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(Self::left(self.read_until)?.min(self.read_wait)))?;
        let count = self.stream.read(bytes)?;
        Self::left(self.read_until)?;
        Ok(count)
    }
}
impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(Self::left(self.write_until)?))?;
        let count = self.stream.write(bytes)?;
        Self::left(self.write_until)?;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Self::left(self.write_until)?;
        self.stream.flush()
    }
}

#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub message: &'static str,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}

fn bad(status: u16, message: &'static str) -> Error {
    Error { status, message }
}

fn io_error(error: io::Error) -> Error {
    if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
        bad(408, "request timed out")
    } else {
        bad(400, "incomplete HTTP request")
    }
}

fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn line<R: BufRead>(reader: &mut R, limit: usize, deadline: Instant) -> Result<Vec<u8>, Error> {
    let mut line = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err(bad(408, "request timed out"));
        }
        let bytes = reader.fill_buf().map_err(io_error)?;
        if Instant::now() >= deadline {
            return Err(bad(408, "request timed out"));
        }
        if bytes.is_empty() {
            return Err(bad(400, "incomplete HTTP headers"));
        }
        let end = bytes.iter().position(|b| *b == b'\n').map(|at| at + 1);
        let count = end.unwrap_or(bytes.len());
        if count > limit.saturating_sub(line.len()) {
            return Err(bad(431, "HTTP line is too large"));
        }
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if end.is_some() {
            if !line.ends_with(b"\r\n") || line[..line.len() - 2].contains(&b'\r') {
                return Err(bad(400, "HTTP lines require CRLF"));
            }
            line.truncate(line.len() - 2);
            return Ok(line);
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

pub fn request<R: Read>(stream: &mut R, limits: &Limits) -> Result<Request, Error> {
    let mut reader = BufReader::with_capacity(8192, stream);
    let deadline = Instant::now() + limits.header_timeout;
    let first = line(&mut reader, limits.request_line, deadline)?;
    if !first.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Err(bad(400, "invalid request line"));
    }
    let first = std::str::from_utf8(&first).map_err(|_| bad(400, "invalid request line"))?;
    let parts: Vec<_> = first.split(' ').collect();
    if parts.len() != 3
        || parts[0].is_empty()
        || !parts[0].bytes().all(token)
        || !parts[1].starts_with('/')
        || parts[1].contains('#')
        || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1")
    {
        return Err(bad(400, "invalid request line"));
    }
    let (method, target) = (parts[0].to_string(), parts[1].to_string());
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut bytes_left = limits.header_bytes;
    let mut count = 0;
    loop {
        let raw = line(&mut reader, bytes_left, deadline)?;
        bytes_left = bytes_left.checked_sub(raw.len() + 2).ok_or_else(|| bad(431, "too many header bytes"))?;
        if raw.is_empty() {
            break;
        }
        count += 1;
        if count > limits.header_count {
            return Err(bad(431, "too many headers"));
        }
        if raw[0] == b' ' || raw[0] == b'\t' {
            return Err(bad(400, "folded headers are refused"));
        }
        let at = raw.iter().position(|b| *b == b':').filter(|at| *at > 0).ok_or_else(|| bad(400, "invalid HTTP header"))?;
        if !raw[..at].iter().copied().all(token) || !raw[at + 1..].iter().all(|b| (0x20..=0x7e).contains(b) || *b == b'\t') {
            return Err(bad(400, "invalid HTTP header"));
        }
        let name = String::from_utf8(raw[..at].to_vec()).expect("ASCII").to_ascii_lowercase();
        let value = String::from_utf8(raw[at + 1..].to_vec()).expect("ASCII").trim_matches([' ', '\t']).to_string();
        if let Some(previous) = headers.get_mut(&name) {
            if !matches!(name.as_str(), "accept" | "accept-encoding" | "accept-language") {
                return Err(bad(400, "duplicate HTTP header"));
            }
            previous.push_str(", ");
            previous.push_str(&value);
        } else {
            headers.insert(name, value);
        }
    }
    if parts[2] == "HTTP/1.1" && headers.get("host").is_none_or(|value| value.is_empty()) {
        return Err(bad(400, "HTTP/1.1 requires Host"));
    }
    if headers.contains_key("transfer-encoding") {
        return Err(bad(400, "Transfer-Encoding is refused"));
    }
    if headers.contains_key("expect") {
        return Err(bad(417, "Expect is refused"));
    }
    let length = match headers.get("content-length") {
        None => 0,
        Some(value) if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
            value.parse::<usize>().map_err(|_| bad(413, "request body is too large"))?
        }
        Some(_) => return Err(bad(400, "invalid Content-Length")),
    };
    if length > limits.body_bytes {
        return Err(bad(413, "request body is too large"));
    }
    let mut body = vec![0; length];
    let mut offset = 0;
    let deadline = Instant::now() + limits.body_timeout;
    while offset < body.len() {
        if Instant::now() >= deadline {
            return Err(bad(408, "request timed out"));
        }
        let end = body.len().min(offset + 64 * 1024);
        let count = reader.read(&mut body[offset..end]).map_err(io_error)?;
        if Instant::now() >= deadline {
            return Err(bad(408, "request timed out"));
        }
        if count == 0 {
            return Err(bad(400, "incomplete HTTP body"));
        }
        offset += count;
    }
    Ok(Request { method, target, headers, body })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    Closed(u64, u64),
    From(u64),
    Suffix(u64),
}

impl Range {
    pub fn parse(value: &str) -> Result<Self, Error> {
        let (start, end) =
            value.strip_prefix("bytes=").and_then(|s| s.split_once('-')).ok_or_else(|| bad(416, "invalid byte range"))?;
        let number = |s: &str| -> Result<u64, Error> {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad(416, "invalid byte range"));
            }
            s.parse().map_err(|_| bad(416, "invalid byte range"))
        };
        match (start.is_empty(), end.is_empty()) {
            (true, false) => {
                let count = number(end)?;
                if count == 0 {
                    return Err(bad(416, "invalid byte range"));
                }
                Ok(Self::Suffix(count))
            }
            (false, true) => Ok(Self::From(number(start)?)),
            (false, false) => {
                let (start, end) = (number(start)?, number(end)?);
                if start > end {
                    return Err(bad(416, "invalid byte range"));
                }
                Ok(Self::Closed(start, end))
            }
            _ => Err(bad(416, "invalid byte range")),
        }
    }

    pub fn resolve(self, length: u64) -> Option<(u64, u64)> {
        if length == 0 {
            return None;
        }
        match self {
            Self::Closed(start, end) if start < length => Some((start, end.min(length - 1))),
            Self::From(start) if start < length => Some((start, length - 1)),
            Self::Suffix(count) => Some((length.saturating_sub(count), length - 1)),
            _ => None,
        }
    }

    pub fn header(self) -> String {
        match self {
            Self::Closed(start, end) => format!("bytes={start}-{end}"),
            Self::From(start) => format!("bytes={start}-"),
            Self::Suffix(count) => format!("bytes=-{count}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRange {
    pub start: u64,
    pub end: u64,
    pub total: u64,
}
impl ContentRange {
    pub fn parse(value: &str) -> Result<Self, Error> {
        let (range, total) =
            value.strip_prefix("bytes ").and_then(|s| s.split_once('/')).ok_or_else(|| bad(502, "invalid Content-Range"))?;
        if total.is_empty() || !total.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad(502, "invalid Content-Range"));
        }
        let total = total.parse::<u64>().map_err(|_| bad(502, "invalid Content-Range"))?;
        match Range::parse(&format!("bytes={range}")).map_err(|_| bad(502, "invalid Content-Range"))? {
            Range::Closed(start, end) if start <= end && end < total => Ok(Self { start, end, total }),
            _ => Err(bad(502, "invalid Content-Range")),
        }
    }
    pub fn header(self) -> String {
        format!("bytes {}-{}/{}", self.start, self.end, self.total)
    }
}

pub fn headers<W: Write>(stream: &mut W, status: u16, length: u64, headers: &[(String, String)]) -> io::Result<()> {
    if !(100..600).contains(&status) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid response status"));
    }
    let mut names = std::collections::BTreeSet::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if name.is_empty()
            || !name.bytes().all(token)
            || !value.bytes().all(|b| (0x20..=0x7e).contains(&b))
            || matches!(lower.as_str(), "content-length" | "connection" | "transfer-encoding")
            || !names.insert(lower)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid response header"));
        }
    }
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        423 => "Locked",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: {length}\r\nConnection: close\r\n")?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    stream.write_all(b"\r\n")
}

pub fn response<W: Write>(stream: &mut W, status: u16, headers: &[(String, String)], body: &[u8]) -> io::Result<()> {
    self::headers(stream, status, body.len() as u64, headers)?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> Limits {
        Limits {
            request_line: 128,
            header_count: 4,
            header_bytes: 128,
            body_bytes: 8,
            header_timeout: Duration::from_secs(1),
            body_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(1),
        }
    }
    fn parse(text: &str) -> Result<Request, Error> {
        request(&mut text.as_bytes(), &limits())
    }
    #[test]
    fn valid_request_and_body() {
        let found = parse("PUT /objects/a HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabc").unwrap();
        assert_eq!(found.body, b"abc");
        assert_eq!(found.header("host"), Some("localhost"));
    }
    #[test]
    fn malformed_requests_are_refused() {
        for text in [
            "GET / HTTP/1.1\nHost: x\n\n",
            "GET / HTTP/1.1\r\nHost : x\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\n folded\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nX: a\rb\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: +1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1,1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n",
            "GET http://other/ HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\na",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }
    #[test]
    fn repeated_list_fields_combine_in_order_with_raw_line_limits() {
        let mut limits = limits();
        limits.header_bytes = 1024;
        limits.header_count = 8;
        for name in ["Accept", "Accept-Encoding", "Accept-Language"] {
            let mut text = "GET / HTTP/1.1\r\nHost: x\r\n".to_string();
            for value in 1..=6 {
                text.push_str(&format!("{name}: type{value}\r\n"));
            }
            text.push_str("\r\n");
            let found = request(&mut text.as_bytes(), &limits).unwrap();
            assert_eq!(found.header(&name.to_ascii_lowercase()), Some("type1, type2, type3, type4, type5, type6"));
            let mut bounded = limits;
            bounded.header_count = 6;
            assert_eq!(request(&mut text.as_bytes(), &bounded).unwrap_err().status, 431);
            bounded = limits;
            bounded.header_bytes = 32;
            assert_eq!(request(&mut text.as_bytes(), &bounded).unwrap_err().status, 431);
        }
    }
    #[test]
    fn other_duplicate_fields_are_refused() {
        for name in ["Host", "Authorization", "Content-Length", "Transfer-Encoding", "Range", "Origin", "Cookie", "X-Other"] {
            let text = format!("GET / HTTP/1.1\r\nHost: x\r\n{name}: 0\r\n{name}: 0\r\n\r\n");
            assert_eq!(request(&mut text.as_bytes(), &limits()).unwrap_err().message, "duplicate HTTP header");
        }
    }
    #[test]
    fn size_limits_are_enforced() {
        assert_eq!(parse(&format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "x".repeat(128))).unwrap_err().status, 431);
        assert_eq!(parse(&format!("GET / HTTP/1.1\r\nHost: x\r\nX: {}\r\n\r\n", "x".repeat(128))).unwrap_err().status, 431);
        assert_eq!(parse("GET / HTTP/1.1\r\nHost: x\r\nA: 1\r\nB: 1\r\nC: 1\r\nD: 1\r\n\r\n").unwrap_err().status, 431);
        assert_eq!(parse("PUT / HTTP/1.1\r\nHost: x\r\nContent-Length: 9\r\n\r\n").unwrap_err().status, 413);
    }
    #[test]
    fn ranges_are_bounded_and_single() {
        assert_eq!(Range::parse("bytes=1-3").unwrap().resolve(3), Some((1, 2)));
        assert_eq!(Range::parse("bytes=-5").unwrap().resolve(3), Some((0, 2)));
        assert_eq!(Range::parse("bytes=2-").unwrap().resolve(2), None);
        for value in ["bytes=3-1", "bytes=-0", "bytes=0-1,3-4", "bytes=+1-2", "bytes=0-2\r\nX:x", "bytes=-"] {
            assert!(Range::parse(value).is_err());
        }
    }
    #[test]
    fn content_ranges_reject_invalid_totals_and_intervals() {
        assert_eq!(ContentRange::parse("bytes 1-2/3").unwrap(), ContentRange { start: 1, end: 2, total: 3 });
        for value in ["bytes 0-3/3", "bytes 0-0/0", "bytes 3-1/4", "bytes 1-/4", "bytes */4", "bytes 1-2/*", "bytes 1-2/3\r\n"] {
            assert!(ContentRange::parse(value).is_err(), "{value}");
        }
    }
    #[test]
    fn response_injection_is_refused_before_any_write() {
        for (name, value) in [("X\r\nEvil", "a"), ("X", "a\r\nEvil: yes"), ("Content-Length", "3")] {
            let mut bytes = Vec::new();
            assert!(headers(&mut bytes, 200, 0, &[(name.into(), value.into())]).is_err());
            assert!(bytes.is_empty());
        }
    }
}
