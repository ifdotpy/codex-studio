//! Read and update durable rule records.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Number, Value, json};
use std::sync::Arc;
use studio_runtime_services::rules::{self, RulesError};
use studio_runtime_store::{RuleRecord, RulesRepository, StoreError};

use crate::error::ApiError;
use crate::router::HttpState;

static RULE_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/rules", OperationStatus::Implemented, list),
        post("/api/rules", OperationStatus::Implemented, update),
    ])
}

pub(crate) async fn list(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::RuleList>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let records = store
        .read_async("runtime-http/routes/rules", move |connection| {
            let (rules, agents) = RulesRepository::list(connection).map_err(store_api_error)?;
            rules::visible_rules(rules, agents).map_err(rules_error)
        })
        .await?;

    let output = records
        .into_iter()
        .map(|rule| rule.value().clone())
        .collect::<Vec<_>>();
    let response = json!({"rules": output});
    Ok(Json(super::generated_json::<crate::generated::RuleList>(
        response,
    )?))
}

async fn update(
    State(state): State<HttpState>,
    runtime: Option<
        Extension<Arc<studio_runtime_services::monitor_runtime_wiring::MonitorRuntime>>,
    >,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<RuleWriteResponse>, ApiError> {
    let action = string(&body, "action").unwrap_or("save");
    let now = now_seconds()?;
    if action == "save"
        && let Some(at) = body.get("at").filter(|value| !value.is_null())
        && at
            .as_f64()
            .is_none_or(|at| at < now - 1.0 || at > now + 31_536_000.0)
    {
        return Err(located("at"));
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let action = action.to_owned();
    let agent_id = string(&body, "agent").unwrap_or_default().to_owned();
    let supplied_id = string(&body, "id").map(str::to_owned);
    let rule_id = supplied_id.unwrap_or_else(new_rule_id);
    let prepare_store = store.clone();
    let prepare_agent_id = agent_id.clone();
    let prepare_rule_id = rule_id.clone();
    let prepare_action = action.clone();
    let prepare_body = body.clone();
    let (prepared_agent, prepared_old, prepared_path) = prepare_store
        .read_async("runtime-http/routes/rules", move |connection| {
            let agent = RulesRepository::agent(connection, &prepare_agent_id)
                .map_err(store_api_error)?
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent"))?;
            let agent_value = agent.value();
            validate_agent(agent.id().as_ref(), &agent_value)?;
            let old =
                RulesRepository::get(connection, &prepare_rule_id).map_err(store_api_error)?;
            let old_value = old.as_ref().map(|old| old.value().clone());
            let path = if prepare_action == "resume"
                && old_value.as_ref().and_then(|value| string(value, "kind")) == Some("file")
            {
                old_value
                    .as_ref()
                    .and_then(|value| string(value, "path"))
                    .map(str::to_owned)
            } else if !matches!(prepare_action.as_str(), "delete" | "pause" | "resume")
                && string(&prepare_body, "kind") == Some("file")
            {
                Some(resolve_workspace_path(
                    &agent_value,
                    string(&prepare_body, "path").unwrap_or(""),
                )?)
            } else {
                None
            };
            Ok::<_, ApiError>((agent_value, old_value, path))
        })
        .await?;
    let prepared_fingerprint = match prepared_path.as_deref() {
        Some(path) => file_fingerprint(Path::new(path)).await,
        None => None,
    };
    let (result, cancel_ids) = store
        .transaction_with(move |transaction| {
            let agent = RulesRepository::agent(transaction, &agent_id)
                .map_err(store_api_error)?
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent"))?;
            let agent_value = agent.value();
            validate_agent(agent.id().as_ref(), &agent_value)?;
            if agent_value != prepared_agent {
                return Err(ApiError::new(StatusCode::CONFLICT, "Agent changed while updating its rule"));
            }
            let old = RulesRepository::get(transaction, &rule_id).map_err(store_api_error)?;
            let old_value = old.as_ref().map(|old| old.value().clone());
            if old_value != prepared_old {
                return Err(ApiError::new(StatusCode::CONFLICT, "Rule changed while applying this update"));
            }
            if let (Some(old), Some(value)) = (&old, &old_value) {
                if string(value, "id") != Some(old.id().as_ref()) {
                    return Err(invalid_rule(old.id().as_ref()));
                }
                if string(value, "agent") != Some(agent_id.as_str()) {
                    return Err(ApiError::new(StatusCode::BAD_REQUEST, "This rule belongs to another agent"));
                }
            }
            if action == "save" && old_value.as_ref().and_then(|v| v.get("inFlight")).and_then(Value::as_bool) == Some(true) {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "Pause the rule and wait for its current check before editing"));
            }
            if action == "resume" && old_value.as_ref().and_then(|v| v.get("inFlight")).and_then(Value::as_bool) == Some(true) {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "Wait for the current rule check before resuming"));
            }
            let cancel_ids = if matches!(action.as_str(), "pause" | "delete") {
                RulesRepository::active_check_ids(transaction, &rule_id).map_err(store_api_error)?
            } else {
                Vec::new()
            };
            let result = match action.as_str() {
                "delete" | "pause" | "resume" => {
                    let (Some(_old), Some(mut value)) = (old, old_value) else {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Unknown rule"));
                    };
                    if action == "delete" {
                        RulesRepository::delete(transaction, &rule_id).map_err(store_api_error)?;
                        json!({"deleted": rule_id})
                    } else {
                        if action == "resume" && agent_value.get("autoWake").and_then(Value::as_bool) != Some(true) {
                            return Err(ApiError::new(StatusCode::BAD_REQUEST, "Resume the agent before its rules"));
                        }
                        // A manual pause or resume replaces a pause that the tick made
                        // while the owner could not run, so the tick no longer resumes it.
                        let owner_held = value.get("ownerHold").is_some_and(|hold| !hold.is_null());
                        if let Some(object) = value.as_object_mut() {
                            object.remove("restartCheck");
                            object.remove("ownerHold");
                        }
                        let desired = if action == "resume" { "active" } else { "paused" };
                        if !owner_held
                            && string(&value, "kind") == Some("low_workers")
                            && string(&value, "status") == Some(desired)
                            && value.get("epoch") == agent_value.get("epoch")
                        {
                            value
                        } else {
                            let interval = value.get("intervalSeconds").and_then(Value::as_f64).unwrap_or(60.0);
                            let kind = string(&value, "kind").unwrap_or("").to_owned();
                            let file_update = (action == "resume" && kind == "file").then(|| (
                                now,
                                prepared_fingerprint.clone().unwrap_or(Value::Null),
                                value.get("fileGeneration").and_then(Value::as_u64).unwrap_or(0).saturating_add(1),
                            ));
                            if let Some(object) = value.as_object_mut() {
                                object.insert("status".into(), json!(desired));
                                object.insert("epoch".into(), agent_value["epoch"].clone());
                                object.insert("nextAt".into(), number(now + interval)?);
                                object.insert("error".into(), Value::Null);
                                if let Some((activity, fingerprint, generation)) = file_update {
                                    object.insert("fileActivityAt".into(), number(activity)?);
                                    object.insert("fingerprint".into(), fingerprint);
                                    object.insert("fileGeneration".into(), json!(generation));
                                    object.insert("stallWakeGeneration".into(), json!(generation.saturating_sub(1)));
                                }
                                if kind == "low_workers" {
                                    object.insert("lowSince".into(), Value::Null);
                                    object.insert("alerted".into(), Value::Bool(false));
                                }
                            }
                            let stored = RuleRecord::from_value(value.clone()).map_err(|_| ApiError::internal())?;
                            RulesRepository::put(transaction, &stored).map_err(store_api_error)?;
                            value
                        }
                    }
                }
                _ => {
                    let kind = string(&body, "kind").unwrap_or("interval");
                    let command = string(&body, "command").unwrap_or("");
                    let raw_path = string(&body, "path");
                    let liveness = string(&body, "livenessCommand").unwrap_or("").trim();
                    if kind == "low_workers" && !command.is_empty() { return Err(located("command")); }
                    if kind == "file" && raw_path.is_none_or(|path| path.trim().is_empty()) { return Err(located("path")); }
                    if !liveness.is_empty() && kind != "file" { return Err(located("livenessCommand")); }
                    if kind == "low_workers" && agent_value.get("isLead").and_then(Value::as_bool) != Some(true) {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Only an orchestrator can own a low-worker alert"));
                    }
                    let interval = body.get("intervalSeconds").and_then(Value::as_u64).unwrap_or(60);
                    let at = body.get("at").and_then(Value::as_f64).unwrap_or(now + interval as f64);
                    if !(now - 1.0..=now + 31_536_000.0).contains(&at) { return Err(located("at")); }
                    let stall = body.get("stallTimeoutSeconds").or_else(|| body.get("stall_timeout_seconds")).and_then(Value::as_u64).unwrap_or(1800);
                    let text = string(&body, "text").unwrap_or("Check this event and continue the original task.").trim();
                    if text.is_empty() { return Err(located("text")); }
                    let name = trimmed_text(&body, "name")?;
                    let path = if kind == "file" { prepared_path.clone() } else { None };
                    let fingerprint = if kind == "file" { prepared_fingerprint.clone() } else { None };
                    let old_record = old_value.as_ref();
                    let value = json!({
                        "id": rule_id, "agent": agent_id,
                        "rootId": agent_value.get("rootId").cloned().unwrap_or_else(|| json!(agent_id)),
                        "epoch": agent_value["epoch"], "name": name, "kind": kind,
                        "intervalSeconds": interval, "nextAt": at, "at": at,
                        "path": path, "event": string(&body, "event").unwrap_or("worker_completed"),
                        "command": command.trim(), "stallTimeoutSeconds": if kind == "file" { stall } else { 0 },
                        "livenessCommand": liveness, "fileActivityAt": old_record.and_then(|v| v.get("fileActivityAt")).cloned().unwrap_or(json!(now)),
                        "fileGeneration": old_record.and_then(|v| v.get("fileGeneration")).cloned().unwrap_or(json!(0)),
                        "stallWakeGeneration": old_record.and_then(|v| v.get("stallWakeGeneration")).cloned().unwrap_or(json!(-1)),
                        "text": text, "status": if agent_value.get("autoWake").and_then(Value::as_bool) == Some(true) { "active" } else { "paused" },
                        "created": old_record.and_then(|v| v.get("created")).cloned().unwrap_or(json!(now)),
                        "inFlight": false, "checks": old_record.and_then(|v| v.get("checks")).cloned().unwrap_or(json!(0)),
                        "wakes": old_record.and_then(|v| v.get("wakes")).cloned().unwrap_or(json!(0)),
                        "fingerprint": fingerprint,
                        "minimumWorkers": if kind == "low_workers" { body.get("minimumWorkers").cloned().unwrap_or(json!(8)) } else { Value::Null },
                        "durationMinutes": if kind == "low_workers" { body.get("durationMinutes").cloned().unwrap_or(json!(30)) } else { Value::Null },
                        "lowSince": Value::Null, "alerted": Value::Bool(false),
                    });
                    let value = strip_null_low_worker_fields(value, kind);
                    if let Some(old) = old_record.filter(|old| kind == "low_workers" && low_worker_rule_unchanged(old, &value)) {
                        old.clone()
                    } else {
                        let mut value = value;
                        if kind == "low_workers" { value["nextAt"] = json!(now); }
                        let stored = RuleRecord::from_value(value.clone()).map_err(|_| ApiError::internal())?;
                        RulesRepository::put(transaction, &stored).map_err(store_api_error)?;
                        value
                    }
                }
            };
            if result.get("deleted").is_some() {
                super::generated_json::<crate::generated::DeletedRuleReceipt>(result)
                    .map(RuleWriteResponse::Deleted)
            } else {
                super::generated_json::<crate::generated::RuleRecordResponse>(result)
                    .map(|value| RuleWriteResponse::Record(Box::new(value)))
            }
                .map(|result| (result, cancel_ids))
        })
    .await?;
    if !cancel_ids.is_empty() {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let ids = cancel_ids;
        if let Some(Extension(runtime)) = runtime {
            for id in ids {
                runtime
                    .cancel(&id, None, now)
                    .await
                    .map_err(monitor_api_error)?;
            }
        } else {
            let wake = state.event_wake.clone();
            store
                .transaction_with_post_commit(
                    move |transaction| {
                        for id in ids {
                            studio_runtime_services::monitor_runtime::cancel_in_transaction(
                                transaction,
                                &id,
                                None,
                                now,
                            )?;
                        }
                        Ok(())
                    },
                    move |_| wake.notify_one(),
                )
                .await
                .map_err(monitor_api_error)?;
        }
    }
    Ok(Json(result))
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum RuleWriteResponse {
    Deleted(crate::generated::DeletedRuleReceipt),
    Record(Box<crate::generated::RuleRecordResponse>),
}

fn rules_error(error: RulesError) -> ApiError {
    match error {
        RulesError::InvalidStoredObject { object, id } => ApiError::invalid_stored(object, &id),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn trimmed_text(value: &Value, key: &str) -> Result<Value, ApiError> {
    let Some(text) = string(value, key) else {
        return Err(located(key));
    };
    if text.trim().is_empty() {
        return Err(located(key));
    }
    Ok(Value::String(text.trim().to_owned()))
}

fn located(field: &str) -> ApiError {
    ApiError::validation(vec![
        Value::String("body".to_owned()),
        Value::String(field.to_owned()),
    ])
}

fn monitor_api_error(error: studio_runtime_services::monitor_policy::MonitorError) -> ApiError {
    use studio_runtime_services::monitor_policy::MonitorError;
    match error {
        MonitorError::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        MonitorError::Unavailable(name) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {name}"),
        ),
        MonitorError::InvalidStored { id, .. } => ApiError::invalid_stored("monitors", &id),
        MonitorError::Event(
            studio_runtime_services::event_write::EventWriteError::RemoteEventsUnavailable,
        ) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires cross-server/remote-events",
        ),
        _ => ApiError::internal(),
    }
}

fn store_api_error(error: StoreError) -> ApiError {
    match error {
        StoreError::InvalidStoredObject { table, id } => {
            let object = table.strip_prefix("runtime_").unwrap_or(table);
            ApiError::invalid_stored(object, &id)
        }
        StoreError::InvalidStoredRecord { object, id } => {
            let object = object.strip_prefix("runtime_").unwrap_or(object);
            ApiError::invalid_stored(object, &id)
        }
        StoreError::InvalidSyncObject { scope, id }
        | StoreError::InvalidSyncObjectField { scope, id, .. } => {
            ApiError::invalid_stored(&scope, &id)
        }
        _ => ApiError::internal(),
    }
}

fn invalid_agent(id: &str) -> ApiError {
    ApiError::invalid_stored("agents", id)
}

fn invalid_rule(id: &str) -> ApiError {
    ApiError::invalid_stored("rules", id)
}

fn validate_agent(id: &str, value: &Value) -> Result<(), ApiError> {
    if string(value, "id") != Some(id)
        || value.get("epoch").and_then(Value::as_u64).is_none()
        || value
            .get("deletedAt")
            .is_some_and(|deleted| !deleted.is_null())
    {
        return Err(invalid_agent(id));
    }
    Ok(())
}

fn strip_null_low_worker_fields(mut value: Value, kind: &str) -> Value {
    if kind != "low_workers"
        && let Some(object) = value.as_object_mut()
    {
        for key in ["minimumWorkers", "durationMinutes", "lowSince", "alerted"] {
            object.remove(key);
        }
    }
    value
}

fn low_worker_rule_unchanged(old: &Value, new: &Value) -> bool {
    [
        "kind",
        "name",
        "text",
        "minimumWorkers",
        "durationMinutes",
        "epoch",
    ]
    .into_iter()
    .all(|key| old.get(key) == new.get(key))
}

async fn file_fingerprint(path: &Path) -> Option<Value> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        metadata.ino()
    };
    #[cfg(not(unix))]
    let inode = 0_u64;
    Some(json!([modified, metadata.len(), inode]))
}

fn resolve_workspace_path(agent: &Value, input: &str) -> Result<String, ApiError> {
    let cwd = string(agent, "cwd").ok_or_else(ApiError::internal)?;
    let mut path = PathBuf::from(input.trim());
    if path == Path::new("~") || path.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| located("path"))?;
        let remainder = path.strip_prefix("~").map_err(|_| located("path"))?;
        path = PathBuf::from(home).join(remainder.strip_prefix("/").unwrap_or(remainder));
    } else if !path.is_absolute() {
        path = Path::new(cwd).join(path);
    }
    studio_runtime_store::path_identity::resolve(&path)
        .map(|path| path.to_string_lossy().into_owned())
        .ok_or_else(|| located("path"))
}

fn now_seconds() -> Result<f64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| ApiError::internal())
}

fn number(value: f64) -> Result<Value, ApiError> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(ApiError::internal)
}

fn new_rule_id() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = RULE_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("rule-{time:x}-{:x}-{sequence:x}", std::process::id())
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "rules",
        routes: Some(routes),
        dependencies: &["rule-driver/event-rules"],
    }
}
