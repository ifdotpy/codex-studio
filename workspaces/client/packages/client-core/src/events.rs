//! Server event parsing, reconnect state and HTTP lifetime belong to Rust.
use crate::{backend::Backend, sync};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

const MAX_EVENT: usize = 4 * 1024 * 1024;

// One opaque credit permits one decoded event, including all its HTTP chunks.
pub struct EventFlow {
    state: Mutex<(String, bool)>,
    ready: Notify,
}
impl EventFlow {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            state: Mutex::new((crate::server_keys::random_id()?, false)),
            ready: Notify::new(),
        })
    }
    pub fn credit(&self) -> String {
        self.state.lock().unwrap().0.clone()
    }
    pub fn grant(&self, credit: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.0 != credit || state.1 {
            return false;
        }
        state.1 = true;
        self.ready.notify_one();
        true
    }
    async fn wait(&self) -> String {
        loop {
            let notified = self.ready.notified();
            {
                let state = self.state.lock().unwrap();
                if state.1 {
                    return state.0.clone();
                }
            }
            notified.await;
        }
    }
    fn finish(&self) -> Result<(String, String), String> {
        let next = crate::server_keys::random_id()?;
        let mut state = self.state.lock().unwrap();
        let consumed = std::mem::replace(&mut state.0, next.clone());
        state.1 = false;
        Ok((consumed, next))
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
                if self.line.len() + self.data.len() >= MAX_EVENT {
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
            let _ = output
                .send(json!({"event":id,"error":sync::error("backend_config",error)}))
                .await;
            return;
        }
    };
    let path = params["path"].as_str().unwrap_or("");
    let mut last_id = String::new();
    let mut retry = 3000;
    loop {
        let started = std::time::Instant::now();
        let mut delivered = 0u64;
        let result: Result<bool, crate::protocol::CommandError> = async {
            let Some(mut response) = backend.event_stream(path, &last_id).await? else {
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
                .map_err(|_| sync::error("disconnected", "The UI disconnected."))?;
            let mut parser = Parser::new(last_id.clone());
            let mut admitted = false;
            loop {
                if let Some(flow) = &flow
                    && !admitted
                {
                    flow.wait().await;
                    admitted = true;
                }
                let chunk = tokio::time::timeout(Duration::from_secs(60), response.chunk())
                    .await
                    .map_err(|_| {
                        sync::error(
                            "event_timeout",
                            "The server event stream stopped responding.",
                        )
                    })?
                    .map_err(|e| sync::error("network", e))?;
                let Some(chunk) = chunk else {
                    return Ok(true);
                };
                let events = parser
                    .feed(&chunk)
                    .map_err(|e| sync::error("invalid_event", e))?;
                last_id = parser.id.clone();
                if let Some(value) = parser.retry {
                    retry = value;
                }
                for event in events {
                    if let Some(flow) = &flow {
                        if !admitted { flow.wait().await; }
                        let (consumed, next) = flow.finish().map_err(|e| sync::error("event_credit", e))?;
                        output.send(json!({"event":id,"consumedCredit":consumed,"credit":next,"value":event}))
                            .await.map_err(|_| sync::error("disconnected", "The UI disconnected."))?;
                        admitted = false;
                        delivered += 1;
                    } else {
                        emit(&output, &id, event).await.map_err(|_| sync::error("disconnected", "The UI disconnected."))?;
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
        crate::stream_log::line(&format!(
            "end {id} after_ms={} events={delivered} permanent={permanent} reason={message}",
            started.elapsed().as_millis()
        ));
        if emit(
            &output,
            &id,
            json!({"type":"error","closed":permanent,"message":message}),
        )
        .await
        .is_err()
            || permanent
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(retry)).await;
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
                tokio::time::advance(Duration::from_millis(100)).await;
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
    fn parser_rejects_unbounded_event_buffers() {
        let mut parser = Parser::new(String::new());
        assert!(parser.feed(&vec![b'a'; MAX_EVENT + 1]).is_err());
    }
}
