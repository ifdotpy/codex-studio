//! Durable terminal state and the narrow AppServer session boundary.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_store::{RuntimeStore, StoreError, TerminalRecord, TerminalsRepository};
use thiserror::Error;

pub const OUTPUT_LIMIT: usize = 1_048_576;
pub const OUTPUT_CHUNK: usize = 65_536;
pub const INPUT_LIMIT: usize = 65_536;
pub const MAX_TITLE_CHARS: usize = 120;

pub trait TerminalSession: Send + Sync {
    fn send_request(
        &self,
        operation_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, SessionError>;
    fn receive_notification(&self) -> Result<Option<TerminalNotification>, SessionError>;
    fn receive_outcome(&self) -> Result<Option<TerminalOutcome>, SessionError>;
    fn close(&self) -> Result<(), SessionError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalNotification {
    pub terminal_id: String,
    pub sequence: u64,
    pub event: TerminalEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalOutcome {
    pub operation_id: String,
    pub result: Result<Value, SessionError>,
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
    store: Arc<RuntimeStore>,
    session: S,
    event_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
    for record in TerminalsRepository::list(&connection)? {
        if record.get("status").and_then(Value::as_str) != Some("closed") {
            items.push(record.value().clone());
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
    let record = row.record;
    let start = row.offset;
    let end = start + row.output.chars().count() as i64;
    let cursor = offset.clamp(start, end);
    let text = slice_chars(&row.output, (cursor - start) as usize, usize::MAX);
    let mut result = json!({
        "text": text,
        "offset": end,
        "truncated": offset < start,
    });
    add_status(&mut result, &record);
    Ok(result)
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
    let current =
        TerminalsRepository::get(&store.read()?, id)?.ok_or(TerminalError::UnknownTerminal)?;
    let record = current.record;
    let row = store.transaction(|transaction| {
        let row = TerminalsRepository::get(transaction, id)?
            .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?;
        if !row.output.is_empty() && TerminalsRepository::archive_start(transaction, id)?.is_none()
        {
            TerminalsRepository::append_output(transaction, id, row.offset, &row.output)?;
        }
        Ok(row)
    })?;
    let connection = store.read()?;
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
    let mut result = json!({
        "text": text,
        "offset": next,
        "availableOffset": available,
        "historyStart": start,
        "hasMore": next < available,
        "truncated": requested_offset < start,
    });
    add_status(&mut result, &record);
    Ok(result)
}

fn add_status(result: &mut Value, record: &TerminalRecord) {
    if let Some(fields) = result.as_object_mut() {
        fields.insert(
            "status".into(),
            record.get("status").cloned().unwrap_or(Value::Null),
        );
        fields.insert(
            "exitCode".into(),
            record.get("exitCode").cloned().unwrap_or(Value::Null),
        );
        fields.insert(
            "error".into(),
            record.get("error").cloned().unwrap_or(Value::Null),
        );
    }
}

impl<S: TerminalSession> TerminalDriver<S> {
    pub fn new(store: std::sync::Arc<RuntimeStore>, session: S) -> Self {
        Self {
            store,
            session,
            event_locks: Mutex::new(HashMap::new()),
        }
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
        if request_id.is_empty() || request_id.chars().count() > 200 || !cwd.is_dir() {
            return Err(TerminalError::InvalidRequest);
        }
        let request = json!({"id": request_id, "agent": agent, "cols": columns, "rows": rows});
        let signature = receipt_signature(&request);
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
        let id = new_id()?;
        let cwd = cwd
            .canonicalize()
            .map_err(|_| TerminalError::InvalidRequest)?;
        let record = json!({
            "id": id, "agent": agent, "title": "Terminal", "cwd": cwd,
            "status": "running", "created": now, "updated": now, "exitCode": null
        });
        let typed = TerminalRecord::from_value(record.clone())
            .map_err(|_| TerminalError::InvalidStoredObject(id.clone()))?;
        self.store.transaction(|transaction| {
            TerminalsRepository::put(transaction, &id, &typed, "", 0)?;
            TerminalsRepository::save_receipt(
                transaction,
                request_id,
                &signature,
                &record.to_string(),
            )
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
        match self.session.send_request(
            &format!("terminal-spawn:{request_id}:{id}"),
            "process/spawn",
            params,
        ) {
            Ok(_) => Ok(record),
            Err(SessionError::UnknownOutcome) => Ok(record),
            Err(error) => {
                let mut failed = typed;
                failed.set("status", Value::String("exited".into()));
                failed.set("error", json!(error.to_string()));
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, &id, &failed)?;
                    TerminalsRepository::update_receipt(
                        transaction,
                        request_id,
                        &failed.value().to_string(),
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
            .ok_or(TerminalError::UnknownTerminal)?;
        let mut record = row.record;
        match action {
            "rename" => {
                let title = data
                    .get("title")
                    .and_then(Value::as_str)
                    .ok_or(TerminalError::InvalidRequest)?;
                record.set("title", json!(terminal_title(title)?));
                record.set("updated", json!(now_seconds()));
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, id, &record)
                })?;
                Ok(record.value().clone())
            }
            "input" => self.send_input(id, data, record),
            "resize" => {
                let columns = size(data, "cols", 100)?;
                let rows = size(data, "rows", 28)?;
                self.require_running(&record)?;
                self.session.send_request(
                    &format!("terminal-resize:{id}:{}:{}", columns, rows),
                    "process/resizePty",
                    json!({"processHandle": id, "size": {"rows": rows, "cols": columns}}),
                )?;
                self.store
                    .transaction(|transaction| {
                        let current = TerminalsRepository::get(transaction, id)?
                            .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?;
                        let mut record = current.record;
                        record.set("updated", json!(now_seconds()));
                        TerminalsRepository::update_record(transaction, id, &record)?;
                        Ok(record.value().clone())
                    })
                    .map_err(TerminalError::Store)
            }
            "close" => {
                record.set("status", json!("closed"));
                record.set("updated", json!(now_seconds()));
                self.store.transaction(|transaction| {
                    TerminalsRepository::update_record(transaction, id, &record)
                })?;
                self.session.send_request(
                    &format!("terminal-close:{id}"),
                    "process/kill",
                    json!({"processHandle": id}),
                )?;
                Ok(record.value().clone())
            }
            _ => Err(TerminalError::InvalidRequest),
        }
    }

    pub fn apply_notification(
        &self,
        notification: TerminalNotification,
    ) -> Result<(), TerminalError> {
        let id = notification.terminal_id.clone();
        let lock = self.event_lock(&id);
        let _guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sequence =
            i64::try_from(notification.sequence).map_err(|_| TerminalError::InvalidRequest)?;
        if sequence <= 0 {
            return Err(TerminalError::InvalidRequest);
        }
        match notification.event {
            TerminalEvent::Output(bytes) => self.append_output(&id, notification.sequence, &bytes),
            TerminalEvent::Exit(code) => {
                self.store.transaction(|transaction| {
                    if TerminalsRepository::event_cursor(transaction, &id)?.unwrap_or(0) >= sequence
                    {
                        return Ok(());
                    }
                    let row = TerminalsRepository::get(transaction, &id)?
                        .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?;
                    let mut record = row.record.clone();
                    let pending =
                        TerminalsRepository::decoder_pending(transaction, &id)?.unwrap_or_default();
                    let (text, _) = decode_utf8(&pending, true);
                    let (output, offset) = TerminalsRepository::append_and_retain(
                        transaction,
                        &id,
                        &row,
                        &text,
                        OUTPUT_LIMIT,
                    )?;
                    if record.get("status").and_then(Value::as_str) != Some("closed") {
                        record.set("status", json!("exited"));
                    }
                    record.set("exitCode", code.map_or(Value::Null, |value| json!(value)));
                    record.set("updated", json!(now_seconds()));
                    record.remove("error");
                    TerminalsRepository::put(transaction, &id, &record, &output, offset)?;
                    TerminalsRepository::save_decoder_pending(transaction, &id, &[])?;
                    TerminalsRepository::save_event_cursor(transaction, &id, sequence)?;
                    Ok(())
                })?;
                Ok(())
            }
        }
    }

    fn append_output(&self, id: &str, sequence: u64, bytes: &[u8]) -> Result<(), TerminalError> {
        self.store.transaction(|transaction| {
            let row = TerminalsRepository::get(transaction, id)?
                .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?;
            let seq = i64::try_from(sequence).map_err(|_| {
                StoreError::SchemaMismatch("terminal event sequence exceeds SQLite range".into())
            })?;
            if TerminalsRepository::event_cursor(transaction, id)?.unwrap_or(0) >= seq {
                return Ok(());
            }
            let mut combined =
                TerminalsRepository::decoder_pending(transaction, id)?.unwrap_or_default();
            combined.extend_from_slice(bytes);
            let (text, pending) = decode_utf8(&combined, false);
            let (output, offset) =
                TerminalsRepository::append_and_retain(transaction, id, &row, &text, OUTPUT_LIMIT)?;
            TerminalsRepository::put(transaction, id, &row.record, &output, offset)?;
            TerminalsRepository::save_decoder_pending(transaction, id, &pending)?;
            TerminalsRepository::save_event_cursor(transaction, id, seq)?;
            Ok(())
        })?;
        Ok(())
    }

    fn send_input(
        &self,
        id: &str,
        data: &Value,
        record: TerminalRecord,
    ) -> Result<Value, TerminalError> {
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
        let signature = receipt_signature(&body);
        if let Some((prior_signature, result)) =
            TerminalsRepository::receipt(&self.store.read()?, request_id)?
        {
            if prior_signature != signature {
                return Err(TerminalError::ReceiptConflict);
            }
            return serde_json::from_str(&result)
                .map_err(|_| TerminalError::InvalidStoredObject(id.into()));
        }
        self.require_running(&record)?;
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
        while let Some(outcome) = self.session.receive_outcome()? {
            self.settle_outcome(outcome)?;
            count += 1;
        }
        Ok(count)
    }

    /// Reconcile notifications delivered after reconnecting to the same
    /// supervisor handle. Existing process handles are never spawned again.
    pub fn reconcile_after_restart(&self) -> Result<usize, TerminalError> {
        let rows = TerminalsRepository::list(&self.store.read()?)?;
        self.store.transaction(|transaction| {
            for mut record in rows {
                if record.get("status").and_then(Value::as_str) == Some("running") {
                    record.set("status", json!("exited"));
                    record.set(
                        "error",
                        json!("The terminal connection closed. The exit code is unknown."),
                    );
                    record.set("updated", json!(now_seconds()));
                    let id = record.id().as_str();
                    TerminalsRepository::update_record(transaction, id, &record)?;
                }
            }
            Ok(())
        })?;
        self.drain_notifications()
    }

    fn event_lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.event_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    fn settle_outcome(&self, outcome: TerminalOutcome) -> Result<(), TerminalError> {
        if let Some(rest) = outcome.operation_id.strip_prefix("terminal-spawn:") {
            if let Some((request_id, id)) = rest.rsplit_once(':') {
                let row = TerminalsRepository::get(&self.store.read()?, id)?
                    .ok_or(TerminalError::UnknownTerminal)?;
                let mut record = row.record;
                if let Err(error) = outcome.result {
                    record.set("status", json!("exited"));
                    record.set("error", json!(error.to_string()));
                    self.store.transaction(|transaction| {
                        TerminalsRepository::update_record(transaction, id, &record)?;
                        TerminalsRepository::update_receipt(
                            transaction,
                            request_id,
                            &record.value().to_string(),
                        )
                    })?;
                }
            }
        } else if let Some(request_id) = outcome.operation_id.strip_prefix("terminal-input:")
            && outcome.result.is_ok()
        {
            let result = json!({"ok":true,"delivery":"sent"}).to_string();
            self.store.transaction(|transaction| {
                TerminalsRepository::update_receipt(transaction, request_id, &result)
            })?;
        }
        Ok(())
    }

    pub fn close_session(&self) -> Result<(), TerminalError> {
        self.session.close().map_err(TerminalError::Session)
    }

    fn require_running(&self, record: &TerminalRecord) -> Result<(), TerminalError> {
        if record.get("status").and_then(Value::as_str) == Some("running") {
            Ok(())
        } else {
            Err(TerminalError::NotRunning)
        }
    }
}

fn size(data: &Value, key: &str, default: u64) -> Result<u64, TerminalError> {
    match data.get(key) {
        None => Ok(default),
        Some(value) => value.as_u64().ok_or(TerminalError::InvalidRequest),
    }
}

fn terminal_title(title: &str) -> Result<&str, TerminalError> {
    if title.trim().is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        Err(TerminalError::InvalidRequest)
    } else {
        Ok(title.trim())
    }
}

fn slice_chars(value: &str, start: usize, count: usize) -> String {
    value.chars().skip(start).take(count).collect()
}

fn decode_utf8(bytes: &[u8], final_chunk: bool) -> (String, Vec<u8>) {
    let mut output = String::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(value) => {
                output.push_str(value);
                return (output, Vec::new());
            }
            Err(error) => {
                let valid = error.valid_up_to();
                output.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
                rest = &rest[valid..];
                match error.error_len() {
                    Some(length) => {
                        output.push('\u{fffd}');
                        rest = &rest[length..];
                    }
                    None if !final_chunk => return (output, rest.to_vec()),
                    None => {
                        output.push('\u{fffd}');
                        return (output, Vec::new());
                    }
                }
            }
        }
    }
    (output, Vec::new())
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

fn new_id() -> Result<String, TerminalError> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| TerminalError::InvalidRequest)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn receipt_signature(value: &Value) -> String {
    fn quote(text: &str) -> String {
        let mut out = String::from("\"");
        for ch in text.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
                ch if ch as u32 > 0x7f => {
                    let code = ch as u32;
                    if code <= 0xffff {
                        out.push_str(&format!("\\u{code:04x}"));
                    } else {
                        let code = code - 0x10000;
                        out.push_str(&format!(
                            "\\u{:04x}\\u{:04x}",
                            0xd800 + (code >> 10),
                            0xdc00 + (code & 0x3ff)
                        ));
                    }
                }
                ch => out.push(ch),
            }
        }
        out.push('"');
        out
    }
    let json = value
        .as_object()
        .map(|object| {
            format!(
                "{{{}}}",
                object
                    .iter()
                    .map(|(key, value)| {
                        let value = value
                            .as_str()
                            .map(quote)
                            .unwrap_or_else(|| value.to_string());
                        format!("{}: {value}", quote(key))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .unwrap_or_else(|| value.to_string());
    Sha256::digest(json.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::{
        OUTPUT_LIMIT, SessionError, TerminalCreate, TerminalDriver, TerminalError, TerminalEvent,
        TerminalNotification, TerminalOutcome, TerminalSession, base64_encode, decode_utf8,
        history_output, now_seconds, output, receipt_signature, size, terminal_title,
    };
    use serde_json::Value;
    use studio_runtime_store::{RuntimeStore, TerminalRecord, TerminalsRepository};

    #[derive(Default)]
    struct FakeSession {
        requests: Mutex<Vec<(String, String, Value)>>,
        notifications: Mutex<VecDeque<TerminalNotification>>,
        outcomes: Mutex<VecDeque<TerminalOutcome>>,
        error: Mutex<Option<SessionError>>,
        resize_barrier: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
        closed: Mutex<bool>,
    }

    impl TerminalSession for FakeSession {
        fn send_request(
            &self,
            operation_id: &str,
            method: &str,
            params: Value,
        ) -> Result<Value, SessionError> {
            if method == "process/resizePty"
                && let Some((entered, release)) =
                    self.resize_barrier.lock().expect("resize barrier").clone()
            {
                entered.wait();
                release.wait();
            }
            self.requests.lock().expect("fake request lock").push((
                operation_id.to_owned(),
                method.to_owned(),
                params,
            ));
            match self.error.lock().expect("fake error lock").take() {
                Some(error) => Err(error),
                None => Ok(Value::Null),
            }
        }

        fn receive_notification(&self) -> Result<Option<TerminalNotification>, SessionError> {
            Ok(self
                .notifications
                .lock()
                .expect("fake notification lock")
                .pop_front())
        }

        fn receive_outcome(&self) -> Result<Option<TerminalOutcome>, SessionError> {
            Ok(self.outcomes.lock().expect("fake outcome lock").pop_front())
        }

        fn close(&self) -> Result<(), SessionError> {
            *self.closed.lock().expect("fake close lock") = true;
            Ok(())
        }
    }

    struct TestState {
        root: PathBuf,
        store: Option<Arc<RuntimeStore>>,
    }

    impl Drop for TestState {
        fn drop(&mut self) {
            self.store.take();
            drop(fs::remove_dir_all(&self.root));
        }
    }

    fn test_state() -> TestState {
        let root = std::env::temp_dir().join(format!(
            "terminal-driver-test-{}-{}",
            std::process::id(),
            now_seconds().to_bits()
        ));
        fs::create_dir_all(&root).expect("test state root");
        let snapshot: Value = serde_json::from_str(include_str!(
            "../../../apps/server/tests/fixtures/runtime-state-schema.json"
        ))
        .expect("schema fixture");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let path = root.join(name);
            let connection = rusqlite::Connection::open(path).expect("test database");
            connection
                .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
                .expect("database pragmas");
            let mut statements = snapshot[name]["sqlite_master"]
                .as_array()
                .expect("schema entries")
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry["type"].as_str()?,
                        entry["name"].as_str()?,
                        entry["sql"].as_str()?,
                    ))
                })
                .collect::<Vec<_>>();
            statements.sort_by_key(|(kind, _, _)| match *kind {
                "table" => 0,
                "index" => 1,
                "view" => 2,
                _ => 3,
            });
            for (_, name, sql) in statements {
                let exists: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                        [name],
                        |row| row.get(0),
                    )
                    .expect("schema lookup");
                if !exists {
                    connection.execute_batch(sql).expect("schema statement");
                }
            }
        }
        let server_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let output = Command::new("python3")
            .arg(server_source.join("codex_python.py"))
            .arg("--exec")
            .arg("-c")
            .arg("import pathlib, sys; sys.path.insert(0, sys.argv[2]); from codex_canvas import Canvas; Canvas(pathlib.Path(sys.argv[1]))")
            .arg(&root)
            .arg(&server_source)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .expect("Canvas schema bootstrap starts");
        assert!(
            output.status.success(),
            "Canvas schema bootstrap succeeds: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let store = Arc::new(RuntimeStore::open(Some(&root)).expect("test runtime store"));
        TestState {
            root,
            store: Some(store),
        }
    }

    fn test_driver(state: &TestState, session: FakeSession) -> TerminalDriver<FakeSession> {
        TerminalDriver::new(state.store.as_ref().expect("store").clone(), session)
    }

    fn create(driver: &TerminalDriver<FakeSession>, cwd: &Path) -> Value {
        driver
            .create(TerminalCreate {
                request_id: "create:request",
                agent: "agent",
                cwd,
                columns: 100,
                rows: 28,
                shell: "/bin/sh",
                environment: serde_json::json!({}),
            })
            .expect("create")
    }

    #[test]
    fn utf8_decoder_retains_an_incomplete_character_between_events() {
        let (first, pending) = decode_utf8(&[0xf0, 0x9f], false);
        assert!(first.is_empty());
        assert_eq!(pending, [0xf0, 0x9f]);
        let mut combined = pending;
        combined.extend_from_slice(&[0x99, 0x82]);
        assert_eq!(decode_utf8(&combined, false), ("🙂".to_owned(), Vec::new()));
        assert_eq!(
            decode_utf8(&[0xf0, 0x9f], true),
            ("�".to_owned(), Vec::new())
        );
        assert_eq!(
            decode_utf8(&[0xff; 100_000], true).0.chars().count(),
            100_000
        );
    }

    #[test]
    fn input_uses_the_appserver_base64_wire_representation() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode("🙂".as_bytes()), "8J+Zgg==");
    }

    #[test]
    fn receipts_match_python_sorted_json_hashes() {
        assert_eq!(
            receipt_signature(
                &serde_json::json!({"action":"input","id":"term","text":"Ж🙂","request_id":"req"})
            ),
            "0424690f3f6c2f444e348cf716bbb16785ccc47fd4e736725fa324ef0c667d6c"
        );
    }

    #[test]
    fn explicit_invalid_dimensions_and_untrimmed_long_titles_are_refused() {
        assert!(size(&serde_json::json!({"cols":true}), "cols", 100).is_err());
        assert!(size(&serde_json::json!({"rows":-1}), "rows", 28).is_err());
        assert_eq!(
            size(&serde_json::json!({}), "cols", 100).expect("default"),
            100
        );
        assert!(terminal_title(&format!("{}x", " ".repeat(120))).is_err());
    }

    #[test]
    fn driver_persists_deduplicates_and_flushes_split_utf8_on_exit() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let record = create(&driver, &state.root);
        let id = record["id"].as_str().expect("terminal id").to_owned();
        let notification = TerminalNotification {
            terminal_id: id.clone(),
            sequence: 1,
            event: TerminalEvent::Output(vec![0xf0, 0x9f]),
        };
        driver
            .apply_notification(notification.clone())
            .expect("apply event");
        driver
            .apply_notification(notification)
            .expect("deduplicate event");
        let mut stored = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("terminal row")
        .expect("terminal exists");
        stored.record.set("error", serde_json::json!("old error"));
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                TerminalsRepository::put(tx, &id, &stored.record, &stored.output, stored.offset)
            })
            .expect("seed error");
        driver
            .apply_notification(TerminalNotification {
                terminal_id: id.clone(),
                sequence: 2,
                event: TerminalEvent::Exit(Some(7)),
            })
            .expect("exit");
        let visible = output(state.store.as_ref().expect("store"), &id, 0).expect("output");
        assert_eq!(visible["text"], "�");
        assert_eq!(visible["offset"], 1);
        assert_eq!(visible["status"], "exited");
        assert!(visible["error"].is_null());
    }

    #[test]
    fn create_replays_a_python_signed_receipt_without_spawning_again() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = "python-terminal";
        let result = serde_json::json!({"id":id,"agent":"agent","title":"Terminal","cwd":state.root,
            "status":"running","created":1.0,"updated":1.0,"exitCode":null});
        let typed = TerminalRecord::from_value(result.clone()).expect("terminal record");
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                TerminalsRepository::put(tx, id, &typed, "", 0)?;
                TerminalsRepository::save_receipt(
                    tx,
                    "python-create",
                    "39c969a6c3bf9f631ba6edf5dc9c2e3e433a9a7ffe808a5bbf0b89a36287b378",
                    &result.to_string(),
                )
            })
            .expect("Python receipt");
        let replay = driver
            .create(TerminalCreate {
                request_id: "python-create",
                agent: "agent",
                cwd: &state.root,
                columns: 100,
                rows: 28,
                shell: "/bin/sh",
                environment: serde_json::json!({}),
            })
            .expect("replay");
        assert_eq!(replay, result);
        assert!(driver.session.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn driver_truncates_retained_output_but_keeps_archived_cursor_history() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("terminal id")
            .to_owned();
        driver
            .apply_notification(TerminalNotification {
                terminal_id: id.clone(),
                sequence: 1,
                event: TerminalEvent::Output(vec![b'x'; OUTPUT_LIMIT + 4]),
            })
            .expect("large output");
        let current = output(state.store.as_ref().expect("store"), &id, 0).expect("current output");
        assert_eq!(
            current["text"].as_str().expect("text").chars().count(),
            OUTPUT_LIMIT
        );
        assert_eq!(current["offset"], (OUTPUT_LIMIT + 4) as i64);
        assert_eq!(current["truncated"], true);
        let history =
            history_output(state.store.as_ref().expect("store"), &id, 0, 7).expect("history");
        assert_eq!(history["text"], "xxxxxxx");
        assert_eq!(history["offset"], 7);
        assert_eq!(history["hasMore"], true);
    }

    #[test]
    fn driver_seeds_python_legacy_output_and_refuses_bad_history_without_writes() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = "legacy-terminal";
        let now = now_seconds();
        let record = TerminalRecord::from_value(serde_json::json!({"id":id,"agent":"agent","title":"old","cwd":"/tmp","status":"exited","created":now,"updated":now,"exitCode":0}))
            .expect("terminal record");
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| TerminalsRepository::put(tx, id, &record, "legacy", 9))
            .expect("legacy row");
        driver
            .apply_notification(TerminalNotification {
                terminal_id: id.into(),
                sequence: 1,
                event: TerminalEvent::Output(b" delta".to_vec()),
            })
            .expect("append delta");
        let history =
            history_output(state.store.as_ref().expect("store"), id, 9, 32).expect("history");
        assert_eq!(history["text"], "legacy delta");

        let bad = "invalid-record";
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                tx.execute(
                "INSERT INTO user_terminals(id,record,output,offset) VALUES(?1,'{}','retained',0)",
                [bad],
            )?;
                Ok(())
            })
            .expect("seed invalid row");
        assert!(history_output(state.store.as_ref().expect("store"), bad, 0, 32).is_err());
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert_eq!(
            TerminalsRepository::archive_start(&connection, bad).expect("archive query"),
            None
        );
    }

    #[test]
    fn late_input_outcome_settles_receipt_and_replay_survives_exit() {
        let state = test_state();
        let session = FakeSession::default();
        let driver = test_driver(&state, session);
        let record = create(&driver, &state.root);
        let id = record["id"].as_str().expect("terminal id");
        *driver.session.error.lock().expect("fake error lock") = Some(SessionError::UnknownOutcome);
        let request = serde_json::json!({"id":id,"text":"hi","request_id":"input-request"});
        assert_eq!(
            driver.action("input", &request).expect("uncertain input")["delivery"],
            "uncertain"
        );
        driver
            .apply_notification(TerminalNotification {
                terminal_id: id.into(),
                sequence: 1,
                event: TerminalEvent::Exit(None),
            })
            .expect("exit");
        driver
            .session
            .outcomes
            .lock()
            .expect("fake outcome lock")
            .push_back(TerminalOutcome {
                operation_id: "terminal-input:input-request".into(),
                result: Ok(Value::Null),
            });
        driver.drain_notifications().expect("settle outcome");
        assert_eq!(
            driver.action("input", &request).expect("receipt replay")["delivery"],
            "sent"
        );
    }

    #[test]
    fn unknown_terminal_is_value_free_and_restart_marks_running_records_exited() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        assert!(matches!(
            driver.action("close", &serde_json::json!({"id":"arbitrary-secret"})),
            Err(TerminalError::UnknownTerminal)
        ));
        let record = create(&driver, &state.root);
        let id = record["id"].as_str().expect("terminal id");
        driver
            .reconcile_after_restart()
            .expect("restart reconciliation");
        let persisted =
            output(state.store.as_ref().expect("store"), id, 0).expect("persisted result");
        assert_eq!(persisted["status"], "exited");
        assert_eq!(
            persisted["error"],
            "The terminal connection closed. The exit code is unknown."
        );
    }

    #[test]
    fn resize_preserves_concurrent_record_changes_after_session_io() {
        let state = test_state();
        let session = FakeSession::default();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *session.resize_barrier.lock().expect("resize barrier") =
            Some((entered.clone(), release.clone()));
        let driver = Arc::new(test_driver(&state, session));
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("terminal id")
            .to_owned();
        let worker = {
            let driver = driver.clone();
            let id = id.clone();
            thread::spawn(move || driver.action("resize", &serde_json::json!({"id":id,"cols":90})))
        };
        entered.wait();
        let store = state.store.as_ref().expect("store");
        let row = TerminalsRepository::get(&store.read().expect("read"), &id)
            .expect("row")
            .expect("terminal");
        let mut record = row.record;
        record.set("status", serde_json::json!("exited"));
        record.set("title", serde_json::json!("concurrent rename"));
        store
            .transaction(|transaction| {
                TerminalsRepository::update_record(transaction, &id, &record)
            })
            .expect("concurrent update");
        release.wait();
        let result = worker.join().expect("resize worker").expect("resize");
        assert_eq!(result["status"], "exited");
        assert_eq!(result["title"], "concurrent rename");
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
