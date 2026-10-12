//! Durable terminal state and the narrow AppServer session boundary.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError, TerminalsRepository};
use thiserror::Error;

pub const OUTPUT_LIMIT: usize = 1_048_576;
pub const OUTPUT_CHUNK: usize = 65_536;
pub const INPUT_LIMIT: usize = 65_536;
pub const MAX_COLUMNS: u64 = 1_000;
pub const MAX_ROWS: u64 = 500;
pub const MAX_TITLE_CHARS: usize = 120;

static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(0);

pub trait TerminalSession: Send + Sync {
    fn send_request(
        &self,
        operation_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, SessionError>;
    fn receive_notification(&self) -> Result<Option<TerminalNotification>, SessionError>;
    fn close(&self) -> Result<(), SessionError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalNotification {
    pub terminal_id: String,
    pub sequence: u64,
    pub event: TerminalEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalEvent {
    Output(Vec<u8>),
    Exit(Option<i64>),
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum SessionError {
    #[error("session request outcome is unknown")]
    UnknownOutcome,
    #[error("session request was rejected")]
    Rejected,
    #[error("session connection failed")]
    Disconnected,
}

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("invalid terminal request")]
    InvalidRequest,
    #[error("unknown terminal")]
    UnknownTerminal,
    #[error("terminal is no longer running")]
    NotRunning,
    #[error("invalid stored terminal object terminals/{0}")]
    InvalidStoredObject(String),
    #[error("terminal request id has different content")]
    ReceiptConflict,
    #[error("terminal output cursor is invalid")]
    InvalidCursor,
    #[error("terminal output history range is invalid")]
    InvalidHistoryRange,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Session(#[from] SessionError),
}

pub struct TerminalDriver<S: TerminalSession> {
    store: std::sync::Arc<RuntimeStore>,
    session: S,
}

pub struct TerminalCreate<'a> {
    pub request_id: &'a str,
    pub agent: &'a str,
    pub cwd: &'a Path,
    pub columns: u64,
    pub rows: u64,
    pub shell: &'a str,
    pub environment: Value,
}

pub fn listing(store: &RuntimeStore) -> Result<Value, TerminalError> {
    let connection = store.read()?;
    let mut items = Vec::new();
    for (id, raw) in TerminalsRepository::list(&connection)? {
        let record = parse_record(&id, &raw)?;
        if record.get("status").and_then(Value::as_str) != Some("closed") {
            items.push(record);
        }
    }
    items.sort_by(|left, right| {
        let left = left
            .get("created")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        let right = right
            .get("created")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        right.total_cmp(&left)
    });
    Ok(json!({"items": items}))
}

pub fn output(store: &RuntimeStore, id: &str, offset: i64) -> Result<Value, TerminalError> {
    if offset < 0 {
        return Err(TerminalError::InvalidCursor);
    }
    let connection = store.read()?;
    let row = TerminalsRepository::get(&connection, id)?.ok_or(TerminalError::UnknownTerminal)?;
    let record = parse_record(id, &row.record)?;
    let start = row.offset;
    let end = start + row.output.chars().count() as i64;
    let cursor = offset.clamp(start, end);
    let text = slice_chars(&row.output, (cursor - start) as usize, usize::MAX);
    Ok(json!({
        "text": text,
        "offset": end,
        "truncated": offset < start,
        "status": record["status"],
        "exitCode": record.get("exitCode").cloned().unwrap_or(Value::Null),
        "error": record.get("error").cloned().unwrap_or(Value::Null),
    }))
}

pub fn history_output(
    store: &RuntimeStore,
    id: &str,
    requested_offset: i64,
    limit: i64,
) -> Result<Value, TerminalError> {
    if requested_offset < 0 || !(1..=OUTPUT_LIMIT as i64).contains(&limit) {
        return Err(TerminalError::InvalidHistoryRange);
    }
    TerminalsRepository::get(&store.read()?, id)?.ok_or(TerminalError::UnknownTerminal)?;
    let row = store.transaction(|transaction| {
        let row = TerminalsRepository::get(transaction, id)?
            .ok_or_else(|| StoreError::SchemaMismatch(format!("terminal {id} disappeared")))?;
        if !row.output.is_empty() && !TerminalsRepository::has_archive(transaction, id)? {
            TerminalsRepository::append_output(transaction, id, row.offset, &row.output)?;
        }
        Ok(row)
    })?;
    let connection = store.read()?;
    let record = parse_record(id, &row.record)?;
    let start = TerminalsRepository::archive_start(&connection, id)?.unwrap_or(row.offset);
    let available = row.offset + row.output.chars().count() as i64;
    let cursor = requested_offset.clamp(start, available);
    let end = (cursor + limit).min(available);
    let chunks = TerminalsRepository::archive_chunks(
        &connection,
        id,
        cursor,
        end,
        limit / OUTPUT_CHUNK as i64 + 2,
    )?;
    let mut text = String::new();
    for (chunk_start, chunk_end, value) in chunks {
        let from = cursor.max(chunk_start);
        let to = end.min(chunk_end);
        text.push_str(&slice_chars(
            &value,
            (from - chunk_start) as usize,
            (to - from) as usize,
        ));
    }
    let next = cursor + text.chars().count() as i64;
    Ok(json!({
        "text": text,
        "offset": next,
        "availableOffset": available,
        "historyStart": start,
        "hasMore": next < available,
        "truncated": requested_offset < start,
        "status": record["status"],
        "exitCode": record.get("exitCode").cloned().unwrap_or(Value::Null),
        "error": record.get("error").cloned().unwrap_or(Value::Null),
    }))
}

impl<S: TerminalSession> TerminalDriver<S> {
    pub fn new(store: std::sync::Arc<RuntimeStore>, session: S) -> Self {
        Self { store, session }
    }

    pub fn create(&self, request: TerminalCreate<'_>) -> Result<Value, TerminalError> {
        let TerminalCreate {
            request_id,
            agent,
            cwd,
            columns,
            rows,
            shell,
            environment,
        } = request;
        validate_dimensions(columns, rows)?;
        if request_id.is_empty() || request_id.chars().count() > 200 || !cwd.is_dir() {
            return Err(TerminalError::InvalidRequest);
        }
        let request = json!({"id": request_id, "agent": agent, "cols": columns, "rows": rows});
        let signature = request.to_string();
        if let Some((prior_signature, result)) =
            TerminalsRepository::receipt(&self.store.read()?, request_id)?
        {
            if prior_signature != signature {
                return Err(TerminalError::ReceiptConflict);
            }
            return serde_json::from_str(&result)
                .map_err(|_| TerminalError::InvalidStoredObject(request_id.into()));
        }
        let now = now_seconds();
        let id = new_id(now);
        let cwd = cwd
            .canonicalize()
            .map_err(|_| TerminalError::InvalidRequest)?;
        let record = json!({
            "id": id, "agent": agent, "title": "Terminal", "cwd": cwd,
            "status": "running", "created": now, "updated": now, "exitCode": null
        });
        let raw = record.to_string();
        self.store.transaction(|transaction| {
            TerminalsRepository::put(transaction, &id, &raw, "", 0)?;
            TerminalsRepository::save_receipt(transaction, request_id, &signature, &raw)
        })?;
        let params = json!({
            "processHandle": id,
            "cwd": cwd,
            "tty": true,
            "command": [shell, "-l"],
            "size": {"rows": rows, "cols": columns},
            "env": environment,
            "timeoutMs": null,
            "outputBytesCap": null
        });
        match self
            .session
            .send_request(&format!("terminal-spawn:{id}"), "process/spawn", params)
        {
            Ok(_) => Ok(record),
            Err(SessionError::UnknownOutcome) => Ok(record),
            Err(error) => {
                let mut failed = record;
                failed["status"] = Value::String("exited".into());
                failed["error"] = json!(error.to_string());
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, &id, &failed.to_string())?;
                    TerminalsRepository::update_receipt(
                        transaction,
                        request_id,
                        &failed.to_string(),
                    )
                })?;
                Err(TerminalError::Session(error))
            }
        }
    }

    pub fn action(&self, action: &str, data: &Value) -> Result<Value, TerminalError> {
        let id = data
            .get("id")
            .and_then(Value::as_str)
            .ok_or(TerminalError::InvalidRequest)?;
        let row = TerminalsRepository::get(&self.store.read()?, id)?
            .ok_or_else(|| StoreError::SchemaMismatch(format!("terminal {id} disappeared")))?;
        let mut record = parse_record(id, &row.record)?;
        match action {
            "rename" => {
                let title = data
                    .get("title")
                    .and_then(Value::as_str)
                    .ok_or(TerminalError::InvalidRequest)?
                    .trim();
                if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
                    return Err(TerminalError::InvalidRequest);
                }
                record["title"] = json!(title);
                record["updated"] = json!(now_seconds());
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, id, &record.to_string())
                })?;
                Ok(record)
            }
            "input" => self.send_input(id, data, record),
            "resize" => {
                let columns = data.get("cols").and_then(Value::as_u64).unwrap_or(100);
                let rows = data.get("rows").and_then(Value::as_u64).unwrap_or(28);
                validate_dimensions(columns, rows)?;
                self.require_running(&record)?;
                self.session.send_request(
                    &format!("terminal-resize:{id}:{}:{}", columns, rows),
                    "process/resizePty",
                    json!({"processHandle": id, "size": {"rows": rows, "cols": columns}}),
                )?;
                record["updated"] = json!(now_seconds());
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, id, &record.to_string())
                })?;
                Ok(record)
            }
            "close" => {
                record["status"] = json!("closed");
                record["updated"] = json!(now_seconds());
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, id, &record.to_string())
                })?;
                self.session.send_request(
                    &format!("terminal-close:{id}"),
                    "process/kill",
                    json!({"processHandle": id}),
                )?;
                Ok(record)
            }
            _ => Err(TerminalError::InvalidRequest),
        }
    }

    pub fn apply_notification(
        &self,
        notification: TerminalNotification,
    ) -> Result<(), TerminalError> {
        let id = notification.terminal_id;
        let sequence =
            i64::try_from(notification.sequence).map_err(|_| TerminalError::InvalidRequest)?;
        if sequence <= 0 {
            return Err(TerminalError::InvalidRequest);
        }
        let last = TerminalsRepository::event_cursor(&self.store.read()?, &id)?.unwrap_or(0);
        if sequence <= last {
            return Ok(());
        }
        match notification.event {
            TerminalEvent::Output(bytes) => self.append_output(&id, notification.sequence, &bytes),
            TerminalEvent::Exit(code) => {
                let row = TerminalsRepository::get(&self.store.read()?, &id)?
                    .ok_or(TerminalError::UnknownTerminal)?;
                let mut record = parse_record(&id, &row.record)?;
                if record["status"] != "closed" {
                    record["status"] = json!("exited");
                }
                record["exitCode"] = code.map_or(Value::Null, |value| json!(value));
                record["updated"] = json!(now_seconds());
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, &id, &record.to_string())?;
                    TerminalsRepository::save_event_cursor(transaction, &id, sequence)?;
                    Ok(())
                })?;
                Ok(())
            }
        }
    }

    fn append_output(&self, id: &str, sequence: u64, bytes: &[u8]) -> Result<(), TerminalError> {
        let pending =
            TerminalsRepository::decoder_pending(&self.store.read()?, id)?.unwrap_or_default();
        let mut combined = pending;
        combined.extend_from_slice(bytes);
        let (text, pending) = decode_utf8(&combined);
        self.store.transaction(|transaction| {
            let row = TerminalsRepository::get(transaction, id)?
                .ok_or_else(|| StoreError::SchemaMismatch(format!("terminal {id} disappeared")))?;
            let mut output = row.output;
            let mut offset = row.offset;
            let chars = text.chars().count();
            let drop = output
                .chars()
                .count()
                .saturating_add(chars)
                .saturating_sub(OUTPUT_LIMIT);
            if !text.is_empty() {
                let start = offset + output.chars().count() as i64;
                TerminalsRepository::append_output(transaction, id, start, &text)?;
                output.push_str(&text);
            }
            if drop > 0 {
                output = slice_chars(&output, drop, usize::MAX);
                offset += drop as i64;
            }
            TerminalsRepository::put(transaction, id, &row.record, &output, offset)?;
            TerminalsRepository::save_decoder_pending(transaction, id, &pending)?;
            let sequence = i64::try_from(sequence).map_err(|_| {
                StoreError::SchemaMismatch("terminal event sequence exceeds SQLite range".into())
            })?;
            TerminalsRepository::save_event_cursor(transaction, id, sequence)?;
            Ok(())
        })?;
        Ok(())
    }

    fn send_input(&self, id: &str, data: &Value, record: Value) -> Result<Value, TerminalError> {
        self.require_running(&record)?;
        let request_id = data
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or(TerminalError::InvalidRequest)?;
        let text = data
            .get("text")
            .and_then(Value::as_str)
            .ok_or(TerminalError::InvalidRequest)?;
        if text.len() > INPUT_LIMIT || request_id.is_empty() || request_id.chars().count() > 200 {
            return Err(TerminalError::InvalidRequest);
        }
        let body = json!({"action":"input", "id":id, "text":text, "request_id":request_id});
        let signature = body.to_string();
        if let Some((prior_signature, result)) =
            TerminalsRepository::receipt(&self.store.read()?, request_id)?
        {
            if prior_signature != signature {
                return Err(TerminalError::ReceiptConflict);
            }
            return serde_json::from_str(&result)
                .map_err(|_| TerminalError::InvalidStoredObject(id.into()));
        }
        let uncertain = json!({"ok":false,"delivery":"uncertain","error":"Input delivery is uncertain. Check the terminal before sending it again."});
        self.store.transaction(|transaction| {
            TerminalsRepository::save_receipt(
                transaction,
                request_id,
                &signature,
                &uncertain.to_string(),
            )
        })?;
        let result = json!({"ok":true,"delivery":"sent"});
        match self.session.send_request(
            &format!("terminal-input:{request_id}"),
            "process/writeStdin",
            json!({"processHandle":id, "deltaBase64":base64_encode(text.as_bytes())}),
        ) {
            Ok(_) => {}
            Err(SessionError::UnknownOutcome) => return Ok(uncertain),
            Err(error) => return Err(TerminalError::Session(error)),
        }
        self.store.transaction(|transaction| {
            TerminalsRepository::update_receipt(transaction, request_id, &result.to_string())
        })?;
        Ok(result)
    }

    pub fn drain_notifications(&self) -> Result<usize, TerminalError> {
        let mut count = 0;
        while let Some(notification) = self.session.receive_notification()? {
            self.apply_notification(notification)?;
            count += 1;
        }
        Ok(count)
    }

    /// Reconcile notifications delivered after reconnecting to the same
    /// supervisor handle. Existing process handles are never spawned again.
    pub fn reconcile_after_restart(&self) -> Result<usize, TerminalError> {
        for (id, raw) in TerminalsRepository::list(&self.store.read()?)? {
            parse_record(&id, &raw)?;
        }
        self.drain_notifications()
    }

    pub fn close_session(&self) -> Result<(), TerminalError> {
        self.session.close().map_err(TerminalError::Session)
    }

    fn require_running(&self, record: &Value) -> Result<(), TerminalError> {
        if record.get("status").and_then(Value::as_str) == Some("running") {
            Ok(())
        } else {
            Err(TerminalError::NotRunning)
        }
    }
}

fn parse_record(id: &str, raw: &str) -> Result<Value, TerminalError> {
    let value = serde_json::from_str::<Value>(raw)
        .map_err(|_| TerminalError::InvalidStoredObject(id.into()))?;
    let Some(object) = value.as_object() else {
        return Err(TerminalError::InvalidStoredObject(id.into()));
    };
    if object.get("id").and_then(Value::as_str) != Some(id)
        || !matches!(
            object.get("status").and_then(Value::as_str),
            Some("running" | "exited" | "closed")
        )
        || object.get("agent").and_then(Value::as_str).is_none()
        || object.get("title").and_then(Value::as_str).is_none()
        || object.get("cwd").and_then(Value::as_str).is_none()
        || object.get("created").and_then(Value::as_f64).is_none()
    {
        return Err(TerminalError::InvalidStoredObject(id.into()));
    }
    Ok(value)
}

fn validate_dimensions(columns: u64, rows: u64) -> Result<(), TerminalError> {
    if !(2..=MAX_COLUMNS).contains(&columns) || !(1..=MAX_ROWS).contains(&rows) {
        return Err(TerminalError::InvalidRequest);
    }
    Ok(())
}

fn slice_chars(value: &str, start: usize, count: usize) -> String {
    value.chars().skip(start).take(count).collect()
}

fn decode_utf8(bytes: &[u8]) -> (String, Vec<u8>) {
    match std::str::from_utf8(bytes) {
        Ok(value) => (value.to_owned(), Vec::new()),
        Err(error) => {
            let valid = error.valid_up_to();
            let mut output = String::from_utf8_lossy(&bytes[..valid]).into_owned();
            let Some(invalid_len) = error.error_len() else {
                return (output, bytes[valid..].to_vec());
            };
            output.push('\u{fffd}');
            let (tail, pending) = decode_utf8(&bytes[valid + invalid_len..]);
            output.push_str(&tail);
            (output, pending)
        }
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for shift in [18, 12, 6, 0] {
            encoded.push(ALPHABET[((value >> shift) & 0x3f) as usize] as char);
        }
        if chunk.len() < 3 {
            encoded.replace_range(encoded.len() - 1.., "=");
        }
        if chunk.len() < 2 {
            encoded.replace_range(encoded.len() - 2..encoded.len() - 1, "=");
        }
    }
    encoded
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn new_id(now: f64) -> String {
    let counter = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "terminal-{:x}-{:x}-{:x}",
        std::process::id(),
        (now * 1_000_000.0) as u64,
        counter
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::{
        SessionError, TerminalEvent, TerminalNotification, TerminalSession, base64_encode,
        decode_utf8, validate_dimensions,
    };
    use serde_json::Value;

    #[derive(Default)]
    struct FakeSession {
        requests: Mutex<Vec<(String, String, Value)>>,
        notifications: Mutex<VecDeque<TerminalNotification>>,
        closed: Mutex<bool>,
    }

    impl TerminalSession for FakeSession {
        fn send_request(
            &self,
            operation_id: &str,
            method: &str,
            params: Value,
        ) -> Result<Value, SessionError> {
            self.requests.lock().expect("fake request lock").push((
                operation_id.to_owned(),
                method.to_owned(),
                params,
            ));
            Ok(Value::Null)
        }

        fn receive_notification(&self) -> Result<Option<TerminalNotification>, SessionError> {
            Ok(self
                .notifications
                .lock()
                .expect("fake notification lock")
                .pop_front())
        }

        fn close(&self) -> Result<(), SessionError> {
            *self.closed.lock().expect("fake close lock") = true;
            Ok(())
        }
    }

    #[test]
    fn dimensions_enforce_the_python_terminal_limits() {
        assert!(validate_dimensions(2, 1).is_ok());
        assert!(validate_dimensions(1, 24).is_err());
        assert!(validate_dimensions(1001, 24).is_err());
        assert!(validate_dimensions(80, 501).is_err());
    }

    #[test]
    fn utf8_decoder_retains_an_incomplete_character_between_events() {
        let (first, pending) = decode_utf8(&[0xf0, 0x9f]);
        assert!(first.is_empty());
        assert_eq!(pending, [0xf0, 0x9f]);
        let mut combined = pending;
        combined.extend_from_slice(&[0x99, 0x82]);
        assert_eq!(decode_utf8(&combined), ("🙂".to_owned(), Vec::new()));
    }

    #[test]
    fn input_uses_the_appserver_base64_wire_representation() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode("🙂".as_bytes()), "8J+Zgg==");
    }

    #[test]
    fn fake_session_captures_requests_and_delivers_terminal_events() {
        let session = FakeSession::default();
        session
            .send_request(
                "terminal-spawn:1",
                "process/spawn",
                serde_json::json!({"processHandle":"1", "tty":true}),
            )
            .expect("fake session accepts request");
        session
            .notifications
            .lock()
            .expect("fake notification lock")
            .push_back(TerminalNotification {
                terminal_id: "1".to_owned(),
                sequence: 1,
                event: TerminalEvent::Output(b"hello".to_vec()),
            });
        assert_eq!(
            session.receive_notification().expect("fake notification"),
            Some(TerminalNotification {
                terminal_id: "1".to_owned(),
                sequence: 1,
                event: TerminalEvent::Output(b"hello".to_vec()),
            })
        );
        assert_eq!(session.requests.lock().expect("fake request lock").len(), 1);
        session.close().expect("fake session closes");
        assert!(*session.closed.lock().expect("fake close lock"));
    }
}
