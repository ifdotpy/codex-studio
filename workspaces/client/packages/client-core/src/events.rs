//! Server event parsing, reconnect state and HTTP lifetime belong to Rust.
use crate::backend::{Backend, error as command_error};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

use studio_client_domain::NATIVE_SSE_EVENT_MAX_BYTES;

/// Longest server silence on an open stream before the core reconnects, while
/// the core reads (never while it waits for a UI credit). The sync server sends
/// a heartbeat every `SYNC_RESOURCE_HEARTBEAT_SECONDS`.
const EVENT_SILENCE_TIMEOUT: Duration = Duration::from_secs(60);
/// First reconnect delay in milliseconds. It doubles per failed attempt.
const RETRY_BASE_MS: u64 = 500;
/// Largest reconnect delay in milliseconds. A connection that stays up this
/// long resets the delay to `RETRY_BASE_MS`.
const RETRY_MAX_MS: u64 = 15_000;

/// Delay before reconnect `attempt` (0 = first), with 50% to 150% jitter. The
/// server's `retry:` value, in milliseconds, is the lower bound.
fn retry_delay(attempt: u32, server_retry_ms: u64, jitter: f64) -> u64 {
    let ceiling = RETRY_BASE_MS
        .saturating_mul(1 << attempt.min(16))
        .min(RETRY_MAX_MS);
    // Delays stay below 2^53, so the f64 round trip is exact enough.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let delay = (ceiling as f64 * (0.5 + jitter.clamp(0.0, 1.0))) as u64;
    delay.max(server_retry_ms).max(100)
}

fn jitter() -> f64 {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut byte = [0u8; 1];
    // Without randomness the delay uses its middle value.
    if SystemRandom::new().fill(&mut byte).is_err() {
        return 0.5;
    }
    f64::from(byte[0]) / 255.0
}

/// The UI side of one flow-controlled stream. One opaque credit permits one
/// decoded event, including all its HTTP chunks. After a lost connection the
/// core asks the UI for the next path (`resume`); the UI can end a connection
/// it cannot use (`restart`). The core owns every retry and delay.
pub struct EventFlow {
    state: Mutex<FlowState>,
    ready: Notify,
    restart: Notify,
}
struct FlowState {
    credit: String,
    granted: bool,
    /// The core waits for a resume path.
    awaiting_path: bool,
    path: Option<String>,
    /// The UI's reason to end the current connection.
    restart: Option<String>,
}
impl EventFlow {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            state: Mutex::new(FlowState {
                credit: crate::server_keys::random_id()?,
                granted: false,
                awaiting_path: false,
                path: None,
                restart: None,
            }),
            ready: Notify::new(),
            restart: Notify::new(),
        })
    }
    fn state(&self) -> std::sync::MutexGuard<'_, FlowState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn credit(&self) -> String {
        self.state().credit.clone()
    }
    pub fn grant(&self, credit: &str) -> bool {
        let mut state = self.state();
        if state.credit != credit || state.granted {
            return false;
        }
        state.granted = true;
        self.ready.notify_one();
        true
    }
    /// Accepts one path per resume request. It also clears earlier restart
    /// requests: they named the connection that this path replaces.
    pub fn resume(&self, path: &str) -> bool {
        let mut state = self.state();
        if !state.awaiting_path {
            return false;
        }
        state.awaiting_path = false;
        state.path = Some(path.to_owned());
        state.restart = None;
        self.ready.notify_one();
        true
    }
    pub fn restart(&self, reason: &str) {
        self.state().restart = Some(reason.chars().take(300).collect());
        self.restart.notify_one();
    }
    async fn wait(&self) -> String {
        loop {
            let notified = self.ready.notified();
            {
                let state = self.state();
                if state.granted {
                    return state.credit.clone();
                }
            }
            notified.await;
        }
    }
    /// Opens one resume request; `resume` answers it.
    pub(crate) fn expect_path(&self) {
        let mut state = self.state();
        state.awaiting_path = true;
        state.path = None;
    }
    async fn wait_path(&self) -> String {
        loop {
            let notified = self.ready.notified();
            if let Some(path) = self.state().path.take() {
                return path;
            }
            notified.await;
        }
    }
    async fn restarted(&self) -> String {
        loop {
            let notified = self.restart.notified();
            if let Some(reason) = self.state().restart.take() {
                return reason;
            }
            notified.await;
        }
    }
    fn finish(&self) -> Result<(String, String), String> {
        let next = crate::server_keys::random_id()?;
        let mut state = self.state();
        let consumed = std::mem::replace(&mut state.credit, next.clone());
        state.granted = false;
        Ok((consumed, next))
    }
}

/// Resolves when the UI ends the current connection; never without a flow.
async fn restarted(flow: Option<&Arc<EventFlow>>) -> String {
    match flow {
        Some(flow) => flow.restarted().await,
        None => std::future::pending().await,
    }
}

#[derive(Default)]
struct Parser {
    line: Vec<u8>,
    data: String,
    event: String,
    first: bool,
    cr: bool,
    id: String,
    retry: Option<u64>,
}
impl Parser {
    fn new(id: String) -> Self {
        Self {
            first: true,
            id,
            ..Self::default()
        }
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.cr && byte == b'\n' {
                self.cr = false;
                continue;
            }
            self.cr = false;
            if byte == b'\r' || byte == b'\n' {
                self.cr = byte == b'\r';
                if let Some(event) = self.line()? {
                    events.push(event);
                }
            } else {
                if self.line.len() + self.data.len() >= NATIVE_SSE_EVENT_MAX_BYTES {
                    return Err("The server event exceeds its size limit.".into());
                }
                self.line.push(byte);
            }
        }
        Ok(events)
    }
    fn line(&mut self) -> Result<Option<Value>, String> {
        let line = std::mem::take(&mut self.line);
        let decoded = String::from_utf8_lossy(&line);
        let text = if self.first {
            decoded.strip_prefix('\u{feff}').unwrap_or(&decoded)
        } else {
            &decoded
        };
        self.first = false;
        if text.is_empty() {
            let event = if self.data.is_empty() {
                None
            } else {
                self.data.pop(); // The final data newline is not part of the event.
                Some(
                    json!({"type":if self.event.is_empty(){"message"}else{&self.event},"data":self.data,"id":self.id}),
                )
            };
            self.data.clear();
            self.event.clear();
            return Ok(event);
        }
        if text.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = text.split_once(':').unwrap_or((text, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            "event" => self.event = value.into(),
            "id" if !value.contains('\0') => self.id = value.into(),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                self.retry = value.parse::<u64>().ok().map(|v| v.clamp(100, 30000));
            }
            _ => {}
        }
        Ok(None)
    }
}

async fn emit(output: &mpsc::Sender<Value>, id: &str, value: Value) -> Result<(), ()> {
    output
        .send(json!({"event":id,"value":value}))
        .await
        .map_err(|_| ())
}

pub async fn run(
    id: String,
    params: Value,
    backend: Arc<Result<Backend, String>>,
    output: mpsc::Sender<Value>,
) {
    run_controlled(id, params, backend, output, None).await;
}

pub async fn run_controlled(
    id: String,
    params: Value,
    backend: Arc<Result<Backend, String>>,
    output: mpsc::Sender<Value>,
    flow: Option<Arc<EventFlow>>,
) {
    let backend = match backend.as_ref() {
        Ok(backend) => backend,
        Err(error) => {
            drop(
                output
                    .send(json!({"event":id,"error":command_error("backend_config",error)}))
                    .await,
            );
            return;
        }
    };
    let mut path = params["path"].as_str().unwrap_or("").to_owned();
    let mut last_id = String::new();
    let mut server_retry = 0;
    let mut attempt = 0u32;
    loop {
        let started = std::time::Instant::now();
        let mut delivered = 0u64;
        let mut opened = false;
        let result: Result<bool, crate::protocol::CommandError> = async {
            let Some(mut response) = backend.event_stream(&path, &last_id).await? else {
                return Ok(false);
            };
            crate::stream_log::line(&format!(
                "open {id} resume={} path={}",
                !last_id.is_empty(),
                path.split('?').next().unwrap_or("")
            ));
            let mut open = json!({"event":id,"value":{"type":"open"}});
            if let Some(flow) = &flow { open["credit"] = json!(flow.credit()); }
            output.send(open)
                .await
                .map_err(|_| command_error("disconnected", "The UI disconnected."))?;
            opened = true;
            let mut parser = Parser::new(last_id.clone());
            let mut admitted = false;
            let restart = |reason: String| command_error("restart", format!("The UI restarted the stream: {reason}"));
            loop {
                if let Some(flow) = &flow
                    && !admitted
                {
                    tokio::select! {
                        _ = flow.wait() => {},
                        reason = flow.restarted() => return Err(restart(reason)),
                    }
                    admitted = true;
                }
                let chunk = tokio::select! {
                    chunk = tokio::time::timeout(EVENT_SILENCE_TIMEOUT, response.chunk()) => chunk,
                    reason = restarted(flow.as_ref()) => return Err(restart(reason)),
                }
                .map_err(|_| {
                    command_error(
                        "event_timeout",
                        "The server event stream stopped responding.",
                    )
                })?
                .map_err(|e| command_error("network", e))?;
                let Some(chunk) = chunk else {
                    return Ok(true);
                };
                let events = parser
                    .feed(&chunk)
                    .map_err(|e| command_error("invalid_event", e))?;
                last_id = parser.id.clone();
                if let Some(value) = parser.retry {
                    server_retry = value;
                }
                for event in events {
                    if let Some(flow) = &flow {
                        if !admitted {
                            tokio::select! {
                                _ = flow.wait() => {},
                                reason = flow.restarted() => return Err(restart(reason)),
                            }
                        }
                        let (consumed, next) = flow.finish().map_err(|e| command_error("event_credit", e))?;
                        output.send(json!({"event":id,"consumedCredit":consumed,"credit":next,"value":event}))
                            .await.map_err(|_| command_error("disconnected", "The UI disconnected."))?;
                        admitted = false;
                        delivered += 1;
                    } else {
                        emit(&output, &id, event).await.map_err(|_| command_error("disconnected", "The UI disconnected."))?;
                    }
                }
            }
        }
        .await;
        let permanent = matches!(result, Ok(false));
        let message = match result {
            Err(error) => error.message,
            _ => "The server event stream closed.".into(),
        };
        if started.elapsed() >= Duration::from_millis(RETRY_MAX_MS) {
            attempt = 0;
        }
        let delay = retry_delay(attempt, server_retry, jitter());
        attempt = attempt.saturating_add(1);
        crate::stream_log::line(&format!(
            "end {id} after_ms={} events={delivered} permanent={permanent} retry_ms={delay} reason={message}",
            started.elapsed().as_millis()
        ));
        if emit(
            &output,
            &id,
            json!({"type":"error","closed":permanent,"message":message,"opened":opened,"retryInMs":delay}),
        )
        .await
        .is_err()
            || permanent
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
        // A flow-controlled stream reconnects with the UI's current resources
        // and cursors. A plain stream resumes its own path with Last-Event-ID.
        if let Some(flow) = &flow {
            flow.expect_path();
            if emit(&output, &id, json!({"type":"resume"})).await.is_err() {
                return;
            }
            path = flow.wait_path().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn controlled_loopback_waits_for_each_credit_and_aborts_while_held() {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Arc::new(Backend::new(&format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            crate::backend::tests::read_http(&mut socket).await;
            // Both events arrive in one HTTP chunk. The second must stay in Rust.
            let body = "data: first\n\ndata: second\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let flow = Arc::new(EventFlow::new().unwrap());
        let (output, mut receive) = mpsc::channel(16);
        let task = tokio::spawn(run_controlled(
            "controlled".into(),
            json!({"path":"/api/sync/stream"}),
            backend,
            output,
            Some(flow.clone()),
        ));
        let open = receive.recv().await.unwrap();
        let first_credit = open["credit"].as_str().unwrap();
        assert!(receive.try_recv().is_err());
        assert!(flow.grant(first_credit));
        assert!(!flow.grant(first_credit));
        let first = receive.recv().await.unwrap();
        assert_eq!(first["value"]["data"], "first");
        assert_eq!(first["consumedCredit"], first_credit);
        // On this current-thread runtime, the producer reaches its next credit
        // wait in the same poll as its unblocked channel send.
        assert!(receive.try_recv().is_err());
        assert!(!flow.grant(first_credit));
        assert!(flow.grant(first["credit"].as_str().unwrap()));
        let second = receive.recv().await.unwrap();
        assert_eq!(second["value"]["data"], "second");
        assert_eq!(second["consumedCredit"], first["credit"]);
        assert!(receive.try_recv().is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        server.await.unwrap();
    }
    #[test]
    fn byte_chunks_preserve_unicode_crlf_event_names_and_reconnect_ids() {
        let stream = "\u{feff}: heartbeat\r\nid: turn-1\rdata: Привет 🦀\r\ndata: second\nevent: unavailable\nretry: 750\n\nid: invalid\0id\ndata:\n\ndata: incomplete";
        let mut parser = Parser::new(String::new());
        let mut events = Vec::new();
        for byte in stream.as_bytes() {
            events.extend(parser.feed(&[*byte]).unwrap());
        }
        assert_eq!(
            events,
            json!([
                {"type":"unavailable","id":"turn-1","data":"Привет 🦀\nsecond"},
                {"type":"message","id":"turn-1","data":""}
            ])
            .as_array()
            .unwrap()
            .to_vec()
        );
        assert_eq!(parser.id, "turn-1");
        assert_eq!(parser.retry, Some(750));
        // A partial final event must not be dispatched after connection loss.
        assert_eq!(events.len(), 2);
    }
    #[tokio::test]
    async fn reconnect_keeps_the_last_event_id_and_204_stops_the_stream() {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Arc::new(Backend::new(&format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut first).await;
            assert!(headers.starts_with("GET /api/transcript/stream?id=one "));
            let body = "id: record-1\nretry: 100\ndata: Привет 🦀\n\n";
            first.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut second).await;
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("last-event-id: record-1")
            );
            second
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let (output, mut receive) = mpsc::channel(16);
        let stream = tokio::spawn(run(
            "stream".into(),
            json!({"path":"/api/transcript/stream?id=one"}),
            backend,
            output,
        ));
        let mut events = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(2), receive.recv())
            .await
            .unwrap()
        {
            if event["value"]["type"] == "error" && event["value"]["closed"] == false {
                // On this current-thread runtime, run() reaches its retry sleep
                // in the same poll as the error send, before this receiver runs.
                tokio::time::pause();
                let delay = event["value"]["retryInMs"].as_u64().unwrap();
                // The server's retry: 100 is only a floor under the backoff.
                assert!((250..=750).contains(&delay), "{delay}");
                tokio::time::advance(Duration::from_millis(delay)).await;
                tokio::time::resume();
            }
            events.push(event["value"].clone());
        }
        assert_eq!(
            events
                .iter()
                .map(|v| v["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["open", "message", "error", "error"]
        );
        assert_eq!(events[1]["data"], "Привет 🦀");
        assert_eq!(events[3]["closed"], true);
        stream.await.unwrap();
        server.await.unwrap();
    }
    #[test]
    fn retry_delay_doubles_to_its_ceiling_and_keeps_the_server_floor() {
        assert_eq!(retry_delay(0, 0, 0.5), 500);
        assert_eq!(retry_delay(1, 0, 0.5), 1000);
        assert_eq!(retry_delay(0, 0, 0.0), 250);
        assert_eq!(retry_delay(0, 0, 1.0), 750);
        assert_eq!(retry_delay(10, 0, 0.5), RETRY_MAX_MS);
        assert_eq!(retry_delay(u32::MAX, 0, 1.0), RETRY_MAX_MS * 3 / 2);
        assert_eq!(retry_delay(0, 4000, 0.5), 4000);
    }

    fn chunk(text: &str) -> String {
        format!("{:x}\r\n{text}\r\n", text.len())
    }
    const CHUNKED: &str =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";

    async fn next(receive: &mut mpsc::Receiver<Value>) -> Value {
        tokio::time::timeout(Duration::from_secs(5), receive.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn wait_retry(
        receive: &mut mpsc::Receiver<Value>,
        range: std::ops::RangeInclusive<u64>,
    ) -> Value {
        let error = next(receive).await;
        assert_eq!(error["value"]["type"], "error", "{error}");
        assert_eq!(error["value"]["closed"], false);
        let delay = error["value"]["retryInMs"].as_u64().unwrap();
        assert!(range.contains(&delay), "{delay}");
        tokio::time::pause();
        tokio::time::advance(Duration::from_millis(delay)).await;
        tokio::time::resume();
        error["value"].clone()
    }

    #[tokio::test]
    async fn managed_stream_reconnects_with_backoff_and_the_path_the_ui_resumes() {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Arc::new(Backend::new(&format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        let (hold, held) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut first).await;
            assert!(headers.starts_with("GET /api/sync/stream?first "));
            let body = "id: 7\ndata: first\n\n";
            first.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut second).await;
            assert!(
                headers.starts_with("GET /api/sync/stream?second "),
                "{headers}"
            );
            assert!(headers.to_ascii_lowercase().contains("last-event-id: 7"));
            second
                .write_all(format!("{CHUNKED}{}", chunk("data: second\n\n")).as_bytes())
                .await
                .unwrap();
            // Keep the connection open until the UI restarts it.
            drop(held.await);
        });
        let flow = Arc::new(EventFlow::new().unwrap());
        let (output, mut receive) = mpsc::channel(16);
        let task = tokio::spawn(run_controlled(
            "managed".into(),
            json!({"path":"/api/sync/stream?first"}),
            backend,
            output,
            Some(flow.clone()),
        ));
        let open = next(&mut receive).await;
        assert!(flow.grant(open["credit"].as_str().unwrap()));
        let first = next(&mut receive).await;
        assert_eq!(first["value"]["data"], "first");
        // A credit granted before the drop admits the next connection's first event.
        assert!(flow.grant(first["credit"].as_str().unwrap()));
        let lost = wait_retry(&mut receive, 250..=750).await;
        assert_eq!(lost["opened"], true);
        assert!(
            !flow.resume("/api/sync/stream?early"),
            "no resume request yet"
        );
        assert_eq!(next(&mut receive).await["value"]["type"], "resume");
        assert!(flow.resume("/api/sync/stream?second"));
        assert!(!flow.resume("/api/sync/stream?duplicate"));
        let reopened = next(&mut receive).await;
        assert_eq!(reopened["value"]["type"], "open");
        assert_eq!(reopened["credit"], first["credit"]);
        let second = next(&mut receive).await;
        assert_eq!(second["value"]["data"], "second");
        // The UI cannot use this connection. The core ends it and backs off further.
        flow.restart("invalid-frame: bad revision");
        let restarted = wait_retry(&mut receive, 500..=1500).await;
        assert!(
            restarted["message"]
                .as_str()
                .unwrap()
                .contains("invalid-frame: bad revision")
        );
        assert_eq!(next(&mut receive).await["value"]["type"], "resume");
        task.abort();
        assert!(hold.send(()).is_ok());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn managed_stream_watchdog_ends_a_silent_connection_but_not_a_credit_wait() {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Arc::new(Backend::new(&format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        let (hold, held) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            crate::backend::tests::read_http(&mut socket).await;
            socket
                .write_all(format!("{CHUNKED}{}", chunk("data: one\n\n")).as_bytes())
                .await
                .unwrap();
            drop(held.await);
        });
        let flow = Arc::new(EventFlow::new().unwrap());
        let (output, mut receive) = mpsc::channel(16);
        let task = tokio::spawn(run_controlled(
            "silent".into(),
            json!({"path":"/api/sync/stream"}),
            backend,
            output,
            Some(flow.clone()),
        ));
        let open = next(&mut receive).await;
        // Without a credit the core waits for the UI, however long that takes.
        tokio::time::pause();
        tokio::time::advance(EVENT_SILENCE_TIMEOUT * 2).await;
        tokio::time::resume();
        assert!(receive.try_recv().is_err());
        assert!(flow.grant(open["credit"].as_str().unwrap()));
        let one = next(&mut receive).await;
        assert_eq!(one["value"]["data"], "one");
        assert!(flow.grant(one["credit"].as_str().unwrap()));
        // The server now stays silent while the core reads.
        tokio::time::pause();
        let error = receive.recv().await.unwrap();
        tokio::time::resume();
        assert_eq!(error["value"]["closed"], false);
        assert_eq!(
            error["value"]["message"],
            "The server event stream stopped responding."
        );
        task.abort();
        assert!(hold.send(()).is_ok());
        server.await.unwrap();
    }
    #[test]
    fn parser_rejects_unbounded_event_buffers() {
        let mut parser = Parser::new(String::new());
        assert!(
            parser
                .feed(&vec![b'a'; NATIVE_SSE_EVENT_MAX_BYTES + 1])
                .is_err()
        );
    }
}
