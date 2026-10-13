//! Durable tool receipt actions shared by HTTP and future provider callback callers.
pub(crate) mod admission;
pub(crate) mod lifecycle;
use crate::event_write::truthy;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_store::{StoreError, system_reads as records};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolRequestError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("Not implemented: requires {0}")]
    RequiresDependency(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
}
impl From<rusqlite::Error> for ToolRequestError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}

#[derive(Clone, Copy)]
pub enum Action<'a> {
    List,
    Get(&'a str),
    Cancel(&'a str),
}
const TABLE: &str = "runtime_tool_requests";
const PAYLOAD: &str = "payload-store/tool-request-recovery";
const MOVE: &str = "multi-server/move-receipt";

pub fn execute(
    tx: &Transaction<'_>,
    actor_id: &str,
    action: Action<'_>,
    now: f64,
) -> Result<Value, ToolRequestError> {
    if !records::exists(tx, TABLE)? {
        return Err(ToolRequestError::RequiresDependency(
            "store-bootstrap/tool-request-schema",
        ));
    }
    let actor = crate::event_write::checked_agent(tx, actor_id).map_err(|e| match e {
        crate::event_write::EventWriteError::Store(e) => ToolRequestError::Store(e),
        crate::event_write::EventWriteError::InvalidStoredObject { object, id } => {
            ToolRequestError::Store(records::invalid(object, &id))
        }
        _ => ToolRequestError::InvalidRequest("Unknown or deleted managed agent"),
    })?;
    let actor = serde_json::to_value(actor).map_err(StoreError::from)?;
    if matches!(action, Action::List) {
        let mut statement=tx.prepare("SELECT id,record FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=?1 ORDER BY json_extract(record,'$.updated') DESC LIMIT 50")?;
        let mut requests = Vec::new();
        for row in statement.query_map([actor_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, raw) = row?;
            let mut record = records::decode(TABLE, &id, &raw)?;
            refresh(tx, &mut record, &actor, now, false)?;
            strip(&mut record, &["result", "signature", "_payloadBlobs"]);
            requests.push(record);
        }
        return Ok(json!({"requests":requests}));
    }
    let (id, cancel) = match action {
        Action::Get(id) => (id, false),
        Action::Cancel(id) => (id, true),
        Action::List => return Err(ToolRequestError::InvalidRequest("Invalid request action")),
    };
    if !cancel && (id.is_empty() || id.chars().count() > 1000) {
        return Err(ToolRequestError::InvalidRequest("Supply request_id"));
    }
    let Some(mut record) = resolve(tx, actor_id, id)? else {
        if !cancel && truthy(actor.get("movedFrom")) {
            return Err(ToolRequestError::RequiresDependency(MOVE));
        }
        return Ok(
            json!({"id":id,"stage":"not_found","outcome":"unknown","message":"No receipt found. This does not prove that the operation did not execute."}),
        );
    };
    if record["stage"] == "ambiguous" {
        return Ok(record);
    }
    if !cancel
        && matches!(
            text(&record, "tool"),
            "orchestration_move" | "orchestration_teleport"
        )
    {
        return Err(ToolRequestError::RequiresDependency(MOVE));
    }
    refresh(tx, &mut record, &actor, now, true)?;
    if cancel && lifecycle::cancel(&mut record)? {
        record["updated"] = json!(now);
        if record["stage"] == "cancelled" {
            record["finished"] = json!(now);
            record["result"] = json!({"success":false,"contentItems":[{"type":"inputText","text":json!({"requestId":record["id"],"stage":"cancelled","outcome":"not_applied","cancelRequested":true,"message":"Cancelled before execution"}).to_string()}]});
        }
        save(tx, &record, &actor)?;
    }
    if !cancel {
        if !terminal(&record)
            && record.get("operationResult").is_none()
            && let Some(result) = operation_result(tx, text(&record, "id"))?
        {
            record["operationApplied"] = json!(true);
            record["operationResult"] = result;
            record["evidenceSource"] = json!("operation_receipt");
        }
        if let Some(ids) = record["agentIds"].as_array().filter(|ids| !ids.is_empty()) {
            let states=ids.iter().map(|id| {
                let child=records::get(tx,"runtime_agents",id.as_str().unwrap_or_default())?;
                Ok::<Value,StoreError>(match child.filter(|child|child["parentId"]==actor_id) {
                    Some(child)=>json!({"id":id,"name":child["name"],"status":child["status"],"turnId":child["turnId"],"model":child["model"],"deleted":truthy(child.get("deletedAt"))}),
                    None=>json!({"id":id,"absent":true}),
                })
            }).collect::<Result<Vec<_>,_>>()?;
            record["agents"] = json!(states);
            record["registryObservedAt"] = json!(now);
        }
    }
    if let Some(result) = record.get_mut("result") {
        expire(result, now);
    }
    strip(&mut record, &["signature", "_payloadBlobs"]);
    Ok(record)
}

fn resolve(tx: &Transaction<'_>, actor: &str, id: &str) -> Result<Option<Value>, ToolRequestError> {
    if let Some(record) = records::get(tx, TABLE, id)?.filter(|r| r["agent"] == actor) {
        return Ok(Some(record));
    }
    let mut statement = tx.prepare(
        "SELECT request FROM runtime_tool_request_aliases WHERE agent=?1 AND alias=?2 LIMIT 2",
    )?;
    let aliases = statement
        .query_map(params![actor, id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for request in &aliases {
        records::validate(
            "runtime_tool_request_aliases",
            id,
            json!({"agent":actor,"alias":id,"request":request}),
        )?;
    }
    if aliases.len() > 1 {
        return Ok(Some(
            json!({"id":id,"stage":"ambiguous","outcome":"unknown","message":"Use the canonical request id from list; this short id names several requests."}),
        ));
    }
    aliases
        .first()
        .map(|id| records::get(tx, TABLE, id).map(|r| r.filter(|r| r["agent"] == actor)))
        .transpose()
        .map(Option::flatten)
        .map_err(Into::into)
}

fn refresh(
    tx: &Transaction<'_>,
    record: &mut Value,
    actor: &Value,
    now: f64,
    specific: bool,
) -> Result<(), ToolRequestError> {
    if !specific && terminal(record) {
        return Ok(());
    }
    if truthy(record.get("_payloadBlobs")) {
        return Err(ToolRequestError::RequiresDependency(PAYLOAD));
    }
    if terminal(record) {
        return Ok(());
    }
    if record["tool"] == "orchestration_servers"
        && record["outcome"] == "unknown"
        && records::exists(tx, "runtime_server_outbox")?
    {
        let key = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!(
                "studio-cross-server:project-location:{}:{}",
                text(record, "agent"),
                text(record, "id")
            )
            .as_bytes(),
        );
        let raw: Option<String> = tx.query_row(
            "SELECT json_object('id',id,'server',server,'body',body,'state',state,'result',result,'fingerprint',fingerprint,'attempts',attempts,'created_at',created_at,'completed_at',completed_at) FROM runtime_server_outbox WHERE id=?1",
            [key.to_string()],
            |r| r.get(0),
        ).optional()?;
        if let Some(raw) = raw {
            // The storage boundary validates the complete delivery row. This
            // consumer only needs its identity, regardless of envelope action.
            #[derive(serde::Deserialize)]
            struct LocationOutbox {
                id: String,
            }
            let row: LocationOutbox = serde_json::from_value(records::decode(
                "runtime_server_outbox",
                &key.to_string(),
                &raw,
            )?)
            .map_err(StoreError::from)?;
            debug_assert_eq!(row.id, key.to_string());
            return Err(ToolRequestError::RequiresDependency(
                "multi-server/project-location-receipt",
            ));
        }
    }
    let saved = records::get(tx, "runtime_tool_results", text(record, "id"))?;
    if saved
        .as_ref()
        .is_some_and(|r| truthy(r.get("_payloadBlobs")))
    {
        return Err(ToolRequestError::RequiresDependency(PAYLOAD));
    }
    let Some(result) = saved
        .map(|mut result| {
            if specific {
                expire(&mut result, now);
            }
            result
        })
        .or_else(|| record.get("result").filter(|r| r.is_object()).cloned())
    else {
        return Ok(());
    };
    let committed = operation_result(tx, text(record, "id"))?.is_some();
    if lifecycle::settle(record, &result, committed)? {
        record["result"] = result;
        record["updated"] = json!(now);
        if record.get("timing").is_none() {
            record["timing"] = json!({});
        }
        record["timing"]["handlerEndedAt"] = json!(crate::event_write::monotonic_ns());
        if record.get("finished").is_none() {
            record["finished"] = json!(now);
        }
        strip(record, &["error"]);
        if matches!(
            text(record, "tool"),
            "orchestration_spawn" | "orchestration_review"
        ) {
            let mut ids = Vec::new();
            for value in record["result"]["contentItems"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] == "inputText")
                .filter_map(|v| serde_json::from_str::<Value>(v["text"].as_str()?).ok())
            {
                for id in value["agents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v["id"].as_str())
                    .chain(["agentIds", "agent_ids"].into_iter().flat_map(|key| {
                        value[key]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                    }))
                {
                    if !ids.iter().any(|v| v == id) {
                        ids.push(id.to_owned());
                    }
                }
            }
            if !ids.is_empty() {
                record["agentIds"] = json!(ids);
            }
        }
        save(tx, record, actor)?;
    }
    Ok(())
}

fn operation_result(tx: &Transaction<'_>, id: &str) -> Result<Option<Value>, ToolRequestError> {
    records::get(tx, "runtime_operation_receipts", id).map_err(Into::into)
}
fn save(tx: &Transaction<'_>, record: &Value, actor: &Value) -> Result<(), ToolRequestError> {
    let id = text(record, "id");
    records::validate(TABLE, id, record.clone())?;
    reconcile_effect(tx, record, actor)?;
    tx.execute(
        "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
        params![record.to_string(), id],
    )?;
    Ok(())
}
fn reconcile_effect(
    tx: &Transaction<'_>,
    record: &Value,
    actor: &Value,
) -> Result<(), ToolRequestError> {
    if !records::exists(tx, "runtime_execution_effects")? {
        return Ok(());
    }
    let key = format!("tool_requests:{}", text(record, "id"));
    let old = records::get(tx, "runtime_execution_effects", &key)?;
    let mut run = old
        .as_ref()
        .and_then(|v| v["runId"].as_str())
        .map(|id| records::get(tx, "runtime_execution_runs", id))
        .transpose()?
        .flatten();
    if run.is_none() {
        let turn = record
            .get("turnId")
            .filter(|v| truthy(Some(v)))
            .or_else(|| actor.get("turnId"));
        let account = actor["accountKey"].as_str().unwrap_or("default");
        let mut statement = tx.prepare(
            "SELECT id,record FROM runtime_execution_runs WHERE agent=?1 AND account=?2
             AND ((?6 AND thread IS ?3 AND turn=?4)
               OR (NOT ?6 AND epoch=?5 AND turn IS NULL AND ?4 IS NULL))
             ORDER BY created DESC LIMIT CASE WHEN ?6 THEN 2 ELSE 1 END",
        )?;
        let rows = statement
            .query_map(
                params![
                    actor["id"].as_str(),
                    account,
                    actor["threadId"].as_str(),
                    turn.and_then(Value::as_str),
                    actor["epoch"].as_i64().unwrap_or(0),
                    truthy(turn)
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() == 1 {
            let candidate = records::decode("runtime_execution_runs", &rows[0].0, &rows[0].1)?;
            if truthy(turn)
                || !matches!(
                    text(&candidate, "status"),
                    "completed" | "failed" | "interrupted" | "cancelled"
                )
            {
                run = Some(candidate);
            }
        }
    }
    let Some(run) = run else {
        return Ok(());
    };
    let run_id = old.as_ref().map(|v| &v["runId"]).unwrap_or(&run["id"]);
    let value = json!({"id":key,"runId":run_id,"kind":"tool_requests","referenceId":record["id"],"attemptId":old.as_ref().map(|v|&v["attemptId"]).unwrap_or(&run["latestAttemptId"]),"requestId":record.get("request_id").filter(|v|truthy(Some(v))).or_else(||record.get("callId")),"status":record["stage"],"outcome":record["outcome"],"resultReference":if truthy(record.get("finished")){record["id"].clone()}else{Value::Null}});
    records::validate("runtime_execution_effects", &key, value.clone())?;
    if old.as_ref() != Some(&value) {
        tx.execute("INSERT INTO runtime_execution_effects(id,run,kind,reference,record) VALUES(?1,?2,'tool_requests',?3,?4) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![key,run_id.as_str(),record["id"].as_str(),value.to_string()])?;
    }
    Ok(())
}
fn terminal(record: &Value) -> bool {
    matches!(text(record, "outcome"), "applied" | "not_applied")
}
fn strip(value: &mut Value, keys: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        for key in keys {
            object.remove(*key);
        }
    }
}
fn text<'a>(record: &'a Value, key: &str) -> &'a str {
    record.get(key).and_then(Value::as_str).unwrap_or_default()
}
pub(crate) fn expire(result: &mut Value, now: f64) {
    if result["serverOutputExpiresAt"]
        .as_f64()
        .is_some_and(|expiry| now >= expiry)
    {
        *result = json!({"success":true,"contentItems":[{"type":"inputText","text":json!({"outcome":"unknown","expired":true,"detail":"The command output has expired. Do not run the command again."}).to_string()}]});
    }
}
