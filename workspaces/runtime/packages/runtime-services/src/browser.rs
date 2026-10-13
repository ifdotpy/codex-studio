//! Browser sign-in forwarding and pure thread configuration.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const FORWARD_SECONDS: u64 = 600;
const MAX_RELAYS: usize = 5;
const HEAD_LIMIT: usize = 16 * 1024;
const BODY_LIMIT: usize = 256 * 1024;
const BODY_BASE64_LIMIT: usize = BODY_LIMIT.div_ceil(3) * 4;
const HEADER_LIMIT: usize = 1024;
const SENT_HEADERS: [&str; 3] = ["user-agent", "accept", "accept-language"];
const SANDBOX: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:";
const REMOTE_ACTION: &str = "browser_callback";
const NODE_REPL_ENV_KEYS: [&str; 9] = [
    "NODE_REPL_NATIVE_PIPE_CONNECT_TIMEOUT_MS",
    "NODE_REPL_NODE_MODULE_DIRS",
    "NODE_REPL_NODE_PATH",
    "NODE_REPL_TRUSTED_CODE_PATHS",
    "NODE_REPL_INSTRUCTIONS_USE_CASE_CHROME",
    "BROWSER_USE_CODEX_APP_BUILD_FLAVOR",
    "BROWSER_USE_CODEX_APP_VERSION",
    "BROWSER_USE_TINYSKY_ENABLED",
    "CODEX_CLI_PATH",
];

pub const BROWSER_GUIDANCE: &str = "For browser interaction, use the installed control-chrome skill and its browser-client through node_repl by default. Honor an explicitly requested different browser or tool. Read the native skill before use; it owns setup, permissions, tab lifecycle, and recovery. Report connection failures instead of silently changing the browser-control method. If native Chrome discovery reports no browser, finish the turn after reporting the error; Studio can refresh the connection at an idle boundary. Do not repeat any browser action with an unknown outcome. Dedicated connectors and web search remain appropriate for tasks that do not need browser interaction.";

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("the sign-in forward is invalid")]
    InvalidForward,
    #[error("the sign-in request is no longer open. Ask the agent to run the command again.")]
    Expired,
    #[error("another sign-in uses port {0} on this Mac. Wait until it ends, then approve again.")]
    PortInUse(u16),
    #[error("the sign-in forward cannot start")]
    Listener(#[from] std::io::Error),
    #[error("the paired server refused the sign-in redirect: {0}")]
    Refused(String),
    #[error("the sign-in redirect outcome is unknown; inspect Studio before retrying")]
    Unknown,
    #[error("the paired exchange failed")]
    Exchange,
    #[error("the paired exchange failed")]
    EncodedBodyLimit,
    #[error("the browser forward worker stopped unexpectedly")]
    WorkerPanicked,
}

#[derive(Debug, PartialEq)]
pub enum ExchangeOutcome {
    Applied(Value),
    NotApplied(String),
    Unknown,
}

/// One remote exchange; the implementation queues and delivers the exact fresh identity once.
pub trait PairedExchange: Send + Sync {
    fn exchange(
        &self,
        server: &str,
        action: &str,
        payload: &Value,
        request_id: &str,
        now: i64,
    ) -> Result<ExchangeOutcome, BrowserError>;
}

#[derive(Clone, PartialEq)]
pub struct ForwardInput {
    pub server: String,
    pub bridge_id: String,
    pub token: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub expires_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForwardReceipt {
    pub replayed: bool,
    pub port: u16,
}

struct ActiveForward {
    input: ForwardInput,
    port: u16,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ActiveForward {
    fn close(&mut self) -> Result<(), BrowserError> {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if !thread.is_finished() {
                if TcpStream::connect(self.address).is_err() {
                    // The accept loop is nonblocking and checks its stop flag every 20 ms.
                }
                thread.thread().unpark();
            }
            thread.join().map_err(|_| BrowserError::WorkerPanicked)?;
        }
        Ok(())
    }
}

impl Drop for ActiveForward {
    fn drop(&mut self) {
        if let Err(_error) = self.close() {
            // Drop cannot return the worker failure; explicit close reports it to its caller.
        }
    }
}

/// Owns active loopback forwards. No browser is opened; the caller owns that UI decision.
pub struct BrowserForwarder {
    exchange: Arc<dyn PairedExchange>,
    forwards: Mutex<HashMap<String, ActiveForward>>,
}

impl BrowserForwarder {
    pub fn new(exchange: Arc<dyn PairedExchange>) -> Self {
        Self {
            exchange,
            forwards: Mutex::new(HashMap::new()),
        }
    }

    pub fn open(
        &self,
        input: ForwardInput,
        now: f64,
        listen_port: Option<u16>,
    ) -> Result<ForwardReceipt, BrowserError> {
        validate_forward(&input)?;
        let remaining = (input.expires_at - now).min(FORWARD_SECONDS as f64);
        if remaining <= 0.0 {
            return Err(BrowserError::Expired);
        }
        if !now.is_finite() {
            return Err(BrowserError::InvalidForward);
        }
        let mut forwards = self.forwards.lock().map_err(|_| BrowserError::Exchange)?;
        forwards.retain(|_, forward| {
            forward
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
        });
        if let Some(existing) = forwards.get(&input.bridge_id)
            && !existing.stop.load(Ordering::Acquire)
            && existing
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
        {
            return Ok(ForwardReceipt {
                replayed: true,
                port: existing.port,
            });
        }
        if forwards.values().any(|f| {
            f.input.port == input.port
                && f.thread
                    .as_ref()
                    .is_some_and(|thread| !thread.is_finished())
        }) {
            return Err(BrowserError::PortInUse(input.port));
        }
        let (listeners, address) =
            bind_loopback(&input.host, listen_port.unwrap_or(input.port), input.port)?;
        let port = address.port();
        for listener in &listeners {
            listener.set_nonblocking(true)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let relays = Arc::new(AtomicUsize::new(0));
        let thread_input = input.clone();
        let thread_exchange = Arc::clone(&self.exchange);
        let thread_stop = Arc::clone(&stop);
        let thread_relays = Arc::clone(&relays);
        let thread = thread::Builder::new()
            .name("studio-browser-forward".into())
            .spawn(move || {
                serve_forward(
                    listeners,
                    thread_input,
                    thread_exchange,
                    thread_stop,
                    thread_relays,
                    remaining,
                )
            })?;
        forwards.insert(
            input.bridge_id.clone(),
            ActiveForward {
                input,
                port,
                address,
                stop,
                thread: Some(thread),
            },
        );
        Ok(ForwardReceipt {
            replayed: false,
            port,
        })
    }

    pub fn close(&self, bridge_id: &str) -> Result<bool, BrowserError> {
        let forward = self
            .forwards
            .lock()
            .map_err(|_| BrowserError::Exchange)?
            .remove(bridge_id);
        if let Some(mut forward) = forward {
            forward.close()?;
            return Ok(true);
        }
        Ok(false)
    }
}

fn validate_forward(input: &ForwardInput) -> Result<(), BrowserError> {
    let host_valid = matches!(input.host.as_str(), "127.0.0.1" | "localhost" | "::1");
    let id_valid = input.bridge_id.len() == 32
        && input
            .bridge_id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if input.server.is_empty()
        || input.server.len() > 200
        || !id_valid
        || input.token.len() < 16
        || input.token.len() > 200
        || !host_valid
        || input.port < 1024
        || input.path.is_empty()
        || input.path.len() > 2048
        || !input.path.starts_with('/')
        || !input.expires_at.is_finite()
    {
        return Err(BrowserError::InvalidForward);
    }
    Ok(())
}

fn bind_loopback(
    host: &str,
    port: u16,
    redirect_port: u16,
) -> Result<(Vec<TcpListener>, SocketAddr), BrowserError> {
    let addresses = match host {
        "localhost" => vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
        ],
        "::1" => vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)],
        _ => vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)],
    };
    let mut listeners = Vec::new();
    let mut bound_port = port;
    let mut primary = None;
    for address in addresses {
        let address = SocketAddr::new(address.ip(), bound_port);
        match TcpListener::bind(address) {
            Ok(listener) => {
                let local = listener.local_addr()?;
                if primary.is_none() {
                    bound_port = local.port();
                    primary = Some(local);
                }
                listeners.push(listener);
            }
            Err(error)
                if host == "localhost"
                    && address.is_ipv6()
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                    ) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(BrowserError::PortInUse(redirect_port));
            }
            Err(error) => return Err(BrowserError::Listener(error)),
        }
    }
    Ok((
        listeners,
        primary.ok_or(BrowserError::PortInUse(redirect_port))?,
    ))
}

fn serve_forward(
    listeners: Vec<TcpListener>,
    input: ForwardInput,
    exchange: Arc<dyn PairedExchange>,
    stop: Arc<AtomicBool>,
    relays: Arc<AtomicUsize>,
    remaining_seconds: f64,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(remaining_seconds);
    while !stop.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        let mut accepted = None;
        for listener in &listeners {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted = Some(stream);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
        if let Some(mut stream) = accepted {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if relays.load(Ordering::Acquire) >= MAX_RELAYS {
                if !respond_best_effort(
                    &mut stream,
                    410,
                    page("This sign-in forward is closed. Return to Studio."),
                    "text/html; charset=utf-8",
                ) {
                    break;
                }
                continue;
            }
            match handle_connection(&mut stream, &input, exchange.as_ref(), &relays) {
                Ok(done) if done => break,
                Err(BrowserError::InvalidForward) => break,
                Err(_) => {
                    if !respond_best_effort(
                        &mut stream,
                        502,
                        page("The sign-in result is unknown. Check the chat in Studio."),
                        "text/html; charset=utf-8",
                    ) {
                        break;
                    }
                    break;
                }
                _ => {}
            }
            if relays.load(Ordering::Acquire) >= MAX_RELAYS {
                break;
            }
        } else {
            thread::park_timeout(Duration::from_millis(20));
        }
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    input: &ForwardInput,
    exchange: &dyn PairedExchange,
    relays: &AtomicUsize,
) -> Result<bool, BrowserError> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let head = read_head(stream)?;
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let parts = first.split(' ').collect::<Vec<_>>();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/1.") {
        respond(
            stream,
            400,
            page("Studio did not understand this request."),
            "text/html; charset=utf-8",
        )?;
        return Ok(false);
    }
    if parts[0] != "GET" {
        respond(
            stream,
            405,
            page("Studio forwards only the sign-in redirect."),
            "text/html; charset=utf-8",
        )?;
        return Ok(false);
    }
    let target = parts[1];
    let path = target.split('?').next().unwrap_or_default();
    if !target.starts_with('/') || path != input.path {
        respond(
            stream,
            404,
            page("Studio forwards only the sign-in redirect."),
            "text/html; charset=utf-8",
        )?;
        return Ok(false);
    }
    let mut headers = Map::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if SENT_HEADERS.contains(&name.as_str()) && value.len() <= HEADER_LIMIT {
                headers.entry(name).or_insert_with(|| json!(value));
            }
        }
    }
    let attempt = relays.fetch_add(1, Ordering::AcqRel) + 1;
    let request_id = fresh_request_id()?;
    let payload =
        json!({"bridgeId":input.bridge_id,"token":input.token,"target":target,"headers":headers});
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BrowserError::Exchange)?
        .as_secs() as i64;
    let result =
        match exchange.exchange(&input.server, REMOTE_ACTION, &payload, &request_id, now)? {
            ExchangeOutcome::Applied(result) => result,
            ExchangeOutcome::NotApplied(reason) => return Err(BrowserError::Refused(reason)),
            ExchangeOutcome::Unknown => return Err(BrowserError::Unknown),
        };
    let (status, content_type, body, done) = cleaned_result(&result)?;
    respond(stream, status, body, &content_type)?;
    Ok(done || attempt >= MAX_RELAYS)
}

fn fresh_request_id() -> Result<String, BrowserError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| BrowserError::Exchange)?;
    Ok(format!("browser-callback:{}", hex(&bytes)))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    result
}

fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, BrowserError> {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !data.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk)?;
        if count == 0 || data.len() + count > HEAD_LIMIT {
            return Err(BrowserError::InvalidForward);
        }
        data.extend_from_slice(&chunk[..count]);
    }
    let end = data
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or(data.len());
    data.truncate(end);
    Ok(data)
}

fn cleaned_result(result: &Value) -> Result<(u16, String, Vec<u8>, bool), BrowserError> {
    let status = result
        .get("status")
        .and_then(Value::as_u64)
        .filter(|s| (100..=599).contains(s))
        .ok_or(BrowserError::Exchange)? as u16;
    if (300..400).contains(&status) {
        return Ok((
            200,
            "text/html; charset=utf-8".into(),
            page(
                "The sign-in command answered with a redirect. Studio does not follow it. Return to Studio.",
            ),
            false,
        ));
    }
    let encoded = result
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if encoded.len() > BODY_BASE64_LIMIT {
        return Err(BrowserError::EncodedBodyLimit);
    }
    let body = BASE64.decode(encoded).map_err(|_| BrowserError::Exchange)?;
    if body.len() > BODY_LIMIT {
        return Err(BrowserError::Exchange);
    }
    let text = result
        .get("contentType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let base = text.split(';').next().unwrap_or_default().trim();
    let leading = body
        .iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace)
        .take(15)
        .collect::<Vec<_>>();
    let base = if base.is_empty()
        && (leading.to_ascii_lowercase().starts_with(b"<!doctype html")
            || leading.to_ascii_lowercase().starts_with(b"<html"))
    {
        "text/html"
    } else if matches!(base, "text/html" | "text/plain") {
        base
    } else {
        "text/plain"
    };
    let charset = text
        .split(';')
        .skip(1)
        .filter_map(|part| part.trim().strip_prefix("charset="))
        .find(|value| {
            !value.is_empty()
                && value.len() <= 40
                && value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        });
    let content_type = format!(
        "{base}; {}",
        charset
            .map(|value| format!("charset={value}"))
            .unwrap_or_else(|| "charset=utf-8".into())
    );
    Ok((
        status,
        content_type,
        body,
        result.get("done").and_then(Value::as_bool) == Some(true),
    ))
}

fn page(text: &str) -> Vec<u8> {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;");
    format!("<!doctype html><meta charset=utf-8><title>LAYR Studio</title><p>{escaped}</p>")
        .into_bytes()
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    body: Vec<u8>,
    content_type: &str,
) -> std::io::Result<()> {
    let phrase = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        410 => "Gone",
        502 => "Bad Gateway",
        _ => "Status",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {phrase}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nContent-Security-Policy: {SANDBOX}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}

fn respond_best_effort(
    stream: &mut TcpStream,
    status: u16,
    body: Vec<u8>,
    content_type: &str,
) -> bool {
    respond(stream, status, body, content_type).is_ok()
}

/// Browser-related inputs discovered by the thread preparation adapter.
pub struct BrowserConfigSnapshot<'a> {
    pub target: &'a Value,
    pub shared: &'a Value,
    pub home: &'a str,
    pub shared_home: &'a str,
    pub skill_available: bool,
    pub node_runtime_available: bool,
    pub trusted_services_valid: bool,
    pub browser_service_available: bool,
}

/// Browser overrides or a visible reason they could not be prepared.
pub struct BrowserConfigDecision {
    pub overrides: Value,
    pub unavailable_reason: Option<&'static str>,
    pub developer_instructions: Option<&'static str>,
}

/// Pure browser-session configuration. Filesystem discovery and host calls stay in the caller.
pub fn prepare_browser_config(snapshot: BrowserConfigSnapshot<'_>) -> BrowserConfigDecision {
    let BrowserConfigSnapshot {
        target,
        shared,
        home,
        shared_home,
        skill_available,
        node_runtime_available,
        trusted_services_valid,
        browser_service_available,
    } = snapshot;
    let target_node = target
        .pointer("/mcp_servers/node_repl")
        .filter(|node| !node.is_null());
    let shared_node = shared.pointer("/mcp_servers/node_repl");
    let reason = if target.pointer("/features/plugins").and_then(Value::as_bool) == Some(false) {
        Some("Plugins are disabled in this account's Codex configuration")
    } else if target
        .pointer("/plugins/chrome@openai-bundled/enabled")
        .and_then(Value::as_bool)
        == Some(false)
    {
        Some("The Chrome plugin is disabled in this account's Codex configuration")
    } else if target_node
        .and_then(|node| node.get("enabled"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        Some("node_repl is disabled in this account's Codex configuration")
    } else if !skill_available {
        Some("The control-chrome skill for the installed Chrome plugin version was not found")
    } else if !node_runtime_available {
        Some("The node_repl runtime from the Codex configuration is missing")
    } else if target_node.and_then(|node| node.get("command"))
        != shared_node.and_then(|node| node.get("command"))
        && target_node.is_some()
    {
        Some("This account uses its own node_repl runtime")
    } else if !trusted_services_valid {
        Some("NODE_REPL_TRUSTED_SERVICES is not valid JSON")
    } else if !browser_service_available {
        Some("The browser service file is missing")
    } else {
        None
    };
    let Some(reason) = reason else {
        let source = shared_node.unwrap_or(&Value::Null);
        let mut node = target_node.cloned().unwrap_or_else(|| {
            let mut value = Map::new();
            for key in ["command", "args", "startup_timeout_sec"] {
                if let Some(source_value) = source.get(key) {
                    value.insert(key.into(), source_value.clone());
                }
            }
            let env = source.pointer("/env").and_then(Value::as_object);
            let mut filtered = Map::new();
            for key in NODE_REPL_ENV_KEYS {
                if let Some(value) = env.and_then(|map| map.get(key)) {
                    filtered.insert(key.into(), value.clone());
                }
            }
            value.insert("env".into(), Value::Object(filtered));
            Value::Object(value)
        });
        let Some(object) = node.as_object_mut() else {
            return BrowserConfigDecision {
                overrides: json!({}),
                unavailable_reason: Some("node_repl configuration is invalid"),
                developer_instructions: None,
            };
        };
        let env = object.entry("env").or_insert_with(|| json!({}));
        let Some(env) = env.as_object_mut() else {
            return BrowserConfigDecision {
                overrides: json!({}),
                unavailable_reason: Some("node_repl configuration is invalid"),
                developer_instructions: None,
            };
        };
        env.insert("CODEX_HOME".into(), json!(home));
        let trusted = env
            .get("NODE_REPL_TRUSTED_CODE_PATHS")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut roots = std::env::split_paths(std::ffi::OsStr::new(trusted))
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for path in [shared_home, home] {
            if !roots.iter().any(|existing| existing == path) {
                roots.push(path.to_owned());
            }
        }
        let joined = std::env::join_paths(roots)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        env.insert("NODE_REPL_TRUSTED_CODE_PATHS".into(), json!(joined));
        env.entry("BROWSER_USE_AVAILABLE_BACKENDS")
            .or_insert_with(|| json!("chrome"));
        if env.get("NODE_REPL_TRUSTED_SERVICES").is_none()
            && let Some(service) = shared_node
                .and_then(|node| node.pointer("/env/NODE_REPL_TRUSTED_SERVICES"))
                .and_then(Value::as_str)
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .and_then(|value| value.get("browser").cloned())
        {
            let trusted = format!("{{\"browser\": {}}}", service);
            env.insert("NODE_REPL_TRUSTED_SERVICES".into(), json!(trusted));
        }
        env.entry("NODE_REPL_INSTRUCTIONS_USE_CASE_CHROME")
            .or_insert_with(|| json!(BROWSER_GUIDANCE));
        let config = json!({"mcp_servers":{"node_repl":node}});
        return BrowserConfigDecision {
            overrides: config,
            unavailable_reason: None,
            developer_instructions: Some(BROWSER_GUIDANCE),
        };
    };
    BrowserConfigDecision {
        overrides: json!({}),
        unavailable_reason: Some(reason),
        developer_instructions: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeExchange {
        outcomes: Mutex<Vec<ExchangeOutcome>>,
        calls: Mutex<Vec<ExchangeCall>>,
    }

    struct ExchangeCall {
        server: String,
        action: String,
        payload: Value,
        request_id: String,
        now: i64,
    }

    impl PairedExchange for FakeExchange {
        fn exchange(
            &self,
            server: &str,
            action: &str,
            payload: &Value,
            request_id: &str,
            now: i64,
        ) -> Result<ExchangeOutcome, BrowserError> {
            self.calls.lock().expect("test mutex").push(ExchangeCall {
                server: server.into(),
                action: action.into(),
                payload: payload.clone(),
                request_id: request_id.into(),
                now,
            });
            Ok(self.outcomes.lock().expect("test mutex").remove(0))
        }
    }

    fn input() -> ForwardInput {
        ForwardInput {
            server: "server-a".into(),
            bridge_id: "0123456789abcdef0123456789abcdef".into(),
            token: "a-token-with-enough-entropy".into(),
            host: "127.0.0.1".into(),
            port: 43210,
            path: "/oauth/callback".into(),
            expires_at: 2000.0,
        }
    }

    fn outcome(done: bool) -> ExchangeOutcome {
        ExchangeOutcome::Applied(json!({
            "status": 200,
            "contentType": "text/html; charset=iso-8859-1",
            "body": BASE64.encode(b"<html>ok</html>"),
            "done": done
        }))
    }

    fn request(port: u16) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("forward bound");
        stream.write_all(b"GET /oauth/callback?code=secret HTTP/1.1\r\nUser-Agent: test-agent\r\nAccept: text/html\r\nCookie: private\r\nAuthorization: private\r\n\r\n").expect("request writes");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("response reads");
        response
    }

    #[test]
    fn listener_relays_only_the_checked_path_and_allowlisted_headers_once() {
        let exchange = Arc::new(FakeExchange {
            outcomes: Mutex::new(vec![outcome(false), outcome(true)]),
            ..FakeExchange::default()
        });
        let forwarder = BrowserForwarder::new(exchange.clone());
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .expect("forward opens");
        assert!(!receipt.replayed);
        assert_ne!(receipt.port, 0);
        let replay = forwarder
            .open(input(), 1000.0, Some(0))
            .expect("same bridge replay");
        assert_eq!(
            replay,
            ForwardReceipt {
                replayed: true,
                port: receipt.port
            }
        );
        let mut other = input();
        other.bridge_id = "1123456789abcdef0123456789abcdef".into();
        assert!(matches!(
            forwarder.open(other, 1000.0, Some(0)),
            Err(BrowserError::PortInUse(43210))
        ));
        let response = request(receipt.port);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Security-Policy: sandbox; default-src 'none'"));
        assert!(response.contains("charset=iso-8859-1"));
        let second_response = request(receipt.port);
        assert!(second_response.starts_with("HTTP/1.1 200 OK\r\n"));
        let calls = exchange.calls.lock().expect("test mutex");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].server, "server-a");
        assert_eq!(calls[0].action, REMOTE_ACTION);
        assert!(
            calls[0].payload["target"]
                .as_str()
                .is_some_and(|value| value.starts_with("/oauth/callback?"))
        );
        assert_eq!(
            calls[0].payload["headers"],
            json!({"user-agent":"test-agent","accept":"text/html"})
        );
        assert!(calls[0].request_id.starts_with("browser-callback:"));
        assert_ne!(calls[0].request_id, calls[1].request_id);
        assert!(calls[0].now > 0);
    }

    #[test]
    fn unknown_exchange_returns_502_and_is_never_retried() {
        let exchange = Arc::new(FakeExchange {
            outcomes: Mutex::new(vec![ExchangeOutcome::Unknown]),
            ..FakeExchange::default()
        });
        let forwarder = BrowserForwarder::new(exchange.clone());
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .expect("forward opens");
        let response = request(receipt.port);
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert_eq!(exchange.calls.lock().expect("test mutex").len(), 1);
    }

    #[test]
    fn invalid_and_expired_forwards_do_not_bind_a_port() {
        let forwarder = BrowserForwarder::new(Arc::new(FakeExchange::default()));
        let mut invalid = input();
        invalid.host = "0.0.0.0".into();
        assert!(matches!(
            forwarder.open(invalid, 1000.0, Some(0)),
            Err(BrowserError::InvalidForward)
        ));
        assert!(matches!(
            forwarder.open(input(), 2001.0, Some(0)),
            Err(BrowserError::Expired)
        ));
        assert!(forwarder.forwards.lock().expect("forward map").is_empty());
    }

    #[test]
    fn redirect_responses_are_not_followed_and_oversized_bodies_are_refused() {
        let (status, content_type, body, done) =
            cleaned_result(&json!({"status":302,"location":"https://private.invalid/"}))
                .expect("redirect is replaced by a safe page");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert!(
            String::from_utf8(body)
                .expect("static HTML is UTF-8")
                .contains("Studio does not follow it")
        );
        assert!(!done);
        let oversized = json!({"status":200,"body":"A".repeat(BODY_BASE64_LIMIT + 1)});
        assert!(matches!(
            cleaned_result(&oversized),
            Err(BrowserError::EncodedBodyLimit)
        ));
    }

    #[test]
    fn oversized_encoded_body_is_refused_before_decode() {
        let oversized = json!({"status":200,"body":"A".repeat(BODY_BASE64_LIMIT + 1)});
        assert!(matches!(
            cleaned_result(&oversized),
            Err(BrowserError::EncodedBodyLimit)
        ));
    }

    #[test]
    fn browser_config_is_pure_and_preserves_account_runtime() {
        let shared = json!({"mcp_servers":{"node_repl":{
            "command":"/usr/bin/node", "args":["server.mjs"], "env":{
                "NODE_REPL_TRUSTED_SERVICES":"{\"browser\":\"/opt/browser.mjs\"}",
                "NODE_REPL_NODE_PATH":"/opt/modules", "SECRET":"not copied"
            }
        }}});
        let config = prepare_browser_config(BrowserConfigSnapshot {
            target: &Value::Null,
            shared: &shared,
            home: "/acct",
            shared_home: "/shared",
            skill_available: true,
            node_runtime_available: true,
            trusted_services_valid: true,
            browser_service_available: true,
        });
        assert_eq!(config.unavailable_reason, None);
        assert_eq!(config.developer_instructions, Some(BROWSER_GUIDANCE));
        assert_eq!(
            config.overrides.pointer("/mcp_servers/node_repl/command"),
            Some(&json!("/usr/bin/node"))
        );
        assert_eq!(
            config
                .overrides
                .pointer("/mcp_servers/node_repl/env/CODEX_HOME"),
            Some(&json!("/acct"))
        );
        assert_eq!(
            config
                .overrides
                .pointer("/mcp_servers/node_repl/env/NODE_REPL_TRUSTED_SERVICES"),
            Some(&json!("{\"browser\": \"/opt/browser.mjs\"}"))
        );
        assert_eq!(
            config
                .overrides
                .pointer("/mcp_servers/node_repl/env/SECRET"),
            None
        );
        let disabled = prepare_browser_config(BrowserConfigSnapshot {
            target: &json!({"features":{"plugins":false}}),
            shared: &shared,
            home: "/acct",
            shared_home: "/shared",
            skill_available: true,
            node_runtime_available: true,
            trusted_services_valid: true,
            browser_service_available: true,
        });
        assert_eq!(disabled.overrides, json!({}));
        assert_eq!(
            disabled.unavailable_reason,
            Some("Plugins are disabled in this account's Codex configuration")
        );
        assert_eq!(disabled.developer_instructions, None);
        let invalid_service = prepare_browser_config(BrowserConfigSnapshot {
            target: &Value::Null,
            shared: &shared,
            home: "/acct",
            shared_home: "/shared",
            skill_available: true,
            node_runtime_available: true,
            trusted_services_valid: false,
            browser_service_available: true,
        });
        assert_eq!(invalid_service.overrides, json!({}));
        assert_eq!(
            invalid_service.unavailable_reason,
            Some("NODE_REPL_TRUSTED_SERVICES is not valid JSON")
        );
    }
}
