//! Serialized analytics writes. Rollout files must be read before entering this adapter.
use crate::{RuntimeStore, StoreError, StoreResult};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
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

pub struct AnalyticsCaptureRepository;
impl AnalyticsCaptureRepository {
    pub fn read(db: &Connection, table: CaptureTable, id: &str) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db
            .query_row(
                &format!("SELECT record FROM {} WHERE id=?", table.name()),
                [id],
                |r| r.get(0),
            )
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
    /// Legacy notices without totalTokens hashed Python object insertion order.
    /// Match their actual counters before choosing a new canonical identity.
    pub fn usage_without_total(
        db: &Connection,
        agent: &str,
        thread: &Value,
        turn: &Value,
        total: &Value,
    ) -> StoreResult<Option<Value>> {
        let mut statement = db.prepare("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND turn IS ? AND json_extract(record,'$.total.totalTokens') IS NULL AND json_extract(record,'$.usageSource') IS NOT 'claudeResponse' ORDER BY seq LIMIT 129")?;
        let rows = statement
            .query_map(params![agent, thread.as_str(), turn.as_str()], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > 128 {
            return Err(StoreError::SchemaValidation(
                "analytics_usage/legacyCounterLookupLimit".into(),
            ));
        }
        for raw in rows {
            let value: Value = serde_json::from_str(&raw)?;
            CaptureTable::Usage.validate(&value)?;
            if &value["total"] == total {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
    pub fn usage_response(
        db: &Connection,
        agent: &str,
        thread: &Value,
        response: &Value,
    ) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db.query_row("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND json_extract(record,'$.responseId')=? LIMIT 1",params![agent,thread.as_str(),response.as_str()],|r|r.get(0)).optional()?;
        Self::usage_raw(raw)
    }
    pub fn usage_prior(
        db: &Connection,
        agent: &str,
        thread: &Value,
        at: f64,
        domain: &str,
    ) -> StoreResult<Option<Value>> {
        let raw: Option<String> = db.query_row("SELECT record FROM analytics_usage WHERE agent=? AND thread IS ? AND at<=? AND json_extract(record,'$.counterDomain')=? ORDER BY at DESC,seq DESC LIMIT 1",params![agent,thread.as_str(),at,domain],|r|r.get(0)).optional()?;
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
            .query_row(
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
                db.execute("INSERT INTO analytics_agents VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record WHERE record IS NOT excluded.record",params![id,raw])?;
            }
            CaptureTable::Turns => {
                db.execute("INSERT INTO analytics_turns VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,agent,root,at,raw])?;
            }
            CaptureTable::Items => {
                db.execute("INSERT INTO analytics_items VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record,at=excluded.at,type=excluded.type,name=excluded.name,is_tool=excluded.is_tool",params![id,agent,root,value["threadId"].as_str(),value["turnId"].as_str(),at,value["type"].as_str(),value["name"].as_str(),value["isTool"].as_bool(),raw])?;
            }
            CaptureTable::Usage => {
                db.execute("INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET root=excluded.root,turn=excluded.turn,at=excluded.at,record=excluded.record",params![id,agent,root,value["threadId"].as_str(),value["turnId"].as_str(),at,raw])?;
            }
            CaptureTable::History => {
                db.execute("INSERT INTO analytics_history VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,record=excluded.record",params![id,value["agent"].as_str(),raw])?;
            }
        }
        Ok(())
    }
    pub fn generation(db: &Transaction<'_>, roots: &[String]) -> StoreResult<()> {
        db.execute("INSERT INTO analytics_meta VALUES ('usageGeneration','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",[])?;
        for root in roots {
            db.execute("INSERT INTO analytics_usage_roots VALUES (?,1) ON CONFLICT(root) DO UPDATE SET generation=generation+1",[root])?;
        }
        Ok(())
    }
}
impl RuntimeStore {
    /// Serialize analytics with canvas writes. The callback must do SQLite and
    /// bounded pure decisions only. Analytics commit is the durable receipt;
    /// a lost commit response is Unknown and must be reconciled by the caller.
    pub fn analytics_transaction<T, E>(
        &self,
        operation: impl FnOnce(&Transaction<'_>, &Connection) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<StoreError>,
    {
        let writer = self.writer.lock().map_err(|_| StoreError::WriterPoisoned)?;
        let mut db = Connection::open_with_flags(
            self.root.join("analytics.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(StoreError::from)?;
        db.busy_timeout(std::time::Duration::from_secs(15))
            .map_err(StoreError::from)?;
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(StoreError::from)?;
        db.pragma_update(None, "synchronous", "FULL")
            .map_err(StoreError::from)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::from)?;
        let result = operation(&tx, &writer)?;
        tx.commit().map_err(StoreError::CommitOutcomeUnknown)?;
        Ok(result)
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
        CaptureTable::History,
        CaptureTable::Items,
        CaptureTable::Turns,
        CaptureTable::Usage,
    ] {
        let mut report = crate::StateCheckTable {
            table: table.name(),
            row_count: 0,
            rejected_count: 0,
            failures: Vec::new(),
        };
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [table.name()],
            |r| r.get(0),
        )?;
        if exists {
            let mut statement = db.prepare(&format!("SELECT id,record FROM {}", table.name()))?;
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
                            (v["id"].as_str() != Some(id.as_str()))
                                .then(|| ("/id".into(), "rowKey".into()))
                        })
                    }
                    Err(_) => Some((String::new(), "json".into())),
                };
                if let Some(error) = error {
                    report.rejected_count += 1;
                    *failures.entry(error).or_default() += 1;
                }
            }
            report.failures = failures
                .into_iter()
                .map(|((pointer, keyword), count)| crate::StateCheckFailure {
                    pointer,
                    keyword,
                    count,
                })
                .collect();
        }
        result.push(report);
    }
    Ok(result)
}
