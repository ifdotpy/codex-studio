//! Serialized analytics writes. Rollout files must be read before entering this adapter.
use crate::CachedSql;
use crate::{RuntimeStore, StoreError, StoreResult};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
    types::ValueRef,
};
use serde_json::Value;

/// One existing analytics table; identifiers are never supplied by a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureTable {
    Agents,
    Turns,
    Items,
    Usage,
    History,
}
impl CaptureTable {
    pub fn name(self) -> &'static str {
        match self {
            Self::Agents => "analytics_agents",
            Self::Turns => "analytics_turns",
            Self::Items => "analytics_items",
            Self::Usage => "analytics_usage",
            Self::History => "analytics_history",
        }
    }
    pub fn schema(self) -> &'static str {
        match self {
            Self::Agents => "AnalyticsAgent",
            Self::Turns => "AnalyticsTurn",
            Self::Items => "AnalyticsItem",
            Self::Usage => "AnalyticsUsage",
            Self::History => "AnalyticsHistory",
        }
    }
    pub fn validate(self, value: &Value) -> StoreResult<()> {
        if let Some((pointer, _)) = studio_runtime_schema::first_error(self.schema(), value)
            .map_err(StoreError::SchemaValidation)?
        {
            return Err(StoreError::SchemaValidation(format!(
                "{}{}",
                self.name(),
                pointer
            )));
        }
        Ok(())
    }
}

/// `analytics_meta` key of the last Canvas `runtime_analytics_projection` sequence
/// applied to analytics. Unit: Canvas log sequence number.
pub const PROJECTION_CURSOR_KEY: &str = "canvasProjectionSeq";

pub struct AnalyticsCaptureRepository;
impl AnalyticsCaptureRepository {
    /// Append one analytics projection record (`NotificationPostCommit` JSON) in
    /// the caller's Canvas transaction, so it commits with the provider event.
    pub fn append_projection(canvas: &Connection, record: &str) -> StoreResult<()> {
        canvas.execute_cached(
            "INSERT INTO runtime_analytics_projection(record) VALUES (?1)",
            [record],
        )?;
        Ok(())
    }
    /// Canvas log rows after `after`, in commit order, at most `limit` rows.
    pub fn projection_page(
        canvas: &Connection,
        after: i64,
        limit: usize,
    ) -> StoreResult<Vec<(i64, String)>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = canvas
            .prepare_cached(
                "SELECT seq,record FROM runtime_analytics_projection WHERE seq>?1 ORDER BY seq LIMIT ?2",
            )?
            .query_map(params![after, limit], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    /// Delete Canvas log rows up to `through`, which analytics holds durably.
    pub fn prune_projection(canvas: &Connection, through: i64) -> StoreResult<usize> {
        Ok(canvas.execute_cached(
            "DELETE FROM runtime_analytics_projection WHERE seq<=?1",
            [through],
        )?)
    }
    /// The last Canvas log sequence applied to analytics; 0 for a fresh analytics database.
    pub fn projection_cursor(analytics: &Connection) -> StoreResult<i64> {
        let raw: Option<String> = analytics
            .query_row_cached(
                "SELECT value FROM analytics_meta WHERE key=?1",
                [PROJECTION_CURSOR_KEY],
                |row| row.get(0),
            )
            .optional()?;
        raw.map_or(Ok(0), |raw| {
            raw.parse().map_err(|_| StoreError::InvalidStoredRecord {
                object: "analytics_meta",
                id: PROJECTION_CURSOR_KEY.to_owned(),
            })
        })
    }
    /// Save the projection cursor in the analytics transaction that applied its rows.
    pub fn set_projection_cursor(analytics: &Connection, seq: i64) -> StoreResult<()> {
        analytics.execute_cached(
            "INSERT INTO analytics_meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![PROJECTION_CURSOR_KEY, seq.to_string()],
        )?;
        Ok(())
    }
    pub fn read(db: &Connection, table: CaptureTable, id: &str) -> StoreResult<Option<Value>> {
        // `main.` and StoredText: the stored form decodes on any connection,
        // with or without the analytics_items decoding view.
        let raw: Option<String> = db
            .prepare_cached(&format!(
                "SELECT record FROM main.{} WHERE id=?",
                table.name()
            ))?
            .query_row([id], |r| {
                r.get::<_, crate::stored_text::StoredText>(0)
                    .map(crate::stored_text::StoredText::into_string)
            })
            .optional()?;
        raw.map(|raw| {
            let value = serde_json::from_str(&raw)?;
            table.validate(&value)?;
            if value["id"].as_str() != Some(id) {
                return Err(StoreError::SchemaValidation(format!("{}/id", table.name())));
            }
            Ok(value)
        })
        .transpose()
    }
    pub fn usage_response(
        db: &Connection,
        agent: &str,
        thread: &Value,
        response: &Value,
    ) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db.query_row_cached("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND json_extract(record,'$.responseId')=? LIMIT 1",params![agent,thread.as_str(),response.as_str()],|r|r.get(0)).optional()?;
        Self::usage_raw(raw)
    }
    pub fn usage_prior(
        db: &Connection,
        agent: &str,
        thread: &Value,
        at: f64,
        domain: &str,
    ) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db.query_row_cached("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND at<=? AND json_extract(record,'$.counterDomain')=? ORDER BY at DESC,seq DESC LIMIT 1",params![agent,thread.as_str(),at,domain],|r|r.get(0)).optional()?;
        Self::usage_raw(raw)
    }
    fn usage_raw(raw: Option<String>) -> StoreResult<Option<Value>> {
        raw.map(|r| {
            let v = serde_json::from_str(&r)?;
            CaptureTable::Usage.validate(&v)?;
            Ok(v)
        })
        .transpose()
    }
    pub fn last_limit(db: &Connection, account: &Value) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db
            .query_row_cached(
                "SELECT record FROM analytics_limits WHERE account=? ORDER BY at DESC LIMIT 1",
                [account.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|r| serde_json::from_str(&r).map_err(StoreError::from))
            .transpose()
    }
    pub fn write(db: &Transaction<'_>, table: CaptureTable, value: &Value) -> StoreResult<()> {
        table.validate(value)?;
        let id = value["id"].as_str();
        let agent = value["agentId"].as_str();
        let root = value["rootId"].as_str();
        let at = value["at"].as_f64();
        let raw = value.to_string();
        match table {
            CaptureTable::Agents => {
                db.execute_cached("INSERT INTO analytics_agents VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record WHERE record IS NOT excluded.record",params![id,raw])?;
            }
            CaptureTable::Turns => {
                db.execute_cached("INSERT INTO analytics_turns VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,agent,root,at,raw])?;
            }
            CaptureTable::Items => {
                let record = crate::stored_text_dictionary::value_for_write(
                    db,
                    crate::stored_text_dictionary::ANALYTICS_ITEMS_RECORD,
                    &raw,
                )?;
                // `main.`: Studio connections read the decoding view
                // `temp.analytics_items`, which shadows the table.
                db.execute_cached("INSERT INTO main.analytics_items VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record,at=excluded.at,type=excluded.type,name=excluded.name,is_tool=excluded.is_tool",params![id,agent,root,value["threadId"].as_str(),value["turnId"].as_str(),at,value["type"].as_str(),value["name"].as_str(),value["isTool"].as_bool(),record])?;
            }
            CaptureTable::Usage => {
                db.execute_cached("INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET root=excluded.root,turn=excluded.turn,at=excluded.at,record=excluded.record",params![id,agent,root,value["threadId"].as_str(),value["turnId"].as_str(),at,raw])?;
                if let Some(id) = id {
                    crate::repositories::cost_aggregate::apply_usage_write(db, id, value)?;
                }
            }
            CaptureTable::History => {
                db.execute_cached("INSERT INTO analytics_history VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,record=excluded.record",params![id,value["agent"].as_str(),raw])?;
            }
        }
        Ok(())
    }
    pub fn generation(db: &Transaction<'_>, roots: &[String]) -> StoreResult<()> {
        db.execute_cached("INSERT INTO analytics_meta VALUES ('usageGeneration','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",[])?;
        for root in roots {
            db.execute_cached("INSERT INTO analytics_usage_roots VALUES (?,1) ON CONFLICT(root) DO UPDATE SET generation=generation+1",[root])?;
        }
        Ok(())
    }
}
impl RuntimeStore {
    /// Run one analytics projection transaction on the blocking pool without
    /// Canvas writer admission: it locks only the analytics connection, and the
    /// projector reads Canvas through a reader before it. The commit uses
    /// [`crate::ANALYTICS_PROJECTION_SYNCHRONOUS`], so on success every analytics
    /// commit up to this one is durable and the projected Canvas rows may be pruned.
    pub async fn analytics_projection_transaction<T, E>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let analytics = std::sync::Arc::clone(&self.analytics);
        let checkpoint_requested = std::sync::Arc::clone(&self.wal_checkpoint_requested);
        crate::blocking_pool::spawn_blocking("analytics/projection", move || {
            let mut db = analytics.lock().map_err(|_| StoreError::WriterPoisoned)?;
            // SQLite refuses a synchronous change inside a transaction.
            db.pragma_update(None, "synchronous", crate::ANALYTICS_PROJECTION_SYNCHRONOUS)
                .map_err(StoreError::from)?;
            let outcome: Result<T, E> = (|| {
                let tx = db
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(StoreError::from)?;
                let value = crate::sql_callback(|| operation(&tx))?;
                tx.commit().map_err(StoreError::commit_outcome_unknown)?;
                Ok(value)
            })();
            db.pragma_update(None, "synchronous", crate::ANALYTICS_SYNCHRONOUS)
                .map_err(StoreError::from)?;
            if outcome.is_ok() {
                checkpoint_requested.store(true, std::sync::atomic::Ordering::Release);
            }
            outcome
        })
        .await
        .map_err(|_| E::from(StoreError::ReadTaskFailed))?
    }

    /// Keep the Canvas actor callback occupied for stable main-store reads through commit:
    /// analytics_capture::{capture,capture_batch}, analytics_history::HistoryBatch::commit,
    /// and analytics_history_repair::{migrate_budget,repair_terminal_errors} rely on it.
    /// Serialize analytics with canvas writes. The callback must do SQLite and
    /// bounded pure decisions only. Analytics commit is the durable receipt;
    /// a lost commit response is Unknown and must be reconciled by the caller.
    pub async fn analytics_transaction<T, E>(
        &self,
        operation: impl FnOnce(&Transaction<'_>, &Connection) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let diagnostics_enabled =
            crate::DIAGNOSTICS_ENABLED.load(std::sync::atomic::Ordering::Acquire);
        let waiting = diagnostics_enabled.then(std::time::Instant::now);
        let did_wait = self.writer.available_admissions() == 0;
        let waiter_registration = did_wait.then(|| {
            crate::WriterWaiterRegistration::begin(&self.writer_waiters, &self.writer_waiter_peak)
        });
        let permit = self
            .writer
            .acquire()
            .await
            .map_err(|_| {
                StoreError::CommitActorOutcomeUnknown(tokio_rusqlite::Error::ConnectionClosed)
            })
            .map_err(E::from)?;
        drop(waiter_registration);
        self.writer_admissions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let wait = waiting.map_or(std::time::Duration::ZERO, |start| start.elapsed());
        if diagnostics_enabled {
            crate::emit_diagnostic(crate::StoreDiagnosticsEvent::WriterAdmission {
                wait,
                waited: did_wait,
                waiting_threads: self
                    .writer_waiters
                    .load(std::sync::atomic::Ordering::Relaxed),
                peak_waiters: self
                    .writer_waiter_peak
                    .load(std::sync::atomic::Ordering::Relaxed),
            });
        }

        let submitted = diagnostics_enabled.then(std::time::Instant::now);
        let origin = std::panic::Location::caller();
        let analytics = std::sync::Arc::clone(&self.analytics);
        let checkpoint_requested = std::sync::Arc::clone(&self.wal_checkpoint_requested);
        let transaction_job = move |canvas: &mut Connection| -> Result<Result<T, E>, StoreError> {
            let mut db = analytics.lock().map_err(|_| StoreError::WriterPoisoned)?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(StoreError::from)?;
            match crate::sql_callback(|| operation(&tx, canvas)) {
                Ok(result) => {
                    let commit_started = diagnostics_enabled.then(std::time::Instant::now);
                    tx.commit().map_err(StoreError::commit_outcome_unknown)?;
                    if let Some(started) = commit_started {
                        crate::emit_diagnostic(crate::StoreDiagnosticsEvent::CommitDuration {
                            database: crate::StoreDatabaseKind::Analytics,
                            elapsed: started.elapsed(),
                        });
                    }
                    Ok(Ok(result))
                }
                Err(error) => {
                    drop(tx);
                    Ok(Err(error))
                }
            }
        };
        // Measure on the actor thread so queued jobs are not reported as hold time.
        let operation = move |canvas: &mut Connection| {
            let started =
                submitted.map(|submitted| (submitted.elapsed(), std::time::Instant::now()));
            let outcome = transaction_job(canvas);
            if let Some((queued, started)) = started {
                crate::emit_diagnostic(crate::StoreDiagnosticsEvent::WriterTransaction {
                    wait: wait.saturating_add(queued),
                    hold: started.elapsed(),
                    origin,
                });
            }
            outcome
        };
        let response = self
            .writer
            .call_admitted_with_completion(permit, operation, move |result, _disposition| {
                if result.as_ref().is_ok_and(Result::is_ok) {
                    checkpoint_requested.store(true, std::sync::atomic::Ordering::Release);
                }
            })
            .await
            .map_err(crate::actor_transport_outcome_unknown)
            .map_err(E::from)?;
        response.resolve_with(|_| {}).map_err(E::from)?
    }
}

/// Include the analytics JSON families in the same read-only cutover report.
pub fn check_state_readonly(root: &std::path::Path) -> StoreResult<Vec<crate::StateCheckTable>> {
    if !root.join("analytics.sqlite3").is_file() {
        return Ok(Vec::new());
    }
    let db = Connection::open_with_flags(
        root.join("analytics.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    db.pragma_update(None, "query_only", true)?;
    let mut result = Vec::new();
    for table in [
        CaptureTable::Agents,
        CaptureTable::Items,
        CaptureTable::Turns,
        CaptureTable::Usage,
    ] {
        result.push(check_analytics_table(&db, table)?);
    }
    let (history, importer) = check_history_rows(&db)?;
    result.insert(1, history);
    result.insert(2, importer);
    Ok(result)
}

fn empty_report(table: &'static str) -> crate::StateCheckTable {
    crate::StateCheckTable {
        table,
        row_count: 0,
        rejected_count: 0,
        failures: Vec::new(),
    }
}

fn check_analytics_table(
    db: &Connection,
    table: CaptureTable,
) -> StoreResult<crate::StateCheckTable> {
    let mut report = empty_report(table.name());
    let exists: bool = db.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [table.name()],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(report);
    }
    let mut statement = db.prepare_cached(&format!("SELECT id,record FROM {}", table.name()))?;
    let mut rows = statement.query([])?;
    let mut failures = std::collections::BTreeMap::<(String, String), u64>::new();
    while let Some(row) = rows.next()? {
        report.row_count += 1;
        let (id, raw): (String, String) = (row.get(0)?, row.get(1)?);
        let error = match serde_json::from_str::<Value>(&raw) {
            Ok(v) => {
                let schema = studio_runtime_schema::first_error(table.schema(), &v)
                    .map_err(StoreError::SchemaValidation)?;
                schema.or_else(|| {
                    (v["id"].as_str() != Some(id.as_str())).then(|| ("/id".into(), "rowKey".into()))
                })
            }
            Err(_) => Some((String::new(), "json".into())),
        };
        if let Some(error) = error {
            record_analytics_failure(&mut report, &mut failures, error);
        }
    }
    finish_analytics_report(&mut report, failures);
    Ok(report)
}

fn check_history_rows(
    db: &Connection,
) -> StoreResult<(crate::StateCheckTable, crate::StateCheckTable)> {
    const IMPORTER_ROW_ID: &str = "importer";
    let mut history = empty_report("analytics_history");
    let mut importer = empty_report("analytics_history_importer");
    let exists: bool = db.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_history')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok((history, importer));
    }
    let mut statement = db.prepare_cached("SELECT id,agent,record FROM analytics_history")?;
    let mut rows = statement.query([])?;
    let mut history_failures = std::collections::BTreeMap::<(String, String), u64>::new();
    let mut importer_failures = std::collections::BTreeMap::<(String, String), u64>::new();
    while let Some(row) = rows.next()? {
        let id = row.get_ref(0)?;
        let is_importer =
            matches!(id, ValueRef::Text(bytes) if bytes == IMPORTER_ROW_ID.as_bytes());
        let (report, failures) = if is_importer {
            (&mut importer, &mut importer_failures)
        } else {
            (&mut history, &mut history_failures)
        };
        report.row_count += 1;

        let id = match id {
            ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        };
        let agent = match row.get_ref(1)? {
            ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        };
        let raw = match row.get_ref(2)? {
            ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        };
        let (Some(id), Some(agent), Some(raw)) = (id, agent, raw) else {
            record_analytics_failure(report, failures, (String::new(), "storageType".into()));
            continue;
        };
        if is_importer && !agent.is_empty() {
            record_analytics_failure(report, failures, ("/agent".into(), "rowKey".into()));
            continue;
        }
        let value = match serde_json::from_str::<Value>(raw) {
            Ok(value) => value,
            Err(_) => {
                record_analytics_failure(report, failures, (String::new(), "json".into()));
                continue;
            }
        };
        let schema = if is_importer {
            "AnalyticsHistoryImporterDiagnostic"
        } else {
            CaptureTable::History.schema()
        };
        let error = studio_runtime_schema::first_error(schema, &value)
            .map_err(StoreError::SchemaValidation)?
            .or_else(|| {
                (!is_importer && value["id"].as_str() != Some(id))
                    .then(|| ("/id".into(), "rowKey".into()))
            });
        if let Some(error) = error {
            record_analytics_failure(report, failures, error);
        }
    }
    finish_analytics_report(&mut history, history_failures);
    finish_analytics_report(&mut importer, importer_failures);
    Ok((history, importer))
}

fn record_analytics_failure(
    report: &mut crate::StateCheckTable,
    failures: &mut std::collections::BTreeMap<(String, String), u64>,
    error: (String, String),
) {
    report.rejected_count += 1;
    *failures.entry(error).or_default() += 1;
}

fn finish_analytics_report(
    report: &mut crate::StateCheckTable,
    failures: std::collections::BTreeMap<(String, String), u64>,
) {
    report.failures = failures
        .into_iter()
        .map(|((pointer, keyword), count)| crate::StateCheckFailure {
            pointer,
            keyword,
            count,
        })
        .collect();
}
