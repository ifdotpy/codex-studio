//! Durable terminal state and the narrow AppServer session boundary.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event_write::request_signature as receipt_signature;
use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, StoreError, TerminalRecord, TerminalsRepository,
};
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
    fn cleanup_orphan(&self, _identity: &Value) -> Result<bool, SessionError> {
        Ok(false)
    }
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
    CleanupFailed(String),
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
    event_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    clock: fn() -> f64,
    id_factory: fn() -> Result<String, TerminalError>,
}

pub struct TerminalCreate<'a> {
    pub request_id: &'a str,
    pub agent: &'a str,
    pub agent_cwd: &'a str,
    pub cwd: &'a Path,
    pub columns: u64,
    pub rows: u64,
    pub shell: &'a str,
    pub environment: Value,
    pub submitted_request: &'a Value,
}

pub fn listing(store: &RuntimeStore) -> Result<Value, TerminalError> {
    let connection = store.read()?;
    let mut items = Vec::new();
    for record in TerminalsRepository::list(&connection)? {
        if record.get("status").and_then(Value::as_str) != Some("closed") {
            let mut value = record.value().clone();
            hide_process_identity(&mut value);
            items.push(value);
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

fn hide_process_identity(record: &mut Value) {
    if let Some(fields) = record.as_object_mut() {
        fields.remove("_processIdentity");
    }
}

impl<S: TerminalSession> TerminalDriver<S> {
    pub fn new(store: std::sync::Arc<RuntimeStore>, session: S) -> Self {
        Self::with_sources(store, session, now_seconds, new_id)
    }

    fn with_sources(
        store: Arc<RuntimeStore>,
        session: S,
        clock: fn() -> f64,
        id_factory: fn() -> Result<String, TerminalError>,
    ) -> Self {
        Self {
            store,
            session,
            event_locks: Mutex::new(HashMap::new()),
            clock,
            id_factory,
        }
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    pub fn create(&self, request: TerminalCreate<'_>) -> Result<Value, TerminalError> {
        let TerminalCreate {
            request_id,
            agent,
            agent_cwd,
            cwd,
            columns,
            rows,
            shell,
            environment,
            submitted_request,
        } = request;
        if request_id.is_empty()
            || request_id.chars().count() > 200
            || !(2..=1000).contains(&columns)
            || !(1..=500).contains(&rows)
            || !cwd.is_dir()
        {
            return Err(TerminalError::InvalidRequest);
        }
        let signature = receipt_signature(submitted_request);
        if let Some((prior_signature, result)) =
            TerminalsRepository::receipt(&self.store.read()?, request_id)?
        {
            if prior_signature != signature {
                return Err(TerminalError::ReceiptConflict);
            }
            return serde_json::from_str(&result)
                .map_err(|_| TerminalError::InvalidStoredObject(request_id.into()));
        }
        let now = self.now();
        let id = (self.id_factory)()?;
        let cwd = cwd
            .canonicalize()
            .map_err(|_| TerminalError::InvalidRequest)?;
        let record = json!({
            "id": id, "agent": agent, "title": "Terminal", "cwd": cwd,
            "status": "running", "created": now, "updated": now, "exitCode": null
        });
        let typed = TerminalRecord::from_value(record.clone())
            .map_err(|_| TerminalError::InvalidStoredObject(id.clone()))?;
        let inserted = self.store.transaction(|transaction| {
            let Some(current_agent) = AgentRepository::get(transaction, &AgentId::from(agent))?
            else {
                return Ok(false);
            };
            if current_agent
                .extra
                .get("deletedAt")
                .is_some_and(json_truthy)
                || current_agent.extra.get("cwd").and_then(Value::as_str) != Some(agent_cwd)
            {
                return Ok(false);
            }
            TerminalsRepository::put(transaction, &id, &typed, "", 0)?;
            TerminalsRepository::save_receipt(
                transaction,
                request_id,
                &signature,
                &record.to_string(),
            )?;
            Ok(true)
        })?;
        if !inserted {
            return Err(TerminalError::InvalidRequest);
        }
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
            Err(SessionError::Rejected) => {
                self.store.transaction(|transaction| {
                    TerminalsRepository::delete(transaction, &id)?;
                    TerminalsRepository::delete_receipt(transaction, request_id)
                })?;
                Err(TerminalError::Session(SessionError::Rejected))
            }
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
        let record = row.record;
        match action {
            "rename" => {
                let title = data
                    .get("title")
                    .and_then(Value::as_str)
                    .ok_or(TerminalError::InvalidRequest)?;
                let title = terminal_title(title)?;
                let updated = self.store.transaction(|transaction| {
                    let mut current = TerminalsRepository::get(transaction, id)?
                        .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?
                        .record;
                    current.set("title", json!(title));
                    current.set("updated", json!(self.now()));
                    TerminalsRepository::update_record(transaction, id, &current)?;
                    Ok(current)
                })?;
                let mut value = updated.value().clone();
                hide_process_identity(&mut value);
                Ok(value)
            }
            "input" => self.send_input(id, data),
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
                        let record = current.record;
                        TerminalsRepository::update_record(transaction, id, &record)?;
                        let mut value = record.value().clone();
                        hide_process_identity(&mut value);
                        Ok(value)
                    })
                    .map_err(TerminalError::Store)
            }
            "close" => {
                let record = self.store.transaction(|transaction| {
                    let mut current = TerminalsRepository::get(transaction, id)?
                        .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?
                        .record;
                    current.set("status", json!("closed"));
                    current.set("updated", json!(self.now()));
                    TerminalsRepository::update_record(transaction, id, &current)?;
                    Ok(current)
                })?;
                if let Err(error) = self.session.send_request(
                    &format!("terminal-close:{id}"),
                    "process/kill",
                    json!({"processHandle": id}),
                ) {
                    self.store.transaction(|transaction| {
                        let mut current = TerminalsRepository::get(transaction, id)?
                            .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?
                            .record;
                        current.set(
                            "error",
                            json!("Terminal teardown failed. The process identity is retained for recovery."),
                        );
                        TerminalsRepository::update_record(transaction, id, &current)
                    })?;
                    return Err(TerminalError::Session(error));
                }
                let mut value = record.value().clone();
                hide_process_identity(&mut value);
                Ok(value)
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
                    record.set("updated", json!(self.now()));
                    record.remove("error");
                    record.remove("_processIdentity");
                    TerminalsRepository::put(transaction, &id, &record, &output, offset)?;
                    TerminalsRepository::save_decoder_pending(transaction, &id, &[])?;
                    TerminalsRepository::save_event_cursor(transaction, &id, sequence)?;
                    Ok(())
                })?;
                Ok(())
            }
            TerminalEvent::CleanupFailed(message) => {
                self.store.transaction(|transaction| {
                    if TerminalsRepository::event_cursor(transaction, &id)?.unwrap_or(0) >= sequence
                    {
                        return Ok(());
                    }
                    let mut record = TerminalsRepository::get(transaction, &id)?
                        .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?
                        .record;
                    record.set("error", json!(message));
                    TerminalsRepository::update_record(transaction, &id, &record)?;
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

    fn send_input(&self, id: &str, data: &Value) -> Result<Value, TerminalError> {
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
        let uncertain = json!({"ok":false,"delivery":"uncertain","error":"Input delivery is uncertain. Check the terminal before sending it again."});
        let reservation = self.store.transaction(|transaction| {
            let current = TerminalsRepository::get(transaction, id)?
                .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?;
            if let Some((prior_signature, prior)) =
                TerminalsRepository::receipt(transaction, request_id)?
            {
                return Ok(if prior_signature == signature {
                    InputReservation::Replay(prior)
                } else {
                    InputReservation::Conflict
                });
            }
            if current.record.get("status").and_then(Value::as_str) != Some("running") {
                return Ok(InputReservation::NotRunning);
            }
            TerminalsRepository::save_receipt(
                transaction,
                request_id,
                &signature,
                &uncertain.to_string(),
            )?;
            Ok(InputReservation::Reserved)
        })?;
        match reservation {
            InputReservation::NotRunning => return Err(TerminalError::NotRunning),
            InputReservation::Conflict => return Err(TerminalError::ReceiptConflict),
            InputReservation::Replay(result) => {
                return serde_json::from_str(&result)
                    .map_err(|_| TerminalError::InvalidStoredObject(id.into()));
            }
            InputReservation::Reserved => {}
        }
        let result = json!({"ok":true,"delivery":"sent"});
        match self.session.send_request(
            &format!("terminal-input:{request_id}"),
            "process/writeStdin",
            json!({
                "processHandle":id,
                "deltaBase64":base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
            }),
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
        for record in &rows {
            if let Some(identity) = record.get("_processIdentity") {
                self.session.cleanup_orphan(identity)?;
            }
        }
        self.store.transaction(|transaction| {
            for mut record in rows {
                let was_running = record.get("status").and_then(Value::as_str) == Some("running");
                let had_identity = record.get("_processIdentity").is_some();
                if had_identity && record.get("_processIdentity").is_some_and(Value::is_null) {
                    record.set(
                        "error",
                        json!("Terminal cleanup is unresolved because this platform cannot verify the saved process identity."),
                    );
                    TerminalsRepository::update_record(transaction, record.id().as_str(), &record)?;
                    continue;
                }
                if was_running {
                    record.set("status", json!("exited"));
                    record.set(
                        "error",
                        json!("The server restarted. This shell cannot be resumed."),
                    );
                    record.set("updated", json!(self.now()));
                }
                if was_running || had_identity {
                    record.remove("_processIdentity");
                    let id = record.id().as_str();
                    TerminalsRepository::update_record(transaction, id, &record)?;
                }
            }
            Ok(())
        })?;
        self.drain_notifications()
    }

    fn event_lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .event_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() != 0);
        if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(id.to_owned(), Arc::downgrade(&lock));
        lock
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

    pub fn persist_process_identity(
        &self,
        id: &str,
        identity: Option<&studio_runtime_process::PtyIdentity>,
    ) -> Result<(), TerminalError> {
        let identity = identity
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| TerminalError::InvalidStoredObject(id.to_owned()))?;
        self.store.transaction(|transaction| {
            let mut record = TerminalsRepository::get(transaction, id)?
                .ok_or_else(|| StoreError::SchemaMismatch("unknown terminal".into()))?
                .record;
            if record.get("status").and_then(Value::as_str) != Some("running") {
                return Err(StoreError::SchemaMismatch(
                    "terminal stopped before process identity persistence".into(),
                ));
            }
            record.set("_processIdentity", identity.unwrap_or(Value::Null));
            TerminalsRepository::update_record(transaction, id, &record)
        })?;
        Ok(())
    }

    fn require_running(&self, record: &TerminalRecord) -> Result<(), TerminalError> {
        if record.get("status").and_then(Value::as_str) == Some("running") {
            Ok(())
        } else {
            Err(TerminalError::NotRunning)
        }
    }
}

enum InputReservation {
    Reserved,
    Replay(String),
    Conflict,
    NotRunning,
}

fn size(data: &Value, key: &str, default: u64) -> Result<u64, TerminalError> {
    let value = match data.get(key) {
        None => default,
        Some(value) => value.as_u64().ok_or(TerminalError::InvalidRequest)?,
    };
    let valid = match key {
        "cols" => (2..=1000).contains(&value),
        "rows" => (1..=500).contains(&value),
        _ => false,
    };
    valid.then_some(value).ok_or(TerminalError::InvalidRequest)
}

fn terminal_title(title: &str) -> Result<&str, TerminalError> {
    if title.trim().is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        Err(TerminalError::InvalidRequest)
    } else {
        Ok(title.trim())
    }
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
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

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn new_id() -> Result<String, TerminalError> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| TerminalError::InvalidRequest)?;
    Ok(uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::{
        INPUT_LIMIT, OUTPUT_LIMIT, SessionError, TerminalCreate, TerminalDriver, TerminalError,
        TerminalEvent, TerminalNotification, TerminalOutcome, TerminalSession, decode_utf8,
        history_output, new_id, now_seconds, output, receipt_signature, size, terminal_title,
    };
    use serde_json::Value;
    use studio_runtime_store::{
        AgentId, AgentRecord, AgentRepository, AgentStatus, RuntimeStore, TerminalRecord,
        TerminalsRepository,
    };

    use crate::agents_create;

    #[derive(Default)]
    struct FakeSession {
        requests: Mutex<Vec<(String, String, Value)>>,
        notifications: Mutex<VecDeque<TerminalNotification>>,
        outcomes: Mutex<VecDeque<TerminalOutcome>>,
        error: Mutex<Option<SessionError>>,
        resize_barrier: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
        closed: Mutex<bool>,
        orphan_identities: Mutex<Vec<Value>>,
        orphan_failure: Mutex<Option<usize>>,
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

        fn cleanup_orphan(&self, identity: &Value) -> Result<bool, SessionError> {
            let mut identities = self.orphan_identities.lock().expect("orphan identity lock");
            identities.push(identity.clone());
            let should_fail =
                *self.orphan_failure.lock().expect("orphan failure lock") == Some(identities.len());
            drop(identities);
            if should_fail {
                return Err(SessionError::Disconnected);
            }
            Ok(true)
        }
    }

    struct TestState {
        root: PathBuf,
        _directory: tempfile::TempDir,
        store: Option<Arc<RuntimeStore>>,
    }

    impl Drop for TestState {
        fn drop(&mut self) {
            self.store.take();
        }
    }

    fn test_state() -> TestState {
        let tmpfs = Path::new("/dev/shm/cs-rust-terminals-tests");
        std::fs::create_dir_all(tmpfs).expect("tmpfs test state directory");
        let directory = tempfile::Builder::new()
            .prefix("terminal-driver-test-")
            .tempdir_in(tmpfs)
            .expect("test state root");
        let root = directory.path().to_path_buf();
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
        let python = agents_create::prepared_python(&server_source);
        let output = Command::new(python)
            .arg(server_source.join("codex_python.py"))
            .arg("--exec")
            .arg("-c")
            .arg("import pathlib, sys; sys.path.insert(0, sys.argv[2]); from codex_canvas import Canvas; from codex_terminals import TerminalManager; root = pathlib.Path(sys.argv[1]); Canvas(root); TerminalManager(root)")
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
        let mut extra = BTreeMap::new();
        extra.insert("cwd".to_owned(), serde_json::json!(root));
        let agent = AgentRecord {
            id: AgentId::from("agent"),
            root_id: AgentId::from("agent"),
            epoch: 0,
            status: AgentStatus::Running,
            extra,
        };
        store
            .transaction(|transaction| AgentRepository::put(transaction, &agent))
            .expect("test agent");
        TestState {
            root,
            _directory: directory,
            store: Some(store),
        }
    }

    fn test_driver(state: &TestState, session: FakeSession) -> TerminalDriver<FakeSession> {
        TerminalDriver::new(state.store.as_ref().expect("store").clone(), session)
    }

    fn action_clock() -> f64 {
        1_700_000_000.25
    }

    fn action_id() -> Result<String, TerminalError> {
        Ok("f48d7c00-0000-4000-8000-000000000001".into())
    }

    fn create(driver: &TerminalDriver<FakeSession>, cwd: &Path) -> Value {
        let submitted =
            serde_json::json!({"id":"create:request","agent":"agent","cols":100,"rows":28});
        driver
            .create(TerminalCreate {
                request_id: "create:request",
                agent: "agent",
                agent_cwd: cwd.to_str().expect("UTF-8 test cwd"),
                cwd,
                columns: 100,
                rows: 28,
                shell: "/bin/sh",
                environment: serde_json::json!({}),
                submitted_request: &submitted,
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
        use base64::Engine as _;

        let encode = base64::engine::general_purpose::STANDARD;
        assert_eq!(encode.encode(b"hello"), "aGVsbG8=");
        assert_eq!(encode.encode("🙂".as_bytes()), "8J+Zgg==");
    }

    #[test]
    fn generated_process_handles_are_version_four_uuids() {
        let uuid = uuid::Uuid::parse_str(&new_id().expect("UUID random bytes are available"))
            .expect("generated UUID parses");
        assert_eq!(uuid.get_version(), Some(uuid::Version::Random));
        assert_eq!(uuid.get_variant(), uuid::Variant::RFC4122);
        let id = uuid.to_string();
        assert_eq!(id.len(), 36);
        assert!(id.bytes().all(|byte| !byte.is_ascii_uppercase()));
        assert_eq!(id, format!("{uuid}"));
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
        assert_eq!(
            size(&serde_json::json!({"cols":2}), "cols", 100).expect("minimum columns"),
            2
        );
        assert_eq!(
            size(&serde_json::json!({"cols":1000}), "cols", 100).expect("maximum columns"),
            1000
        );
        assert!(size(&serde_json::json!({"cols":1001}), "cols", 100).is_err());
        assert!(size(&serde_json::json!({"rows":501}), "rows", 28).is_err());
        assert!(terminal_title(&format!("{}x", " ".repeat(120))).is_err());
    }

    #[test]
    fn input_accepts_exactly_64_kib_and_refuses_larger_text_without_a_receipt() {
        use base64::Engine as _;

        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let record = create(&driver, &state.root);
        let id = record["id"].as_str().expect("terminal id");
        let text = "x".repeat(INPUT_LIMIT);
        let request = serde_json::json!({"id":id,"text":text,"request_id":"input-limit-valid"});
        let result = driver.action("input", &request).expect("64 KiB input");
        assert_eq!(result, serde_json::json!({"ok":true,"delivery":"sent"}));
        let request = driver.session.requests.lock().expect("requests");
        let (_, method, params) = request.last().expect("input request");
        assert_eq!(method, "process/writeStdin");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(params["deltaBase64"].as_str().expect("base64 text"))
            .expect("valid base64");
        assert_eq!(decoded.len(), INPUT_LIMIT);
        drop(request);

        let before = TerminalsRepository::receipt(
            &state.store.as_ref().expect("store").read().expect("read"),
            "input-limit-too-large",
        )
        .expect("receipt read");
        let too_large = serde_json::json!({"id":id,"text":"x".repeat(INPUT_LIMIT + 1),"request_id":"input-limit-too-large"});
        assert!(matches!(
            driver.action("input", &too_large),
            Err(TerminalError::InvalidRequest)
        ));
        let after = TerminalsRepository::receipt(
            &state.store.as_ref().expect("store").read().expect("read"),
            "input-limit-too-large",
        )
        .expect("receipt reread");
        assert_eq!(before, after);
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
        let submitted =
            serde_json::json!({"id":"python-create","agent":"agent","cols":100,"rows":28});
        let signature = receipt_signature(&submitted);
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                TerminalsRepository::put(tx, id, &typed, "", 0)?;
                TerminalsRepository::save_receipt(
                    tx,
                    "python-create",
                    &signature,
                    &result.to_string(),
                )
            })
            .expect("Python receipt");
        let replay = driver
            .create(TerminalCreate {
                request_id: "python-create",
                agent: "agent",
                agent_cwd: state.root.to_str().expect("UTF-8 test cwd"),
                cwd: &state.root,
                columns: 100,
                rows: 28,
                shell: "/bin/sh",
                environment: serde_json::json!({}),
                submitted_request: &submitted,
            })
            .expect("replay");
        assert_eq!(replay, result);
        assert!(driver.session.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn create_receipt_refuses_a_retry_with_changed_submitted_content() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let _created = create(&driver, &state.root);
        let changed =
            serde_json::json!({"id":"create:request","agent":"agent","cols":100,"rows":29});
        let result = driver.create(TerminalCreate {
            request_id: "create:request",
            agent: "agent",
            agent_cwd: state.root.to_str().expect("UTF-8 test cwd"),
            cwd: &state.root,
            columns: 100,
            rows: 28,
            shell: "/bin/sh",
            environment: serde_json::json!({}),
            submitted_request: &changed,
        });
        assert!(matches!(result, Err(TerminalError::ReceiptConflict)));
        assert_eq!(driver.session.requests.lock().expect("requests").len(), 1);
    }

    #[test]
    fn restart_reconciles_only_after_cleaning_the_persisted_process_identity() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let created = create(&driver, &state.root);
        let id = created["id"].as_str().expect("terminal id");
        let store = state.store.as_ref().expect("store");
        store
            .transaction(|transaction| {
                let mut record = TerminalsRepository::get(transaction, id)?
                    .expect("terminal exists")
                    .record;
                record.set(
                    "_processIdentity",
                    serde_json::json!({"pid":2345,"session":2345,"start_time_ticks":98765}),
                );
                TerminalsRepository::update_record(transaction, id, &record)
            })
            .expect("persist process identity");

        driver
            .reconcile_after_restart()
            .expect("clean and reconcile restart");
        assert_eq!(
            *driver
                .session
                .orphan_identities
                .lock()
                .expect("orphan identity lock"),
            vec![serde_json::json!({"pid":2345,"session":2345,"start_time_ticks":98765})]
        );
        let row = TerminalsRepository::get(&store.read().expect("read"), id)
            .expect("terminal row")
            .expect("terminal exists");
        assert_eq!(
            row.record.get("status").and_then(Value::as_str),
            Some("exited")
        );
        assert!(row.record.get("_processIdentity").is_none());
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
    fn create_failpoint_rolls_back_terminal_and_receipt_before_spawn() {
        let state = test_state();
        let session = FakeSession::default();
        let driver = test_driver(&state, session);
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open create fail-point database");
        database
            .execute_batch(
                "CREATE TRIGGER fail_terminal_receipt BEFORE INSERT ON user_terminal_receipts
             BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;",
            )
            .expect("install create fail point");
        let request =
            serde_json::json!({"id":"create-failpoint","agent":"agent","cols":100,"rows":28});
        assert!(
            driver
                .create(TerminalCreate {
                    request_id: "create-failpoint",
                    agent: "agent",
                    agent_cwd: state.root.to_str().expect("cwd"),
                    cwd: &state.root,
                    columns: 100,
                    rows: 28,
                    shell: "/bin/sh",
                    environment: serde_json::json!({}),
                    submitted_request: &request,
                })
                .is_err()
        );
        assert!(
            driver.session.requests.lock().expect("requests").is_empty(),
            "reservation failure must happen before process/spawn"
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert_eq!(
            TerminalsRepository::list(&connection).expect("list"),
            Vec::new()
        );
        assert_eq!(
            TerminalsRepository::receipt(&connection, "create-failpoint").expect("receipt"),
            None
        );
    }

    #[test]
    fn rejected_create_removes_its_reserved_row_and_receipt() {
        let state = test_state();
        let session = FakeSession::default();
        *session.error.lock().expect("fake error") = Some(SessionError::Rejected);
        let driver = test_driver(&state, session);
        let request =
            serde_json::json!({"id":"create-rejected","agent":"agent","cols":100,"rows":28});
        assert!(
            driver
                .create(TerminalCreate {
                    request_id: "create-rejected",
                    agent: "agent",
                    agent_cwd: state.root.to_str().expect("cwd"),
                    cwd: &state.root,
                    columns: 100,
                    rows: 28,
                    shell: "/bin/sh",
                    environment: serde_json::json!({}),
                    submitted_request: &request,
                })
                .is_err()
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert!(
            TerminalsRepository::list(&connection)
                .expect("list")
                .is_empty()
        );
        assert_eq!(
            TerminalsRepository::receipt(&connection, "create-rejected").expect("receipt"),
            None
        );
    }

    #[test]
    fn rejected_create_compensation_failure_preserves_reservation_and_receipt() {
        let state = test_state();
        let session = FakeSession::default();
        *session.error.lock().expect("fake error") = Some(SessionError::Rejected);
        let driver = test_driver(&state, session);
        let request =
            serde_json::json!({"id":"create-delete-fail","agent":"agent","cols":100,"rows":28});
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open rejected create database");
        database
            .execute_batch(
                "CREATE TRIGGER fail_terminal_delete BEFORE DELETE ON user_terminals
             BEGIN SELECT RAISE(ABORT, 'injected compensation failure'); END;",
            )
            .expect("install rejected create fail point");
        assert!(
            driver
                .create(TerminalCreate {
                    request_id: "create-delete-fail",
                    agent: "agent",
                    agent_cwd: state.root.to_str().expect("cwd"),
                    cwd: &state.root,
                    columns: 100,
                    rows: 28,
                    shell: "/bin/sh",
                    environment: serde_json::json!({}),
                    submitted_request: &request,
                })
                .is_err()
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert_eq!(
            TerminalsRepository::list(&connection).expect("list").len(),
            1
        );
        assert!(
            TerminalsRepository::receipt(&connection, "create-delete-fail")
                .expect("receipt")
                .is_some()
        );
        assert_eq!(driver.session.requests.lock().expect("requests").len(), 1);
    }

    #[test]
    fn create_identity_and_error_record_failpoints_keep_durable_reservation() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let created = create(&driver, &state.root);
        let id = created["id"].as_str().expect("terminal id").to_owned();
        let identity = studio_runtime_process::PtyIdentity {
            pid: 2345,
            process_group: 2345,
            session: 2345,
            start_time_ticks: 98765,
            boot_id: "00000000-0000-0000-0000-000000000001".into(),
        };
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open identity persistence database");
        database
            .execute_batch(
                "CREATE TRIGGER fail_identity_persist BEFORE UPDATE ON user_terminals
             WHEN json_extract(NEW.record,'$._processIdentity') IS NOT NULL
             BEGIN SELECT RAISE(ABORT,'identity commit'); END;",
            )
            .expect("install identity persistence fail point");
        assert!(
            driver
                .persist_process_identity(&id, Some(&identity))
                .is_err()
        );
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("row")
        .expect("terminal");
        assert!(row.record.get("_processIdentity").is_none());
        database
            .execute_batch("DROP TRIGGER fail_identity_persist;")
            .expect("remove identity fail point");

        database
            .execute_batch(
                "CREATE TRIGGER fail_spawn_error_record BEFORE UPDATE ON user_terminals
             WHEN json_extract(NEW.record,'$.error') IS NOT NULL
             BEGIN SELECT RAISE(ABORT,'spawn error record'); END;",
            )
            .expect("install spawn error record fail point");
        let outcome = TerminalOutcome {
            operation_id: format!("terminal-spawn:create:request:{id}"),
            result: Err(SessionError::Rejected),
        };
        assert!(driver.settle_outcome(outcome).is_err());
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("row")
        .expect("terminal");
        assert_eq!(
            row.record.get("status").and_then(Value::as_str),
            Some("running")
        );
        assert!(row.record.get("error").is_none());
        assert!(
            TerminalsRepository::receipt(
                &state.store.as_ref().expect("store").read().expect("read"),
                "create:request",
            )
            .expect("receipt")
            .is_some()
        );
    }

    #[test]
    fn input_reservation_failpoint_does_not_write_to_the_process() {
        let state = test_state();
        let session = FakeSession::default();
        let driver = test_driver(&state, session);
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open input fail-point database");
        database
            .execute_batch(
                "CREATE TRIGGER fail_input_receipt BEFORE INSERT ON user_terminal_receipts
             BEGIN SELECT RAISE(ABORT, 'injected input receipt failure'); END;",
            )
            .expect("install input fail point");
        assert!(
            driver
                .action(
                    "input",
                    &serde_json::json!({"id":id,"text":"harmless","request_id":"input-failpoint"})
                )
                .is_err()
        );
        assert!(
            driver
                .session
                .requests
                .lock()
                .expect("requests")
                .iter()
                .all(|(_, method, _)| method != "process/writeStdin")
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert_eq!(
            TerminalsRepository::receipt(&connection, "input-failpoint").expect("receipt"),
            None
        );
    }

    #[test]
    fn input_unknown_and_settlement_failures_replay_without_resending() {
        let state = test_state();
        let session = FakeSession::default();
        let driver = test_driver(&state, session);
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("id")
            .to_owned();
        *driver.session.error.lock().expect("fake error") = Some(SessionError::UnknownOutcome);
        let uncertain = serde_json::json!({"ok":false,"delivery":"uncertain","error":"Input delivery is uncertain. Check the terminal before sending it again."});
        let unknown_request =
            serde_json::json!({"id":id,"text":"once","request_id":"input-unknown"});
        assert_eq!(
            driver
                .action("input", &unknown_request)
                .expect("unknown result"),
            uncertain
        );
        assert_eq!(
            driver
                .action("input", &unknown_request)
                .expect("unknown replay"),
            uncertain
        );

        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open settlement fail-point database");
        database
            .execute_batch(
                "CREATE TRIGGER fail_input_settlement BEFORE UPDATE ON user_terminal_receipts
             WHEN OLD.id='input-settlement'
             BEGIN SELECT RAISE(ABORT, 'injected settlement failure'); END;",
            )
            .expect("install receipt settlement fail point");
        let settlement_request =
            serde_json::json!({"id":id,"text":"once-too","request_id":"input-settlement"});
        assert!(driver.action("input", &settlement_request).is_err());
        database
            .execute_batch("DROP TRIGGER fail_input_settlement;")
            .expect("remove settlement fail point");
        let sent = driver
            .session
            .requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|(_, method, _)| method == "process/writeStdin")
            .count();
        assert_eq!(sent, 2, "one write for each distinct receipt");
        assert_eq!(
            driver
                .action("input", &settlement_request)
                .expect("settlement replay"),
            uncertain
        );
        assert_eq!(
            driver
                .session
                .requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|(_, method, _)| method == "process/writeStdin")
                .count(),
            sent
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        for request_id in ["input-unknown", "input-settlement"] {
            let receipt = TerminalsRepository::receipt(&connection, request_id)
                .expect("receipt")
                .expect("reserved");
            assert_eq!(receipt.1, uncertain.to_string());
        }

        let before_send = serde_json::json!({"action":"input","id":id,"text":"not-sent","request_id":"input-before-send"});
        let signature = receipt_signature(&before_send);
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|transaction| {
                TerminalsRepository::save_receipt(
                    transaction,
                    "input-before-send",
                    &signature,
                    &uncertain.to_string(),
                )
            })
            .expect("persist reservation before simulated process interruption");
        let sent_before = driver
            .session
            .requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|(_, method, _)| method == "process/writeStdin")
            .count();
        assert_eq!(
            driver
                .action(
                    "input",
                    &serde_json::json!({"id":id,"text":"not-sent","request_id":"input-before-send"})
                )
                .expect("reserved replay"),
            uncertain
        );
        assert_eq!(
            driver
                .session
                .requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|(_, method, _)| method == "process/writeStdin")
                .count(),
            sent_before,
            "a durable reservation from before delivery must not be resent after restart"
        );
    }

    #[test]
    fn close_and_restart_failpoints_preserve_current_identity_and_do_not_send_kill() {
        let state = test_state();
        let session = FakeSession::default();
        let driver = test_driver(&state, session);
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let saved_identity = serde_json::json!({"pid":7,"process_group":7,"session":7,"start_time_ticks":11,"boot_id":"boot"});
        let mut seeded = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("row")
        .expect("terminal exists")
        .record;
        seeded.set("_processIdentity", saved_identity.clone());
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| TerminalsRepository::update_record(tx, &id, &seeded))
            .expect("seed identity before close fail point");
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open close fail-point database");
        database
            .execute_batch(
                "CREATE TRIGGER fail_terminal_close BEFORE UPDATE ON user_terminals
             WHEN json_extract(NEW.record,'$.status')='closed'
             BEGIN SELECT RAISE(ABORT, 'injected close failure'); END;",
            )
            .expect("install close fail point");
        assert!(
            driver
                .action("close", &serde_json::json!({"id":id}))
                .is_err()
        );
        assert!(
            driver
                .session
                .requests
                .lock()
                .expect("requests")
                .iter()
                .all(|(_, method, _)| method != "process/kill")
        );
        let failed_close = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("row")
        .expect("exists")
        .record;
        assert_eq!(failed_close.get("_processIdentity"), Some(&saved_identity));
        database
            .execute_batch("DROP TRIGGER fail_terminal_close;")
            .expect("remove close fail point");
        database
            .execute_batch(
                "CREATE TRIGGER fail_restart_cleanup BEFORE UPDATE ON user_terminals
             WHEN json_extract(NEW.record,'$.status')='exited'
             BEGIN SELECT RAISE(ABORT, 'injected restart failure'); END;",
            )
            .expect("install restart fail point");
        assert!(driver.reconcile_after_restart().is_err());
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("row")
        .expect("exists")
        .record;
        assert_eq!(row.get("status").and_then(Value::as_str), Some("running"));
        assert!(row.get("_processIdentity").is_some());
        assert_eq!(
            driver
                .session
                .orphan_identities
                .lock()
                .expect("orphan calls")
                .len(),
            1
        );
    }

    #[test]
    fn close_commit_survives_a_lost_kill_response() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let recovery_identity = serde_json::json!({"pid":73,"process_group":73,"session":73,"start_time_ticks":51,"boot_id":"test-boot"});
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|transaction| {
                let mut record = TerminalsRepository::get(transaction, &id)?
                    .expect("terminal")
                    .record;
                record.set("_processIdentity", recovery_identity.clone());
                TerminalsRepository::update_record(transaction, &id, &record)
            })
            .expect("seed recovery identity");
        *driver.session.error.lock().expect("fake error") = Some(SessionError::UnknownOutcome);
        assert!(
            driver
                .action("close", &serde_json::json!({"id":id}))
                .is_err()
        );
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("row")
        .expect("terminal");
        assert_eq!(
            row.record.get("status").and_then(Value::as_str),
            Some("closed")
        );
        assert_eq!(row.record.get("_processIdentity"), Some(&recovery_identity));
        assert!(row.record.get("error").and_then(Value::as_str).is_some());
        let kills = driver
            .session
            .requests
            .lock()
            .expect("requests")
            .iter()
            .filter(|(_, method, _)| method == "process/kill")
            .count();
        assert_eq!(kills, 1, "close commit precedes one possibly-lost kill");
    }

    #[test]
    fn cleanup_failure_notification_keeps_identity_and_reports_unresolved_child() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("terminal ID")
            .to_owned();
        let identity = serde_json::json!({"pid":91,"process_group":91,"session":91,"start_time_ticks":42,"boot_id":"boot"});
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|transaction| {
                let mut record = TerminalsRepository::get(transaction, &id)?
                    .expect("terminal")
                    .record;
                record.set("_processIdentity", identity.clone());
                TerminalsRepository::update_record(transaction, &id, &record)
            })
            .expect("persist recovery identity");

        driver
            .apply_notification(TerminalNotification {
                terminal_id: id.clone(),
                sequence: 1,
                event: TerminalEvent::CleanupFailed("injected signal failure".into()),
            })
            .expect("record unresolved child");
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("row")
        .expect("terminal");
        assert_eq!(row.record.get("_processIdentity"), Some(&identity));
        assert_eq!(
            row.record.get("status").and_then(Value::as_str),
            Some("running")
        );
        assert_eq!(
            row.record.get("error").and_then(Value::as_str),
            Some("injected signal failure")
        );
    }

    #[test]
    fn exit_transaction_failpoints_rollback_record_archive_decoder_and_cursor() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("id")
            .to_owned();
        driver
            .apply_notification(TerminalNotification {
                terminal_id: id.clone(),
                sequence: 1,
                event: TerminalEvent::Output(b"prefix".to_vec()),
            })
            .expect("seed output");
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|transaction| {
                TerminalsRepository::save_decoder_pending(transaction, &id, b"tail")
            })
            .expect("seed incomplete UTF-8 decoder bytes");
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open exit transaction database");
        let baseline_archive: i64 = state
            .store
            .as_ref()
            .expect("store")
            .read()
            .expect("read")
            .query_row(
                "SELECT COUNT(*) FROM user_terminal_output WHERE terminal=?1",
                [&id],
                |record| record.get(0),
            )
            .expect("baseline archive count");
        for (trigger, statement) in [
            (
                "fail_exit_record",
                "CREATE TRIGGER fail_exit_record BEFORE UPDATE ON user_terminals BEGIN SELECT RAISE(ABORT,'record'); END;",
            ),
            (
                "fail_exit_archive",
                "CREATE TRIGGER fail_exit_archive BEFORE UPDATE ON user_terminal_output BEGIN SELECT RAISE(ABORT,'archive'); END;",
            ),
            (
                "fail_exit_decoder",
                "CREATE TRIGGER fail_exit_decoder BEFORE UPDATE ON user_terminal_decoder_state BEGIN SELECT RAISE(ABORT,'decoder'); END;",
            ),
            (
                "fail_exit_cursor",
                "CREATE TRIGGER fail_exit_cursor BEFORE UPDATE ON user_terminal_event_cursor BEGIN SELECT RAISE(ABORT,'cursor'); END;",
            ),
        ] {
            database
                .execute_batch(statement)
                .expect("install exit fail point");
            assert!(
                driver
                    .apply_notification(TerminalNotification {
                        terminal_id: id.clone(),
                        sequence: 2,
                        event: TerminalEvent::Exit(Some(0)),
                    })
                    .is_err(),
                "injected {trigger} failure must abort exit transaction"
            );
            let connection = state.store.as_ref().expect("store").read().expect("read");
            let row = TerminalsRepository::get(&connection, &id)
                .expect("row")
                .expect("terminal");
            assert_eq!(
                row.record.get("status").and_then(Value::as_str),
                Some("running")
            );
            assert_eq!(row.output, "prefix");
            assert_eq!(
                TerminalsRepository::event_cursor(&connection, &id).expect("cursor"),
                Some(1)
            );
            assert_eq!(
                TerminalsRepository::decoder_pending(&connection, &id)
                    .expect("decoder")
                    .as_deref(),
                Some(&b"tail"[..])
            );
            let archived: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM user_terminal_output WHERE terminal=?1",
                    [&id],
                    |record| record.get(0),
                )
                .expect("archive count");
            assert_eq!(
                archived, baseline_archive,
                "archive changed after {trigger} failure"
            );
            drop(connection);
            database
                .execute_batch(&format!("DROP TRIGGER {trigger};"))
                .expect("remove exit fail point");
        }
    }

    #[test]
    fn restart_cleanup_store_failure_rolls_back_all_rows_after_multiple_cleanups() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let first = create(&driver, &state.root)["id"]
            .as_str()
            .expect("first ID")
            .to_owned();
        let second_request =
            serde_json::json!({"id":"create:second","agent":"agent","cols":100,"rows":28});
        let second = driver
            .create(TerminalCreate {
                request_id: "create:second",
                agent: "agent",
                agent_cwd: state.root.to_str().expect("cwd"),
                cwd: &state.root,
                columns: 100,
                rows: 28,
                shell: "/bin/sh",
                environment: serde_json::json!({}),
                submitted_request: &second_request,
            })
            .expect("second create")["id"]
            .as_str()
            .expect("second ID")
            .to_owned();
        let identity = serde_json::json!({"pid":2345,"process_group":2345,"session":2345,"start_time_ticks":98765,"boot_id":"boot"});
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|transaction| {
                for id in [&first, &second] {
                    let mut row = TerminalsRepository::get(transaction, id)?
                        .expect("terminal")
                        .record;
                    row.set("_processIdentity", identity.clone());
                    TerminalsRepository::update_record(transaction, id, &row)?;
                }
                Ok(())
            })
            .expect("seed two persisted identities");
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open restart cleanup database");
        database
            .execute_batch(&format!(
                "CREATE TRIGGER fail_second_restart BEFORE UPDATE ON user_terminals
             WHEN OLD.id='{}' AND json_extract(NEW.record,'$.status')='exited'
             BEGIN SELECT RAISE(ABORT,'restart commit'); END;",
                second
            ))
            .expect("install restart commit fail point");
        assert!(driver.reconcile_after_restart().is_err());
        assert_eq!(
            driver
                .session
                .orphan_identities
                .lock()
                .expect("cleanup calls")
                .len(),
            2
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        for id in [&first, &second] {
            let row = TerminalsRepository::get(&connection, id)
                .expect("row")
                .expect("terminal");
            assert_eq!(
                row.record.get("status").and_then(Value::as_str),
                Some("running")
            );
            assert_eq!(row.record.get("_processIdentity"), Some(&identity));
        }
    }

    #[test]
    fn restart_without_platform_identity_keeps_a_named_unresolved_row() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("terminal ID")
            .to_owned();
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|transaction| {
                let mut record = TerminalsRepository::get(transaction, &id)?
                    .expect("terminal")
                    .record;
                record.set("_processIdentity", Value::Null);
                TerminalsRepository::update_record(transaction, &id, &record)
            })
            .expect("seed unavailable platform identity");

        driver
            .reconcile_after_restart()
            .expect("unavailable identity is retained without signaling");
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("terminal row")
        .expect("row remains available for reconciliation");
        assert_eq!(
            row.record.get("status").and_then(Value::as_str),
            Some("running")
        );
        assert_eq!(row.record.get("_processIdentity"), Some(&Value::Null));
        assert!(
            row.record
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("cleanup is unresolved"))
        );
    }

    #[test]
    fn restart_cleanup_error_after_first_identity_keeps_all_rows_reconcilable() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let first = create(&driver, &state.root)["id"]
            .as_str()
            .expect("first ID")
            .to_owned();
        let second_request =
            serde_json::json!({"id":"create:second-error","agent":"agent","cols":100,"rows":28});
        let second = driver
            .create(TerminalCreate {
                request_id: "create:second-error",
                agent: "agent",
                agent_cwd: state.root.to_str().expect("cwd"),
                cwd: &state.root,
                columns: 100,
                rows: 28,
                shell: "/bin/sh",
                environment: serde_json::json!({}),
                submitted_request: &second_request,
            })
            .expect("second create")["id"]
            .as_str()
            .expect("second ID")
            .to_owned();
        let identity = serde_json::json!({"pid":2345,"process_group":2345,"session":2345,"start_time_ticks":98765,"boot_id":"boot"});
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|transaction| {
                for id in [&first, &second] {
                    let mut row = TerminalsRepository::get(transaction, id)?
                        .expect("terminal")
                        .record;
                    row.set("_processIdentity", identity.clone());
                    TerminalsRepository::update_record(transaction, id, &row)?;
                }
                Ok(())
            })
            .expect("seed identities");
        *driver
            .session
            .orphan_failure
            .lock()
            .expect("orphan failure") = Some(2);
        assert!(driver.reconcile_after_restart().is_err());
        assert_eq!(
            driver
                .session
                .orphan_identities
                .lock()
                .expect("cleanups")
                .len(),
            2
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        for id in [&first, &second] {
            let row = TerminalsRepository::get(&connection, id)
                .expect("row")
                .expect("terminal");
            assert_eq!(
                row.record.get("status").and_then(Value::as_str),
                Some("running")
            );
            assert_eq!(row.record.get("_processIdentity"), Some(&identity));
        }
    }

    #[test]
    fn output_store_failpoint_rolls_back_rows_and_cursor_before_replay() {
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let id = create(&driver, &state.root)["id"]
            .as_str()
            .expect("terminal id")
            .to_owned();
        let notification = TerminalNotification {
            terminal_id: id.clone(),
            sequence: 1,
            event: TerminalEvent::Output(b"atomic".to_vec()),
        };
        let database = rusqlite::Connection::open(state.root.join("canvas.sqlite3"))
            .expect("open fail-point database");
        database
            .execute_batch(
                "CREATE TRIGGER fail_terminal_cursor BEFORE INSERT ON user_terminal_event_cursor
                 BEGIN SELECT RAISE(ABORT, 'injected cursor failure'); END;",
            )
            .expect("install cursor fail point");

        assert!(driver.apply_notification(notification.clone()).is_err());
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert_eq!(
            output(state.store.as_ref().expect("store"), &id, 0).expect("output")["text"],
            ""
        );
        assert_eq!(
            TerminalsRepository::event_cursor(&connection, &id).expect("cursor"),
            None
        );
        drop(connection);

        database
            .execute_batch("DROP TRIGGER fail_terminal_cursor;")
            .expect("remove cursor fail point");
        drop(database);
        driver
            .apply_notification(notification)
            .expect("replay after transaction rollback");
        assert_eq!(
            output(state.store.as_ref().expect("store"), &id, 0).expect("committed output")["text"],
            "atomic"
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert_eq!(
            TerminalsRepository::event_cursor(&connection, &id).expect("cursor"),
            Some(1)
        );
    }

    #[test]
    fn terminal_action_corpus_covers_complete_returns_wire_requests_and_rows() {
        use base64::Engine as _;
        let state = test_state();
        let driver = test_driver(&state, FakeSession::default());
        let created = create(&driver, &state.root);
        let id = created["id"].as_str().expect("terminal id").to_owned();
        assert_eq!(created["status"], "running");
        let mut seeded = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("terminal row")
        .expect("terminal exists")
        .record;
        let saved_identity = serde_json::json!({"pid":7,"process_group":7,"session":7,"start_time_ticks":11,"boot_id":"boot"});
        seeded.set("_processIdentity", saved_identity.clone());
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| TerminalsRepository::update_record(tx, &id, &seeded))
            .expect("seed process identity");

        let renamed = driver
            .action("rename", &serde_json::json!({"id":id,"title":"  Shell  "}))
            .expect("rename");
        assert_eq!(renamed["title"], "Shell");
        assert_eq!(renamed["id"], id);
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("renamed row")
        .expect("exists");
        assert_eq!(row.record.get("_processIdentity"), Some(&saved_identity));
        let mut visible_row = row.record.value().clone();
        visible_row
            .as_object_mut()
            .expect("object")
            .remove("_processIdentity");
        assert_eq!(renamed, visible_row);

        let input = driver
            .action(
                "input",
                &serde_json::json!({"id":id,"text":"hi🙂","request_id":"actions-input"}),
            )
            .expect("input");
        assert_eq!(input, serde_json::json!({"ok":true,"delivery":"sent"}));
        let request = driver
            .session
            .requests
            .lock()
            .expect("requests")
            .iter()
            .find(|(_, method, _)| method == "process/writeStdin")
            .expect("input request")
            .clone();
        assert_eq!(
            request.2["deltaBase64"],
            base64::engine::general_purpose::STANDARD.encode("hi🙂")
        );
        let connection = state.store.as_ref().expect("store").read().expect("read");
        assert_eq!(
            TerminalsRepository::receipt(&connection, "actions-input")
                .expect("receipt")
                .expect("saved")
                .1,
            r#"{"delivery":"sent","ok":true}"#
        );
        drop(connection);

        let resized = driver
            .action("resize", &serde_json::json!({"id":id,"cols":91,"rows":33}))
            .expect("resize");
        assert_eq!(resized["id"], id);
        assert_eq!(
            driver
                .session
                .requests
                .lock()
                .expect("requests")
                .iter()
                .find(|(_, method, _)| method == "process/resizePty")
                .expect("resize request")
                .2["size"],
            serde_json::json!({"cols":91,"rows":33})
        );

        let closed = driver
            .action("close", &serde_json::json!({"id":id}))
            .expect("close");
        assert_eq!(closed["status"], "closed");
        let row = TerminalsRepository::get(
            &state.store.as_ref().expect("store").read().expect("read"),
            &id,
        )
        .expect("closed row")
        .expect("exists");
        assert_eq!(row.record.get("_processIdentity"), Some(&saved_identity));
        let mut visible_row = row.record.value().clone();
        visible_row
            .as_object_mut()
            .expect("object")
            .remove("_processIdentity");
        assert_eq!(closed, visible_row);
        assert!(
            driver
                .session
                .requests
                .lock()
                .expect("requests")
                .iter()
                .any(
                    |(_, method, params)| method == "process/kill" && params["processHandle"] == id
                )
        );
    }

    #[test]
    fn python_terminal_action_differential_compares_all_returns_and_rows() {
        use crate::agents_create;

        fn normalize(value: &mut Value, root: &Path) {
            match value {
                Value::Object(object) => {
                    for (key, value) in object.iter_mut() {
                        if key == "cwd" && value.as_str() == root.to_str() {
                            *value = Value::String("$ROOT".into());
                        } else {
                            normalize(value, root);
                        }
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        normalize(value, root);
                    }
                }
                Value::String(text) => {
                    if let Ok(mut nested) = serde_json::from_str::<Value>(text) {
                        normalize(&mut nested, root);
                        *text = nested.to_string();
                    }
                }
                _ => {}
            }
        }

        fn snapshot(store: &RuntimeStore) -> Value {
            fn rows(connection: &rusqlite::Connection, table: &str) -> Value {
                let mut statement = connection
                    .prepare(&format!("SELECT * FROM \"{table}\" ORDER BY rowid"))
                    .expect("prepare action corpus table");
                let count = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..count)
                            .map(|index| {
                                let value: rusqlite::types::Value = row.get(index)?;
                                Ok(match value {
                                    rusqlite::types::Value::Null => Value::Null,
                                    rusqlite::types::Value::Integer(value) => Value::from(value),
                                    rusqlite::types::Value::Real(value) => Value::from(value),
                                    rusqlite::types::Value::Text(value) => Value::String(value),
                                    rusqlite::types::Value::Blob(value) => {
                                        Value::Array(value.into_iter().map(Value::from).collect())
                                    }
                                })
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .expect("query action corpus table")
                    .map(|row| Value::Array(row.expect("read action corpus row")))
                    .collect::<Vec<_>>();
                Value::Array(rows)
            }
            let connection = store.read().expect("read action corpus rows");
            serde_json::json!({
                "user_terminals":rows(&connection,"user_terminals"),
                "user_terminal_receipts":rows(&connection,"user_terminal_receipts"),
                "user_terminal_output":rows(&connection,"user_terminal_output"),
                "user_terminal_event_cursor":rows(&connection,"user_terminal_event_cursor"),
                "user_terminal_decoder_state":rows(&connection,"user_terminal_decoder_state"),
            })
        }

        let state = test_state();
        let rust_directory = tempfile::Builder::new()
            .prefix("terminal-action-rust-")
            .tempdir_in("/dev/shm/cs-rust-terminals-tests")
            .expect("Rust action corpus root");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = agents_create::prepared_python(&source);
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/terminal_actions_oracle.py");
        let output = Command::new(python)
            .arg(script)
            .arg(&source)
            .arg(&state.root)
            .arg(rust_directory.path())
            .env_clear()
            .env("HOME", rust_directory.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", "/dev/shm/cs-rust-terminals-tests")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .expect("run real Python terminal actions");
        assert!(
            output.status.success(),
            "Python terminal action oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oracle: Value = serde_json::from_slice(&output.stdout).expect("action oracle JSON");
        let python_actions = oracle["actions"].as_array().expect("Python actions");
        assert_eq!(python_actions.len(), 7);

        let store = Arc::new(
            RuntimeStore::open(Some(rust_directory.path())).expect("open Python-written pre-state"),
        );
        let driver = TerminalDriver::with_sources(
            store.clone(),
            FakeSession::default(),
            action_clock,
            action_id,
        );
        let create_request = serde_json::json!({
            "id":"action-create", "agent":"agent", "cols":91, "rows":33
        });
        let create_result = driver
            .create(TerminalCreate {
                request_id: "action-create",
                agent: "agent",
                agent_cwd: state.root.to_str().expect("test cwd"),
                cwd: &state.root,
                columns: 91,
                rows: 33,
                shell: "/bin/sh",
                environment: serde_json::json!({}),
                submitted_request: &create_request,
            })
            .expect("Rust create action");
        let rust_id = create_result["id"]
            .as_str()
            .expect("Rust terminal ID")
            .to_owned();
        assert_eq!(rust_id, python_actions[0]["result"]["id"]);

        let mut rust_results = vec![(create_result, None::<String>)];
        let mut rust_tables = vec![snapshot(&store)];
        for (action, data) in [
            (
                "rename",
                serde_json::json!({"id":rust_id,"title":"  Shell  "}),
            ),
            (
                "input",
                serde_json::json!({"id":rust_id,"text":"hi🙂","request_id":"actions-input"}),
            ),
            (
                "input",
                serde_json::json!({"id":rust_id,"text":"hi🙂","request_id":"actions-input"}),
            ),
            (
                "resize",
                serde_json::json!({"id":rust_id,"cols":91,"rows":33}),
            ),
            ("close", serde_json::json!({"id":rust_id})),
        ] {
            rust_results.push((driver.action(action, &data).expect("Rust action"), None));
            rust_tables.push(snapshot(&store));
        }
        let rejected = serde_json::json!({
            "id":rust_id,"text":"different","request_id":"actions-input"
        });
        let rust_rejection = driver.action("input", &rejected);
        assert!(matches!(
            rust_rejection,
            Err(TerminalError::ReceiptConflict)
        ));
        rust_results.push((Value::Null, Some("receipt_conflict".to_owned())));
        rust_tables.push(snapshot(&store));

        for (index, (python, (rust, rust_error))) in
            python_actions.iter().zip(rust_results.iter()).enumerate()
        {
            let mut expected_result = python["result"].clone();
            let mut actual_result = rust.clone();
            normalize(&mut expected_result, &state.root);
            normalize(&mut actual_result, &state.root);
            if let Some(error) = rust_error {
                assert_eq!(python["error"].as_str(), Some(error.as_str()));
                assert_eq!(actual_result, expected_result, "rejected action return");
            } else {
                assert_eq!(
                    actual_result, expected_result,
                    "complete return for Python action {}",
                    python["action"]
                );
            }

            let mut expected_tables = python["tables"].clone();
            let mut actual_tables = rust_tables[index].clone();
            normalize(&mut expected_tables, &state.root);
            normalize(&mut actual_tables, &state.root);
            assert_eq!(
                actual_tables, expected_tables,
                "stored rows after action {} (index {index})",
                python["action"]
            );
        }
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
            "The server restarted. This shell cannot be resumed."
        );
    }

    #[test]
    fn terminal_commits_advance_the_shared_change_notification_hub() {
        let state = test_state();
        let store = state.store.as_ref().expect("store").clone();
        let hub = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
        let notifier = Arc::clone(&hub);
        store.set_commit_notifier(move || notifier.notify_one());
        let driver = test_driver(&state, FakeSession::default());
        let initial = hub.snapshot().revision;
        let record = create(&driver, &state.root);
        assert!(hub.snapshot().revision > initial);
        let after_create = hub.snapshot().revision;
        driver
            .apply_notification(TerminalNotification {
                terminal_id: record["id"].as_str().expect("terminal ID").into(),
                sequence: 1,
                event: TerminalEvent::Output(b"event".to_vec()),
            })
            .expect("append output notification");
        assert!(hub.snapshot().revision > after_create);
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
