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
        Ok(value) => {
            let mut output = String::new();
            format_python_json(&value, 0, &mut output);
            println!("{output}");
        }
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

fn format_python_json(value: &Value, depth: usize, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(number) => {
            let raw = number.to_string();
            if raw.contains(['.', 'e', 'E']) {
                if let Ok(float) = raw.parse::<f64>() {
                    output.push_str(&python_float(float));
                } else {
                    output.push_str(&raw);
                }
            } else if raw == "-0" {
                output.push('0');
            } else {
                output.push_str(&raw);
            }
        }
        Value::String(value) => format_string(value, output),
        Value::Array(values) => {
            if values.is_empty() {
                output.push_str("[]");
            } else {
                output.push_str("[\n");
                for (index, value) in values.iter().enumerate() {
                    indent(depth + 1, output);
                    format_python_json(value, depth + 1, output);
                    if index + 1 < values.len() {
                        output.push(',');
                    }
                    output.push('\n');
                }
                indent(depth, output);
                output.push(']');
            }
        }
        Value::Object(values) => {
            if values.is_empty() {
                output.push_str("{}");
            } else {
                output.push_str("{\n");
                for (index, (key, value)) in values.iter().enumerate() {
                    indent(depth + 1, output);
                    format_string(key, output);
                    output.push_str(": ");
                    format_python_json(value, depth + 1, output);
                    if index + 1 < values.len() {
                        output.push(',');
                    }
                    output.push('\n');
                }
                indent(depth, output);
                output.push('}');
            }
        }
    }
}

fn indent(depth: usize, output: &mut String) {
    for _ in 0..depth * 2 {
        output.push(' ');
    }
}

fn format_string(value: &str, output: &mut String) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character
                if (character as u32) <= 0x1f || (0x7f..=0xff).contains(&(character as u32)) =>
            {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character if character.is_ascii() => output.push(character),
            character if (character as u32) <= 0xffff => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => {
                let scalar = character as u32 - 0x1_0000;
                let high = 0xd800 + (scalar >> 10);
                let low = 0xdc00 + (scalar & 0x3ff);
                output.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
        }
    }
    output.push('"');
}

fn python_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }
    let absolute = value.abs();
    let mut rendered = if !(1e-4..1e16).contains(&absolute) {
        format!("{value:?}")
    } else {
        let decimal = value.to_string();
        if decimal.contains(['.', 'e', 'E']) {
            decimal
        } else {
            format!("{decimal}.0")
        }
    };
    rendered = normalize_exponent(rendered);
    rendered
}

fn normalize_exponent(mut rendered: String) -> String {
    let Some(index) = rendered.find('e') else {
        return rendered;
    };
    let exponent = rendered[index + 1..].parse::<i32>().unwrap_or(0);
    rendered.truncate(index);
    rendered.push('e');
    if exponent >= 0 {
        rendered.push('+');
    } else {
        rendered.push('-');
    }
    let digits = exponent.unsigned_abs();
    if digits < 10 {
        rendered.push('0');
    }
    rendered.push_str(&digits.to_string());
    rendered
}

#[cfg(test)]
mod tests {
    use super::{format_python_json, parse_port, python_float};
    use serde_json::Value;
    use std::io::{BufRead, Write};
    use std::time::Duration;

    #[test]
    fn formatter_matches_python_json_dump_golden() {
        let input = include_str!("../tests/fixtures/formatter-input.json");
        let expected = include_str!("../tests/fixtures/formatter-output.txt");
        let parsed: Value = serde_json::from_str(input).expect("test fixture is valid JSON");
        let mut actual = String::new();
        format_python_json(&parsed, 0, &mut actual);
        assert_eq!(format!("{actual}\n"), expected);
    }

    #[test]
    fn formatter_preserves_python_float_spellings() {
        for (input, expected) in [
            ("1.0", "1.0"),
            ("-0.0", "-0.0"),
            ("1e+06", "1000000.0"),
            ("1e+16", "1e+16"),
            ("1e-05", "1e-05"),
            ("1e-04", "0.0001"),
            ("1e400", "Infinity"),
            ("-1e400", "-Infinity"),
            ("1e-4000", "0.0"),
        ] {
            let value = input.parse::<f64>().expect("valid float literal");
            assert_eq!(python_float(value), expected, "{input}");
        }
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
