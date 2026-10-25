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

#[cfg(test)]
mod idle_return_tests {
    use super::*;
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore, StoreError};

    #[tokio::test]
    async fn workers_that_finish_during_idle_reach_the_return_input_as_current_state() {
        let root = tempfile::tempdir().expect("root");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        store.transaction(|tx| {
            let lead = json!({"id":"lead","rootId":"lead","epoch":1,"events":0,"isLead":true,"provider":"claude","status":"idle","autoWake":true});
            let mut worker = json!({"id":"worker","rootId":"lead","epoch":1,"events":0,"parentId":"lead","name":"Worker","provider":"claude","status":"running","autoWake":true});
            AgentRepository::put(tx,&AgentRecord::from_value(lead.clone()).expect("lead"))?;
            AgentRepository::put(tx,&AgentRecord::from_value(worker.clone()).expect("worker"))?;
            let previous = snapshot(tx,&lead).expect("snapshot").expect("team");
            assert_eq!(previous["children"][0]["status"],"running");
            worker["status"] = json!("completed");
            AgentRepository::put(tx,&AgentRecord::from_value(worker).expect("worker"))?;
            tx.execute("INSERT INTO runtime_work(id,record) VALUES('assignment',?1)", [json!({"id":"assignment","rootId":"lead","owner":"worker","status":"review","title":"Finished work","results":[{"summary":"complete"}]}).to_string()])?;
            let current = snapshot(tx,&lead).expect("snapshot").expect("team");
            assert_eq!(current["children"],json!([]));
            assert_eq!(current["tasks"][0]["status"],"review");
            let input: crate::turn_prepare::PreparationSnapshot = serde_json::from_value(json!({
                "provider":"claude","idle_summary_return":true,
                "actor":{"id":"lead","root_id":"lead","epoch":["thread",0],"is_lead":true,"forked_from":null,"prepared_context":null,"delivered_mode":null},
                "root":{"concurrency":32,"mode_revision":0},"delivered_manifests":[],"plan":null,
                "complaints":[],"workers":[],"team_state":current,"delivered_reminders":[],"assets":[],
                "guidance":{"role":"Role"},"artifact_paths":{},"prepared_at":1.0,"operation_id":"return"
            })).expect("input");
            let event = crate::turn_prepare::Event {
                id:"return".into(),kind:"user".into(),text:"continue".into(),created:1.0,
                metadata:crate::turn_prepare::EventMetadata {assets:vec![],accepted_at:None,extra:serde_json::Map::new()},
                preserve_progress:false,preserve_complaint:false,
            };
            let compiled=crate::turn_prepare::compile(&input,&[event]).expect("compile");
            let wire=serde_json::to_string(&compiled.provider_inputs).expect("wire");
            assert!(wire.contains("Studio team state after compaction"));
            assert!(wire.contains("Finished work"));
            assert!(wire.contains("review"));
            assert!(!wire.contains("running"));
            assert_eq!(AgentRepository::get(tx,&AgentId::from("lead"))?.expect("lead").value()["compactions"].as_u64().unwrap_or(0),0);
            Ok::<_,StoreError>(())
        }).await.expect("transaction");
    }
}
