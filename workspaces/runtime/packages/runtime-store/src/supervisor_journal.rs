//! The supervisor's independent journal. Never opens the runtime DB. It follows
//! the shared durability policy of [`crate::sqlite_durability`].
use crate::{python_json, python_json_with_separators, sha256_hex};
use rusqlite::{
    Connection, OptionalExtension, Transaction, params,
    types::{Value as SqlValue, ValueRef},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
use studio_runtime_providers::supervisor_protocol::{
    EVENT_KIND_EXIT, EVENT_KIND_STDERR, EVENT_KIND_STDOUT, EventKind, HANDLE_LIMIT, HandleSnapshot,
    NativeId, ReceiptSnapshot, Refusal, WriteDecision,
};
use studio_runtime_schema::queue_command::Field;

/// Recovery evidence in the journal is historically unrestricted JSON; known
/// object fields are typed as presence-aware evidence without narrowing it.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum SupervisorRecovery {
    Object(RecoveryFields),
    Other(Value),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryFields {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    degraded: Field<Value>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    fallback_ready: Field<Value>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    blocked: Field<Value>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    notice: Field<Value>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl<'de> Deserialize<'de> for SupervisorRecovery {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.is_object() {
            RecoveryFields::deserialize(value)
                .map(Self::Object)
                .map_err(serde::de::Error::custom)
        } else {
            Ok(Self::Other(value))
        }
    }
}

impl SupervisorRecovery {
    pub fn clean() -> Self {
        Self::Object(RecoveryFields {
            degraded: Field::Value(json!(false)),
            fallback_ready: Field::Value(json!(false)),
            blocked: Field::Null,
            notice: Field::Missing,
            extra: Default::default(),
        })
    }
    pub fn blocked(&self) -> bool {
        matches!(self, Self::Object(fields) if fields.blocked.value().is_some())
    }
    pub fn mark_degraded(&mut self, notice: &str) {
        let Self::Object(fields) = self else {
            unreachable!("recovery is constructed as an object")
        };
        fields.degraded = Field::Value(json!(true));
        fields.notice = Field::Value(json!(notice));
    }
    pub fn block(&mut self, reason: &str) {
        let Self::Object(fields) = self else {
            unreachable!("recovery is constructed as an object")
        };
        fields.blocked = Field::Value(json!(reason));
    }
    pub fn clear_blocked(&mut self) {
        let Self::Object(fields) = self else {
            unreachable!("recovery is constructed as an object")
        };
        fields.blocked = Field::Null;
        fields.fallback_ready = match &fields.degraded {
            Field::Value(value) => Field::Value(value.clone()),
            Field::Missing | Field::Null => Field::Null,
        };
    }
    pub fn finish_recovery(&mut self) {
        let ready = matches!(self, Self::Object(fields) if fields.degraded.value() == Some(&json!(true)) && fields.blocked.value().is_none());
        let Self::Object(fields) = self else {
            unreachable!("recovery is constructed as an object")
        };
        fields.fallback_ready = Field::Value(json!(ready));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("supervisor journal periodic flush failed to start: {0}")]
    FlushThread(#[from] std::io::Error),
    #[error("supervisor journal: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error("supervisor journal contains invalid JSON")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Migration(#[from] Box<crate::StoreError>),
    #[error("supervisor journal lock poisoned")]
    Poisoned,
}
pub type Result<T> = std::result::Result<T, JournalError>;

/// Sole DDL owner for both supervisor structure validation and installation.
const SUPERVISOR_SCHEMA: &str = include_str!("supervisor_schema.sql");
const WRITTEN_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS operation_writes(handle TEXT NOT NULL,operation_id TEXT NOT NULL,PRIMARY KEY(handle,operation_id));";

#[derive(Clone, Copy)]
pub(crate) enum SchemaPresence {
    Existing,
    Complete,
}

pub(crate) fn validate_schema(db: &Connection, presence: SchemaPresence) -> crate::StoreResult<()> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SUPERVISOR_SCHEMA)?;
    expected.execute_batch(WRITTEN_SCHEMA)?;
    let mut tables = expected.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    for name in tables.query_map([], |row| row.get::<_, String>(0))? {
        let name = name?;
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [&name],
            |row| row.get(0),
        )?;
        if !exists && matches!(presence, SchemaPresence::Existing) {
            continue;
        }
        if !crate::bootstrap::same_table_layout(&expected, db, &name)? {
            return Err(crate::StoreError::SchemaObjectMismatch {
                database: "supervisor.sqlite3".into(),
                object: name,
                reason: "unsupported supervisor table structure".into(),
            });
        }
    }
    Ok(())
}

/// `handles.closed_reason` of a reservation whose child process never ran, or
/// was killed and reaped before its identity was committed. No native outcome
/// is unknown for such a row, so a later open of the handle may proceed.
pub const START_FAILED_REASON: &str = "Native child failed to start; no child process remains";
pub struct SupervisorJournal {
    /// Shared with the periodic WAL flush thread, which ends when the journal drops.
    db: Arc<Mutex<Connection>>,
    buffered_by_handle: Mutex<HashMap<String, u64>>,
}

pub struct StoredHandle {
    pub id: String,
    pub signature: String,
    pub pid: u32,
    pub snapshot: HandleSnapshot,
    pub init_result: Option<Value>,
    pub closed_at: Option<f64>,
    pub closed_reason: Option<String>,
    pub start_time: Option<String>,
    pub pgid: Option<u32>,
    pub buffered: u64,
    pub recovery_proof: bool,
}

/// A committed event row with its payload left in its original JSON encoding.
/// Protocol adapters can embed the raw payload without parsing and re-encoding it.
#[derive(Debug)]
pub struct StoredEvent {
    pub sequence: u64,
    pub kind: JournalEventKind,
    pub payload: Box<serde_json::value::RawValue>,
    pub generation: u64,
}

/// Journal rows allow only known kinds; protocol ingress also permits future kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalEventKind {
    Stdout,
    Stderr,
    Exit,
}

impl JournalEventKind {
    fn parse(value: &str) -> Result<Self> {
        match EventKind::parse(value) {
            EventKind::Stdout => Ok(Self::Stdout),
            EventKind::Stderr => Ok(Self::Stderr),
            EventKind::Exit => Ok(Self::Exit),
            EventKind::Unknown(_) => Err(Refusal("Invalid supervisor event kind").into()),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => EVENT_KIND_STDOUT,
            Self::Stderr => EVENT_KIND_STDERR,
            Self::Exit => EVENT_KIND_EXIT,
        }
    }
}

impl From<JournalEventKind> for EventKind {
    fn from(value: JournalEventKind) -> Self {
        match value {
            JournalEventKind::Stdout => Self::Stdout,
            JournalEventKind::Stderr => Self::Stderr,
            JournalEventKind::Exit => Self::Exit,
        }
    }
}

impl StoredEvent {
    fn from_columns(sequence: i64, kind: &str, payload: String, generation: i64) -> Result<Self> {
        let sequence =
            u64::try_from(sequence).map_err(|_| Refusal("Invalid supervisor event sequence"))?;
        let generation = u64::try_from(generation)
            .map_err(|_| Refusal("Invalid supervisor event generation"))?;
        Self::new(
            sequence,
            JournalEventKind::parse(kind)?,
            payload,
            generation,
        )
    }
    fn new(
        sequence: u64,
        kind: JournalEventKind,
        payload: String,
        generation: u64,
    ) -> Result<Self> {
        Ok(Self {
            sequence,
            kind,
            // RawValue validates JSON syntax without requiring numbers to fit f64.
            payload: serde_json::value::RawValue::from_string(payload)?,
            generation,
        })
    }
    fn into_legacy(self) -> Value {
        json!({"sequence":self.sequence,"kind":EventKind::from(self.kind),"payload":self.payload.get(),"generation":self.generation})
    }
}

fn snapshot(db: &Connection, handle: &str) -> Result<HandleSnapshot> {
    let (sequence, acknowledged, rpc_sequence, generation) = db.query_row(
        "SELECT sequence,acknowledged,rpc_sequence,generation FROM handles WHERE id=?",
        [handle],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    Ok(HandleSnapshot::from_columns(
        sequence,
        acknowledged,
        rpc_sequence,
        generation,
    )?)
}
/// Read `operations.native_id` exactly as stored. The column is declared INTEGER, but the
/// Python supervisor also stores text ids there (see [`NativeId`]).
fn stored_native_id(value: ValueRef<'_>) -> Result<Option<NativeId>> {
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(id) => Ok(Some(NativeId::Integer(id))),
        ValueRef::Text(text) => std::str::from_utf8(text)
            .map(|text| Some(NativeId::Text(text.to_owned())))
            .map_err(|_| Refusal("Invalid supervisor operations native id").into()),
        ValueRef::Real(_) | ValueRef::Blob(_) => {
            Err(Refusal("Invalid supervisor operations native id").into())
        }
    }
}
fn native_sql(id: &NativeId) -> SqlValue {
    match id {
        NativeId::Integer(id) => SqlValue::Integer(*id),
        NativeId::Text(text) => SqlValue::Text(text.clone()),
    }
}
fn receipt(db: &Connection, handle: &str, operation: &str) -> Result<Option<ReceiptSnapshot>> {
    let raw = db.query_row("SELECT digest,native_id,generation,response,response_sequence FROM operations WHERE handle=? AND operation_id=?", params![handle,operation], |row| {
        Ok((row.get::<_,String>(0)?,stored_native_id(row.get_ref(1)?),row.get::<_,Option<i64>>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<i64>>(4)?))
    }).optional()?;
    raw.map(
        |(digest, native_id, generation, response, response_sequence)| {
            Ok(ReceiptSnapshot::from_columns(
                digest,
                native_id?,
                generation,
                response,
                response_sequence,
            )?)
        },
    )
    .transpose()
}
fn buffered_sizes(db: &Connection) -> Result<HashMap<String, u64>> {
    let mut statement =
        db.prepare("SELECT handle,COALESCE(sum(size),0) FROM events GROUP BY handle")?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .map(|row| {
            let (handle, size) = row?;
            let size = u64::try_from(size).map_err(|_| Refusal("Invalid supervisor event size"))?;
            Ok((handle, size))
        })
        .collect()
}
fn stored(db: &Connection, handle: &str, buffered: u64) -> Result<Option<StoredHandle>> {
    let row = db.query_row("SELECT h.signature,h.pid,h.init_result,h.closed_at,h.closed_reason,c.start_time,c.pgid,c.pid FROM handles h LEFT JOIN child_identities c ON c.handle=h.id WHERE h.id=?", [handle], |r| {
        Ok((r.get::<_,String>(0)?,r.get::<_,u32>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<f64>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<u32>>(6)?,r.get::<_,Option<u32>>(7)?))
    }).optional()?;
    row.map(|(signature,pid,init,closed_at,closed_reason,start_time,pgid,identity_pid)| {
        if identity_pid.is_some_and(|identity| identity != pid) { return Err(Refusal("Supervisor child identity disagrees with its handle").into()); }
        let recovery_proof = db.query_row("SELECT 1 FROM degraded_handles d JOIN child_identities c ON c.handle=d.handle WHERE d.handle=? AND d.prior_pid=? AND c.pid=d.prior_pid AND c.start_time=d.start_time", params![handle,pid], |_| Ok(true)).optional()?.unwrap_or(false);
        Ok(StoredHandle { id:handle.to_owned(), signature,pid, snapshot:snapshot(db,handle)?, init_result:init.map(|s|serde_json::from_str(&s)).transpose()?, closed_at,closed_reason,start_time,pgid,buffered,recovery_proof })
    }).transpose()
}

impl SupervisorJournal {
    pub fn open(root: &Path) -> Result<Self> {
        let mut db = Connection::open(root.join("supervisor.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        crate::sqlite_durability::configure_writer(&db)?;
        let page_size: u64 = db.query_row("PRAGMA page_size", [], |r| {
            r.get::<_, u32>(0).map(u64::from)
        })?;
        let existing: u64 = db.query_row("PRAGMA page_count", [], |r| {
            r.get::<_, u32>(0).map(u64::from)
        })?;
        // Preserve oversized existing journals while refusing further growth.
        let maximum = (1024 * 1024 * 1024 / page_size.max(1)).max(existing);
        db.pragma_update(None, "max_page_count", sql(maximum)?)?;
        crate::migrations::supervisor(&mut db).map_err(Box::new)?;
        let buffered_by_handle = buffered_sizes(&db)?;
        let journal = Self {
            db: Arc::new(Mutex::new(db)),
            buffered_by_handle: Mutex::new(buffered_by_handle),
        };
        crate::sqlite_durability::spawn_periodic_flush(
            "supervisor-journal",
            Arc::downgrade(&journal.db),
        )?;
        // Check every existing handle before any process effects.
        journal.handles(false)?;
        journal.validate_rows()?;
        Ok(journal)
    }
    fn validate_rows(&self) -> Result<()> {
        let db = self.lock()?;
        let mut statement = db.prepare("SELECT handle,operation_id FROM operations")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (handle, operation) = row?;
            receipt(&db, &handle, &operation)?;
        }
        let mut statement = db.prepare("SELECT sequence,kind,payload,generation FROM events")?;
        for row in statement.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })? {
            let (sequence, kind, payload, generation) = row?;
            StoredEvent::from_columns(sequence, &kind, payload, generation)?;
        }
        Ok(())
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db.lock().map_err(|_| JournalError::Poisoned)
    }
    fn transaction<T>(
        &self,
        action: impl FnOnce(
            &Transaction<'_>,
            &HashMap<String, u64>,
        ) -> Result<(T, Option<(String, Option<i64>)>)>,
    ) -> Result<T> {
        let mut db = self.lock()?;
        let mut buffered = self
            .buffered_by_handle
            .lock()
            .map_err(|_| JournalError::Poisoned)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (result, delta) = action(&tx, &buffered)?;
        tx.commit()?;
        if let Some((handle, delta)) = delta {
            if let Some(delta) = delta {
                let current = buffered.entry(handle).or_default();
                if delta >= 0 {
                    *current = current.saturating_add(delta as u64);
                } else {
                    *current = current.saturating_sub(delta.unsigned_abs());
                }
            } else {
                buffered.remove(&handle);
            }
        }
        Ok(result)
    }
    pub fn handle(&self, handle: &str) -> Result<Option<StoredHandle>> {
        let db = self.lock()?;
        let buffered = self
            .buffered_by_handle
            .lock()
            .map_err(|_| JournalError::Poisoned)?;
        stored(
            &db,
            handle,
            buffered.get(handle).copied().unwrap_or_default(),
        )
    }
    pub fn handles(&self, active: bool) -> Result<Vec<StoredHandle>> {
        let db = self.lock()?;
        let buffered = self
            .buffered_by_handle
            .lock()
            .map_err(|_| JournalError::Poisoned)?;
        let mut statement =
            db.prepare("SELECT id FROM handles WHERE ?=0 OR closed_at IS NULL ORDER BY rowid")?;
        let ids = statement
            .query_map([active], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                stored(&db, &id, buffered.get(&id).copied().unwrap_or_default())?
                    .ok_or_else(|| Refusal("Supervisor handle disappeared").into())
            })
            .collect()
    }
    /// Create this journal's random identity once and return it. Event sequences
    /// start again in another journal, so the identity is part of each handle epoch.
    pub fn ensure_journal_id(&self, key: &str) -> Result<String> {
        let db = self.lock()?;
        db.execute(
            "INSERT OR IGNORE INTO supervisor_state VALUES (?,?)",
            params![key, uuid::Uuid::new_v4().simple().to_string()],
        )?;
        Ok(db.query_row(
            "SELECT value FROM supervisor_state WHERE key=?",
            [key],
            |row| row.get(0),
        )?)
    }
    /// Name one handle row of this journal: `<journal id>:<floor(created seconds * 1_000_000)>`,
    /// the same value as `codex_process_supervisor.journal_epoch` in the Python supervisor.
    /// A recreated handle row gets a new creation time and so a new epoch.
    pub fn epoch(&self, handle: &str, journal_id: &str) -> Result<Option<String>> {
        let db = self.lock()?;
        let created: Option<f64> = db
            .query_row("SELECT created FROM handles WHERE id=?", [handle], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(created
            .filter(|created| created.is_finite())
            .map(|created| format!("{journal_id}:{}", (created * 1_000_000.0).floor() as i64)))
    }
    /// Reserve before spawn. A crash at this point leaves an orphan refusal, never a duplicate child.
    pub fn reserve(&self, handle: &str, signature: &str, now: f64) -> Result<()> {
        self.transaction(|tx, _buffered| {
            tx.execute(
                "INSERT INTO handles(id,signature,pid,created) VALUES (?,?,0,?) ON CONFLICT(id) DO UPDATE SET pid=0,signature=excluded.signature,closed_at=NULL,closed_reason=NULL",
                params![handle, signature, now],
            )?;
            tx.execute("DELETE FROM child_identities WHERE handle=?", [handle])?;
            Ok(((), None))
        })
    }
    /// Close a reservation whose child never started, or was killed and reaped
    /// before its identity was committed. Only the unopened reservation
    /// (pid 0, still open, same launch signature) changes, so a repeated call
    /// records nothing new. Returns whether this call closed the row.
    pub fn start_failed(&self, handle: &str, signature: &str, now: f64) -> Result<bool> {
        self.transaction(|tx, _buffered| {
            let closed = tx.execute(
                "UPDATE handles SET closed_at=?,closed_reason=? WHERE id=? AND pid=0 AND signature=? AND closed_at IS NULL",
                params![now, START_FAILED_REASON, handle, signature],
            )?;
            Ok((closed == 1, None))
        })
    }
    pub fn opened(
        &self,
        handle: &str,
        signature: &str,
        pid: u32,
        start: &str,
        generation: Option<u64>,
    ) -> Result<u64> {
        self.transaction(|tx, _buffered| {
            let generation = generation.map(|value| i64::try_from(value).map_err(|_| Refusal("Invalid generation"))).transpose()?;
            tx.execute("UPDATE handles SET pid=?,signature=?,rpc_sequence=0,generation=COALESCE(?,generation+1),init_result=NULL,closed_at=NULL,closed_reason=NULL WHERE id=?",params![pid,signature,generation,handle])?;
            tx.execute("INSERT INTO child_identities(handle,pid,pgid,start_time,job_name,job_kill_on_close) VALUES (?,?,?,?,'',0) ON CONFLICT(handle) DO UPDATE SET pid=excluded.pid,pgid=excluded.pgid,start_time=excluded.start_time,job_name='',job_kill_on_close=0",params![handle,pid,pid,start])?;
            tx.execute("DELETE FROM degraded_handles WHERE handle=?",[handle])?;
            Ok((snapshot(tx,handle)?.generation(), None))
        })
    }
    /// This commit is the only admission point. The caller sends only Accept, once.
    pub fn accept(
        &self,
        handle: &str,
        operation: &str,
        native: Option<&NativeId>,
        message: &Value,
        now: f64,
    ) -> Result<WriteDecision> {
        let mut canonical = message.clone();
        canonical
            .as_object_mut()
            .ok_or(Refusal("Invalid supervisor write message"))?
            .remove("id");
        let digest = sha256_hex(python_json_with_separators(&canonical, ",", ":").as_bytes());
        self.transaction(|tx, buffered| {
            let state = snapshot(tx,handle)?;
            let prior = receipt(tx,handle,operation)?;
            let decision = state.write(
                prior.as_ref(),
                &digest,
                native,
                message,
                buffered.get(handle).copied().unwrap_or_default(),
            )?;
            if let WriteDecision::Accept{remote_id,rpc_sequence,..} = &decision {
                tx.execute("UPDATE handles SET rpc_sequence=? WHERE id=?",params![sql(*rpc_sequence)?,handle])?;
                tx.execute("INSERT INTO operations(handle,operation_id,digest,native_id,accepted,generation) VALUES (?,?,?,?,?,?)",params![handle,operation,digest,remote_id.as_ref().map(native_sql),now,sql(state.generation())?])?;
                // INTEGER affinity turns numeric text such as "42" into a number. Refuse a
                // write whose receipt would not return the same id, before it commits.
                let stored = tx.query_row("SELECT native_id FROM operations WHERE handle=? AND operation_id=?",params![handle,operation],|row| Ok(stored_native_id(row.get_ref(0)?)))??;
                if stored != *remote_id {
                    return Err(Refusal("Supervisor native id cannot be journaled exactly").into());
                }
            }
            Ok((decision, None))
        })
    }
    pub fn operation_status(&self, handle: &str, operation: &str) -> Result<Value> {
        if !operation.starts_with("monitor:") {
            return Err(Refusal("Invalid monitor operation identity").into());
        }
        let db = self.lock()?;
        match receipt(&db, handle, operation)? {
            Some(receipt) => Ok(receipt.status(&snapshot(&db, handle)?)),
            None => Ok(json!({"accepted":false,"reason":"operation_missing"})),
        }
    }
    pub fn mark_written(&self, handle: &str, operation: &str) -> Result<()> {
        self.transaction(|tx, _| {
            let changed = tx.execute("INSERT OR IGNORE INTO operation_writes(handle,operation_id) SELECT handle,operation_id FROM operations WHERE handle=?1 AND operation_id=?2", params![handle, operation])?;
            if changed == 0 {
                let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operation_writes WHERE handle=?1 AND operation_id=?2)", params![handle,operation], |row| row.get(0))?;
                if !exists { return Err(Refusal("Supervisor operation is not accepted").into()); }
            }
            Ok(((), None))
        })
    }

    pub fn provider_operation_status(
        &self,
        handle: &str,
        operation: &str,
    ) -> Result<studio_runtime_providers::supervisor_protocol::ProviderOperationStatus> {
        use studio_runtime_providers::supervisor_protocol::{
            ProviderOperationState, ProviderOperationStatus,
        };
        let db = self.lock()?;
        let record = db.query_row("SELECT o.generation,h.generation,o.native_id,EXISTS(SELECT 1 FROM operation_writes w WHERE w.handle=o.handle AND w.operation_id=o.operation_id) FROM operations o JOIN handles h ON h.id=o.handle WHERE o.handle=?1 AND o.operation_id=?2", params![handle,operation], |row| Ok((row.get::<_,Option<i64>>(0)?,row.get::<_,i64>(1)?,stored_native_id(row.get_ref(2)?),row.get::<_,bool>(3)?))).optional()?;
        match record {
            Some((Some(generation), current, native_id, written)) if generation == current => {
                Ok(ProviderOperationStatus {
                    state: if written {
                        ProviderOperationState::Written
                    } else {
                        ProviderOperationState::Accepted
                    },
                    native_id: native_id?,
                })
            }
            _ => Ok(ProviderOperationStatus {
                state: ProviderOperationState::Unknown,
                native_id: None,
            }),
        }
    }
    /// Candidate sequence stays fixed across retries and even a lost commit confirmation.
    pub fn append(
        &self,
        handle: &str,
        generation: u64,
        sequence: u64,
        kind: &str,
        payload: &Value,
    ) -> Result<AppendResult> {
        if sequence >= i64::MAX as u64 {
            return Err(Refusal("Supervisor event sequence exhausted").into());
        }
        let raw = serde_json::to_string(payload)?;
        self.transaction(|tx, buffered| {
            let state = snapshot(tx,handle)?;
            if state.generation() != generation { return Ok((AppendResult::Obsolete, None)); }
            if state.sequence() == sequence { return Ok((AppendResult::Saved, None)); }
            if state.sequence()+1 != sequence { return Err(Refusal("Supervisor output sequence changed before its receipt").into()); }
            let kind = JournalEventKind::parse(kind)?;
            if buffered.get(handle).copied().unwrap_or_default().saturating_add(raw.len() as u64) > HANDLE_LIMIT { return Ok((AppendResult::Full, None)); }
            tx.execute("UPDATE handles SET sequence=? WHERE id=?",params![sql(sequence)?,handle])?;
            tx.execute("INSERT INTO events(handle,sequence,kind,payload,size,generation) VALUES (?,?,?,?,?,?)",params![handle,sql(sequence)?,kind.as_str(),raw,sql(raw.len() as u64)?,sql(generation)?])?;
            if kind == JournalEventKind::Stdout {
                if payload.get("id").and_then(Value::as_u64) == Some(1) && payload.get("result").is_some() {
                    tx.execute("UPDATE handles SET init_result=? WHERE id=?",params![serde_json::to_string(&payload["result"])?,handle])?;
                }
                if let Some(id) = payload.get("id").and_then(Value::as_i64).filter(|_|payload.get("method").is_none()) {
                    tx.execute("UPDATE operations SET response=?,response_sequence=? WHERE handle=? AND native_id=? AND generation=? AND substr(operation_id,1,8)='monitor:' AND response IS NULL",params![raw,sql(sequence)?,handle,id,sql(generation)?])?;
                }
            }
            Ok((
                AppendResult::Saved,
                Some((handle.to_owned(), Some(raw.len() as i64))),
            ))
        })
    }
    pub fn ack(&self, handle: &str, cursor: u64) -> Result<()> {
        self.transaction(|tx, _buffered| {
            snapshot(tx, handle)?.cursor(cursor, true)?;
            let reclaimed: i64 = tx.query_row(
                "SELECT COALESCE(sum(size),0) FROM events WHERE handle=? AND sequence<=?",
                params![handle, sql(cursor)?],
                |row| row.get(0),
            )?;
            tx.execute(
                "DELETE FROM events WHERE handle=? AND sequence<=?",
                params![handle, sql(cursor)?],
            )?;
            tx.execute(
                "UPDATE handles SET acknowledged=? WHERE id=?",
                params![sql(cursor)?, handle],
            )?;
            Ok(((), Some((handle.to_owned(), Some(-reclaimed)))))
        })
    }
    pub fn events(&self, handle: &str, cursor: u64, one: bool) -> Result<Value> {
        let db = self.lock()?;
        let state = snapshot(&db, handle)?;
        state.cursor(cursor, false)?;
        let mut stmt = db.prepare("SELECT sequence,kind,payload,generation FROM events WHERE handle=? AND sequence>? ORDER BY sequence LIMIT ?")?;
        let events = stmt
            .query_map(
                params![handle, sql(cursor)?, if one { 1 } else { -1 }],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )?
            .map(|row| {
                let (sequence, kind, payload, generation) = row?;
                Ok(StoredEvent::from_columns(sequence, &kind, payload, generation)?.into_legacy())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"events":events,"sequence":state.sequence(),"acknowledged":state.acknowledged()}))
    }

    /// Read an ordered event batch without loading payloads that cannot fit the wire frame.
    pub fn events_v2(
        &self,
        handle: &str,
        cursor: u64,
        generation: u64,
        limit: usize,
        frame_bytes: usize,
    ) -> Result<Vec<StoredEvent>> {
        let db = self.lock()?;
        let state = snapshot(&db, handle)?;
        if state.generation() != generation {
            return Err(Refusal("Supervisor handle generation changed").into());
        }
        state.cursor(cursor, false)?;
        let mut statement = db.prepare(
            "SELECT sequence,kind,length(CAST(payload AS BLOB)),generation FROM events WHERE handle=? AND sequence>? ORDER BY sequence LIMIT ?",
        )?;
        let candidates = statement
            .query_map(params![handle, sql(cursor)?, sql(limit as u64)?], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut selected = Vec::new();
        let mut frame_usage = 256usize;
        for (sequence, kind, payload_bytes, event_generation) in candidates {
            let kind = JournalEventKind::parse(&kind)?;
            let payload_bytes = usize::try_from(payload_bytes)
                .map_err(|_| Refusal("Invalid supervisor event payload size"))?;
            // Reserve room for all event metadata and the enclosing RPC frame.
            let candidate_usage = frame_usage
                .saturating_add(payload_bytes)
                .saturating_add(128);
            if candidate_usage > frame_bytes {
                if selected.is_empty() {
                    return Err(Refusal("Supervisor event exceeds the frame size limit").into());
                }
                break;
            }
            frame_usage = candidate_usage;
            selected.push((sequence, kind, event_generation));
        }
        selected
            .into_iter()
            .map(|(sequence, kind, event_generation)| {
                let sequence = u64::try_from(sequence)
                    .map_err(|_| Refusal("Invalid supervisor event sequence"))?;
                let generation = u64::try_from(event_generation)
                    .map_err(|_| Refusal("Invalid supervisor event generation"))?;
                let payload = db.query_row(
                    "SELECT payload FROM events WHERE handle=? AND sequence=?",
                    params![handle, sql(sequence)?],
                    |row| row.get::<_, String>(0),
                )?;
                StoredEvent::new(sequence, kind, payload, generation)
            })
            .collect()
    }

    pub fn retire(
        &self,
        handle: &str,
        generation: u64,
        exited: bool,
        readers_done: bool,
    ) -> Result<()> {
        self.transaction(|tx, _buffered| {
            snapshot(tx, handle)?.retire(handle, generation, exited, readers_done)?;
            for table in [
                "events",
                "operation_writes",
                "operations",
                "child_identities",
                "degraded_handles",
            ] {
                tx.execute(&format!("DELETE FROM {table} WHERE handle=?"), [handle])?;
            }
            tx.execute("DELETE FROM handles WHERE id=?", [handle])?;
            Ok(((), Some((handle.to_owned(), None))))
        })
    }
    pub fn close(&self, handle: &str, reason: &str, now: f64) -> Result<()> {
        self.transaction(|tx, _buffered| {
            tx.execute(
                "UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",
                params![now, reason, handle],
            )?;
            Ok(((), None))
        })
    }
    pub fn recovered(
        &self,
        row: &StoredHandle,
        outcome: &str,
        detail: &str,
        now: f64,
    ) -> Result<()> {
        self.transaction(|tx, _buffered| {
            tx.execute("INSERT INTO recovery_events(at,handle,pid,start_time,outcome,detail) VALUES (?,?,?,?,?,?)",params![now,row.id,row.pid,row.start_time.as_deref().unwrap_or(""),outcome,detail])?;
            tx.execute("DELETE FROM recovery_events WHERE handle=? AND id NOT IN (SELECT id FROM recovery_events WHERE handle=? ORDER BY id DESC LIMIT 16)",params![row.id,row.id])?;
            let closed = matches!(outcome, "already-exited"|"terminated"|"killed"|"identity-mismatch").then_some(now);
            tx.execute("UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",params![closed,detail,row.id])?;
            if matches!(outcome,"already-exited"|"terminated"|"killed") {
                tx.execute("INSERT OR REPLACE INTO degraded_handles VALUES (?,?,?,?)",params![row.id,row.pid,row.start_time,now])?;
            }
            Ok(((), None))
        })
    }
    pub fn close_unknown(
        &self,
        expected: &StoredHandle,
        now: f64,
        recovery: &SupervisorRecovery,
    ) -> Result<()> {
        self.transaction(|tx, buffered| {
            let current = stored(
                tx,
                &expected.id,
                buffered.get(&expected.id).copied().unwrap_or_default(),
            )?
            .ok_or(Refusal("Unknown supervisor handle"))?;
            if current.closed_at.is_some() || current.pid != expected.pid
                || current.start_time != expected.start_time || current.signature != expected.signature {
                return Err(Refusal("Operator close refused: handle identity changed").into());
            }
            tx.execute("UPDATE handles SET closed_at=?,closed_reason='Closed by operator after verified test/unknown-client ownership' WHERE id=?", params![now,expected.id])?;
            tx.execute("INSERT INTO supervisor_state VALUES ('recovery',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(recovery)?])?;
            Ok(((), None))
        })
    }
    pub fn save_recovery(&self, recovery: &SupervisorRecovery) -> Result<()> {
        self.transaction(|tx, _buffered| { tx.execute("INSERT INTO supervisor_state VALUES ('recovery',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(recovery)?])?; Ok(((), None)) })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum AppendResult {
    Saved,
    Full,
    Obsolete,
}

pub fn launch_signature(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: Option<&str>,
) -> String {
    sha256_hex(python_json(&json!({"command":command,"env":env,"cwd":cwd})).as_bytes())
}

impl JournalError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Sqlite(rusqlite::Error::SqliteFailure(e,_)) if matches!(e.code,rusqlite::ErrorCode::DiskFull|rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked|rusqlite::ErrorCode::SystemIoFailure))
    }
}

fn sql(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Refusal("Supervisor integer exceeds SQLite range").into())
}

pub(crate) fn prepare_schema(db: &Transaction<'_>) -> crate::StoreResult<()> {
    db.execute_batch(SUPERVISOR_SCHEMA)?;
    db.execute_batch("CREATE INDEX IF NOT EXISTS operations_monitor_native ON operations(handle,generation,native_id) WHERE substr(operation_id,1,8)='monitor:'")?;
    Ok(())
}

pub(crate) fn prepare_written_schema(db: &Transaction<'_>) -> crate::StoreResult<()> {
    db.execute_batch(WRITTEN_SCHEMA)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_result_uses_native_json() {
        let root = tempfile::tempdir().unwrap();
        let journal = SupervisorJournal::open(root.path()).unwrap();
        journal.reserve("handle", "signature", 1.0).unwrap();
        let generation = journal
            .opened("handle", "signature", 41, "start", None)
            .unwrap();
        journal
            .append(
                "handle",
                generation,
                1,
                "stdout",
                &json!({"id":1,"result":{"text":"é"}}),
            )
            .unwrap();
        let db = journal.lock().unwrap();
        let init: String = db
            .query_row(
                "SELECT init_result FROM handles WHERE id='handle'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(init, r#"{"text":"é"}"#);
    }

    #[test]
    fn recovery_owner_preserves_open_wire_values_and_journal_writes() {
        let root = tempfile::tempdir().unwrap();
        let journal = SupervisorJournal::open(root.path()).unwrap();
        for input in [
            json!(null),
            json!(false),
            json!([1, 2]),
            json!({}),
            json!({"blocked":null,"notice":{"future":true}}),
            json!({"degraded":"legacy","fallbackReady":0,"blocked":false,"extra":[1,null]}),
        ] {
            let recovery = SupervisorRecovery::deserialize(input.clone()).unwrap();
            let encoded = serde_json::to_string(&recovery).unwrap();
            let decoded: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, input);
            journal.save_recovery(&recovery).unwrap();
            let saved: String = journal
                .lock()
                .unwrap()
                .query_row(
                    "SELECT value FROM supervisor_state WHERE key='recovery'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(serde_json::from_str::<Value>(&saved).unwrap(), input);
        }
        for blocked in [json!(false), json!(0), json!(""), json!({}), json!([])] {
            let recovery = SupervisorRecovery::deserialize(json!({"blocked":blocked})).unwrap();
            assert!(recovery.blocked());
        }
        let mut recovery =
            SupervisorRecovery::deserialize(json!({"degraded":"legacy","blocked":false,"extra":3}))
                .unwrap();
        recovery.clear_blocked();
        let saved = recovery.serialize(serde_json::value::Serializer).unwrap();
        assert_eq!(
            saved,
            json!({"degraded":"legacy","fallbackReady":"legacy","blocked":null,"extra":3})
        );
        recovery.finish_recovery();
        let saved = recovery.serialize(serde_json::value::Serializer).unwrap();
        assert_eq!(saved["fallbackReady"], false);
    }

    #[test]
    fn append_refuses_unknown_kind_without_advancing_the_journal() {
        let root = tempfile::tempdir().unwrap();
        let journal = SupervisorJournal::open(root.path()).unwrap();
        journal.reserve("raw", "signature", 1.0).unwrap();
        let generation = journal
            .opened("raw", "signature", 41, "start", None)
            .unwrap();
        assert!(
            journal
                .append("raw", generation, 1, "future", &json!({}))
                .is_err()
        );
        assert_eq!(
            journal.handle("raw").unwrap().unwrap().snapshot.sequence(),
            0
        );
        assert_eq!(
            journal
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            journal
                .append("raw", generation, 1, "stdout", &json!({}))
                .unwrap(),
            AppendResult::Saved
        );
        assert_eq!(
            journal
                .append("raw", generation, 1, "future", &json!({}))
                .unwrap(),
            AppendResult::Saved
        );
        assert_eq!(
            journal
                .append("raw", generation + 1, 2, "future", &json!({}))
                .unwrap(),
            AppendResult::Obsolete
        );
    }

    #[test]
    fn journal_reads_raw_valid_json_without_number_normalization() {
        let root = tempfile::tempdir().unwrap();
        let journal = SupervisorJournal::open(root.path()).unwrap();
        journal.reserve("raw", "signature", 1.0).unwrap();
        let generation = journal
            .opened("raw", "signature", 41, "start", None)
            .unwrap();
        let raw = "{ \"huge\" : 1e400, \"text\" : \"\\u0061\" }";
        {
            let db = journal.lock().unwrap();
            db.execute("UPDATE handles SET sequence=2 WHERE id='raw'", [])
                .unwrap();
            db.execute("INSERT INTO events(handle,sequence,kind,payload,size,generation) VALUES('raw',2,'exit','null',4,?1)", [generation as i64]).unwrap();
            db.execute("INSERT INTO events(handle,sequence,kind,payload,size,generation) VALUES('raw',1,'stdout',?1,?2,?3)", params![raw,raw.len() as i64,generation as i64]).unwrap();
        }
        drop(journal);
        let journal = SupervisorJournal::open(root.path()).unwrap();
        let legacy = journal.events("raw", 0, false).unwrap();
        assert_eq!(legacy["events"][0]["payload"], raw);
        assert_eq!(legacy["events"][0]["sequence"], 1);
        assert_eq!(legacy["events"][1]["sequence"], 2);
        assert_eq!(
            journal.events("raw", 0, true).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let events = journal.events_v2("raw", 0, generation, 10, 4096).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(events[0].payload.get(), raw);
    }

    #[test]
    fn malformed_journal_events_are_refused_on_read_and_startup() {
        for (sequence, kind, payload, generation) in [
            (1, "future", "{}", 1),
            (1, "stdout", "{bad", 1),
            (1, "stdout", "{}", -1),
            (-1, "stdout", "{}", 1),
        ] {
            let root = tempfile::tempdir().unwrap();
            let journal = SupervisorJournal::open(root.path()).unwrap();
            journal.reserve("raw", "signature", 1.0).unwrap();
            journal
                .opened("raw", "signature", 41, "start", None)
                .unwrap();
            {
                let db = journal.lock().unwrap();
                db.execute("UPDATE handles SET sequence=1 WHERE id='raw'", [])
                    .unwrap();
                db.execute("INSERT INTO events(handle,sequence,kind,payload,size,generation) VALUES('raw',?1,?2,?3,?4,?5)", params![sequence,kind,payload,payload.len() as i64,generation]).unwrap();
            }
            if sequence > 0 {
                assert!(journal.events("raw", 0, false).is_err());
                assert!(journal.events_v2("raw", 0, 1, 10, 4096).is_err());
            }
            drop(journal);
            assert!(SupervisorJournal::open(root.path()).is_err());
        }
    }

    #[test]
    fn supervisor_status_tracks_outbox_operation_from_accept_to_write() {
        #[derive(serde::Serialize)]
        struct Message {
            method: &'static str,
            id: i64,
        }
        #[derive(serde::Deserialize, Debug, PartialEq)]
        #[serde(rename_all = "snake_case")]
        enum State {
            Unknown,
            Accepted,
            Written,
        }
        #[derive(serde::Deserialize)]
        struct Status {
            state: State,
        }
        let root = tempfile::tempdir().expect("journal root");
        let journal = SupervisorJournal::open(root.path()).expect("journal");
        journal
            .reserve("account:a", "signature", 1.0)
            .expect("reserve");
        journal
            .opened("account:a", "signature", 41, "start", None)
            .expect("open");
        let status = || -> State {
            let raw = journal
                .provider_operation_status("account:a", "outbox:effect")
                .expect("status");
            serde_json::from_str::<Status>(&serde_json::to_string(&raw).expect("serialize status"))
                .expect("typed status")
                .state
        };
        assert_eq!(status(), State::Unknown);
        let message = crate::projection_value(&Message {
            method: "turn/start",
            id: 5,
        });
        journal
            .accept(
                "account:a",
                "outbox:effect",
                Some(&NativeId::Integer(5)),
                &message,
                2.0,
            )
            .expect("accept");
        assert_eq!(status(), State::Accepted);
        journal
            .mark_written("account:a", "outbox:effect")
            .expect("written");
        assert_eq!(status(), State::Written);
    }

    #[test]
    fn journal_epoch_names_one_handle_row_of_one_journal() {
        let root = tempfile::tempdir().expect("temporary supervisor journal root");
        let journal = SupervisorJournal::open(root.path()).expect("open journal");
        let id = journal.ensure_journal_id("journalId").expect("journal id");
        assert_eq!(journal.ensure_journal_id("journalId").expect("same id"), id);
        assert_eq!(journal.epoch("account:a", &id).expect("missing row"), None);
        journal
            .reserve("account:a", "signature", 1792051725.893665)
            .expect("reserve handle");
        // codex_process_supervisor.journal_epoch builds the same literal.
        assert_eq!(
            journal.epoch("account:a", "journal").expect("epoch"),
            Some("journal:1792051725893665".to_owned())
        );
        // A repeated reservation keeps the row and its epoch.
        journal
            .reserve("account:a", "signature", 5.0)
            .expect("reserve again");
        assert_eq!(
            journal.epoch("account:a", "journal").expect("epoch"),
            Some("journal:1792051725893665".to_owned())
        );
        drop(journal);
        let reopened = SupervisorJournal::open(root.path()).expect("reopen journal");
        assert_eq!(
            reopened.ensure_journal_id("journalId").expect("kept id"),
            id
        );
    }

    #[test]
    fn v2_batch_rejects_an_oversized_first_event_without_advancing_ack() {
        let root = tempfile::tempdir().expect("temporary supervisor journal root");
        let journal = SupervisorJournal::open(root.path()).expect("open journal");
        journal
            .reserve("server-command:oversized", "signature", 1.0)
            .expect("reserve handle");
        let generation = journal
            .opened("server-command:oversized", "signature", 41, "start", None)
            .expect("open child handle");
        let payload = json!({"text": "x".repeat(1024)});
        assert_eq!(
            journal
                .append(
                    "server-command:oversized",
                    generation,
                    1,
                    "stdout",
                    &payload
                )
                .expect("append output"),
            AppendResult::Saved
        );

        let error = journal
            .events_v2("server-command:oversized", 0, generation, 64, 512)
            .expect_err("oversized first event must be refused");
        assert!(error.to_string().contains("frame size limit"));
        let row = journal
            .handle("server-command:oversized")
            .expect("load handle")
            .expect("saved handle");
        assert_eq!(row.snapshot.sequence(), 1);
        assert_eq!(row.snapshot.acknowledged(), 0);
        let batch = journal
            .events_v2("server-command:oversized", 0, generation, 64, 2048)
            .expect("read event with a sufficient frame budget");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload.get(), payload.to_string());
    }

    #[test]
    fn retiring_handle_drops_its_buffered_cache_entry() {
        let root = tempfile::tempdir().expect("temporary supervisor journal root");
        let journal = SupervisorJournal::open(root.path()).expect("open journal");
        journal
            .reserve("server-command:fixture", "signature", 1.0)
            .expect("reserve handle");
        let generation = journal
            .opened("server-command:fixture", "signature", 41, "start", None)
            .expect("open child handle");
        assert_eq!(generation, 1);
        assert_eq!(
            journal
                .append(
                    "server-command:fixture",
                    generation,
                    1,
                    "stdout",
                    &json!({"text": "buffered"}),
                )
                .expect("append output"),
            AppendResult::Saved
        );
        journal
            .ack("server-command:fixture", 1)
            .expect("ack output");
        assert_eq!(
            journal
                .buffered_by_handle
                .lock()
                .expect("cache lock")
                .get("server-command:fixture"),
            Some(&0)
        );

        journal
            .retire("server-command:fixture", generation, true, true)
            .expect("retire completed handle");

        assert!(
            !journal
                .buffered_by_handle
                .lock()
                .expect("cache lock")
                .contains_key("server-command:fixture")
        );
    }
}
