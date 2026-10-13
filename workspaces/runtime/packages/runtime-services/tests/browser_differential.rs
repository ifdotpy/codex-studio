use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use studio_runtime_services::browser::{
    BrowserConfigSnapshot, BrowserError, BrowserForwarder, ExchangeOutcome, ForwardInput,
    PairedExchange, prepare_browser_config,
};

#[path = "support/prepared_python.rs"]
mod prepared_python;

type ExchangeCall = (String, String, Value, String, i64);

#[derive(Default)]
struct Exchange {
    calls: Mutex<Vec<ExchangeCall>>,
    outcome: Mutex<Option<ExchangeOutcome>>,
}

impl PairedExchange for Exchange {
    fn exchange(
        &self,
        server: &str,
        action: &str,
        payload: &Value,
        request_id: &str,
        now: i64,
    ) -> Result<ExchangeOutcome, BrowserError> {
        self.calls
            .lock()
            .expect("valid browser differential fixture")
            .push((
                server.into(),
                action.into(),
                payload.clone(),
                request_id.into(),
                now,
            ));
        self.outcome
            .lock()
            .expect("valid browser differential fixture")
            .take()
            .ok_or(BrowserError::Exchange)
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
        "body-bound" => json!({"status":200,"body":BASE64.encode(vec![b'x'; 256*1024+1])}),
        "encoded-body-bound" => {
            json!({"status":200,"body":"A".repeat((256_usize*1024).div_ceil(3)*4+1)})
        }
        _ => json!({"status":200,"contentType":"text/plain; charset=iso-8859-1",
                    "body":"YWNjZXB0ZWQ=","done":true}),
    };
    if name == "unknown" {
        *exchange
            .outcome
            .lock()
            .expect("valid browser differential fixture") = Some(ExchangeOutcome::Unknown);
    } else {
        *exchange
            .outcome
            .lock()
            .expect("valid browser differential fixture") = Some(ExchangeOutcome::Applied(result));
    }
    let forwarder = BrowserForwarder::new(exchange.clone());
    let receipt = forwarder
        .open(input(), 1000.0, Some(0))
        .expect("open test loopback forward");
    let response = match name {
        "path-refusal" => local_request(
            receipt.port,
            "GET",
            "/wrong?code=do-not-forward",
            "User-Agent: test\r\n",
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
    let calls = exchange.calls.lock().expect("valid browser differential fixture").iter().map(|(server, action, payload, request_id, now)| {
        json!({"server":server,"action":action,"payload":payload,"requestId":request_id,"now":now})
    }).collect::<Vec<_>>();
    forwarder
        .close(&input().bridge_id)
        .expect("close browser differential listener");
    if response.is_empty() {
        return (Value::Null, calls);
    }
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("valid browser differential fixture");
    let status = head
        .lines()
        .next()
        .expect("valid browser differential fixture")
        .split_whitespace()
        .nth(1)
        .expect("valid browser differential fixture")
        .parse::<u16>()
        .expect("valid browser differential fixture");
    let mut headers = serde_json::Map::new();
    for line in head.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.to_ascii_lowercase(), json!(value.trim()));
        }
    }
    (
        json!({"status":status,"headers":headers,"body":body}),
        calls,
    )
}

#[test]
fn python_browser_corpus_matches_rust_and_config_decisions() {
    let oracle = python_corpus();
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
                Some("enabled-default" | "plugins-disabled" | "invalid-services")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(forward_cases.len(), if full { 9 } else { 8 });
    assert_eq!(config_cases.len(), if full { 9 } else { 3 });
    for case in forward_cases {
        let (actual, rust_calls) = rust_response(case);
        assert!(
            actual == case["python"]["response"],
            "browser forward response differs"
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
        if let Some(expected) = calls.first() {
            assert_eq!(rust_calls[0]["server"], expected["server"]);
            assert_eq!(rust_calls[0]["action"], expected["action"]);
            assert!(
                rust_calls[0]["payload"] == expected["payload"],
                "callback payload differs"
            );
            let identity = rust_calls[0]["requestId"]
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
        }
    }
    for case in config_cases {
        let target = &case["target"];
        let shared = &case["shared"];
        let decision = prepare_browser_config(BrowserConfigSnapshot {
            target,
            shared,
            home: case["home"]
                .as_str()
                .expect("valid browser differential fixture"),
            shared_home: case["sharedHome"]
                .as_str()
                .expect("valid browser differential fixture"),
            skill_available: case["skillAvailable"]
                .as_bool()
                .expect("valid browser differential fixture"),
            node_runtime_available: case["nodeRuntimeAvailable"]
                .as_bool()
                .expect("valid browser differential fixture"),
            trusted_services_valid: case["trustedServicesValid"]
                .as_bool()
                .expect("valid browser differential fixture"),
            browser_service_available: case["browserServiceAvailable"]
                .as_bool()
                .expect("valid browser differential fixture"),
        });
        let mut python_overrides = json!({});
        if let Some(node) = case["python"]["overrides"].get("mcp_servers.node_repl") {
            python_overrides = json!({"mcp_servers":{"node_repl":node}});
        }
        if decision.overrides != python_overrides {
            let mut paths = Vec::new();
            differing_paths(&decision.overrides, &python_overrides, "", &mut paths);
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

    let forwarder = BrowserForwarder::new(Arc::new(Exchange::default()));
    let mut expired = input();
    expired.expires_at = 999.0;
    assert!(matches!(
        forwarder.open(expired, 1000.0, Some(0)),
        Err(BrowserError::Expired)
    ));
    let first = forwarder.open(input(), 1000.0, Some(0)).expect("open");
    let replay = forwarder.open(input(), 1000.0, Some(0)).expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.port, first.port);
    let mut conflict = input();
    conflict.bridge_id = "1123456789abcdef0123456789abcdef".into();
    assert!(matches!(
        forwarder.open(conflict, 1000.0, Some(0)),
        Err(BrowserError::PortInUse(43210))
    ));
    forwarder.close(&input().bridge_id).expect("close listener");
}
