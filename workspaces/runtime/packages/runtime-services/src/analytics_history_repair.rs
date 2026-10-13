//! Bounded legacy analytics repairs. Budget receipts are obtained outside the
//! analytics transaction; a compare-and-swap acknowledges their durable page.
use crate::analytics_capture::{CaptureError, check_identity, number, text, truthy};
use crate::analytics_history::{HistoryBudgetSink, HistoryError};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository as Repo, CaptureTable};
use studio_runtime_store::{AnalyticsRepository, RuntimeStore};
const PAGE_SIZE: i64 = 64;
fn metadata(db: &Connection, key: &str) -> Result<Option<Value>, HistoryError> {
    let raw: Option<String> = db
        .query_row("SELECT value FROM analytics_meta WHERE key=?", [key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(studio_runtime_store::StoreError::from)?;
    raw.map(|v| {
        serde_json::from_str(&v)
            .map_err(studio_runtime_store::StoreError::from)
            .map_err(HistoryError::from)
    })
    .transpose()
}
fn cursor(state: &Value) -> Result<(i64, i64), HistoryError> {
    let value = |k| {
        state[k]
            .as_i64()
            .filter(|n| *n >= 0)
            .ok_or(CaptureError::Invalid("/history/migration/cursor"))
    };
    Ok((value("cursor")?, value("end")?))
}

pub fn migrate_budget(
    store: &RuntimeStore,
    agent: &Value,
    budget: &dyn HistoryBudgetSink,
) -> Result<bool, HistoryError> {
    let key = format!("budgetUsageMigrationV1:{}", text(&agent["id"]));
    let db = AnalyticsRepository::new(store.root())
        .open_read_only()
        .map_err(|_| HistoryError::Read)?;
    let previous = metadata(&db, &key)?;
    let mut state = match &previous {
        Some(v) => v.clone(),
        None => {
            json!({"cursor":0,"end":db.query_row("SELECT COALESCE(MAX(seq),0) FROM analytics_usage",[],|r|r.get::<_,i64>(0)).map_err(studio_runtime_store::StoreError::from)?})
        }
    };
    let (start, end) = cursor(&state)?;
    if start >= end {
        return Ok(false);
    }
    let records=db.prepare("SELECT seq,record FROM analytics_usage WHERE agent=? AND seq>? AND seq<=? ORDER BY seq LIMIT ?").map_err(studio_runtime_store::StoreError::from)?.query_map(params![agent["id"].as_str(),start,end,PAGE_SIZE],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).map_err(studio_runtime_store::StoreError::from)?.collect::<Result<Vec<_>,_>>().map_err(studio_runtime_store::StoreError::from)?;
    drop(db);
    for (seq, raw) in &records {
        let record: Value =
            serde_json::from_str(raw).map_err(studio_runtime_store::StoreError::from)?;
        CaptureTable::Usage.validate(&record)?;
        let params = json!({"threadId":record["threadId"],"turnId":record["turnId"],"responseId":record["responseId"],"rawTokenUsageRecord":record["rawTokenUsageRecord"],"requestUsage":record["requestUsage"],"tokenUsage":{"total":record["total"],"last":record["last"]},"_analyticsTimestampSource":record["timestampSource"]});
        budget.capture(
            agent,
            &params,
            number(&record["at"]).ok_or(CaptureError::Invalid("/analytics_usage/at"))?,
        )?;
        state["cursor"] = json!(seq);
    }
    if records.len() < PAGE_SIZE as usize {
        state["cursor"] = json!(end)
    }
    store.analytics_transaction(|tx, canvas| {
        check_identity(canvas, agent)?;
        if metadata(tx, &key)? != previous {
            return Err(HistoryError::StaleCheckpoint);
        }
        tx.execute(
            "INSERT INTO analytics_meta VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, state.to_string()],
        ).map_err(studio_runtime_store::StoreError::from)?;
        Ok(())
    })?;
    Ok(!records.is_empty())
}
/// Repair exactly identified terminal errors; ambiguous old threadless notices
/// advance the repair cursor but never replace a turn's error evidence.
pub fn repair_terminal_errors(store: &RuntimeStore) -> Result<(), HistoryError> {
    store.analytics_transaction(|tx, canvas| {
        let key = "terminalErrorRepairV1";
        let mut state = match metadata(tx, key)? {
            Some(v) => v,
            None => {
                let end = tx.query_row("SELECT COALESCE(MAX(rowid),0) FROM analytics_turns", [], |r| r.get::<_, i64>(0))
                    .map_err(studio_runtime_store::StoreError::from)?;
                json!({"cursor":0,"end":end,"repaired":0,"ambiguous":0})
            }
        };
        let (start, end) = cursor(&state)?;
        if start >= end { return Ok(()); }
        let mut statement = tx.prepare(
            "SELECT rowid,agent,record FROM analytics_turns WHERE rowid>? AND rowid<=? ORDER BY rowid LIMIT ?"
        ).map_err(studio_runtime_store::StoreError::from)?;
        let rows = statement.query_map(params![start, end, PAGE_SIZE], |r| {
            Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))
        }).map_err(studio_runtime_store::StoreError::from)?
            .collect::<Result<Vec<_>,_>>().map_err(studio_runtime_store::StoreError::from)?;
        for (rowid, agent, raw) in &rows {
            state["cursor"] = json!(rowid);
            let record: Value = serde_json::from_str(raw).map_err(studio_runtime_store::StoreError::from)?;
            CaptureTable::Turns.validate(&record)?;
            if !truthy(&record["turnId"]) || record["status"] == "failed" && truthy(&record["error"]) { continue; }
            let raw: Option<String> = canvas.query_row(
                "SELECT record FROM runtime_items WHERE id=? AND agent=?",
                params![format!("{agent}:native-notice:error:{}", text(&record["turnId"])), agent], |r| r.get(0)
            ).optional().map_err(studio_runtime_store::StoreError::from)?;
            let Some(raw) = raw else { continue; };
            let notice: Value = serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
            if notice["turnId"] != record["turnId"] || !truthy(&notice["nativeError"]) { continue; }
            let exact = if truthy(&notice["threadId"]) {
                notice["threadId"] == record["threadId"]
            } else {
                tx.query_row(
                    "SELECT COUNT(*) FROM analytics_turns WHERE agent=? AND json_extract(record,'$.turnId')=?",
                    params![agent, record["turnId"].as_str()], |r| r.get::<_,i64>(0)
                ).map_err(studio_runtime_store::StoreError::from)? == 1
            };
            let field = if exact { "repaired" } else { "ambiguous" };
            let count = state[field].as_u64().ok_or(CaptureError::Invalid("/history/repair/count"))?;
            state[field] = json!(count.checked_add(1).ok_or(CaptureError::Invalid("/history/repair/count"))?);
            if exact {
                Repo::write(tx, CaptureTable::Turns, &repaired_turn(record, &notice))?;
            }
        }
        if rows.len() < PAGE_SIZE as usize { state["cursor"] = json!(end); }
        tx.execute(
            "INSERT INTO analytics_meta VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, state.to_string()]
        ).map_err(studio_runtime_store::StoreError::from)?;
        Ok(())
    })
}
/// The caller supplies the validated turn and exact persisted native notice.
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
