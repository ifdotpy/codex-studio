use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const DEFAULT_PORT: u16 = 4620;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DIAGNOSTICS_PATH: &str = "/api/diagnostics";

fn main() {
    let port = match parse_port(std::env::args().skip(1)) {
        Ok(Some(port)) => port,
        Ok(None) => return,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    match fetch_diagnostics(port) {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(output) => println!("{output}"),
            Err(error) => {
                eprintln!("codex-diagnostics: invalid JSON response: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!("codex-diagnostics: {error}");
            std::process::exit(1);
        }
    }
}

fn parse_port(arguments: impl Iterator<Item = String>) -> Result<Option<u16>, String> {
    let mut arguments = arguments;
    let mut port = DEFAULT_PORT;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: codex-diagnostics [-h] [--port PORT]\n\nPrint the shareable diagnostics snapshot from a running Studio backend.\n\noptions:\n  -h, --help   show this help message and exit\n  --port PORT"
                );
                return Ok(None);
            }
            "--port" => {
                let value = arguments.next().ok_or_else(|| {
                    "usage: codex-diagnostics [-h] [--port PORT]\ncodex-diagnostics: error: argument --port: expected one argument".to_owned()
                })?;
                port = value.parse::<u16>().map_err(|_| {
                    format!("usage: codex-diagnostics [-h] [--port PORT]\ncodex-diagnostics: error: argument --port: invalid int value: '{value}'")
                })?;
            }
            value if value.starts_with("--port=") => {
                let port_value = &value[7..];
                port = port_value.parse::<u16>().map_err(|_| {
                    format!("usage: codex-diagnostics [-h] [--port PORT]\ncodex-diagnostics: error: argument --port: invalid int value: '{port_value}'")
                })?;
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "usage: codex-diagnostics [-h] [--port PORT]\ncodex-diagnostics: error: unrecognized arguments: {value}"
                ));
            }
            value => {
                return Err(format!(
                    "usage: codex-diagnostics [-h] [--port PORT]\ncodex-diagnostics: error: unrecognized arguments: {value}"
                ));
            }
        }
    }
    Ok(Some(port))
}

fn fetch_diagnostics(port: u16) -> Result<Value, String> {
    fetch_diagnostics_with_timeout(port, REQUEST_TIMEOUT)
}

fn fetch_diagnostics_with_timeout(port: u16, timeout: Duration) -> Result<Value, String> {
    let address = ("127.0.0.1", port)
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve loopback address: {error}"))?
        .next()
        .ok_or_else(|| "could not resolve loopback address".to_owned())?;
    let mut stream =
        TcpStream::connect_timeout(&address, timeout).map_err(|error| format_http_error(&error))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("could not set response timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("could not set request timeout: {error}"))?;
    write!(
        stream,
        "GET {DIAGNOSTICS_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| format_http_error(&error))?;

    let mut response = BufReader::new(stream);
    let mut status_line = String::new();
    response
        .read_line(&mut status_line)
        .map_err(|error| format_http_error(&error))?;
    if status_line.is_empty() {
        return Err("server returned an empty HTTP response".to_owned());
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("invalid HTTP status line: {}", status_line.trim_end()))?;
    let mut content_length = None;
    let mut is_chunked = false;
    loop {
        let mut line = String::new();
        response
            .read_line(&mut line)
            .map_err(|error| format_http_error(&error))?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        if line.is_empty() {
            return Err("incomplete HTTP response headers".to_owned());
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            }
        }
    }
    if !(200..300).contains(&status) {
        let reason = status_line
            .split_whitespace()
            .skip(2)
            .collect::<Vec<_>>()
            .join(" ");
        return Err(if reason.is_empty() {
            format!("HTTP Error {status}")
        } else {
            format!("HTTP Error {status}: {reason}")
        });
    }
    let body = if is_chunked {
        read_chunked(&mut response).map_err(|error| format_http_error(&error))?
    } else if let Some(length) = content_length {
        let mut body = vec![0; length];
        response
            .read_exact(&mut body)
            .map_err(|error| format_http_error(&error))?;
        body
    } else {
        let mut body = Vec::new();
        response
            .read_to_end(&mut body)
            .map_err(|error| format_http_error(&error))?;
        body
    };
    serde_json::from_slice(&body).map_err(|error| format!("invalid JSON response: {error}"))
}

fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let size_text = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if size == 0 {
            loop {
                let mut trailer = String::new();
                reader.read_line(&mut trailer)?;
                if trailer == "\r\n" || trailer == "\n" || trailer.is_empty() {
                    return Ok(body);
                }
            }
        }
        let old_length = body.len();
        body.resize(old_length + size, 0);
        reader.read_exact(&mut body[old_length..])?;
        let mut ending = [0; 2];
        reader.read_exact(&mut ending)?;
        if ending != *b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid chunk ending",
            ));
        }
    }
}

fn format_http_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => match error.raw_os_error() {
            Some(errno) => {
                let message = io::Error::from_raw_os_error(errno).to_string();
                let suffix = format!(" (os error {errno})");
                let message = message.strip_suffix(&suffix).unwrap_or(&message);
                format!("<urlopen error [Errno {errno}] {message}>")
            }
            None => format!("<urlopen error {error}>"),
        },
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            "<urlopen error timed out>".to_owned()
        }
        _ => format!("<urlopen error {error}>"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_port;
    use serde_json::Value;
    use std::io::{BufRead, Write};
    use std::time::Duration;

    #[test]
    fn formatter_matches_json_golden() {
        let input = include_str!("../tests/fixtures/formatter-input.json");
        let expected = include_str!("../tests/fixtures/formatter-output.txt");
        let parsed: Value = serde_json::from_str(input).expect("test fixture is valid JSON");
        assert_eq!(
            format!(
                "{}\n",
                serde_json::to_string_pretty(&parsed).expect("fixture serializes")
            ),
            expected
        );
        assert_eq!(parsed["i64-min"].as_i64(), Some(i64::MIN));
        assert_eq!(parsed["u64-max"].as_u64(), Some(u64::MAX));
        assert_eq!(
            parsed["u64-overflow"].as_f64(),
            Some(18_446_744_073_709_551_616_f64)
        );
        assert_eq!(
            parsed["u64-overflow-plus-one"].as_f64(),
            Some(18_446_744_073_709_551_616_f64)
        );
        assert_eq!(
            parsed["i64-underflow"].as_f64(),
            Some(-9_223_372_036_854_775_809_f64)
        );
    }

    #[test]
    fn stalled_loopback_response_obeys_the_configured_timeout() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind timeout fixture");
        let port = listener
            .local_addr()
            .expect("read timeout fixture port")
            .port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept timeout fixture");
            let mut request = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            request.read_line(&mut line).expect("read request line");
            std::thread::sleep(Duration::from_millis(50));
            if let Err(error) = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            {
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ));
            }
        });
        let result = super::fetch_diagnostics_with_timeout(port, Duration::from_millis(5));
        assert!(
            result
                .expect_err("stalled response should time out")
                .contains("timed out")
        );
        server.join().expect("timeout fixture completed");
    }

    #[test]
    fn timeout_is_a_request_error_with_clear_stderr_text() {
        let error = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        assert_eq!(
            super::format_http_error(&error),
            "<urlopen error timed out>"
        );
    }

    #[test]
    fn parser_accepts_port_and_help_and_reports_invalid_arguments() {
        assert_eq!(
            parse_port(["--port".into(), "1234".into()].into_iter()).expect("valid test arguments"),
            Some(1234)
        );
        assert_eq!(
            parse_port(["--port=4321".into()].into_iter()).expect("valid test arguments"),
            Some(4321)
        );
        assert_eq!(
            parse_port(["--help".into()].into_iter()).expect("valid test arguments"),
            None
        );
        assert!(parse_port(["--port".into(), "x".into()].into_iter()).is_err());
    }
}
