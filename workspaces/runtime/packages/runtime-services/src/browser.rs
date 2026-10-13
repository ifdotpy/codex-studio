//! Browser sign-in forwarding and pure thread configuration.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

const FORWARD_SECONDS: u64 = 600;
const MAX_RELAYS: usize = 5;
const MAX_CONNECTIONS: usize = 8;
const CONNECTION_SECONDS: u64 = 10;
const RESPONSE_RESERVE: Duration = Duration::from_secs(1);
const HEAD_LIMIT: usize = 16 * 1024;
const MAX_HEADER_COUNT: usize = HEAD_LIMIT / 4;
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

#[derive(Error)]
pub enum BrowserError {
    #[error("the sign-in forward is invalid")]
    InvalidForward,
    #[error("the sign-in request is no longer open. Ask the agent to run the command again.")]
    Expired,
    #[error("another sign-in uses port {0} on this Mac. Wait until it ends, then approve again.")]
    PortInUse(u16),
    #[error("the sign-in forward cannot start")]
    Listener(std::io::ErrorKind),
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

impl std::fmt::Debug for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserError(<redacted>)")
    }
}

#[derive(PartialEq)]
pub enum ExchangeOutcome {
    Applied(Value),
    NotApplied(String),
    Unknown,
}

/// A cancellable remote exchange. Implementations must stop work when its deadline elapses.
pub trait PairedExchange: Send + Sync {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        action: &'a str,
        payload: &'a Value,
        request_id: &'a str,
        now: i64,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>;
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
    stop: watch::Sender<bool>,
    closed: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl ActiveForward {
    async fn close(self) -> Result<(), BrowserError> {
        self.stop.send_replace(true);
        self.task.await.map_err(|_| BrowserError::WorkerPanicked)
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

    pub async fn open(
        &self,
        input: ForwardInput,
        now: f64,
        listen_port: Option<u16>,
    ) -> Result<ForwardReceipt, BrowserError> {
        validate_forward(&input)?;
        let remaining = (input.expires_at - now).min(FORWARD_SECONDS as f64);
        if !now.is_finite() {
            return Err(BrowserError::InvalidForward);
        }
        if remaining <= 0.0 {
            return Err(BrowserError::Expired);
        }
        let deadline = Instant::now() + Duration::from_secs_f64(remaining);
        let mut forwards = self.forwards.lock().map_err(|_| BrowserError::Exchange)?;
        let expired = forwards
            .iter()
            .filter_map(|(id, f)| f.task.is_finished().then_some(id.clone()))
            .collect::<Vec<_>>();
        for id in expired {
            forwards.remove(&id);
        }
        if let Some(existing) = forwards.get(&input.bridge_id) {
            if existing.input != input {
                return Err(BrowserError::Conflict);
            }
            return Ok(ForwardReceipt {
                replayed: true,
                port: existing.port,
            });
        }
        if forwards.values().any(|f| f.input.port == input.port) {
            return Err(BrowserError::PortInUse(input.port));
        }
        let listeners = bind_loopback(&input.host, listen_port.unwrap_or(input.port), input.port)?;
        let mut async_listeners = Vec::with_capacity(listeners.len());
        let mut port = 0;
        for listener in listeners {
            listener
                .set_nonblocking(true)
                .map_err(|e| BrowserError::Listener(e.kind()))?;
            if port == 0 {
                port = listener
                    .local_addr()
                    .map_err(|e| BrowserError::Listener(e.kind()))?
                    .port();
            }
            async_listeners.push(
                TokioTcpListener::from_std(listener)
                    .map_err(|e| BrowserError::Listener(e.kind()))?,
            );
        }
        let (stop, stop_rx) = watch::channel(false);
        let (closed_tx, closed) = watch::channel(false);
        let exchange = Arc::clone(&self.exchange);
        let task_input = input.clone();
        let task = tokio::spawn(async move {
            run_forward(async_listeners, task_input, exchange, stop_rx, deadline).await;
            closed_tx.send_replace(true);
        });
        forwards.insert(
            input.bridge_id.clone(),
            ActiveForward {
                input,
                port,
                stop,
                closed,
                task,
            },
        );
        Ok(ForwardReceipt {
            replayed: false,
            port,
        })
    }

    pub async fn close(&self, bridge_id: &str) -> Result<bool, BrowserError> {
        let forward = self
            .forwards
            .lock()
            .map_err(|_| BrowserError::Exchange)?
            .remove(bridge_id);
        if let Some(forward) = forward {
            forward.close().await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Waits for the forward listener and its connection tasks to finish.
    pub async fn wait_closed(&self, bridge_id: &str) -> Result<bool, BrowserError> {
        let mut closed = self
            .forwards
            .lock()
            .map_err(|_| BrowserError::Exchange)?
            .get(bridge_id)
            .map(|forward| forward.closed.clone());
        let Some(mut closed) = closed.take() else {
            return Ok(false);
        };
        loop {
            if *closed.borrow() {
                return Ok(true);
            }
            if closed.changed().await.is_err() {
                return Ok(*closed.borrow());
            }
        }
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
) -> Result<Vec<TcpListener>, BrowserError> {
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
    for address in addresses {
        let address = SocketAddr::new(address.ip(), bound_port);
        match TcpListener::bind(address) {
            Ok(listener) => {
                if bound_port == 0 {
                    bound_port = listener
                        .local_addr()
                        .map_err(|e| BrowserError::Listener(e.kind()))?
                        .port();
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
            Err(error) => return Err(BrowserError::Listener(error.kind())),
        }
    }
    Ok(listeners)
}

async fn run_forward(
    listeners: Vec<TokioTcpListener>,
    input: ForwardInput,
    exchange: Arc<dyn PairedExchange>,
    mut stop: watch::Receiver<bool>,
    deadline: Instant,
) {
    let relays = Arc::new(AtomicUsize::new(0));
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tasks = JoinSet::new();
    let mut accepting = true;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            changed = stop.changed() => if changed.is_err() || *stop.borrow() { break; },
            result = tasks.join_next(), if !tasks.is_empty() => {
                match result {
                    Some(Ok((true, _))) => break,
                    Some(Ok((false, true))) => accepting = false,
                    _ => {}
                }
                if !accepting && tasks.is_empty() { break; }
            }
            accepted = accept_any(&listeners), if accepting => {
                let Ok((stream, _)) = accepted else { break; };
                let Some(permit) = acquire_connection_slot(&permits) else { drop(stream); continue; };
                let connection_deadline = std::cmp::min(deadline, Instant::now() + Duration::from_secs(CONNECTION_SECONDS));
                let input = input.clone();
                let exchange = Arc::clone(&exchange);
                let relays = Arc::clone(&relays);
                let signals = ConnectionSignals::default();
                let task_signals = signals.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let result = tokio::select! {
                        biased;
                        _ = tokio::time::sleep_until(connection_deadline) => None,
                        result = serve_connection(stream, input, exchange, relays, connection_deadline, task_signals) => Some(result),
                    };
                    let timely = result.is_some() && Instant::now() < connection_deadline;
                    (
                        (timely && matches!(result, Some(Ok(true))))
                            || signals.close_requested.load(Ordering::Acquire)
                            || signals.exchange_pending.load(Ordering::Acquire),
                        signals.relay_limit_completed.load(Ordering::Acquire),
                    )
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

fn acquire_connection_slot(permits: &Arc<Semaphore>) -> Option<tokio::sync::OwnedSemaphorePermit> {
    Arc::clone(permits).try_acquire_owned().ok()
}

async fn accept_any(
    listeners: &[TokioTcpListener],
) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
    if listeners.len() == 2 {
        tokio::select! {
            result = listeners[0].accept() => result,
            result = listeners[1].accept() => result,
        }
    } else {
        listeners[0].accept().await
    }
}

#[derive(Clone, Default)]
struct ConnectionSignals {
    close_requested: Arc<std::sync::atomic::AtomicBool>,
    exchange_pending: Arc<std::sync::atomic::AtomicBool>,
    relay_limit_completed: Arc<std::sync::atomic::AtomicBool>,
}

/// Captures the raw target before Hyper normalizes input.
struct RawTargetGuardIo {
    stream: tokio::net::TcpStream,
    raw_line: Vec<u8>,
    checked_line: bool,
    has_fragment: Arc<std::sync::atomic::AtomicBool>,
}

impl RawTargetGuardIo {
    fn new(
        stream: tokio::net::TcpStream,
        has_fragment: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            stream,
            raw_line: Vec::new(),
            checked_line: false,
            has_fragment,
        }
    }

    fn inspect_input(&mut self, bytes: &mut [u8]) {
        if !self.checked_line {
            let prior_len = self.raw_line.len();
            let mut candidate = self.raw_line.clone();
            candidate.extend_from_slice(bytes);
            if let Some(end) = candidate.windows(2).position(|window| window == b"\r\n") {
                candidate.truncate(end);
            }
            if let Some(method_end) = candidate.iter().position(|byte| *byte == b' ') {
                let target_start = method_end + 1;
                let target_end = candidate[target_start..]
                    .iter()
                    .position(|byte| *byte == b' ')
                    .map_or(candidate.len(), |offset| target_start + offset);
                for (offset, byte) in candidate[target_start..target_end].iter().enumerate() {
                    if *byte == b'#' {
                        self.has_fragment.store(true, Ordering::Release);
                        let candidate_index = target_start + offset;
                        if candidate_index >= prior_len && candidate_index - prior_len < bytes.len()
                        {
                            bytes[candidate_index - prior_len] = b'x';
                        }
                    }
                }
            }
            let remaining = HEAD_LIMIT.saturating_sub(self.raw_line.len());
            self.raw_line
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            if let Some(end) = self
                .raw_line
                .windows(2)
                .position(|window| window == b"\r\n")
            {
                let _request_line = &self.raw_line[..end];
                self.checked_line = true;
                self.raw_line.clear();
            } else if self.raw_line.len() == HEAD_LIMIT {
                self.checked_line = true;
                self.raw_line.clear();
            }
        }
    }
}

impl AsyncRead for RawTargetGuardIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        destination: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let read_size = destination.remaining().min(8192);
        if read_size == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut bytes = vec![0; read_size];
        let mut read_buffer = ReadBuf::new(&mut bytes);
        match Pin::new(&mut self.stream).poll_read(context, &mut read_buffer) {
            Poll::Ready(Ok(())) => {
                let mut filled = read_buffer.filled().to_vec();
                self.inspect_input(&mut filled);
                destination.put_slice(&filled);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for RawTargetGuardIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, bytes)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    input: ForwardInput,
    exchange: Arc<dyn PairedExchange>,
    relays: Arc<AtomicUsize>,
    deadline: Instant,
    signals: ConnectionSignals,
) -> Result<bool, BrowserError> {
    let raw_fragment = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler_raw_fragment = Arc::clone(&raw_fragment);
    let handler_signals = signals.clone();
    let service = service_fn(move |request: Request<Incoming>| {
        let exchange = Arc::clone(&exchange);
        let input = input.clone();
        let relays = Arc::clone(&relays);
        let signals = handler_signals.clone();
        let raw_fragment = Arc::clone(&handler_raw_fragment);
        async move {
            let (response, should_close) = handle_request(
                request,
                input,
                exchange,
                relays,
                deadline,
                signals.clone(),
                raw_fragment,
            )
            .await;
            signals
                .close_requested
                .store(should_close, Ordering::Release);
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .max_buf_size(HEAD_LIMIT * 2)
        .max_header_size(HEAD_LIMIT)
        .max_headers(MAX_HEADER_COUNT)
        .half_close(true)
        .keep_alive(false)
        .auto_date_header(false)
        .serve_connection(
            TokioIo::new(RawTargetGuardIo::new(stream, raw_fragment)),
            service,
        )
        .await
        .map_err(|_| BrowserError::Exchange)?;
    Ok(signals.close_requested.load(Ordering::Acquire))
}

async fn handle_request(
    request: Request<Incoming>,
    input: ForwardInput,
    exchange: Arc<dyn PairedExchange>,
    relays: Arc<AtomicUsize>,
    deadline: Instant,
    signals: ConnectionSignals,
    raw_fragment: Arc<std::sync::atomic::AtomicBool>,
) -> (Response<Full<Bytes>>, bool) {
    if *request.method() != Method::GET {
        return (
            response(
                405,
                page("Studio forwards only the sign-in redirect."),
                "text/html; charset=utf-8",
            ),
            false,
        );
    }
    let target = request
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default();
    if raw_fragment.load(Ordering::Acquire)
        || request.uri().scheme().is_some()
        || request.uri().authority().is_some()
        || !target.starts_with('/')
        || path != input.path
    {
        return (
            response(
                404,
                page("Studio forwards only the sign-in redirect."),
                "text/html; charset=utf-8",
            ),
            false,
        );
    }
    let mut headers = Map::new();
    for name in SENT_HEADERS {
        let name_header = HeaderName::from_static(name);
        if let Some(first) = request.headers().get_all(name_header).iter().next() {
            let value = first
                .as_bytes()
                .iter()
                .copied()
                .map(char::from)
                .collect::<String>()
                .trim()
                .to_owned();
            if value.chars().count() <= HEADER_LIMIT {
                headers.insert(name.to_owned(), json!(value));
            }
        }
    }
    let exchange_deadline = deadline.checked_sub(RESPONSE_RESERVE).unwrap_or(deadline);
    if Instant::now() >= exchange_deadline {
        return (failure_response(), true);
    }
    match relays.try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
        (count < MAX_RELAYS).then_some(count + 1)
    }) {
        Ok(_) => {}
        Err(_) => {
            return (
                response(
                    410,
                    page("This sign-in forward is closed. Return to Studio."),
                    "text/html; charset=utf-8",
                ),
                false,
            );
        }
    };
    let request_id = match fresh_request_id() {
        Ok(id) => id,
        Err(_) => return (failure_response(), true),
    };
    let payload =
        json!({"bridgeId":input.bridge_id,"token":input.token,"target":target,"headers":headers});
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs() as i64)
        .unwrap_or_default();
    signals.exchange_pending.store(true, Ordering::Release);
    let outcome = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(exchange_deadline) => None,
        result = exchange.exchange(
            &input.server,
            REMOTE_ACTION,
            &payload,
            &request_id,
            now,
            exchange_deadline,
        ) => Some(result),
    };
    signals.exchange_pending.store(false, Ordering::Release);
    if Instant::now() >= exchange_deadline {
        signals.close_requested.store(true, Ordering::Release);
        return (failure_response(), true);
    }
    let result = match outcome {
        Some(Ok(ExchangeOutcome::Applied(result))) => result,
        Some(Ok(ExchangeOutcome::NotApplied(_)))
        | Some(Ok(ExchangeOutcome::Unknown))
        | Some(Err(_))
        | None => {
            signals.close_requested.store(true, Ordering::Release);
            return (failure_response(), true);
        }
    };
    let (status, content_type, body, done) = match cleaned_result(&result) {
        Ok(v) => v,
        Err(_) => {
            signals.close_requested.store(true, Ordering::Release);
            return (failure_response(), true);
        }
    };
    if Instant::now() >= exchange_deadline {
        signals.close_requested.store(true, Ordering::Release);
        return (failure_response(), true);
    }
    if relays.load(Ordering::Acquire) >= MAX_RELAYS {
        signals.relay_limit_completed.store(true, Ordering::Release);
    }
    let should_close = done;
    if done {
        signals.close_requested.store(true, Ordering::Release);
    }
    (response(status, body, &content_type), should_close)
}

fn failure_response() -> Response<Full<Bytes>> {
    response(
        502,
        page("The sign-in result is unknown. Check the chat in Studio."),
        "text/html; charset=utf-8",
    )
}

fn response(status: u16, body: Vec<u8>, content_type: &str) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let content_length = body.len().to_string();
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("text/plain; charset=utf-8")),
    );
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&content_length).unwrap_or(HeaderValue::from_static("0")),
    );
    headers.insert("content-security-policy", HeaderValue::from_static(SANDBOX));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

fn fresh_request_id() -> Result<String, BrowserError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| BrowserError::Exchange)?;
    Ok(format!(
        "browser-callback:{}",
        uuid::Uuid::from_bytes(bytes).simple()
    ))
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

/// Filesystem facts discovered alongside the Python-written config records.
fn plugin_version(plugin: &std::path::Path) -> Option<String> {
    let path = plugin.join(".codex-plugin/plugin.json");
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    value
        .get("version")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn canonical_paths_match(left: &std::path::Path, right: &std::path::Path) -> bool {
    std::fs::canonicalize(left)
        .ok()
        .zip(std::fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
}

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
        let skill_available = assets.control_chrome_skill.as_deref().is_some_and(|path| {
            let path = std::path::Path::new(path);
            let installed = std::path::Path::new(shared_home)
                .join("plugins/cache/openai-bundled/chrome/latest/skills/control-chrome/SKILL.md");
            if path.is_file() && canonical_paths_match(path, &installed) {
                return true;
            }
            let market = shared.pointer("/marketplaces/openai-bundled");
            let Some(source) = market
                .filter(|m| m.get("source_type").and_then(Value::as_str) == Some("local"))
                .and_then(|m| m.get("source"))
                .and_then(Value::as_str)
            else {
                return false;
            };
            let plugin = std::path::Path::new(source).join("plugins/chrome");
            let installed_plugin = std::path::Path::new(shared_home)
                .join("plugins/cache/openai-bundled/chrome/latest");
            let expected = plugin.join("skills/control-chrome/SKILL.md");
            let client = plugin.join("scripts/browser-client.mjs");
            let market_version = plugin_version(&plugin);
            let installed_version = plugin_version(&installed_plugin);
            path.is_file()
                && canonical_paths_match(path, &expected)
                && client.is_file()
                && market_version.is_some()
                && market_version == installed_version
        });
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
        let object = node
            .as_object_mut()
            .expect("checked BrowserConfigSnapshot guarantees an object node_repl");
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
    use std::sync::atomic::AtomicBool;
    use tokio::sync::{Notify, Semaphore};

    type ExchangeCall = (String, String, Value, String, i64);
    type IdentityChange = (&'static str, fn(&mut ForwardInput));

    #[test]
    fn eight_connection_slots_refuse_ninth_and_recover() {
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut admitted = (0..MAX_CONNECTIONS)
            .map(|_| acquire_connection_slot(&permits))
            .collect::<Vec<_>>();
        assert!(admitted.iter().all(Option::is_some));
        assert!(acquire_connection_slot(&permits).is_none());

        drop(admitted.pop());
        let recovered = acquire_connection_slot(&permits);
        assert!(recovered.is_some());
        assert!(acquire_connection_slot(&permits).is_none());
    }

    #[derive(Default)]
    struct FakeExchange {
        calls: Mutex<Vec<ExchangeCall>>,
        count: AtomicUsize,
        gate: Option<Arc<Semaphore>>,
        entered: Option<Arc<Notify>>,
        done: bool,
    }

    impl PairedExchange for FakeExchange {
        fn exchange<'a>(
            &'a self,
            server: &'a str,
            action: &'a str,
            payload: &'a Value,
            request_id: &'a str,
            now: i64,
            _deadline: Instant,
        ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.calls.lock().expect("calls").push((
                    server.into(),
                    action.into(),
                    payload.clone(),
                    request_id.into(),
                    now,
                ));
                let call = self.count.fetch_add(1, Ordering::AcqRel) + 1;
                if let Some(entered) = &self.entered {
                    entered.notify_one();
                }
                if call == MAX_RELAYS
                    && let Some(gate) = &self.gate
                {
                    gate.acquire().await.expect("gate").forget();
                }
                Ok(ExchangeOutcome::Applied(
                    json!({"status":200,"body":BASE64.encode(b"<html>ok</html>"),"done":self.done}),
                ))
            })
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

    fn request(port: u16, raw: &'static [u8]) -> String {
        let mut stream =
            std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("loopback");
        stream.write_all(raw).expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        response
    }

    fn normal_request(port: u16) -> String {
        request(port, b"GET /oauth/callback?code=secret HTTP/1.1\r\nUser-Agent: test-agent\r\nAccept: text/html\r\nCookie: private\r\nAuthorization: private\r\n\r\n")
    }

    fn half_closed_request(port: u16) -> String {
        let mut stream =
            std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("loopback");
        stream
            .write_all(b"GET /oauth/callback?code=secret HTTP/1.1\r\n\r\n")
            .expect("complete request");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("half-close request side");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("response after half-close");
        response
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listener_relays_only_checked_path_and_allowlisted_headers() {
        let exchange = Arc::new(FakeExchange::default());
        let forwarder = BrowserForwarder::new(exchange.clone());
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        assert_eq!(
            forwarder
                .open(input(), 1000.0, Some(0))
                .await
                .expect("replay"),
            ForwardReceipt {
                replayed: true,
                port: receipt.port
            }
        );
        let response = normal_request(receipt.port);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(
            response
                .to_ascii_lowercase()
                .contains("content-security-policy: sandbox; default-src 'none'")
        );
        {
            let calls = exchange.calls.lock().expect("calls");
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].2["headers"],
                json!({"user-agent":"test-agent","accept":"text/html"})
            );
            assert!(calls[0].3.starts_with("browser-callback:"));
        }
        assert!(forwarder.close(&input().bridge_id).await.expect("close"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_request_half_close_still_gets_held_exchange_response() {
        struct HeldExchange {
            entered: Arc<Notify>,
            release: Arc<Semaphore>,
        }
        impl PairedExchange for HeldExchange {
            fn exchange<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a Value,
                _: &'a str,
                _: i64,
                _: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>
            {
                Box::pin(async move {
                    self.entered.notify_one();
                    self.release.acquire().await.expect("release").forget();
                    Ok(ExchangeOutcome::Applied(json!({
                        "status": 200,
                        "body": BASE64.encode(b"accepted"),
                        "done": false
                    })))
                })
            }
        }

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let forwarder = BrowserForwarder::new(Arc::new(HeldExchange {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        let request = tokio::task::spawn_blocking(move || half_closed_request(receipt.port));
        entered.notified().await;
        release.add_permits(1);
        assert!(
            request
                .await
                .expect("half-closed client response")
                .starts_with("HTTP/1.1 200 OK\r\n")
        );
        assert!(forwarder.close(&input().bridge_id).await.expect("close"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fifth_admitted_relay_receives_response_before_close() {
        let gate = Arc::new(Semaphore::new(0));
        let entered = Arc::new(Notify::new());
        let exchange = Arc::new(FakeExchange {
            gate: Some(gate.clone()),
            entered: Some(entered.clone()),
            ..FakeExchange::default()
        });
        let forwarder = Arc::new(BrowserForwarder::new(exchange.clone()));
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        let mut requests = Vec::new();
        for _ in 0..MAX_RELAYS {
            let port = receipt.port;
            requests.push(tokio::task::spawn_blocking(move || normal_request(port)));
        }
        while exchange.count.load(Ordering::Acquire) < MAX_RELAYS {
            entered.notified().await;
        }
        assert!(
            forwarder
                .open(input(), 1000.0, Some(0))
                .await
                .expect("still active")
                .replayed
        );
        gate.add_permits(1);
        let mut responses = 0;
        for task in requests {
            if task
                .await
                .expect("client")
                .starts_with("HTTP/1.1 200 OK\r\n")
            {
                responses += 1;
            }
        }
        assert_eq!(responses, MAX_RELAYS);
        assert!(
            forwarder
                .close(&input().bridge_id)
                .await
                .expect("close joins")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_limit_drains_admitted_exchanges_before_closing() {
        struct GatedExchange {
            earlier_gate: Arc<Semaphore>,
            fifth_gate: Arc<Semaphore>,
            entered: Arc<Notify>,
            count: AtomicUsize,
        }
        impl PairedExchange for GatedExchange {
            fn exchange<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a Value,
                _: &'a str,
                _: i64,
                _: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>
            {
                Box::pin(async move {
                    let call = self.count.fetch_add(1, Ordering::AcqRel) + 1;
                    self.entered.notify_one();
                    let gate = if call == MAX_RELAYS {
                        &self.fifth_gate
                    } else {
                        &self.earlier_gate
                    };
                    gate.acquire().await.expect("gate").forget();
                    Ok(ExchangeOutcome::Applied(json!({
                        "status": 200,
                        "body": BASE64.encode(b"<html>ok</html>"),
                        "done": false
                    })))
                })
            }
        }

        let earlier_gate = Arc::new(Semaphore::new(0));
        let fifth_gate = Arc::new(Semaphore::new(0));
        let entered = Arc::new(Notify::new());
        let exchange = Arc::new(GatedExchange {
            earlier_gate: earlier_gate.clone(),
            fifth_gate: fifth_gate.clone(),
            entered: entered.clone(),
            count: AtomicUsize::new(0),
        });
        let forwarder = Arc::new(BrowserForwarder::new(exchange.clone()));
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        let requests = (0..MAX_RELAYS - 1)
            .map(|_| {
                let port = receipt.port;
                tokio::task::spawn_blocking(move || normal_request(port))
            })
            .collect::<Vec<_>>();
        while exchange.count.load(Ordering::Acquire) < MAX_RELAYS - 1 {
            entered.notified().await;
        }

        let fifth = tokio::task::spawn_blocking({
            let port = receipt.port;
            move || normal_request(port)
        });
        while exchange.count.load(Ordering::Acquire) < MAX_RELAYS {
            entered.notified().await;
        }

        let sixth = tokio::task::spawn_blocking({
            let port = receipt.port;
            move || normal_request(port)
        });
        assert!(
            sixth
                .await
                .expect("sixth refused client")
                .starts_with("HTTP/1.1 410 Gone\r\n")
        );

        fifth_gate.add_permits(1);
        assert!(
            fifth
                .await
                .expect("fifth admitted client")
                .starts_with("HTTP/1.1 200 OK\r\n")
        );
        earlier_gate.add_permits(MAX_RELAYS - 1);
        for request in requests {
            assert!(
                request
                    .await
                    .expect("admitted client")
                    .starts_with("HTTP/1.1 200 OK\r\n")
            );
        }
        assert!(
            forwarder
                .close(&input().bridge_id)
                .await
                .expect("close joins")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn absolute_deadline_cancels_exchange_and_close_joins() {
        struct Pending {
            entered: Arc<Notify>,
            dropped: Arc<AtomicBool>,
        }
        impl PairedExchange for Pending {
            fn exchange<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a Value,
                _: &'a str,
                _: i64,
                _: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>
            {
                Box::pin(async move {
                    struct Guard(Arc<AtomicBool>);
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::Release);
                        }
                    }
                    let _guard = Guard(self.dropped.clone());
                    self.entered.notify_one();
                    std::future::pending().await
                })
            }
        }
        let entered = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let forwarder = Arc::new(BrowserForwarder::new(Arc::new(Pending {
            entered: entered.clone(),
            dropped: dropped.clone(),
        })));
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        let port = receipt.port;
        let request = tokio::task::spawn_blocking(move || normal_request(port));
        entered.notified().await;
        tokio::time::advance(Duration::from_secs(CONNECTION_SECONDS) - RESPONSE_RESERVE).await;
        let response = request.await.expect("client receives timeout response");
        assert!(dropped.load(Ordering::Acquire));
        assert!(
            forwarder
                .wait_closed(&input().bridge_id)
                .await
                .expect("deadline closure")
        );
        assert!(
            forwarder
                .close(&input().bridge_id)
                .await
                .expect("close joins")
        );
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(!response.contains("secret"));
    }

    #[tokio::test(start_paused = true)]
    async fn exchange_ready_after_deadline_is_refused_and_closes_forward() {
        struct ReadyAfterDeadline {
            entered: Arc<Notify>,
            release: Arc<Semaphore>,
            dropped: Arc<AtomicBool>,
        }
        impl PairedExchange for ReadyAfterDeadline {
            fn exchange<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a Value,
                _: &'a str,
                _: i64,
                _: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>
            {
                Box::pin(async move {
                    struct DropSignal(Arc<AtomicBool>);
                    impl Drop for DropSignal {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::Release);
                        }
                    }
                    let _drop_signal = DropSignal(self.dropped.clone());
                    self.entered.notify_one();
                    self.release.acquire().await.expect("release").forget();
                    Ok(ExchangeOutcome::Applied(json!({
                        "status": 200,
                        "body": BASE64.encode(b"late"),
                        "done": false
                    })))
                })
            }
        }

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let forwarder = BrowserForwarder::new(Arc::new(ReadyAfterDeadline {
            entered: entered.clone(),
            release: release.clone(),
            dropped: dropped.clone(),
        }));
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        let request = tokio::task::spawn_blocking(move || normal_request(receipt.port));
        entered.notified().await;
        tokio::time::advance(
            Duration::from_secs(CONNECTION_SECONDS) - RESPONSE_RESERVE + Duration::from_millis(1),
        )
        .await;
        release.add_permits(1);

        let response = request.await.expect("late result response");
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(dropped.load(Ordering::Acquire));
        assert!(
            forwarder
                .wait_closed(&input().bridge_id)
                .await
                .expect("late result closes")
        );
        assert!(
            forwarder
                .close(&input().bridge_id)
                .await
                .expect("join closed forward")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_close_cancels_and_joins_an_inflight_exchange() {
        struct Pending {
            entered: Arc<Notify>,
            dropped: Arc<AtomicBool>,
        }
        impl PairedExchange for Pending {
            fn exchange<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a Value,
                _: &'a str,
                _: i64,
                _: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>
            {
                Box::pin(async move {
                    struct Guard(Arc<AtomicBool>);
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::Release);
                        }
                    }
                    let _guard = Guard(self.dropped.clone());
                    self.entered.notify_one();
                    std::future::pending().await
                })
            }
        }
        let entered = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let forwarder = Arc::new(BrowserForwarder::new(Arc::new(Pending {
            entered: entered.clone(),
            dropped: dropped.clone(),
        })));
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        let port = receipt.port;
        let request = tokio::task::spawn_blocking(move || normal_request(port));
        entered.notified().await;
        assert!(
            forwarder
                .close(&input().bridge_id)
                .await
                .expect("close waits for task tree")
        );
        assert!(dropped.load(Ordering::Acquire));
        let _ = request
            .await
            .expect("client exits when connection is cancelled");
    }

    #[tokio::test(start_paused = true)]
    async fn forward_expiry_stops_the_listener_task() {
        let forwarder = BrowserForwarder::new(Arc::new(FakeExchange::default()));
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        tokio::time::advance(Duration::from_secs(FORWARD_SECONDS)).await;
        assert!(
            forwarder
                .wait_closed(&input().bridge_id)
                .await
                .expect("expiry closure")
        );
        assert!(
            forwarder
                .close(&input().bridge_id)
                .await
                .expect("join expiry")
        );
        assert_ne!(receipt.port, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_exchange_returns_generic_502_once() {
        struct Unknown(AtomicUsize);
        impl PairedExchange for Unknown {
            fn exchange<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: &'a Value,
                _: &'a str,
                _: i64,
                _: Instant,
            ) -> Pin<Box<dyn Future<Output = Result<ExchangeOutcome, BrowserError>> + Send + 'a>>
            {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::AcqRel);
                    Ok(ExchangeOutcome::Unknown)
                })
            }
        }
        let exchange = Arc::new(Unknown(AtomicUsize::new(0)));
        let forwarder = BrowserForwarder::new(exchange.clone());
        let receipt = forwarder
            .open(input(), 1000.0, Some(0))
            .await
            .expect("open");
        let response = normal_request(receipt.port);
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(!response.contains("secret"));
        assert_eq!(exchange.0.load(Ordering::Acquire), 1);
        assert!(forwarder.close(&input().bridge_id).await.expect("close"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changed_identity_and_first_duplicate_header_are_refused_or_preserved() {
        let exchange = Arc::new(FakeExchange::default());
        let forwarder = BrowserForwarder::new(exchange.clone());
        let original = input();
        let receipt = forwarder
            .open(original.clone(), 1000.0, Some(0))
            .await
            .expect("open");
        let port = receipt.port;
        let raw = format!(
            "GET /oauth/callback HTTP/1.1\r\nUser-Agent: {}\r\nUser-Agent: short\r\n\r\n",
            "x".repeat(HEADER_LIMIT + 1)
        )
        .into_bytes();
        tokio::task::spawn_blocking(move || {
            let mut s =
                std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("loopback");
            s.write_all(&raw).expect("request");
            let mut r = String::new();
            s.read_to_string(&mut r).expect("response");
            r
        })
        .await
        .expect("client");
        assert_eq!(
            exchange.calls.lock().expect("calls")[0].2["headers"],
            json!({})
        );
        let changes: [IdentityChange; 6] = [
            ("server", |value| value.server.push_str("-changed")),
            ("token", |value| value.token.push_str("-changed")),
            ("host", |value| value.host = "localhost".into()),
            ("port", |value| value.port += 1),
            ("path", |value| value.path.push_str("/changed")),
            ("expiry", |value| value.expires_at += 1.0),
        ];
        for (field, change) in changes {
            let mut changed = original.clone();
            change(&mut changed);
            assert!(
                matches!(
                    forwarder.open(changed, 1000.0, Some(0)).await,
                    Err(BrowserError::Conflict)
                ),
                "a changed {field} under the same bridge identity conflicts"
            );
        }
        assert!(forwarder.close(&original.bridge_id).await.expect("close"));
    }

    #[test]
    fn errors_are_redacted_and_redirect_completion_is_retained() {
        assert_eq!(
            format!("{:?}", BrowserError::Listener(std::io::ErrorKind::Other)),
            "BrowserError(<redacted>)"
        );
        assert!(
            cleaned_result(&json!({"status":302,"done":true}))
                .expect("redirect")
                .3
        );
        assert!(matches!(
            cleaned_result(&json!({"status":200,"body":{}})),
            Err(BrowserError::Exchange)
        ));
    }

    #[test]
    fn arbitrary_file_is_not_a_control_chrome_skill() {
        let root = tempfile::tempdir().expect("fixture");
        let shared = root.path();
        let chrome = shared.join("plugins/cache/openai-bundled/chrome");
        let plugin = chrome.join("latest");
        let versioned = chrome.join("1.2.3");
        let skill = versioned.join("skills/control-chrome/SKILL.md");
        std::fs::create_dir_all(skill.parent().expect("parent")).expect("skill dirs");
        std::fs::write(&skill, "skill").expect("skill");
        std::os::unix::fs::symlink("1.2.3", &plugin).expect("latest points to installed version");
        let other = shared.join("node");
        std::fs::write(&other, "node").expect("node");
        let records = json!({"mcp_servers":{"node_repl":{"command":other}}});
        let mk = |path: String| BrowserConfigAssets {
            control_chrome_skill: Some(path),
            ..BrowserConfigAssets::default()
        };
        let good = BrowserConfigSnapshot::from_stored_records(
            &json!({}),
            &records,
            "/acct",
            &shared.to_string_lossy(),
            mk(std::fs::canonicalize(&skill)
                .expect("canonical skill")
                .to_string_lossy()
                .into_owned()),
        )
        .expect("snapshot");
        assert!(good.skill_available);
        let bad = BrowserConfigSnapshot::from_stored_records(
            &json!({}),
            &records,
            "/acct",
            &shared.to_string_lossy(),
            mk(other.to_string_lossy().into_owned()),
        )
        .expect("snapshot");
        assert!(!bad.skill_available);
    }
}
