//! The supervisor's independent FULL-sync journal. Never opens the runtime DB.
use crate::{python_json, python_json_with_separators, sha256_hex};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};
use studio_runtime_providers::supervisor_v1::{
    HANDLE_LIMIT, HandleSnapshot, ReceiptSnapshot, Refusal, WriteDecision,
};

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("supervisor journal: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error("supervisor journal contains invalid JSON")]
    Json(#[from] serde_json::Error),
    #[error("supervisor journal lock poisoned")]
    Poisoned,
}
pub type Result<T> = std::result::Result<T, JournalError>;
pub struct SupervisorJournal {
    db: Mutex<Connection>,
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

fn snapshot(db: &Connection, handle: &str) -> Result<HandleSnapshot> {
    let raw = db.query_row("SELECT sequence,acknowledged,rpc_sequence,generation FROM handles WHERE id=?", [handle], |row| {
        Ok(json!({"sequence":row.get::<_,i64>(0)?,"acknowledged":row.get::<_,i64>(1)?,"rpc_sequence":row.get::<_,i64>(2)?,"generation":row.get::<_,i64>(3)?}))
    })?;
    Ok(HandleSnapshot::from_record(raw)?)
}
fn receipt(db: &Connection, handle: &str, operation: &str) -> Result<Option<ReceiptSnapshot>> {
    let raw = db.query_row("SELECT digest,native_id,generation,response,response_sequence FROM operations WHERE handle=? AND operation_id=?", params![handle,operation], |row| {
        Ok(json!({"digest":row.get::<_,String>(0)?,"native_id":row.get::<_,Option<i64>>(1)?,"generation":row.get::<_,Option<i64>>(2)?,"response":row.get::<_,Option<String>>(3)?,"response_sequence":row.get::<_,Option<i64>>(4)?}))
    }).optional()?;
    raw.map(ReceiptSnapshot::from_record)
        .transpose()
        .map_err(Into::into)
}
fn buffered(db: &Connection, handle: &str) -> Result<u64> {
    let value: i64 = db.query_row(
        "SELECT COALESCE(sum(size),0) FROM events WHERE handle=?",
        [handle],
        |row| row.get(0),
    )?;
    u64::try_from(value).map_err(|_| Refusal("Invalid supervisor event size").into())
}
fn stored(db: &Connection, handle: &str) -> Result<Option<StoredHandle>> {
    let row = db.query_row("SELECT h.signature,h.pid,h.init_result,h.closed_at,h.closed_reason,c.start_time,c.pgid,c.pid FROM handles h LEFT JOIN child_identities c ON c.handle=h.id WHERE h.id=?", [handle], |r| {
        Ok((r.get::<_,String>(0)?,r.get::<_,u32>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<f64>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<u32>>(6)?,r.get::<_,Option<u32>>(7)?))
    }).optional()?;
    row.map(|(signature,pid,init,closed_at,closed_reason,start_time,pgid,identity_pid)| {
        if identity_pid.is_some_and(|identity| identity != pid) { return Err(Refusal("Supervisor child identity disagrees with its handle").into()); }
        let recovery_proof = db.query_row("SELECT 1 FROM degraded_handles d JOIN child_identities c ON c.handle=d.handle WHERE d.handle=? AND d.prior_pid=? AND c.pid=d.prior_pid AND c.start_time=d.start_time", params![handle,pid], |_| Ok(true)).optional()?.unwrap_or(false);
        Ok(StoredHandle { id:handle.to_owned(), signature,pid, snapshot:snapshot(db,handle)?, init_result:init.map(|s|serde_json::from_str(&s)).transpose()?, closed_at,closed_reason,start_time,pgid,buffered:buffered(db,handle)?,recovery_proof })
    }).transpose()
}

impl SupervisorJournal {
    pub fn open(root: &Path) -> Result<Self> {
        let db = Connection::open(root.join("supervisor.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=100;",
        )?;
        migrate(&db)?;
        let journal = Self { db: Mutex::new(db) };
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
        for row in statement.query_map([], |r| Ok(json!({"sequence":r.get::<_,i64>(0)?,"kind":r.get::<_,String>(1)?,"payload":r.get::<_,String>(2)?,"generation":r.get::<_,i64>(3)?})))? {
            validate_event(&row?)?;
        }
        Ok(())
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db.lock().map_err(|_| JournalError::Poisoned)
    }
    fn transaction<T>(&self, action: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = action(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn handle(&self, handle: &str) -> Result<Option<StoredHandle>> {
        stored(&*self.lock()?, handle)
    }
    pub fn handles(&self, active: bool) -> Result<Vec<StoredHandle>> {
        let db = self.lock()?;
        let mut statement =
            db.prepare("SELECT id FROM handles WHERE ?=0 OR closed_at IS NULL ORDER BY rowid")?;
        let ids = statement
            .query_map([active], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                stored(&db, &id)?.ok_or_else(|| Refusal("Supervisor handle disappeared").into())
            })
            .collect()
    }
    /// Reserve before spawn. A crash at this point leaves an orphan refusal, never a duplicate child.
    pub fn reserve(&self, handle: &str, signature: &str, now: f64) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO handles(id,signature,pid,created) VALUES (?,?,0,?) ON CONFLICT(id) DO UPDATE SET pid=0,signature=excluded.signature,closed_at=NULL,closed_reason=NULL",
                params![handle, signature, now],
            )?;
            tx.execute("DELETE FROM child_identities WHERE handle=?", [handle])?;
            Ok(())
        })
    }
    pub fn opened(&self, handle: &str, signature: &str, pid: u32, start: &str) -> Result<u64> {
        self.transaction(|tx| {
            tx.execute("UPDATE handles SET pid=?,signature=?,rpc_sequence=0,generation=generation+1,init_result=NULL,closed_at=NULL,closed_reason=NULL WHERE id=?",params![pid,signature,handle])?;
            tx.execute("INSERT INTO child_identities(handle,pid,pgid,start_time,job_name,job_kill_on_close) VALUES (?,?,?,?,'',0) ON CONFLICT(handle) DO UPDATE SET pid=excluded.pid,pgid=excluded.pgid,start_time=excluded.start_time,job_name='',job_kill_on_close=0",params![handle,pid,pid,start])?;
            tx.execute("DELETE FROM degraded_handles WHERE handle=?",[handle])?;
            Ok(snapshot(tx,handle)?.generation())
        })
    }
    /// FULL-sync commit is the only admission point. The caller sends only Accept, once.
    pub fn accept(
        &self,
        handle: &str,
        operation: &str,
        native: Option<u64>,
        message: &Value,
        now: f64,
    ) -> Result<WriteDecision> {
        let mut canonical = message.clone();
        canonical
            .as_object_mut()
            .ok_or(Refusal("Invalid supervisor write message"))?
            .remove("id");
        let digest = sha256_hex(python_json_with_separators(&canonical, ",", ":").as_bytes());
        self.transaction(|tx| {
            let state = snapshot(tx,handle)?;
            let prior = receipt(tx,handle,operation)?;
            let decision = state.write(prior.as_ref(),&digest,native,message,buffered(tx,handle)?)?;
            if let WriteDecision::Accept{remote_id,rpc_sequence,..} = &decision {
                tx.execute("UPDATE handles SET rpc_sequence=? WHERE id=?",params![sql(*rpc_sequence)?,handle])?;
                tx.execute("INSERT INTO operations(handle,operation_id,digest,native_id,accepted,generation) VALUES (?,?,?,?,?,?)",params![handle,operation,digest,remote_id.map(sql).transpose()?,now,sql(state.generation())?])?;
            }
            Ok(decision)
        })
    }
    pub fn operation_status(&self, handle: &str, operation: &str) -> Result<Value> {
        if !operation.starts_with("monitor:") {
            return Err(Refusal("Invalid monitor operation identity").into());
        }
        let db = self.lock()?;
        match receipt(&db, handle, operation)? {
            Some(receipt) => Ok(receipt.status(&snapshot(&db, handle)?)?),
            None => Ok(json!({"accepted":false,"reason":"operation_missing"})),
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
        self.transaction(|tx| {
            let state = snapshot(tx,handle)?;
            if state.generation() != generation { return Ok(AppendResult::Obsolete); }
            if state.sequence() == sequence { return Ok(AppendResult::Saved); }
            if state.sequence()+1 != sequence { return Err(Refusal("Supervisor output sequence changed before its receipt").into()); }
            if buffered(tx,handle)?.saturating_add(raw.len() as u64) > HANDLE_LIMIT { return Ok(AppendResult::Full); }
            tx.execute("UPDATE handles SET sequence=? WHERE id=?",params![sql(sequence)?,handle])?;
            tx.execute("INSERT INTO events(handle,sequence,kind,payload,size,generation) VALUES (?,?,?,?,?,?)",params![handle,sql(sequence)?,kind,raw,sql(raw.len() as u64)?,sql(generation)?])?;
            if kind == "stdout" {
                if payload.get("id").and_then(Value::as_u64) == Some(1) && payload.get("result").is_some() {
                    tx.execute("UPDATE handles SET init_result=? WHERE id=?",params![python_json(&payload["result"]),handle])?;
                }
                if let Some(id) = payload.get("id").and_then(Value::as_i64).filter(|_|payload.get("method").is_none()) {
                    tx.execute("UPDATE operations SET response=?,response_sequence=? WHERE handle=? AND native_id=? AND generation=? AND substr(operation_id,1,8)='monitor:' AND response IS NULL",params![raw,sql(sequence)?,handle,id,sql(generation)?])?;
                }
            }
            Ok(AppendResult::Saved)
        })
    }
    pub fn ack(&self, handle: &str, cursor: u64) -> Result<()> {
        self.transaction(|tx| {
            snapshot(tx, handle)?.cursor(cursor, true)?;
            tx.execute(
                "DELETE FROM events WHERE handle=? AND sequence<=?",
                params![handle, sql(cursor)?],
            )?;
            tx.execute(
                "UPDATE handles SET acknowledged=? WHERE id=?",
                params![sql(cursor)?, handle],
            )?;
            Ok(())
        })
    }
    pub fn events(&self, handle: &str, cursor: u64, one: bool) -> Result<Value> {
        let db = self.lock()?;
        let state = snapshot(&db, handle)?;
        state.cursor(cursor, false)?;
        let mut stmt = db.prepare("SELECT sequence,kind,payload,generation FROM events WHERE handle=? AND sequence>? ORDER BY sequence LIMIT ?")?;
        let events = stmt.query_map(params![handle,sql(cursor)?,if one {1} else {-1}], |r| Ok(json!({"sequence":r.get::<_,i64>(0)?,"kind":r.get::<_,String>(1)?,"payload":r.get::<_,String>(2)?,"generation":r.get::<_,i64>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        for event in &events {
            validate_event(event)?;
        }
        Ok(json!({"events":events,"sequence":state.sequence(),"acknowledged":state.acknowledged()}))
    }
    pub fn retire(
        &self,
        handle: &str,
        generation: u64,
        exited: bool,
        readers_done: bool,
    ) -> Result<()> {
        self.transaction(|tx| {
            snapshot(tx, handle)?.retire(handle, generation, exited, readers_done)?;
            for table in [
                "events",
                "operations",
                "child_identities",
                "degraded_handles",
            ] {
                tx.execute(&format!("DELETE FROM {table} WHERE handle=?"), [handle])?;
            }
            tx.execute("DELETE FROM handles WHERE id=?", [handle])?;
            Ok(())
        })
    }
    pub fn close(&self, handle: &str, reason: &str, now: f64) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",
                params![now, reason, handle],
            )?;
            Ok(())
        })
    }
    pub fn recovered(
        &self,
        row: &StoredHandle,
        outcome: &str,
        detail: &str,
        now: f64,
    ) -> Result<()> {
        self.transaction(|tx| {
            tx.execute("INSERT INTO recovery_events(at,handle,pid,start_time,outcome,detail) VALUES (?,?,?,?,?,?)",params![now,row.id,row.pid,row.start_time.as_deref().unwrap_or(""),outcome,detail])?;
            let closed = matches!(outcome, "already-exited"|"terminated"|"killed"|"identity-mismatch").then_some(now);
            tx.execute("UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?",params![closed,detail,row.id])?;
            if matches!(outcome,"already-exited"|"terminated"|"killed") {
                tx.execute("INSERT OR REPLACE INTO degraded_handles VALUES (?,?,?,?)",params![row.id,row.pid,row.start_time,now])?;
            }
            Ok(())
        })
    }
    pub fn save_recovery(&self, recovery: &Value) -> Result<()> {
        self.transaction(|tx| { tx.execute("INSERT INTO supervisor_state VALUES ('recovery',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[recovery.to_string()])?; Ok(()) })
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

fn validate_event(event: &Value) -> Result<()> {
    if event["sequence"].as_u64().is_none()
        || event["generation"].as_u64().is_none()
        || !matches!(event["kind"].as_str(), Some("stdout" | "stderr" | "exit"))
    {
        return Err(Refusal("Invalid supervisor events record").into());
    }
    let raw = event["payload"]
        .as_str()
        .ok_or(Refusal("Invalid supervisor events payload"))?;
    serde_json::from_str::<Value>(raw)?;
    Ok(())
}

fn migrate(db: &Connection) -> Result<()> {
    // Match Journal's additive legacy migrations, atomically and before readers.
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        db.execute_batch(include_str!("supervisor_schema.sql"))?;
        for (table, column, declaration) in [
            ("handles", "closed_at", "REAL"),
            ("handles", "closed_reason", "TEXT"),
            ("handles", "generation", "INTEGER NOT NULL DEFAULT 0"),
            ("events", "generation", "INTEGER NOT NULL DEFAULT 1"),
            ("child_identities", "job_name", "TEXT NOT NULL DEFAULT ''"),
            (
                "child_identities",
                "job_kill_on_close",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("operations", "generation", "INTEGER"),
            ("operations", "response", "TEXT"),
            ("operations", "response_sequence", "INTEGER"),
        ] {
            let present: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name=?)",
                params![table, column],
                |row| row.get(0),
            )?;
            if !present {
                db.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
                ))?;
                if table == "handles" && column == "generation" {
                    db.execute("UPDATE handles SET generation=1 WHERE pid>0", [])?;
                }
            }
        }
        db.execute_batch("CREATE INDEX IF NOT EXISTS operations_monitor_native ON operations(handle,generation,native_id) WHERE substr(operation_id,1,8)='monitor:'")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            db.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(error) => {
            db.execute_batch("ROLLBACK")?;
            Err(error)
        }
    }
}
