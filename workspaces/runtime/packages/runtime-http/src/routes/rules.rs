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
use studio_runtime_services::rules::{self, RuleAction, RulesError};
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
        .map(|rule| rule.value())
        .collect::<Vec<_>>();
    let response = json!({"rules": output});
    Ok(Json(super::generated_json::<crate::generated::RuleList>(
        response,
    )?))
}

// OpenAPI validation owns the wire shape; numeric fields retain JSON number spelling.
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleWire {
    agent: String,
    action: Option<crate::generated::RuleBodyAction>,
    id: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    text: Option<String>,
    command: Option<String>,
    path: Option<String>,
    liveness_command: Option<String>,
    interval_seconds: Option<Number>,
    at: Option<Number>,
    #[serde(default)]
    stall_timeout_seconds: studio_runtime_schema::queue_command::Field<Number>,
    #[serde(rename = "stall_timeout_seconds", default)]
    stall_alias: studio_runtime_schema::queue_command::Field<Number>,
    minimum_workers: Option<Number>,
    duration_minutes: Option<Number>,
    event: Option<crate::generated::RuleBodyEvent>,
}

async fn update(
    State(state): State<HttpState>,
    runtime: Option<
        Extension<Arc<studio_runtime_services::monitor_runtime_wiring::MonitorRuntime>>,
    >,
    ApiJson(body): ApiJson<RuleWire>,
) -> Result<Json<RuleWriteResponse>, ApiError> {
    use crate::generated::RuleBodyAction;
    let action_name = body.action.clone().unwrap_or_default();
    let now = now_seconds()?;
    let checked_at = if matches!(action_name, RuleBodyAction::Save) {
        let interval = body
            .interval_seconds
            .as_ref()
            .and_then(Number::as_u64)
            .unwrap_or(60) as f64;
        let at = body
            .at
            .as_ref()
            .and_then(Number::as_f64)
            .unwrap_or(now + interval);
        Some(rules::RuleTime::check(at, now).map_err(rule_save_error)?)
    } else {
        None
    };
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let agent_id = body.agent.clone();
    let supplied_id = body.id.clone();
    let rule_id = supplied_id.unwrap_or_else(new_rule_id);
    let action = match action_name {
        RuleBodyAction::Save => RuleAction::Save {
            id: rule_id.clone(),
            data: body.clone(),
        },
        RuleBodyAction::Pause => RuleAction::Pause {
            id: rule_id.clone(),
        },
        RuleBodyAction::Resume => RuleAction::Resume {
            id: rule_id.clone(),
        },
        RuleBodyAction::Delete => RuleAction::Delete {
            id: rule_id.clone(),
        },
    };
    let rule_id = action.id().to_owned();
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
            let old_value = old.as_ref().map(|old| old.value());
            let path = if matches!(prepare_action, RuleAction::Resume { .. })
                && old_value.as_ref().and_then(|value| string(value, "kind")) == Some("file")
            {
                old_value
                    .as_ref()
                    .and_then(|value| string(value, "path"))
                    .map(str::to_owned)
            } else if matches!(prepare_action, RuleAction::Save { .. })
                && prepare_body.kind.as_deref() == Some("file")
            {
                Some(resolve_workspace_path(
                    &agent_value,
                    prepare_body.path.as_deref().unwrap_or(""),
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
            let old_value = old.as_ref().map(|old| old.value());
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
            if matches!(action, RuleAction::Save { .. }) && old_value.as_ref().and_then(|v| v.get("inFlight")).and_then(Value::as_bool) == Some(true) {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "Pause the rule and wait for its current check before editing"));
            }
            if matches!(action, RuleAction::Resume { .. }) && old_value.as_ref().and_then(|v| v.get("inFlight")).and_then(Value::as_bool) == Some(true) {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "Wait for the current rule check before resuming"));
            }
            let cancel_ids = if matches!(action, RuleAction::Pause { .. } | RuleAction::Delete { .. }) {
                RulesRepository::active_check_ids(transaction, &rule_id).map_err(store_api_error)?
            } else {
                Vec::new()
            };
            let result = match &action {
                RuleAction::Delete { .. } | RuleAction::Pause { .. } | RuleAction::Resume { .. } => {
                    let (Some(_old), Some(mut value)) = (old, old_value) else {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Unknown rule"));
                    };
                    if matches!(action, RuleAction::Delete { .. }) {
                        RulesRepository::delete(transaction, &rule_id).map_err(store_api_error)?;
                        json!({"deleted": rule_id})
                    } else {
                        if matches!(action, RuleAction::Resume { .. }) && agent_value.get("autoWake").and_then(Value::as_bool) != Some(true) {
                            return Err(ApiError::new(StatusCode::BAD_REQUEST, "Resume the agent before its rules"));
                        }
                        // A manual pause or resume replaces a pause that the tick made
                        // while the owner could not run, so the tick no longer resumes it.
                        let owner_held = value.get("ownerHold").is_some_and(|hold| !hold.is_null());
                        if let Some(object) = value.as_object_mut() {
                            object.remove("restartCheck");
                            object.remove("ownerHold");
                        }
                        let desired = if matches!(action, RuleAction::Resume { .. }) { "active" } else { "paused" };
                        if !owner_held
                            && string(&value, "kind") == Some("low_workers")
                            && string(&value, "status") == Some(desired)
                            && value.get("epoch") == agent_value.get("epoch")
                        {
                            value
                        } else {
                            let interval = value.get("intervalSeconds").and_then(Value::as_f64).unwrap_or(60.0);
                            let kind = string(&value, "kind").unwrap_or("").to_owned();
                            let file_update = (matches!(action, RuleAction::Resume { .. }) && kind == "file").then(|| (
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
                RuleAction::Save { data: body, .. } => {
                    let kind = body.kind.as_deref().unwrap_or("interval");
                    let command = body.command.as_deref().unwrap_or("");
                    let raw_path = body.path.as_deref();
                    let liveness = body.liveness_command.as_deref().unwrap_or("").trim();
                    if kind == "file" && raw_path.is_none_or(|path| path.trim().is_empty()) { return Err(located("path")); }
                    let interval = body.interval_seconds.as_ref().and_then(Number::as_u64).unwrap_or(60);
                    let stall_field = if body.stall_timeout_seconds.is_missing() { &body.stall_alias } else { &body.stall_timeout_seconds };
                    let stall = stall_field.value().and_then(Number::as_u64).unwrap_or(1800);
                    let text = body.text.as_deref().unwrap_or("Check this event and continue the original task.").trim();
                    let name = body.name.as_deref().map(str::trim).unwrap_or("");
                    let checked = rules::CheckedRuleSave::new(rules::RuleSaveInput {
                        kind: kind.to_owned(),
                        minimum: Ok(Some(body.minimum_workers.clone().unwrap_or_else(|| 8.into()))),
                        duration: Ok(Some(body.duration_minutes.clone().unwrap_or_else(|| 30.into()))),
                        interval: Ok(interval as i64),
                        stall: Ok(stall as i64),
                        at: rules::RuleTimeInput::Checked(checked_at.ok_or_else(ApiError::internal)?),
                        event: body.event.as_ref().map_or("worker_completed", |event| event.as_str()).to_owned(),
                        command: Ok(command.to_owned()),
                        liveness: Ok(liveness.to_owned()),
                        name: Ok(name.to_owned()),
                        text: Ok(text.to_owned()),
                    }, now, agent_value.get("isLead").and_then(Value::as_bool) == Some(true), rules::RuleSaveOrder::Http).map_err(rule_save_error)?;
                    let checked = checked.fields();
                    let kind = checked.kind.as_str();
                    let interval = checked.interval;
                    let at = checked.at;
                    let stall = checked.kind.stall();
                    let text = &checked.text;
                    let name = &checked.name;
                    let command = &checked.command;
                    let liveness = checked.kind.liveness();
                    let path = if kind == "file" { prepared_path.clone() } else { None };
                    let fingerprint = if kind == "file" { prepared_fingerprint.clone() } else { None };
                    let old_record = old_value.as_ref();
                    let value = json!({
                        "id": rule_id, "agent": agent_id,
                        "rootId": agent_value.get("rootId").cloned().unwrap_or_else(|| json!(agent_id)),
                        "epoch": agent_value["epoch"], "name": name, "kind": kind,
                        "intervalSeconds": interval, "nextAt": at, "at": at,
                        "path": path, "event": checked.event.as_str(),
                        "command": command, "stallTimeoutSeconds": if kind == "file" { stall } else { 0 },
                        "livenessCommand": liveness, "fileActivityAt": old_record.and_then(|v| v.get("fileActivityAt")).cloned().unwrap_or(json!(now)),
                        "fileGeneration": old_record.and_then(|v| v.get("fileGeneration")).cloned().unwrap_or(json!(0)),
                        "stallWakeGeneration": old_record.and_then(|v| v.get("stallWakeGeneration")).cloned().unwrap_or(json!(-1)),
                        "text": text, "status": if agent_value.get("autoWake").and_then(Value::as_bool) == Some(true) { "active" } else { "paused" },
                        "created": old_record.and_then(|v| v.get("created")).cloned().unwrap_or(json!(now)),
                        "inFlight": false, "checks": old_record.and_then(|v| v.get("checks")).cloned().unwrap_or(json!(0)),
                        "wakes": old_record.and_then(|v| v.get("wakes")).cloned().unwrap_or(json!(0)),
                        "fingerprint": fingerprint,
                        "minimumWorkers": if kind == "low_workers" { json!(checked.kind.minimum()) } else { Value::Null },
                        "durationMinutes": if kind == "low_workers" { json!(checked.kind.duration()) } else { Value::Null },
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

fn rule_save_error(error: rules::RuleSaveError) -> ApiError {
    use rules::RuleSaveError as E;
    match error {
        E::Integer | E::InputText => ApiError::internal(),
        E::Lead => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Only an orchestrator can own a low-worker alert",
        ),
        E::Kind => located("kind"),
        E::Minimum => located("minimumWorkers"),
        E::Duration => located("durationMinutes"),
        E::Interval => located("intervalSeconds"),
        E::Stall => located("stallTimeoutSeconds"),
        E::At => located("at"),
        E::Event => located("event"),
        E::Command => located("command"),
        E::Liveness => located("livenessCommand"),
        E::Name => located("name"),
        E::Text => located("text"),
    }
}

fn rules_error(error: RulesError) -> ApiError {
    match error {
        RulesError::InvalidStoredObject { object, id } => ApiError::invalid_stored(object, &id),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
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
        MonitorError::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
        MonitorError::Store(error) => ApiError::stored(error),
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
        MonitorError::InvalidResultReceipt(_)
        | MonitorError::InvalidDeliveryIdentity(_)
        | MonitorError::InvalidOutputReceipt(_)
        | MonitorError::TaskFailed
        | MonitorError::InputOutcomeUnknown
        | MonitorError::ProviderRequest { .. }
        | MonitorError::ConfigurationRejected { .. }
        | MonitorError::ConfigurationReceipt { .. }
        | MonitorError::Event(_)
        | MonitorError::Rules(_)
        | MonitorError::Io(_) => ApiError::internal(),
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
        StoreError::RequestResult(error) => error.into(),
        StoreError::Outbox(error) => error.into(),
        StoreError::StateDirectory(_)
        | StoreError::AlreadyOwned
        | StoreError::MissingDatabase(_)
        | StoreError::Migration { .. }
        | StoreError::UnsafeStateDirectory
        | StoreError::SchemaMismatch(_)
        | StoreError::SchemaObjectMismatch { .. }
        | StoreError::Sqlite(_)
        | StoreError::CommitOutcomeUnknown(_)
        | StoreError::Json(_)
        | StoreError::SchemaValidation(_)
        | StoreError::RecordContract(_)
        | StoreError::WriterPoisoned
        | StoreError::WriterActorInitialization(_)
        | StoreError::WriterActorControl(_)
        | StoreError::CommitActorOutcomeUnknown(_)
        | StoreError::ReadTaskFailed => ApiError::internal(),
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
