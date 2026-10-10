use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::thread;

const RESPONSE: &[u8] = include_bytes!("fixtures/formatter-input.json");
const EXPECTED: &[u8] = include_bytes!("fixtures/formatter-output.txt");

fn fixture_server(request_count: usize) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let task = thread::spawn(move || {
        for _ in 0..request_count {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            serve(&mut stream);
        }
    });
    (port, task)
}

fn serve(stream: &mut TcpStream) {
    let mut request = BufReader::new(stream.try_clone().expect("clone fixture stream"));
    loop {
        let mut line = String::new();
        request.read_line(&mut line).expect("read fixture request");
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    let split = RESPONSE.len() / 2;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
        split
    )
    .expect("write first chunk header");
    stream
        .write_all(&RESPONSE[..split])
        .expect("write first chunk");
    write!(stream, "\r\n{:x}\r\n", RESPONSE.len() - split).expect("write second chunk header");
    stream
        .write_all(&RESPONSE[split..])
        .expect("write second chunk");
    stream
        .write_all(b"\r\n0\r\n\r\n")
        .expect("finish chunked response");
}

fn fixed_response_server(
    status: &'static str,
    body: &'static [u8],
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept fixture request");
        let mut request = BufReader::new(stream.try_clone().expect("clone fixture stream"));
        loop {
            let mut line = String::new();
            request.read_line(&mut line).expect("read fixture request");
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write fixed response headers");
        stream.write_all(body).expect("write fixed response body");
    });
    (port, task)
}

fn command(port: u16) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codex-diagnostics"))
        .args(["--port", &port.to_string()])
        .output()
        .expect("run Rust diagnostics binary")
}

#[test]
fn fixture_output_matches_python_golden_and_optional_python_cli() {
    let python = std::env::var_os("PYTHON")
        .and_then(|path| {
            Command::new(path)
                .args(["--version"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|_| std::env::var_os("PYTHON").expect("checked Python path"))
        })
        .or_else(|| {
            ["python3", "python"]
                .into_iter()
                .find(|name| {
                    Command::new(name)
                        .arg("--version")
                        .output()
                        .is_ok_and(|output| output.status.success())
                })
                .map(Into::into)
        });
    let comparisons = usize::from(python.is_some()) + 1;
    let (port, server) = fixture_server(comparisons);
    let rust = command(port);
    assert!(
        rust.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rust.stderr)
    );
    assert_eq!(rust.stdout, EXPECTED);
    let printed = std::str::from_utf8(&rust.stdout).expect("CLI UTF-8 JSON output");
    assert!(printed.contains("\"dup\": 2"));
    assert!(printed.contains("\"empty-array\": []"));
    assert!(printed.contains("\"big\": 1234567890123456789012345678901234567890"));

    if let Some(python) = python {
        let script = "import json,sys,urllib.request; data=json.load(urllib.request.urlopen('http://127.0.0.1:'+sys.argv[1]+'/api/diagnostics', timeout=30)); print(json.dumps(data, indent=2, sort_keys=True))";
        let output = Command::new(python)
            .args(["-c", script, &port.to_string()])
            .output()
            .expect("run Python compatibility oracle");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(rust.stdout, output.stdout);
    } else {
        eprintln!(
            "SKIP: optional Python byte-comparison oracle unavailable; Rust golden oracle passed"
        );
    }
    server.join().expect("fixture server completed");
}

#[test]
fn cli_help_and_errors_use_expected_status_codes() {
    let help = Command::new(env!("CARGO_BIN_EXE_codex-diagnostics"))
        .arg("--help")
        .output()
        .expect("run help");
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--port PORT"));

    let invalid = Command::new(env!("CARGO_BIN_EXE_codex-diagnostics"))
        .args(["--port", "not-a-number"])
        .output()
        .expect("run invalid port");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid int value"));
}

#[test]
fn content_length_non_200_and_invalid_json_responses_are_handled() {
    let (port, server) = fixed_response_server("200 OK", RESPONSE);
    let output = command(port);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, EXPECTED);
    server.join().expect("content-length fixture completed");

    let (port, server) = fixed_response_server("503 Service Unavailable", b"unavailable");
    let output = command(port);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("HTTP Error 503"));
    assert!(output.stdout.is_empty());
    server.join().expect("status fixture completed");

    let (port, server) = fixed_response_server("200 OK", b"not JSON");
    let output = command(port);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid JSON response"));
    assert!(output.stdout.is_empty());
    server.join().expect("invalid JSON fixture completed");
}

#[test]
fn connection_refused_uses_request_failure_status() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind refused port");
    let port = listener.local_addr().expect("read refused port").port();
    drop(listener);
    let output = command(port);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Connection refused"));
    assert!(output.stdout.is_empty());
}
