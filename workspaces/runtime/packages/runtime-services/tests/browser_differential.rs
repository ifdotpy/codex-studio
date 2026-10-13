use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use std::future::Future;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use studio_runtime_services::browser::{
    BrowserConfigSnapshot, BrowserError, BrowserForwarder, ExchangeOutcome, ForwardInput,
    PairedExchange, prepare_browser_config,
};
use tokio::time::Instant;

#[path = "support/prepared_python.rs"]
mod prepared_python;

type ExchangeCall = (String, String, Value, String, i64);

#[derive(Default)]
struct Exchange {
    calls: Mutex<Vec<ExchangeCall>>,
    outcomes: Mutex<Vec<ExchangeOutcome>>,
}

impl PairedExchange for Exchange {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        action: &'a str,
        payload: &'a Value,
        request_id: &'a str,
        now: i64,
        _deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.lock().expect("browser fixture").push((
                server.into(),
                action.into(),
                payload.clone(),
                request_id.into(),
                now,
            ));
            let mut outcomes = self.outcomes.lock().expect("browser fixture");
            if outcomes.is_empty() {
                Err(BrowserError::Exchange)
            } else {
                Ok(outcomes.remove(0))
            }
        })
    }
}

fn python_corpus() -> Value {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/browser_bridge/oracle.py"
    );
    let output = Command::new(python)
        .arg(script)
        .output()
        .expect("run guarded Python browser oracle");
    assert!(output.status.success(), "guarded browser oracle failed");
    serde_json::from_slice(&output.stdout).expect("oracle emits complete JSON outputs")
}

fn input() -> ForwardInput {
    ForwardInput {
        server: "peer".into(),
        bridge_id: "0123456789abcdef0123456789abcdef".into(),
        token: "0123456789abcdef".into(),
        host: "127.0.0.1".into(),
        port: 43210,
        path: "/oauth/callback".into(),
        expires_at: 2000.0,
    }
}

fn local_request(port: u16, method: &str, target: &str, headers: &str) -> String {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("loopback listener");
    write!(stream, "{method} {target} HTTP/1.1\r\n{headers}\r\n")
        .expect("valid browser differential fixture");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("valid browser differential fixture");
    response
}

fn local_raw_request(port: u16, raw: &[u8]) -> String {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("loopback listener");
    stream.write_all(raw).expect("write raw callback request");
    if !raw.windows(4).any(|window| window == b"\r\n\r\n") {
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("finish partial request");
    }
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read callback response");
    response
}

fn local_chunked_request(port: u16, chunks: &[&[u8]]) -> String {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("loopback listener");
    stream.set_nodelay(true).expect("disable Nagle for chunks");
    for chunk in chunks {
        stream.write_all(chunk).expect("write request fragment");
    }
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read callback response");
    response
}

fn response_value(response: &str) -> Value {
    if response.is_empty() {
        return Value::Null;
    }
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response head");
    let status = head
        .lines()
        .next()
        .expect("HTTP status line")
        .split_whitespace()
        .nth(1)
        .expect("HTTP status")
        .parse::<u16>()
        .expect("numeric status");
    let mut headers = serde_json::Map::new();
    for line in head.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.to_ascii_lowercase(), json!(value.trim()));
        }
    }
    json!({"status":status,"headers":headers,"body":body})
}

fn loopback_listener_closed(runtime: &tokio::runtime::Runtime, port: u16) -> bool {
    runtime.block_on(async {
        for _ in 0..64 {
            match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => return true,
                Err(_) => {}
                Ok(stream) => drop(stream),
            }
            tokio::task::yield_now().await;
        }
        false
    })
}

fn differing_paths(left: &Value, right: &Value, prefix: &str, out: &mut Vec<String>) {
    if left == right {
        return;
    }
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys()) {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => differing_paths(x, y, &path, out),
                    _ => out.push(path),
                }
            }
        }
        _ => out.push(prefix.to_owned()),
    }
}

fn rust_response(case: &Value) -> (Value, Vec<Value>) {
    let name = case["name"]
        .as_str()
        .expect("valid browser differential fixture");
    let exchange = Arc::new(Exchange::default());
    let result = match name {
        "redirect-response" => json!({"status":302,"location":"https://private.invalid/"}),
        "redirect-done" => json!({"status":302,"done":true}),
        "fragmented-complete-head" => {
            json!({"status":200,"contentType":"text/plain; charset=iso-8859-1","body":"YWNjZXB0ZWQ=","done":true})
        }
        "duplicate-content-length" | "get-body" => {
            json!({"status":200,"contentType":"text/plain; charset=iso-8859-1","body":"YWNjZXB0ZWQ=","done":true})
        }
        "body-type-refusal" => json!({"status":200,"body":{}}),
        "charset-contenttype" => json!({
            "status":200,
            "contentType":"text/plain; charset=iso-8859-1 extra",
            "body":"YWNjZXB0ZWQ=",
            "done":true
        }),
        "body-bound" => json!({"status":200,"body":BASE64.encode(vec![b'x'; 256*1024+1])}),
        "encoded-body-bound" => {
            json!({"status":200,"body":"A".repeat(256_usize * 1024 * 4 / 3 + 1)})
        }
        _ => json!({"status":200,"contentType":"text/plain; charset=iso-8859-1",
                    "body":"YWNjZXB0ZWQ=","done":true}),
    };
    let outcomes = if name == "unknown" {
        vec![ExchangeOutcome::Unknown]
    } else if name == "two-valid-relays" {
        vec![
            ExchangeOutcome::Applied(json!({"status":200,"body":"Zmlyc3Q=","done":false})),
            ExchangeOutcome::Applied(json!({"status":200,"body":"c2Vjb25k","done":true})),
        ]
    } else if name == "fifth-relay" {
        (0..5)
            .map(|index| {
                ExchangeOutcome::Applied(json!({
                    "status":200,"body":"b2s=","done":false,"ordinal":index
                }))
            })
            .collect()
    } else {
        vec![ExchangeOutcome::Applied(result)]
    };
    *exchange.outcomes.lock().expect("browser outcomes") = outcomes;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    let forwarder = BrowserForwarder::new(exchange.clone());
    let receipt = runtime
        .block_on(forwarder.open(input(), 1000.0, Some(0)))
        .expect("open test loopback forward");
    let responses = if name == "incomplete-then-valid" {
        vec![
            local_raw_request(
                receipt.port,
                b"GET /oauth/callback HTTP/1.1\r\nUser-Agent: incomplete",
            ),
            local_request(receipt.port, "GET", "/oauth/callback?code=example-code", ""),
        ]
    } else if name == "two-valid-relays" {
        vec![
            local_request(receipt.port, "GET", "/oauth/callback?code=first", ""),
            local_request(receipt.port, "GET", "/oauth/callback?code=second", ""),
        ]
    } else if name == "fifth-relay" {
        (0..5)
            .map(|_| local_request(receipt.port, "GET", "/oauth/callback?code=relay", ""))
            .collect()
    } else {
        let response = match name {
            "duplicate-user-agent" => local_raw_request(
            receipt.port,
            &format!("GET /oauth/callback?code=example-code HTTP/1.1\r\nUser-Agent: {}\r\nUser-Agent: short\r\n\r\n", "x".repeat(1025)).into_bytes(),
        ),
            "user-agent-latin1-bound" => {
                let mut raw = b"GET /oauth/callback?code=example-code HTTP/1.1\r\nUser-Agent: ".to_vec();
                raw.extend_from_slice(&[0xe9; 1024]);
                raw.extend_from_slice(b"\r\n\r\n");
                local_raw_request(receipt.port, &raw)
            }
            "path-refusal" => local_request(
                receipt.port,
                "GET",
                "/wrong?code=do-not-forward",
                "User-Agent: test\r\n",
            ),
            "fragmented-complete-head" => local_chunked_request(
                receipt.port,
                &[
                    b"GET /oauth/callback?code=fragmented HTTP/1.1\r\nUser-Agent: frag",
                    b"mented-agent\r\nAccept: text/html\r\n\r\n",
                ],
            ),
            "duplicate-content-length" => local_raw_request(
                receipt.port,
                b"GET /oauth/callback?code=duplicate HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            ),
            "get-body" => local_raw_request(
                receipt.port,
                b"GET /oauth/callback?code=body HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc",
            ),
            "method-refusal" => local_request(
                receipt.port,
                "POST",
                "/oauth/callback",
                "User-Agent: test\r\n",
            ),
            "header-bound" => local_request(
                receipt.port,
                "GET",
                "/oauth/callback",
                &format!("User-Agent: {}\r\nAccept: text/html\r\n", "x".repeat(1025)),
            ),
            "head-bound" => local_request(
                receipt.port,
                "GET",
                "/oauth/callback",
                &format!("X-Test: {}\r\n", "x".repeat(16 * 1024)),
            ),
            _ => local_request(
                receipt.port,
                "GET",
                "/oauth/callback?code=example-code",
                "User-Agent: test-agent\r\nAccept: text/html\r\nAccept-Language: en\r\nCookie: secret-cookie\r\nAuthorization: secret-auth\r\n",
            ),
        };
        vec![response]
    };
    let calls = exchange.calls.lock().expect("valid browser differential fixture").iter().map(|(server, action, payload, request_id, now)| {
        json!({"server":server,"action":action,"payload":payload,"requestId":request_id,"now":now})
    }).collect::<Vec<_>>();
    let closed = loopback_listener_closed(&runtime, receipt.port);
    runtime
        .block_on(forwarder.close(&input().bridge_id))
        .expect("close browser differential listener");
    (
        json!({"responses":responses.iter().map(|response| response_value(response)).collect::<Vec<_>>(),"closed":closed}),
        calls,
    )
}

#[test]
fn python_browser_corpus_matches_rust_and_config_decisions() {
    let oracle = python_corpus();
    assert_eq!(oracle["lifecycle"]["expiredClosed"], true);
    assert_eq!(oracle["lifecycle"]["eightAdmitted"], 8);
    assert_eq!(oracle["lifecycle"]["ninthClosed"], true);
    assert_eq!(oracle["lifecycle"]["exchangeEntered"], true);
    assert_eq!(oracle["lifecycle"]["responseAfterClose"], true);
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let forward_cases = oracle["forward"]
        .as_array()
        .expect("valid browser differential fixture")
        .iter()
        .filter(|case| {
            full || matches!(
                case["name"].as_str(),
                Some(
                    "allowed-headers"
                        | "path-refusal"
                        | "header-bound"
                        | "head-bound"
                        | "redirect-response"
                        | "body-bound"
                        | "encoded-body-bound"
                        | "unknown"
                        | "incomplete-then-valid"
                        | "two-valid-relays"
                        | "redirect-done"
                        | "duplicate-user-agent"
                        | "body-type-refusal"
                        | "charset-contenttype"
                        | "user-agent-latin1-bound"
                        | "fragmented-complete-head"
                        | "duplicate-content-length"
                        | "get-body"
                        | "fifth-relay"
                )
            )
        })
        .collect::<Vec<_>>();
    let config_cases = oracle["config"]
        .as_array()
        .expect("valid browser differential fixture")
        .iter()
        .filter(|case| {
            full || matches!(
                case["name"].as_str(),
                Some("enabled-default" | "plugins-disabled" | "invalid-services" | "account-node")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(forward_cases.len(), if full { 20 } else { 19 });
    assert_eq!(config_cases.len(), if full { 10 } else { 4 });
    for case in forward_cases {
        let (actual, rust_calls) = rust_response(case);
        assert_eq!(
            actual["responses"], case["python"]["responses"],
            "browser forward responses differ for {}",
            case["name"]
        );
        assert!(
            actual["closed"] == case["python"]["closed"],
            "browser listener lifecycle differs"
        );
        let calls = case["python"]["calls"]
            .as_array()
            .expect("valid browser differential fixture");
        assert!(
            case["python"]["open"] == json!({"status":"forwarding"}),
            "open receipt differs"
        );
        assert_eq!(
            rust_calls.len(),
            calls.len(),
            "paired exchange count differs"
        );
        let mut identities = std::collections::HashSet::new();
        for (actual, expected) in rust_calls.iter().zip(calls) {
            assert_eq!(actual["server"], expected["server"]);
            assert_eq!(actual["action"], expected["action"]);
            assert_eq!(
                actual["payload"], expected["payload"],
                "complete callback payload differs"
            );
            let identity = actual["requestId"]
                .as_str()
                .expect("valid browser differential fixture");
            let random = identity
                .strip_prefix("browser-callback:")
                .unwrap_or_default();
            assert!(
                random.len() == 32
                    && random
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "callback identity format differs"
            );
            assert!(
                identities.insert(identity),
                "each relay has a fresh request identity"
            );
        }
    }
    for case in config_cases {
        let target = &case["target"];
        let shared = &case["shared"];
        let assets: studio_runtime_services::browser::BrowserConfigAssets =
            serde_json::from_value(case["assets"].clone()).expect("asset facts are typed");
        let snapshot = BrowserConfigSnapshot::from_stored_records(
            target,
            shared,
            case["home"].as_str().expect("account home path"),
            case["sharedHome"].as_str().expect("shared home path"),
            assets,
        )
        .expect("stored records form a checked config snapshot");
        let decision = prepare_browser_config(snapshot);
        let python_overrides = &case["python"]["overrides"];
        if &decision.overrides != python_overrides {
            let mut paths = Vec::new();
            differing_paths(&decision.overrides, python_overrides, "", &mut paths);
            paths.sort();
            paths.dedup();
            panic!(
                "browser config differs for {} at {paths:?}",
                case["name"]
                    .as_str()
                    .expect("valid browser differential fixture")
            );
        }
        assert!(
            decision.unavailable_reason == case["python"]["reason"].as_str(),
            "browser config reason differs"
        );
        assert!(
            decision.developer_instructions == case["python"]["developerInstructions"].as_str(),
            "browser guidance differs"
        );
    }
    let fixture_root = oracle["fixtureRoot"].as_str().expect("oracle fixture root");
    std::fs::remove_dir_all(fixture_root).expect("remove tmpfs Python config fixtures");
    let exchange = &oracle["pairedExchange"];
    assert_eq!(
        exchange["calls"]
            .as_array()
            .expect("valid browser differential fixture")
            .len(),
        4
    );
    let queued = &exchange["calls"][0];
    assert_eq!(queued["phase"], "queue");
    assert_eq!(queued["action"], "browser_callback");
    assert!(
        queued["requestId"]
            .as_str()
            .expect("valid browser differential fixture")
            .starts_with("browser-callback:")
    );
    assert_eq!(exchange["calls"][1]["phase"], "deliver");
    assert_eq!(exchange["calls"][2]["phase"], "queue");
    assert_ne!(queued["requestId"], exchange["calls"][2]["requestId"]);
    assert_eq!(oracle["expired"]["type"], "ValueError");
    assert_eq!(
        oracle["conflict"]
            .as_array()
            .expect("valid browser differential fixture")
            .len(),
        1
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    let forwarder = BrowserForwarder::new(Arc::new(Exchange::default()));
    let mut expired = input();
    expired.expires_at = 999.0;
    assert!(matches!(
        runtime.block_on(forwarder.open(expired, 1000.0, Some(0))),
        Err(BrowserError::Expired)
    ));
    let first = runtime
        .block_on(forwarder.open(input(), 1000.0, Some(0)))
        .expect("open");
    let replay = runtime
        .block_on(forwarder.open(input(), 1000.0, Some(0)))
        .expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.port, first.port);
    let mut conflict = input();
    conflict.bridge_id = "1123456789abcdef0123456789abcdef".into();
    assert!(matches!(
        runtime.block_on(forwarder.open(conflict, 1000.0, Some(0))),
        Err(BrowserError::PortInUse(43210))
    ));
    runtime
        .block_on(forwarder.close(&input().bridge_id))
        .expect("close listener");
}
