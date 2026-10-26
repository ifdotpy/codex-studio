//! Source-complete terminal-error conversion. Only persisted turns and exact
//! Canvas notice evidence participate; ambiguous evidence never changes a turn.
use crate::{
    StoreError, StoreResult,
    analytics_capture::{
        AnalyticsCaptureRepository as Repo, CaptureTable, number, repair_checkpoint_cursor, text,
    },
    truthy,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

/// Metadata key owning the original bounded repair checkpoint.
const TERMINAL_ERROR_REPAIR_KEY: &str = "terminalErrorRepairV1";
/// Maximum terminal records retained in memory per conversion page. Unit: rows.
const TERMINAL_REPAIR_PAGE_ROWS: i64 = 64;

#[derive(Clone, Copy)]
enum Source<'a, 'db> {
    Validate(&'a Connection),
    Apply(&'a Transaction<'db>),
}
impl Source<'_, '_> {
    fn db(&self) -> &Connection {
        match self {
            Self::Validate(db) => db,
            Self::Apply(tx) => tx,
        }
    }
    fn write(&self, record: &Value) -> StoreResult<()> {
        match self {
            Self::Validate(_) => CaptureTable::Turns.validate(record),
            Self::Apply(tx) => Repo::write(tx, CaptureTable::Turns, record),
        }
    }
}

pub(super) fn preflight(analytics: &Connection, canvas: &Connection) -> StoreResult<()> {
    repair(Source::Validate(analytics), canvas)
}
pub(super) fn apply(analytics: &Transaction<'_>, canvas: &Connection) -> StoreResult<()> {
    repair(Source::Apply(analytics), canvas)
}

fn repair(source: Source<'_, '_>, canvas: &Connection) -> StoreResult<()> {
    let db = source.db();
    let mut state = match Repo::metadata(db, TERMINAL_ERROR_REPAIR_KEY)? {
        Some(state) => state,
        None => {
            json!({"cursor":0,"end":db.query_row("SELECT COALESCE(MAX(rowid),0) FROM analytics_turns",[],|row|row.get::<_,i64>(0))?,"repaired":0,"ambiguous":0})
        }
    };
    loop {
        let (start, end) = repair_checkpoint_cursor(&state)?;
        if start >= end {
            return Ok(());
        }
        let rows=db.prepare("SELECT rowid,agent,record FROM analytics_turns WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT ?3")?.query_map(params![start,end,TERMINAL_REPAIR_PAGE_ROWS],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<Result<Vec<_>,_>>()?;
        for (rowid, agent, raw) in &rows {
            state["cursor"] = json!(rowid);
            let record: Value = serde_json::from_str(raw)?;
            CaptureTable::Turns.validate(&record)?;
            if !truthy(Some(&record["turnId"]))
                || record["status"] == "failed" && truthy(Some(&record["error"]))
            {
                continue;
            }
            let raw: Option<String> = canvas
                .query_row(
                    "SELECT record FROM runtime_items WHERE id=?1 AND agent=?2",
                    params![
                        format!("{agent}:native-notice:error:{}", text(&record["turnId"])),
                        agent
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(raw) = raw else {
                continue;
            };
            let notice: Value = serde_json::from_str(&raw)?;
            if notice["turnId"] != record["turnId"] || !truthy(Some(&notice["nativeError"])) {
                continue;
            }
            let exact = if truthy(Some(&notice["threadId"])) {
                notice["threadId"] == record["threadId"]
            } else {
                db.query_row("SELECT COUNT(*) FROM analytics_turns WHERE agent=?1 AND json_extract(record,'$.turnId')=?2",params![agent,record["turnId"].as_str()],|row|row.get::<_,i64>(0))?==1
            };
            let field = if exact { "repaired" } else { "ambiguous" };
            let count = state[field]
                .as_u64()
                .and_then(|count| count.checked_add(1))
                .ok_or(StoreError::RecordContract("invalid terminal repair count"))?;
            state[field] = json!(count);
            if exact {
                source.write(&repaired_turn(record, &notice))?;
            }
        }
        if rows.len() < TERMINAL_REPAIR_PAGE_ROWS as usize {
            state["cursor"] = json!(end);
        }
        if let Source::Apply(tx) = source {
            tx.execute("INSERT INTO analytics_meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![TERMINAL_ERROR_REPAIR_KEY,state.to_string()])?;
        }
    }
}

fn repaired_turn(mut record: Value, notice: &Value) -> Value {
    record["status"] = json!("failed");
    record["error"] = notice["nativeError"].clone();
    record["terminalSource"] = json!("liveNoticeRepair");
    if let Some(at) = number(&notice["at"]) {
        record["finishedAt"] = notice["at"].clone();
        if let Some(start) = number(&record["startedAt"]) {
            record["durationMs"] = json!(((at - start) * 1000.).max(0.));
        }
    }
    record
}
