//! Committed managed transcript projection shared by HTTP history and sync pull.
use crate::{event_write::truthy, transcript::TranscriptError};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use studio_runtime_schema::DeliveryMoment;
use studio_runtime_store::python_json_utf8 as python_json;
use studio_runtime_store::{
    AnalyticsRepository, RuntimeStore, TranscriptRepository, system_reads as records,
};

pub const PAGE_SIZE: i64 = 120;
const MAX_PAGE_SIZE: i64 = 500;

#[derive(Clone, Debug, Default)]
pub struct PageRequest {
    pub before: Option<String>,
    pub around: Option<String>,
    pub after: Option<String>,
    pub limit: Option<i64>,
}

pub async fn page(
    store: &RuntimeStore,
    agent: String,
    request: PageRequest,
) -> Result<Value, TranscriptError> {
    page_at(store, agent, request, observed_at()?).await
}

/// Use one server-owned observation time for all reasoning intervals in a page.
pub async fn page_at(
    store: &RuntimeStore,
    agent: String,
    request: PageRequest,
    now: f64,
) -> Result<Value, TranscriptError> {
    page_at_snapshot(store, agent, request, now)
        .await
        .map(|(page, _)| page)
}

/// Project a page and its transcript revision from the same SQLite snapshot.
/// The revision lets callers persist a derived view without pairing it with a
/// revision observed before the projection began. Tool results kept in
/// payload files are resolved after the snapshot is released.
pub async fn page_at_snapshot(
    store: &RuntimeStore,
    agent: String,
    request: PageRequest,
    now: f64,
) -> Result<(Value, i64), TranscriptError> {
    if !now.is_finite() || now < 0.0 {
        return Err(TranscriptError::Store);
    }
    let state_dir = store.root().to_path_buf();
    let analytics_root = state_dir.clone();
    let (mut value, revision) = store
        .read_async("runtime-services/transcript_runtime", move |transaction| {
            let analytics = AnalyticsRepository::new(&analytics_root)
                .open_read_only()
                .map_err(|_| TranscriptError::Store)?;
            let value = project(transaction, &analytics, &agent, &request, now)?;
            let revision = TranscriptRepository::revision(transaction, &agent)?;
            Ok::<_, TranscriptError>((value, revision))
        })
        .await?;
    if !has_pending_tool_results(&value) {
        return Ok((value, revision));
    }
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/transcript_runtime",
        move || {
            project_tool_results(&state_dir, &mut value, now)?;
            Ok((value, revision))
        },
    )
    .await
    .map_err(|_| TranscriptError::Store)?
}

/// Project the complete sync payload from the writer transaction that will
/// persist it. This keeps ordinary transcript updates from pairing a page
/// with a revision observed in a different SQLite snapshot.
pub(crate) fn page_in_transaction(
    store: &RuntimeStore,
    transaction: &rusqlite::Transaction<'_>,
    agent: &str,
    request: &PageRequest,
    now: f64,
) -> Result<Value, TranscriptError> {
    if !now.is_finite() || now < 0.0 {
        return Err(TranscriptError::Store);
    }
    let analytics = AnalyticsRepository::new(store.root())
        .open_read_only()
        .map_err(|_| TranscriptError::Store)?;
    let mut value = project(transaction, &analytics, agent, request, now)?;
    project_tool_results(store.root(), &mut value, now)?;
    Ok(value)
}

// Reasoning retains only row identity and timing; payloads move into items.
struct PageRowMetadata {
    id: String,
    created: f64,
    turn_id: Option<String>,
}

fn rows(
    connection: &Connection,
    agent: &str,
    request: &PageRequest,
) -> Result<Vec<(PageRowMetadata, Value)>, TranscriptError> {
    let before = request.before.as_deref().filter(|v| !v.is_empty());
    let around = request.around.as_deref().filter(|v| !v.is_empty());
    let after = request.after.as_deref().filter(|v| !v.is_empty());
    if [before, around, after].into_iter().flatten().count() > 1 {
        return Err(TranscriptError::HistoryCursor);
    }
    let limit = request.limit.unwrap_or(PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let mut anchor = match before.or(around).or(after) {
        Some(identity) => {
            let direct = connection.query_row(
                "SELECT created,id FROM runtime_items WHERE id=?1 AND agent=?2 AND json_extract(record,'$.afterRestore') IS NULL",
                params![identity, agent], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?)),
            ).optional()?;
            Some(match direct {
                Some(anchor) => anchor,
                None => connection.query_row(
                    "SELECT i.created,i.id FROM runtime_items i JOIN json_each(i.record,'$.inputs') input
                     WHERE i.agent=?1 AND json_extract(i.record,'$.afterRestore') IS NULL
                     AND CASE WHEN json_type(input.value,'$.id')='text'
                         THEN ?1 || ':' || json_extract(input.value,'$.id')
                         ELSE i.id || ':' || input.key END=?2 ORDER BY i.created,i.id LIMIT 1",
                    params![agent, identity], |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional()?.ok_or(TranscriptError::UnknownMessage)?,
            })
        }
        None => None,
    };
    if around.is_some()
        && limit / 2 > 0
        && let Some((created, id)) = &anchor
    {
        let newer = connection.query_row("SELECT created,id FROM (SELECT created,id FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.afterRestore') IS NULL AND (created,id)>(?2,?3) ORDER BY created,id LIMIT ?4) ORDER BY created DESC,id DESC LIMIT 1", params![agent, created, id, limit/2], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if newer.is_some() {
            anchor = newer;
        }
    }
    let relation = if after.is_some() {
        ">"
    } else if around.is_some() {
        "<="
    } else {
        "<"
    };
    let order = if after.is_some() {
        "created,id"
    } else {
        "created DESC,id DESC"
    };
    let filter = if anchor.is_some() {
        format!(" AND (created,id){relation}(?3,?4)")
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT id,created,record FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.afterRestore') IS NULL{filter} ORDER BY {order} LIMIT ?2"
    );
    let mut statement = connection.prepare_cached(&sql)?;
    let mut values = vec![rusqlite::types::Value::Text(agent.to_owned()), limit.into()];
    if let Some((at, id)) = anchor {
        values.extend([at.into(), id.into()]);
    }
    let mut rows = statement
        .query_map(rusqlite::params_from_iter(values), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .map(|row| {
            let (id, created, raw) = row?;
            let record = records::decode("runtime_items", &id, &raw)?;
            let turn_id = record["turnId"].as_str().map(str::to_owned);
            Ok((
                PageRowMetadata {
                    id,
                    created,
                    turn_id,
                },
                record,
            ))
        })
        .collect::<Result<Vec<_>, TranscriptError>>()?;
    if after.is_none() {
        rows.reverse();
    }
    Ok(rows)
}

pub(crate) fn project(
    connection: &Connection,
    analytics: &Connection,
    agent: &str,
    request: &PageRequest,
    now: f64,
) -> Result<Value, TranscriptError> {
    let actor =
        TranscriptRepository::agent(connection, agent)?.ok_or(TranscriptError::UnknownMessage)?;
    if truthy(actor.get("deletedAt")) {
        return Err(TranscriptError::DeletedConversation);
    }
    let (rows, mut items): (Vec<_>, Vec<_>) = rows(connection, agent, request)?.into_iter().unzip();
    let has_cursor = [&request.before, &request.around, &request.after]
        .into_iter()
        .any(|v| v.as_ref().is_some_and(|v| !v.is_empty()));
    let mut next = None;
    let mut next_after = None;
    if let Some(row) = rows.first() && connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.afterRestore') IS NULL AND (created,id)<(?2,?3))",params![agent,row.created,row.id],|r|r.get::<_,bool>(0))? { next=Some(row.id.clone()); }
    if has_cursor && let Some(row)=rows.last() && connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.afterRestore') IS NULL AND (created,id)>(?2,?3))",params![agent,row.created,row.id],|r|r.get::<_,bool>(0))? { next_after=Some(row.id.clone()); }
    receipts(connection, agent, &mut items, !has_cursor)?;
    let live = matches!(
        actor["status"].as_str(),
        Some("running" | "starting" | "approval")
    ) && truthy(actor.get("autoWake"));
    let failed_turns = items
        .iter()
        .filter(|i| i["turnStatus"] == "failed")
        .filter_map(|i| {
            i["turnId"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut turn_errors = BTreeMap::new();
    if let Some(thread) = actor["threadId"].as_str() {
        for turn in &failed_turns {
            let identity = format!("{agent}:{thread}:{turn}");
            let result =
                studio_runtime_store::analytics::turn_errors(analytics, vec![identity.clone()])?;
            for record in result["turns"].as_array().into_iter().flatten() {
                if record["agentId"].as_str() != Some(agent)
                    || record["turnId"].as_str() != Some(turn)
                    || record["status"].as_str().is_none()
                    || (!record["threadId"].is_null()
                        && record["threadId"].as_str() != Some(thread))
                {
                    return Err(invalid("analytics_turns", &identity));
                }
                if record["status"] == "failed" && truthy(record.get("error")) {
                    turn_errors.insert(turn.clone(), record["error"].clone());
                }
            }
        }
    }
    for item in &mut items {
        if let Some(turn) = item["turnId"].as_str()
            && !turn.is_empty()
        {
            let ended = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_completed_turns WHERE id=?1)",
                [format!("{agent}:{turn}")],
                |r| r.get::<_, bool>(0),
            )?;
            if ended && item.get("turnStatus").is_none() {
                item["turnStatus"] = json!("ended");
            }
        }
        if let Some(turn) = item["turnId"]
            .as_str()
            .filter(|v| failed_turns.contains(*v))
        {
            item["turnError"] = turn_errors.get(turn).cloned().unwrap_or(Value::Null);
            item["turnErrorResolved"] = json!(true);
        }
        // Inline receipts are projected within the snapshot. Local blobs are
        // resolved later by page_at, after this transaction has closed.
        tool_result(connection, None, &actor, item, now)?;
        if !live || item["turnId"] != actor["turnId"] {
            if item["streaming"] == true {
                item["streaming"] = json!(false);
            }
            if item["toolStatus"] == "running" {
                item["toolStatus"] = records::get(
                    connection,
                    "runtime_tasks",
                    item["id"].as_str().unwrap_or_default(),
                )?
                .map(|r| r["status"].clone())
                .unwrap_or(json!("interrupted"));
            }
        }
    }
    let items = reasoning(analytics, connection, &actor, &rows, items, now)?;
    let view = [
        "id",
        "status",
        "activity",
        "inFlight",
        "contextUsage",
        "compactions",
        "compactionsObservedOnly",
    ]
    .into_iter()
    .map(|key| {
        (
            key.to_owned(),
            actor.get(key).cloned().unwrap_or(Value::Null),
        )
    })
    .collect::<serde_json::Map<_, _>>();
    let text = |key: &str| actor.get(key).and_then(Value::as_str).unwrap_or("None");
    Ok(
        json!({"items":items,"truncated":next.is_some(),"nextCursor":next,"nextAfterCursor":next_after,"historyVersion":format!("{}:{}",text("threadId"),text("restoredCheckpoint")),"unavailable":null,"agent":view}),
    )
}

type ItemLocation = (usize, Option<usize>);
fn represented(items: &[Value], agent: &str) -> BTreeMap<String, ItemLocation> {
    let mut result = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        let id = item["id"].as_str().unwrap_or_default();
        let event = id.strip_prefix(&format!("{agent}:")).unwrap_or(id);
        if let Some(inputs) = item["inputs"].as_array().filter(|v| !v.is_empty()) {
            for (j, input) in inputs.iter().enumerate() {
                if input["kind"] == "user"
                    && let Some(id) = input["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .or_else(|| (j == 0).then_some(event))
                {
                    result.insert(id.to_owned(), (i, Some(j)));
                }
            }
        } else if item["role"] == "user" {
            result.insert(event.to_owned(), (i, None));
        }
    }
    result
}
fn receipts(
    connection: &Connection,
    agent: &str,
    items: &mut Vec<Value>,
    outstanding: bool,
) -> Result<(), TranscriptError> {
    let mut locations = represented(items, agent);
    if outstanding {
        let lower = items.first().and_then(|i| i["at"].as_f64()).unwrap_or(0.0);
        let mut statement=connection.prepare_cached("SELECT id,text,created,status FROM runtime_events WHERE agent=?1 AND kind='user' AND (status IN ('pending','reserved','dispatching','uncertain') OR (status='failed' AND created>=?2)) ORDER BY created")?;
        let events = statement
            .query_map(params![agent, lower], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, text, created, status) in events {
            if locations.contains_key(&id) {
                continue;
            }
            let meta = records::get(connection, "runtime_event_meta", &id)?.unwrap_or(json!({}));
            let identity = meta["transcriptItemId"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{agent}:{id}"));
            let exists = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_items WHERE id=?1 AND agent=?2)",
                params![identity, agent],
                |r| r.get::<_, bool>(0),
            )?;
            if exists
                || (status == "uncertain"
                    && !truthy(meta.get("transcriptItemId"))
                    && !items.is_empty()
                    && created < lower)
            {
                continue;
            }
            let mut assets = Vec::new();
            for asset in meta["assets"].as_array().into_iter().flatten() {
                let asset_id = asset
                    .as_str()
                    .ok_or_else(|| invalid("runtime_event_meta", &id))?;
                let mut asset = records::get(connection, "runtime_assets", asset_id)?
                    .ok_or_else(|| invalid("runtime_assets", asset_id))?;
                let owner_id = asset["agent"]
                    .as_str()
                    .ok_or_else(|| invalid("runtime_assets", asset_id))?;
                let owner = TranscriptRepository::agent(connection, owner_id)?
                    .ok_or(TranscriptError::UnknownAssetOwner)?;
                if truthy(owner.get("deletedAt")) {
                    return Err(TranscriptError::DeletedAssetOwner);
                }
                asset
                    .as_object_mut()
                    .ok_or_else(|| invalid("runtime_assets", asset_id))?
                    .remove("path");
                assets.push(asset);
            }
            locations.insert(id.clone(), (items.len(), None));
            items.push(json!({"id":format!("{agent}:{id}"),"role":"user","title":"You","text":text,"at":created,"materialized":false,"assets":assets}));
        }
    }
    for (id, (i, j)) in locations {
        let entry = match j {
            Some(j) => &mut items[i]["inputs"][j],
            None => &mut items[i],
        };
        entry["requestedDelivery"] = json!(DeliveryMoment::CurrentTurn);
        let event = connection
            .query_row(
                "SELECT status,error FROM runtime_events WHERE id=?1 AND agent=?2 AND kind='user'",
                params![id, agent],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        if let Some((status, error)) = event {
            let meta = records::get(connection, "runtime_event_meta", &id)?.unwrap_or(json!({}));
            let requested = meta
                .get("requestedDelivery")
                .or_else(|| meta.get("delivery"));
            let delivery =
                DeliveryMoment::for_event("user", "", Some(&json!({"delivery":requested})))
                    .map_err(|_| TranscriptError::InvalidStoredObject {
                        table: "runtime_event_meta",
                        id: id.clone(),
                    })?;
            entry["clientMessageId"] = json!(id);
            entry["deliveryStatus"] = json!(status);
            entry["requestedDelivery"] = json!(delivery);
            entry["deliveryError"] = json!(error);
            entry["pending"] = json!(status == "pending");
            if entry.get("materialized").is_none() {
                entry["materialized"] = json!(true);
            }
        }
    }
    Ok(())
}

fn tool_result(
    connection: &Connection,
    state_root: Option<&Path>,
    actor: &Value,
    item: &mut Value,
    now: f64,
) -> Result<(), TranscriptError> {
    if item["title"] != "dynamicToolCall" {
        return Ok(());
    }
    let Some(payload) = item["text"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(Value::is_object)
    else {
        return Ok(());
    };
    let Some(call) = payload["id"].as_str() else {
        return Ok(());
    };
    let projected = payload["contentItems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "inputText")
        .filter_map(|c| serde_json::from_str::<Value>(c["text"].as_str()?).ok())
        .any(|v| v["truncated"] == true && v["outputRef"].is_string());
    if !projected
        && (!payload["status"].is_null() && payload["status"] != "inProgress"
            || !payload["success"].is_null())
    {
        return Ok(());
    }
    let mut aliases = connection.prepare_cached(
        "SELECT request FROM runtime_tool_request_aliases WHERE agent=?1 AND alias=?2 LIMIT 2",
    )?;
    let aliases = aliases
        .query_map(params![actor["id"].as_str(), call], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if aliases.len() > 1 {
        return Ok(());
    }
    let account = actor["accountKey"].as_str().unwrap_or("default");
    let thread = actor["threadId"].as_str().unwrap_or("None");
    let identity = aliases.first().cloned().unwrap_or_else(|| {
        format!(
            "{}{thread}:{call}",
            if account == "default" {
                String::new()
            } else {
                format!("{account}:")
            }
        )
    });
    let Some(mut result) = records::get(connection, "runtime_tool_results", &identity)? else {
        return Ok(());
    };
    if truthy(result.get("_payloadBlobs")) {
        let Some(state_root) = state_root else {
            item["_transcriptPendingBlobResult"] = json!({
                "result": result,
                "projected": projected,
            });
            return Ok(());
        };
        result = crate::payload_blobs::resolve_record(state_root, &result)
            .map_err(|_| TranscriptError::Store)?;
    }
    apply_tool_result(result, item, now, projected)
}

fn apply_tool_result(
    mut result: Value,
    item: &mut Value,
    now: f64,
    projected: bool,
) -> Result<(), TranscriptError> {
    let Some(mut payload) = item["text"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
    else {
        return Ok(());
    };
    crate::tool_requests::expire(&mut result, now);
    let Some(success) = result["success"].as_bool() else {
        return Ok(());
    };
    let mut content = result.get("contentItems").cloned().unwrap_or(json!([]));
    let encoded = python_json(&content);
    let limit = if projected { 20000 } else { 12000 };
    if encoded.chars().count() > limit {
        content = json!([{"type":"inputText","text":format!("{}\n[Result truncated]",encoded.chars().take(limit).collect::<String>())}]);
        item["truncated"] = json!(true);
    }
    item["toolStatus"] = json!(if success { "completed" } else { "failed" });
    payload["status"] = item["toolStatus"].clone();
    payload["success"] = json!(success);
    payload["contentItems"] = content;
    item["text"] = json!(python_json(&payload));
    Ok(())
}

fn has_pending_tool_results(page: &Value) -> bool {
    page.get("items")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("_transcriptPendingBlobResult").is_some())
        })
}

fn project_tool_results(
    state_dir: &Path,
    page: &mut Value,
    now: f64,
) -> Result<(), TranscriptError> {
    if let Some(items) = page.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            let pending = item
                .as_object_mut()
                .and_then(|item| item.remove("_transcriptPendingBlobResult"));
            let Some(pending) = pending else {
                continue;
            };
            let result = crate::payload_blobs::resolve_record(state_dir, &pending["result"])
                .map_err(|_| TranscriptError::Store)?;
            apply_tool_result(
                result,
                item,
                now,
                pending["projected"].as_bool().unwrap_or(false),
            )?;
        }
    }
    Ok(())
}

fn invalid(table: &'static str, id: &str) -> TranscriptError {
    TranscriptError::InvalidStoredObject {
        table,
        id: id.to_owned(),
    }
}

#[derive(Default)]
struct ReasoningGroup {
    id: String,
    start: f64,
    intervals: Vec<(f64, f64)>,
    since: Option<f64>,
    observed: bool,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningRecord {
    id: String,
    agent_id: String,
    #[serde(rename = "type")]
    kind: String,
    turn_id: String,
    started_at: f64,
    finished_at: Option<f64>,
}

fn reasoning(
    analytics: &Connection,
    connection: &Connection,
    actor: &Value,
    rows: &[PageRowMetadata],
    items: Vec<Value>,
    now: f64,
) -> Result<Vec<Value>, TranscriptError> {
    let Some(first) = rows.first() else {
        return Ok(items);
    };
    let Some(last) = rows.last() else {
        return Ok(items);
    };
    let upper=connection.query_row("SELECT created FROM runtime_items WHERE agent=?1 AND json_extract(record,'$.afterRestore') IS NULL AND (created,id)>(?2,?3) ORDER BY created,id LIMIT 1",params![actor["id"].as_str(),last.created,last.id],|r|r.get::<_,f64>(0)).optional()?.unwrap_or(now);
    let mut statement=analytics.prepare_cached("SELECT id,record FROM analytics_items WHERE agent=?1 AND at>=?2 AND at<?3 AND type='reasoning' ORDER BY at,id")?;
    let records = statement
        .query_map(params![actor["id"].as_str(), first.created, upper,], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
    let mut groups = BTreeMap::<(usize, String), ReasoningGroup>::new();
    for row in records {
        let (id, raw) = row?;
        let record: ReasoningRecord =
            serde_json::from_str(&raw).map_err(|_| invalid("analytics_items", &id))?;
        if record.id != id
            || record.agent_id != actor["id"]
            || record.kind != "reasoning"
            || !record.started_at.is_finite()
            || record
                .finished_at
                .is_some_and(|v| !v.is_finite() || v < record.started_at)
        {
            return Err(invalid("analytics_items", &id));
        }
        let turn = record.turn_id.as_str();
        if !items.iter().any(|i| i["turnId"] == turn) {
            continue;
        }
        let start = record.started_at;
        let mut end = record.finished_at;
        let mut observed = false;
        if record.finished_at.is_none() {
            let next=analytics.query_row("SELECT at FROM analytics_items WHERE agent=?1 AND at>?2 AND at<?3 AND json_extract(record,'$.turnId')=?4 ORDER BY at LIMIT 1",params![actor["id"].as_str(),start,upper,turn],|r|r.get::<_,f64>(0)).optional()?;
            let message = rows
                .iter()
                .find(|r| r.created > start && r.turn_id.as_deref() == Some(turn))
                .map(|r| r.created);
            end = next.into_iter().chain(message).min_by(f64::total_cmp);
            observed = end.is_some();
        }
        let running = end.is_none()
            && truthy(actor.get("inFlight"))
            && actor["status"] == "running"
            && actor["turnId"] == turn
            && actor["activity"]["phase"] == "thinking";
        if !running && end.is_none_or(|v| !v.is_finite() || v < start) {
            continue;
        }
        let Some(slot) = rows.iter().rposition(|r| r.created <= start) else {
            continue;
        };
        let group = groups
            .entry((slot, turn.to_owned()))
            .or_insert_with(|| ReasoningGroup {
                id: format!("reasoning:{id}"),
                start,
                ..Default::default()
            });
        group.observed |= observed;
        if running {
            group.since = Some(start);
        } else if let Some(end) = end {
            group.intervals.push((start, end));
        }
    }
    let mut additions = BTreeMap::<String, Vec<Value>>::new();
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by(|a, b| a.1.start.total_cmp(&b.1.start).then(a.1.id.cmp(&b.1.id)));
    for ((slot, turn), mut group) in groups {
        group
            .intervals
            .sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let (mut duration, mut edge) = (0.0_f64, 0.0_f64);
        for (start, end) in group.intervals {
            duration += (end - start.max(edge)).max(0.0);
            edge = edge.max(end);
        }
        let since = group.since.map(|v| v.max(edge));
        if duration < 1.0 && since.is_none() {
            continue;
        }
        additions.entry(rows[slot].id.clone()).or_default().push(json!({"id":group.id,"role":"reasoning","text":"","turnId":turn,"at":group.start,"reasoningMs":duration*1000.0,"reasoningSince":since,"reasoningObservedAt":now,"observedWait":group.observed}));
    }
    let mut result = Vec::new();
    for item in items {
        let extra = additions.remove(item["id"].as_str().unwrap_or_default());
        result.push(item);
        if let Some(extra) = extra {
            result.extend(extra);
        }
    }
    Ok(result)
}

pub(crate) fn observed_at() -> Result<f64, TranscriptError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| TranscriptError::Store)?
        .as_secs_f64())
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    #[test]
    fn transcript_reasoning_decodes_each_row_before_fetching_the_next() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let db = Connection::open_in_memory().expect("fixture database");
        db.execute_batch(
            "CREATE TABLE runtime_items(id TEXT,agent TEXT,created REAL,record TEXT);
             CREATE TABLE reasoning_source(id TEXT,agent TEXT,at REAL,type TEXT,record TEXT);
             CREATE INDEX reasoning_order ON reasoning_source(agent,type,at,id);
             CREATE VIEW analytics_items AS
                 SELECT id,agent,at,type,observe_raw(record) AS record FROM reasoning_source;",
        )
        .expect("fixture schema");
        let fetched = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&fetched);
        db.create_scalar_function(
            "observe_raw",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |context| {
                observed.fetch_add(1, Ordering::SeqCst);
                context.get::<String>(0)
            },
        )
        .expect("raw row observer");
        let actor = json!({"id":"agent"});
        let rows = [PageRowMetadata {
            id: "message".to_owned(),
            created: 0.0,
            turn_id: Some("turn".to_owned()),
        }];
        // Invalid records must be checked even when their turn is absent from the page.
        let invalid_irrelevant_turn = json!({
            "id":"first", "agentId":"agent", "type":"reasoning", "turnId":"absent",
            "startedAt":1.0, "finishedAt":0.0,
        })
        .to_string();
        for raw in ["{", invalid_irrelevant_turn.as_str()] {
            db.execute("DELETE FROM reasoning_source", [])
                .expect("reset fixture");
            db.execute(
                "INSERT INTO reasoning_source VALUES('first','agent',1,'reasoning',?1),
                                                    ('second','agent',2,'reasoning','{}')",
                [raw],
            )
            .expect("reasoning records");
            fetched.store(0, Ordering::SeqCst);
            let error = reasoning(
                &db,
                &db,
                &actor,
                &rows,
                vec![json!({"id":"message", "turnId":"turn"})],
                10.0,
            )
            .expect_err("first raw record is invalid");
            assert!(
                matches!(error, TranscriptError::InvalidStoredObject { table: "analytics_items", id } if id == "first")
            );
            assert_eq!(
                fetched.load(Ordering::SeqCst),
                1,
                "the next raw row must not be fetched before the current row is decoded and validated"
            );
        }

        db.execute("DELETE FROM reasoning_source", [])
            .expect("reset valid fixture");
        for (id, start, end) in [("first", 1.0, 3.0), ("second", 2.0, 4.0)] {
            let raw = json!({
                "id":id, "agentId":"agent", "type":"reasoning", "turnId":"turn",
                "startedAt":start, "finishedAt":end,
            })
            .to_string();
            db.execute(
                "INSERT INTO reasoning_source VALUES(?1,'agent',?2,'reasoning',?3)",
                params![id, start, raw],
            )
            .expect("valid reasoning record");
        }
        fetched.store(0, Ordering::SeqCst);
        let items = vec![json!({"id":"message", "turnId":"turn"})];
        assert_eq!(
            reasoning(&db, &db, &actor, &rows, items.clone(), 10.0)
                .expect("all reasoning rows contribute"),
            vec![
                items[0].clone(),
                json!({
                    "id":"reasoning:first", "role":"reasoning", "text":"", "turnId":"turn",
                    "at":1.0, "reasoningMs":3000.0, "reasoningSince":null,
                    "reasoningObservedAt":10.0, "observedWait":false,
                })
            ],
        );
        assert_eq!(fetched.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn transcript_receipts_normalize_legacy_and_keep_original_timing() {
        let db = Connection::open_in_memory().expect("fixture database");
        db.execute_batch(
            "CREATE TABLE runtime_events(id TEXT,agent TEXT,kind TEXT,status TEXT,error TEXT);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
        )
        .expect("fixture schema");
        for (id, metadata) in [
            ("legacy", json!({"delivery":"after_turn"})),
            (
                "promoted",
                json!({"delivery":"current_turn","requestedDelivery":"next_turn","sendNow":true}),
            ),
        ] {
            db.execute(
                "INSERT INTO runtime_events VALUES(?1,'agent','user','delivered',NULL)",
                [id],
            )
            .expect("fixture event");
            db.execute(
                "INSERT INTO runtime_event_meta VALUES(?1,?2)",
                params![id, metadata.to_string()],
            )
            .expect("fixture metadata");
        }
        let mut items = vec![
            json!({"id":"agent:legacy","role":"user"}),
            json!({"id":"agent:promoted","role":"user"}),
            json!({"id":"agent:historical","role":"user"}),
        ];
        receipts(&db, "agent", &mut items, false).expect("receipt projection");
        assert_eq!(
            items[0]["requestedDelivery"],
            json!(DeliveryMoment::NextTurn)
        );
        assert_eq!(
            items[1]["requestedDelivery"],
            json!(DeliveryMoment::NextTurn)
        );
        assert_eq!(
            items[2]["requestedDelivery"],
            json!(DeliveryMoment::CurrentTurn)
        );
    }
}
