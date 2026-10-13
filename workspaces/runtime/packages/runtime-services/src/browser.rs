//! Browser sign-in forwarding and pure thread configuration.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const FORWARD_SECONDS: u64 = 600;
const MAX_RELAYS: usize = 5;
const MAX_CONNECTIONS: usize = 8;
const MAX_ACTIVE_EXCHANGES: usize = 16;
const CONNECTION_SECONDS: u64 = 10;
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
static ACTIVE_EXCHANGES: AtomicUsize = AtomicUsize::new(0);

struct ActiveExchangePermit;

impl Drop for ActiveExchangePermit {
    fn drop(&mut self) {
        ACTIVE_EXCHANGES.fetch_sub(1, Ordering::AcqRel);
    }
}

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
    #[error("the paired server refused the sign-in redirect")]
    Refused,
    #[error("a different sign-in request already uses this bridge identity")]
    Conflict,
    #[error("the sign-in redirect outcome is unknown; inspect Studio before retrying")]
    Unknown,
    #[error("the paired exchange failed")]
    Exchange,
    #[error("the paired exchange failed")]
    EncodedBodyLimit,
    #[error("the browser forward worker stopped unexpectedly")]
    WorkerPanicked,
}

#[derive(PartialEq)]
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
            if existing.input != input {
                return Err(BrowserError::Conflict);
            }
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
        let connections = Arc::new(AtomicUsize::new(0));
        let thread_input = input.clone();
        let thread_exchange = Arc::clone(&self.exchange);
        let thread_stop = Arc::clone(&stop);
        let thread_relays = Arc::clone(&relays);
        let thread_connections = Arc::clone(&connections);
        let thread = thread::Builder::new()
            .name("studio-browser-forward".into())
            .spawn(move || {
                serve_forward(
                    listeners,
                    thread_input,
                    thread_exchange,
                    thread_stop,
                    thread_relays,
                    thread_connections,
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
    connections: Arc<AtomicUsize>,
    remaining_seconds: f64,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(remaining_seconds);
    while !stop.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        if relays.load(Ordering::Acquire) >= MAX_RELAYS {
            stop.store(true, Ordering::Release);
            break;
        }
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
            let active = connections.fetch_add(1, Ordering::AcqRel);
            if active >= MAX_CONNECTIONS {
                connections.fetch_sub(1, Ordering::AcqRel);
                drop(stream);
                continue;
            }
            let worker_input = input.clone();
            let worker_exchange = Arc::clone(&exchange);
            let worker_stop = Arc::clone(&stop);
            let worker_relays = Arc::clone(&relays);
            let worker_connections = Arc::clone(&connections);
            let worker_deadline =
                deadline.min(std::time::Instant::now() + Duration::from_secs(CONNECTION_SECONDS));
            let spawned = thread::Builder::new()
                .name("studio-browser-callback".into())
                .spawn(move || {
                    let result = handle_connection(
                        &mut stream,
                        &worker_input,
                        Arc::clone(&worker_exchange),
                        &worker_relays,
                        worker_deadline,
                        &worker_stop,
                    );
                    if matches!(result, Ok(true)) {
                        worker_stop.store(true, Ordering::Release);
                    }
                    worker_connections.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                connections.fetch_sub(1, Ordering::AcqRel);
            }
        } else {
            thread::park_timeout(Duration::from_millis(20));
        }
    }
    stop.store(true, Ordering::Release);
}

fn handle_connection(
    stream: &mut TcpStream,
    input: &ForwardInput,
    exchange: Arc<dyn PairedExchange>,
    relays: &AtomicUsize,
    deadline: std::time::Instant,
    stop: &AtomicBool,
) -> Result<bool, BrowserError> {
    let head = match read_head(stream, deadline) {
        Ok(head) => head,
        Err(_) => return Ok(false),
    };
    if stop.load(Ordering::Acquire) || std::time::Instant::now() >= deadline {
        return Ok(false);
    }
    stream.set_write_timeout(Some(
        deadline.saturating_duration_since(std::time::Instant::now()),
    ))?;
    let mut headers_buf = vec![httparse::EMPTY_HEADER; HEAD_LIMIT / 4];
    let mut request = httparse::Request::new(&mut headers_buf);
    if !matches!(request.parse(&head), Ok(httparse::Status::Complete(_))) {
        drop(respond(
            stream,
            400,
            page("Studio did not understand this request."),
            "text/html; charset=utf-8",
        ));
        return Ok(false);
    }
    let method = request.method.unwrap_or_default();
    let target = request.path.unwrap_or_default();
    let version_ok = request.version.is_some();
    if !version_ok {
        drop(respond(
            stream,
            400,
            page("Studio did not understand this request."),
            "text/html; charset=utf-8",
        ));
        return Ok(false);
    }
    if method != "GET" {
        drop(respond(
            stream,
            405,
            page("Studio forwards only the sign-in redirect."),
            "text/html; charset=utf-8",
        ));
        return Ok(false);
    }
    let path = target.split('?').next().unwrap_or_default();
    if !target.starts_with('/') || path != input.path {
        drop(respond(
            stream,
            404,
            page("Studio forwards only the sign-in redirect."),
            "text/html; charset=utf-8",
        ));
        return Ok(false);
    }
    let mut headers = Map::new();
    let mut seen = HashSet::new();
    for header in request.headers {
        let name = header.name.to_ascii_lowercase();
        if SENT_HEADERS.contains(&name.as_str()) && seen.insert(name.clone()) {
            let value = header
                .value
                .iter()
                .copied()
                .map(char::from)
                .collect::<String>()
                .trim()
                .to_owned();
            if value.chars().count() <= HEADER_LIMIT {
                headers.insert(name, json!(value));
            }
        }
    }
    if stop.load(Ordering::Acquire) || std::time::Instant::now() >= deadline {
        return Ok(false);
    }
    let attempt = match relays.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        (value < MAX_RELAYS).then_some(value + 1)
    }) {
        Ok(value) => value + 1,
        Err(_) => {
            drop(respond(
                stream,
                410,
                page("This sign-in forward is closed. Return to Studio."),
                "text/html; charset=utf-8",
            ));
            stop.store(true, Ordering::Release);
            return Ok(false);
        }
    };
    let request_id = fresh_request_id()?;
    let payload =
        json!({"bridgeId":input.bridge_id,"token":input.token,"target":target,"headers":headers});
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BrowserError::Exchange)?
        .as_secs() as i64;
    let result = match exchange_until(
        exchange,
        &input.server,
        REMOTE_ACTION,
        payload,
        request_id,
        now,
        deadline,
    ) {
        Ok(ExchangeOutcome::Applied(result)) => result,
        Ok(ExchangeOutcome::NotApplied(_)) => {
            respond_best_effort(
                stream,
                502,
                page("The sign-in result is unknown. Check the chat in Studio."),
                "text/html; charset=utf-8",
            );
            return Ok(true);
        }
        Ok(ExchangeOutcome::Unknown) | Err(_) => {
            respond_best_effort(
                stream,
                502,
                page("The sign-in result is unknown. Check the chat in Studio."),
                "text/html; charset=utf-8",
            );
            return Ok(true);
        }
    };
    let (status, content_type, body, done) = match cleaned_result(&result) {
        Ok(cleaned) => cleaned,
        Err(_) => {
            respond_best_effort(
                stream,
                502,
                page("The sign-in result is unknown. Check the chat in Studio."),
                "text/html; charset=utf-8",
            );
            return Ok(true);
        }
    };
    if stop.load(Ordering::Acquire) || std::time::Instant::now() >= deadline {
        return Ok(false);
    }
    stream.set_write_timeout(Some(
        deadline.saturating_duration_since(std::time::Instant::now()),
    ))?;
    respond(stream, status, body, &content_type)?;
    Ok(done || attempt >= MAX_RELAYS)
}

fn exchange_until(
    exchange: Arc<dyn PairedExchange>,
    server: &str,
    action: &str,
    payload: Value,
    request_id: String,
    now: i64,
    deadline: std::time::Instant,
) -> Result<ExchangeOutcome, BrowserError> {
    if std::time::Instant::now() >= deadline {
        return Err(BrowserError::Unknown);
    }
    let active = ACTIVE_EXCHANGES.fetch_add(1, Ordering::AcqRel);
    if active >= MAX_ACTIVE_EXCHANGES {
        ACTIVE_EXCHANGES.fetch_sub(1, Ordering::AcqRel);
        return Err(BrowserError::Exchange);
    }
    let server = server.to_owned();
    let action = action.to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("studio-browser-exchange".into())
        .spawn(move || {
            let _permit = ActiveExchangePermit;
            let result = exchange.exchange(&server, &action, &payload, &request_id, now);
            drop(sender.send(result));
        });
    if worker.is_err() {
        ACTIVE_EXCHANGES.fetch_sub(1, Ordering::AcqRel);
        return Err(BrowserError::Exchange);
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    receiver
        .recv_timeout(remaining)
        .map_err(|_| BrowserError::Unknown)?
}

fn fresh_request_id() -> Result<String, BrowserError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| BrowserError::Exchange)?;
    Ok(format!(
        "browser-callback:{}",
        uuid::Uuid::from_bytes(bytes).simple()
    ))
}

fn read_head(
    stream: &mut TcpStream,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, BrowserError> {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !data.windows(4).any(|window| window == b"\r\n\r\n") {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(BrowserError::Expired);
        }
        stream.set_read_timeout(Some(remaining))?;
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
    data.truncate(end + 4);
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
            result.get("done").and_then(Value::as_bool) == Some(true),
        ));
    }
    let encoded = match result.get("body") {
        None => "",
        Some(Value::String(value)) => value.as_str(),
        Some(_) => return Err(BrowserError::Exchange),
    };
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
    let charset = text.match_indices("charset=").find_map(|(index, _)| {
        let value = &text[index + "charset=".len()..];
        let bounded = value
            .bytes()
            .take(40)
            .take_while(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
            .count();
        (bounded > 0).then_some(&value[..bounded])
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

/// Filesystem facts discovered alongside the Python-written config records.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConfigAssets {
    pub control_chrome_skill: Option<String>,
    pub node_runtime: Option<String>,
    pub browser_service: Option<String>,
}

/// A validated snapshot assembled from the stored account and shared config records.
pub struct BrowserConfigSnapshot {
    target: Value,
    shared: Value,
    home: String,
    shared_home: String,
    skill_available: bool,
    node_runtime_available: bool,
    browser_service_available: bool,
    trusted_services_valid: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserConfigSnapshotError {
    #[error("stored browser configuration is invalid")]
    InvalidRecord,
}

impl BrowserConfigSnapshot {
    pub fn from_stored_records(
        target: &Value,
        shared: &Value,
        home: &str,
        shared_home: &str,
        assets: BrowserConfigAssets,
    ) -> Result<Self, BrowserConfigSnapshotError> {
        if !target.is_object() || !shared.is_object() {
            return Err(BrowserConfigSnapshotError::InvalidRecord);
        }
        for record in [target, shared] {
            if record
                .get("mcp_servers")
                .is_some_and(|servers| !servers.is_object())
                || record
                    .pointer("/mcp_servers/node_repl")
                    .is_some_and(|node| !node.is_object() && !node.is_null())
            {
                return Err(BrowserConfigSnapshotError::InvalidRecord);
            }
        }
        let shared_node = shared.pointer("/mcp_servers/node_repl");
        let source_command = shared_node
            .and_then(|node| node.get("command"))
            .and_then(Value::as_str);
        if assets
            .node_runtime
            .as_deref()
            .is_some_and(|path| Some(path) != source_command)
        {
            return Err(BrowserConfigSnapshotError::InvalidRecord);
        }
        let service_value =
            shared_node.and_then(|node| node.pointer("/env/NODE_REPL_TRUSTED_SERVICES"));
        let parsed = match service_value {
            None => Some(json!({})),
            Some(Value::String(source)) => serde_json::from_str::<Value>(source).ok(),
            Some(_) => None,
        };
        let trusted_services_valid = parsed.is_some();
        let configured_browser = parsed.and_then(|value| {
            value
                .get("browser")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        if assets
            .browser_service
            .as_deref()
            .is_some_and(|path| Some(path) != configured_browser.as_deref())
        {
            return Err(BrowserConfigSnapshotError::InvalidRecord);
        }
        let skill_available = assets
            .control_chrome_skill
            .as_deref()
            .is_some_and(|path| std::path::Path::new(path).is_file());
        let node_runtime_available = assets
            .node_runtime
            .as_deref()
            .is_some_and(|path| std::path::Path::new(path).is_file());
        let browser_service_available = assets
            .browser_service
            .as_deref()
            .is_some_and(|path| std::path::Path::new(path).is_file());
        Ok(Self {
            target: target.clone(),
            shared: shared.clone(),
            home: home.to_owned(),
            shared_home: shared_home.to_owned(),
            skill_available,
            node_runtime_available,
            browser_service_available,
            trusted_services_valid,
        })
    }
}

/// Browser overrides or a visible reason they could not be prepared.
pub struct BrowserConfigDecision {
    pub overrides: Value,
    pub unavailable_reason: Option<&'static str>,
    pub developer_instructions: Option<&'static str>,
}

/// Pure browser-session configuration. Filesystem discovery and host calls stay in the caller.
pub fn prepare_browser_config(snapshot: BrowserConfigSnapshot) -> BrowserConfigDecision {
    let BrowserConfigSnapshot {
        target,
        shared,
        home,
        shared_home,
        skill_available,
        node_runtime_available,
        browser_service_available,
        trusted_services_valid,
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
    } else if target_node.is_some_and(|node| !node.as_object().is_some_and(Map::is_empty))
        && target_node.and_then(|node| node.get("command"))
            != shared_node.and_then(|node| node.get("command"))
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
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for path in std::env::split_paths(std::ffi::OsStr::new(trusted)) {
            let path = path.to_string_lossy().into_owned();
            if !path.is_empty() && seen.insert(path.clone()) {
                roots.push(path);
            }
        }
        for path in [shared_home, home] {
            if !path.is_empty() && seen.insert(path.clone()) {
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
        let config = json!({"mcp_servers.node_repl":node});
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
        let mut abandoned = TcpStream::connect((Ipv4Addr::LOCALHOST, receipt.port))
            .expect("listener remains available beside an incomplete request");
        abandoned
            .write_all(b"GET /oauth/callback HTTP/1.1\r\nUser-Agent: slow")
            .expect("partial request writes");
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
        drop(abandoned);
    }

    #[test]
    fn oversized_first_user_agent_duplicate_is_not_replaced_by_a_later_short_value() {
        let exchange = Arc::new(FakeExchange {
            outcomes: Mutex::new(vec![outcome(true)]),
            ..FakeExchange::default()
        });
        let forwarder = BrowserForwarder::new(exchange.clone());
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .expect("forward opens");
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, receipt.port)).expect("loopback");
        write!(
            stream,
            "GET /oauth/callback HTTP/1.1\r\nUser-Agent: {}\r\nUser-Agent: short\r\n\r\n",
            "x".repeat(HEADER_LIMIT + 1)
        )
        .expect("request writes");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("response reads");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        let calls = exchange.calls.lock().expect("exchange calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].payload["headers"], json!({}));
    }

    #[test]
    fn active_bridge_identity_replay_refuses_changed_request_fields() {
        let forwarder = BrowserForwarder::new(Arc::new(FakeExchange {
            outcomes: Mutex::new(vec![outcome(true)]),
            ..FakeExchange::default()
        }));
        let original = input();
        let receipt = forwarder
            .open(original.clone(), 1000.0, Some(0))
            .expect("open");
        for mutate in [0, 1, 2, 3, 4] {
            let mut changed = original.clone();
            match mutate {
                0 => changed.server = "other-server".into(),
                1 => changed.token = "different-token-with-entropy".into(),
                2 => changed.path = "/different/callback".into(),
                3 => changed.port += 1,
                _ => changed.expires_at += 1.0,
            }
            assert!(matches!(
                forwarder.open(changed, 1000.0, Some(0)),
                Err(BrowserError::Conflict)
            ));
        }
        assert!(forwarder.close(&original.bridge_id).expect("close"));
        assert_ne!(receipt.port, 0);
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
    fn paired_refusal_reason_is_not_exposed_to_the_callback_client() {
        let exchange = Arc::new(FakeExchange {
            outcomes: Mutex::new(vec![ExchangeOutcome::NotApplied(
                "code=callback-secret token=private".into(),
            )]),
            ..FakeExchange::default()
        });
        let forwarder = BrowserForwarder::new(exchange);
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .expect("forward opens");
        let response = request(receipt.port);
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(!response.contains("callback-secret"));
        assert!(!response.contains("private"));
    }

    #[test]
    fn exchange_wait_uses_the_connection_deadline_and_does_not_repeat_the_call() {
        struct DelayedExchange(Arc<AtomicUsize>);
        impl PairedExchange for DelayedExchange {
            fn exchange(
                &self,
                _server: &str,
                _action: &str,
                _payload: &Value,
                _request_id: &str,
                _now: i64,
            ) -> Result<ExchangeOutcome, BrowserError> {
                self.0.fetch_add(1, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(100));
                Ok(ExchangeOutcome::Applied(json!({"status":200,"body":""})))
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let exchange: Arc<dyn PairedExchange> = Arc::new(DelayedExchange(Arc::clone(&calls)));
        let started = std::time::Instant::now();
        let result = exchange_until(
            exchange,
            "peer",
            REMOTE_ACTION,
            json!({}),
            "request-id".into(),
            0,
            started + Duration::from_millis(10),
        );
        assert!(matches!(result, Err(BrowserError::Unknown)));
        assert!(started.elapsed() < Duration::from_millis(80));
        thread::sleep(Duration::from_millis(120));
        assert_eq!(calls.load(Ordering::Acquire), 1);
        let not_started = Arc::new(AtomicUsize::new(0));
        let result = exchange_until(
            Arc::new(DelayedExchange(Arc::clone(&not_started))),
            "peer",
            REMOTE_ACTION,
            json!({}),
            "expired-request".into(),
            0,
            std::time::Instant::now() - Duration::from_millis(1),
        );
        assert!(matches!(result, Err(BrowserError::Unknown)));
        assert_eq!(not_started.load(Ordering::Acquire), 0);
    }

    #[test]
    fn connection_read_timeout_is_absolute_and_incomplete_requests_do_not_relay() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback listener");
        let client = TcpStream::connect(listener.local_addr().expect("local address"))
            .expect("loopback client");
        let (mut accepted, _) = listener.accept().expect("accept loopback client");
        let deadline = std::time::Instant::now() + Duration::from_millis(25);
        assert!(matches!(
            read_head(&mut accepted, deadline),
            Err(BrowserError::Listener(_))
        ));
        drop(client);
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
        let (_, _, _, done) =
            cleaned_result(&json!({"status":302,"done":true})).expect("safe redirect replacement");
        assert!(done, "redirect replacement retains completion semantics");
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
    fn non_string_response_body_is_refused_and_error_formatting_is_redacted() {
        assert!(matches!(
            cleaned_result(&json!({"status":200,"body":{}})),
            Err(BrowserError::Exchange)
        ));
        let error = BrowserError::Refused;
        assert_eq!(
            error.to_string(),
            "the paired server refused the sign-in redirect"
        );
        assert!(!format!("{error:?}").contains("callback-secret"));
    }

    #[test]
    fn charset_search_matches_python_pattern_inside_a_content_type_parameter() {
        let (_, content_type, _, _) = cleaned_result(&json!({
            "status":200,
            "contentType":"text/plain; charset=iso-8859-1 extra",
            "body":""
        }))
        .expect("valid minimal response");
        assert_eq!(content_type, "text/plain; charset=iso-8859-1");
    }

    #[test]
    fn browser_config_is_pure_and_preserves_account_runtime() {
        let root = tempfile::tempdir().expect("asset fixtures");
        let skill_path = root.path().join("SKILL.md");
        let node_path = root.path().join("node");
        let service_path = root.path().join("browser.mjs");
        std::fs::write(&skill_path, "skill").expect("skill fixture");
        std::fs::write(&node_path, "node").expect("node fixture");
        std::fs::write(&service_path, "service").expect("service fixture");
        let skill = skill_path.to_string_lossy().into_owned();
        let node = node_path.to_string_lossy().into_owned();
        let service = service_path.to_string_lossy().into_owned();
        let shared = json!({"mcp_servers":{"node_repl":{
            "command":node, "args":["server.mjs"], "env":{
                "NODE_REPL_TRUSTED_SERVICES":format!("{{\"browser\":{}}}", json!(service)),
                "NODE_REPL_NODE_PATH":"/opt/modules", "SECRET":"not copied"
            }
        }}});
        let assets = BrowserConfigAssets {
            control_chrome_skill: Some(skill.clone()),
            node_runtime: Some(node.clone()),
            browser_service: Some(service.clone()),
        };
        let config = prepare_browser_config(
            BrowserConfigSnapshot::from_stored_records(
                &json!({}),
                &shared,
                "/acct",
                "/shared",
                assets.clone(),
            )
            .expect("valid stored records"),
        );
        assert_eq!(config.unavailable_reason, None);
        assert_eq!(config.developer_instructions, Some(BROWSER_GUIDANCE));
        assert_eq!(
            config
                .overrides
                .get("mcp_servers.node_repl")
                .and_then(|node| node.get("command")),
            Some(&json!(node))
        );
        assert_eq!(
            config
                .overrides
                .get("mcp_servers.node_repl")
                .and_then(|node| node.pointer("/env/CODEX_HOME")),
            Some(&json!("/acct"))
        );
        assert_eq!(
            config
                .overrides
                .get("mcp_servers.node_repl")
                .and_then(|node| node.pointer("/env/NODE_REPL_TRUSTED_SERVICES")),
            Some(&json!(format!("{{\"browser\": {}}}", json!(service))))
        );
        assert_eq!(
            config
                .overrides
                .get("mcp_servers.node_repl")
                .and_then(|node| node.pointer("/env/SECRET")),
            None
        );
        let disabled = prepare_browser_config(
            BrowserConfigSnapshot::from_stored_records(
                &json!({"features":{"plugins":false}}),
                &shared,
                "/acct",
                "/shared",
                assets.clone(),
            )
            .expect("valid stored records"),
        );
        assert_eq!(disabled.overrides, json!({}));
        assert_eq!(
            disabled.unavailable_reason,
            Some("Plugins are disabled in this account's Codex configuration")
        );
        assert_eq!(disabled.developer_instructions, None);
        let invalid_service_config = json!({"mcp_servers":{"node_repl":{"command":node,"env":{"NODE_REPL_TRUSTED_SERVICES":"not-json"}}}});
        let invalid_service = prepare_browser_config(
            BrowserConfigSnapshot::from_stored_records(
                &json!({}),
                &invalid_service_config,
                "/acct",
                "/shared",
                BrowserConfigAssets {
                    browser_service: None,
                    ..assets.clone()
                },
            )
            .expect("valid stored records"),
        );
        assert_eq!(invalid_service.overrides, json!({}));
        assert_eq!(
            invalid_service.unavailable_reason,
            Some("NODE_REPL_TRUSTED_SERVICES is not valid JSON")
        );
        let empty_node = json!({"mcp_servers":{"node_repl":{}}});
        let empty_target = prepare_browser_config(
            BrowserConfigSnapshot::from_stored_records(
                &empty_node,
                &shared,
                "/acct",
                "/shared",
                assets,
            )
            .expect("valid stored records"),
        );
        assert_eq!(empty_target.unavailable_reason, None);
        assert_eq!(
            empty_target
                .overrides
                .get("mcp_servers.node_repl")
                .and_then(|node| node.get("enabled")),
            None
        );
    }
}
