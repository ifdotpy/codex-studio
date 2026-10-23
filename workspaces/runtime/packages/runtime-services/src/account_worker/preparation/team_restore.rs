//! Authoritative post-compaction projection. No event history or model summary
//! decides whether an assignment or command still exists.
use super::*;
use rusqlite::Transaction;

fn rows(
    tx: &Transaction<'_>,
    sql: &str,
    root: &str,
    actor: &str,
    lead: bool,
) -> Result<Vec<Value>, WorkerError> {
    let mut query = tx.prepare_cached(sql)?;
    query
        .query_map(rusqlite::params![root, actor, lead], |r| {
            r.get::<_, String>(0)
        })?
        .map(|row| {
            serde_json::from_str(&row?)
                .map_err(|_| WorkerError::PreparationRefused("invalid team state".into()))
        })
        .collect()
}

pub(super) fn snapshot(tx: &Transaction<'_>, actor: &Value) -> Result<Option<Value>, WorkerError> {
    let root = actor["rootId"]
        .as_str()
        .ok_or_else(|| refusal("missing team root".into()))?;
    let id = actor["id"]
        .as_str()
        .ok_or_else(|| refusal("missing agent id".into()))?;
    let lead = actor["isLead"] == true;
    // Status and autoWake can live in the volatile table. Use the canonical
    // merged reader, never the stale JSON fields of runtime_agents alone.
    let team = studio_runtime_store::AgentIndexedReads::team_records(tx, root)?;
    let mut children = Vec::new();
    let mut has_children = false;
    for (_, raw) in team {
        let worker: Value =
            serde_json::from_str(&raw).map_err(|_| refusal("invalid child record".into()))?;
        if worker["id"] == id
            || (!lead && worker["parentId"] != id)
            || studio_runtime_store::truthy(worker.get("deletedAt"))
        {
            continue;
        }
        has_children = true;
        if matches!(
            worker["status"].as_str(),
            Some("completed" | "failed" | "interrupted")
        ) || (worker["status"] == "paused"
            && !studio_runtime_store::truthy(worker.get("autoWake")))
        {
            continue;
        }
        children.push(json!({"id":worker["id"],"name":worker["name"],"status":worker["status"]}));
    }
    children.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let tasks = rows(tx,
        "SELECT json_object('id',id,'title',json_extract(record,'$.title'),'status',json_extract(record,'$.status'),
            'owner',json_extract(record,'$.owner'),'resultsAwaitingReview',CASE WHEN json_extract(record,'$.status')='review' THEN json_array_length(record,'$.results') ELSE 0 END)
         FROM runtime_work WHERE json_extract(record,'$.rootId')=?1
         AND (?3 OR json_extract(record,'$.owner')=?2 OR json_extract(record,'$.createdBy')=?2)
         AND json_extract(record,'$.status') NOT IN ('accepted','cancelled') ORDER BY id", root, id, lead)?;
    for child in &mut children {
        // A worker can own more than one assignment. Task IDs are authoritative.
        let mut query = tx.prepare_cached("SELECT id FROM runtime_work WHERE json_extract(record,'$.rootId')=?1 AND json_extract(record,'$.owner')=?2 AND json_extract(record,'$.status') NOT IN ('accepted','cancelled') ORDER BY id")?;
        let assignments = query
            .query_map(rusqlite::params![root, child["id"].as_str()], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        child["tasks"] = json!(assignments);
    }
    let mut monitors = Vec::new();
    // Native background commands and managed monitors both survive compaction.
    for table in ["runtime_monitors", "runtime_tasks"] {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !exists {
            continue;
        }
        monitors.extend(rows(tx, &format!(
            "SELECT json_object('id',m.id,'kind','{table}','agent',json_extract(m.record,'$.agent'),
                'status',json_extract(m.record,'$.status'),'command',json_extract(m.record,'$.command'))
             FROM {table} m JOIN runtime_agents a ON a.id=json_extract(m.record,'$.agent')
             WHERE json_extract(a.record,'$.rootId')=?1 AND (?3 OR a.id=?2)
             AND json_extract(m.record,'$.status') IN ('running','starting','approval') ORDER BY m.id"
        ), root, id, lead)?);
    }
    let mut query = tx.prepare_cached("SELECT json_object('id',id,'kind',kind,'status',status,'outputRef','event:' || id) FROM runtime_events WHERE agent=?1 AND kind IN ('child_result','work_review') AND status IN ('pending','reserved') ORDER BY created,id")?;
    let results = query
        .query_map([id], |row| row.get::<_, String>(0))?
        .map(|row| {
            serde_json::from_str::<Value>(&row?).map_err(|_| refusal("invalid result event".into()))
        })
        .collect::<Result<Vec<_>, WorkerError>>()?;
    if !lead && !has_children && monitors.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        json!({"children":children,"monitors":monitors,"tasks":tasks,"pendingResults":results}),
    ))
}
