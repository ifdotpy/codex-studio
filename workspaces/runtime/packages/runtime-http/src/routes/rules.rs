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
use serde_json::{Number, Value};
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

pub(crate) async fn list(State(state): State<HttpState>) -> Result<Json<RuleList>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let records = store
        .read_async("runtime-http/routes/rules", move |connection| {
            let (rules, agents) = RulesRepository::list(connection).map_err(store_api_error)?;
            rules::visible_rules(rules, agents).map_err(rules_error)
        })
        .await?;

    Ok(Json(RuleList { rules: records }))
}

#[derive(serde::Serialize)]
pub(crate) struct RuleList {
    rules: Vec<RuleRecord>,
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
    stall_timeout_seconds: Option<Number>,
    #[serde(rename = "stall_timeout_seconds", default)]
    stall_alias: Option<Number>,
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
            let path = if matches!(prepare_action, RuleAction::Resume { .. }) {
                old.as_ref().and_then(|rule| match &rule.kind {
                    studio_runtime_schema::rule_record::RuleKind::File { path } => {
                        Some(path.clone())
                    }
                    studio_runtime_schema::rule_record::RuleKind::Interval
                    | studio_runtime_schema::rule_record::RuleKind::Once
                    | studio_runtime_schema::rule_record::RuleKind::Event
                    | studio_runtime_schema::rule_record::RuleKind::LowWorkers { .. } => None,
                })
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
            Ok::<_, ApiError>((agent_value, old, path))
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
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "Agent changed while updating its rule",
                ));
            }
            let old = RulesRepository::get(transaction, &rule_id).map_err(store_api_error)?;
            if old != prepared_old {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "Rule changed while applying this update",
                ));
            }
            if old.as_ref().is_some_and(|rule| rule.agent != agent_id) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "This rule belongs to another agent",
                ));
            }
            if old.as_ref().is_some_and(|rule| rule.in_flight) {
                if matches!(action, RuleAction::Save { .. }) {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "Pause the rule and wait for its current check before editing",
                    ));
                }
                if matches!(action, RuleAction::Resume { .. }) {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "Wait for the current rule check before resuming",
                    ));
                }
            }
            let cancel_ids =
                if matches!(action, RuleAction::Pause { .. } | RuleAction::Delete { .. }) {
                    RulesRepository::active_check_ids(transaction, &rule_id)
                        .map_err(store_api_error)?
                } else {
                    Vec::new()
                };
            let result = match &action {
                RuleAction::Delete { .. }
                | RuleAction::Pause { .. }
                | RuleAction::Resume { .. } => {
                    let Some(mut rule) = old else {
                        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Unknown rule"));
                    };
                    if matches!(action, RuleAction::Delete { .. }) {
                        RulesRepository::delete(transaction, &rule_id).map_err(store_api_error)?;
                        RuleWriteResponse::Deleted(crate::generated::DeletedRuleReceipt {
                            deleted: rule_id.clone(),
                        })
                    } else {
                        use studio_runtime_schema::rule_record::{RuleKind, RuleStatus};
                        if matches!(action, RuleAction::Resume { .. })
                            && agent_value.get("autoWake").and_then(Value::as_bool) != Some(true)
                        {
                            return Err(ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "Resume the agent before its rules",
                            ));
                        }
                        let owner_held = rule.owner_hold.take().is_some();
                        rule.restart_check = None;
                        let desired = if matches!(action, RuleAction::Resume { .. }) {
                            RuleStatus::Active
                        } else {
                            RuleStatus::Paused
                        };
                        if owner_held
                            || !matches!(rule.kind, RuleKind::LowWorkers { .. })
                            || rule.status != desired
                            || rule.epoch.get() != agent.epoch
                        {
                            rule.status = desired;
                            rule.epoch =
                                agent.epoch.try_into().map_err(|_| ApiError::internal())?;
                            rule.next_at = number(now + rule.interval_seconds.get() as f64)?;
                            rule.error = None;
                            if matches!(action, RuleAction::Resume { .. })
                                && matches!(rule.kind, RuleKind::File { .. })
                            {
                                let generation = rule.file_generation.get().saturating_add(1);
                                rule.file_activity_at = Some(number(now)?);
                                rule.fingerprint = prepared_fingerprint;
                                rule.file_generation =
                                    generation.try_into().map_err(|_| ApiError::internal())?;
                                rule.stall_wake_generation = generation
                                    .saturating_sub(1)
                                    .try_into()
                                    .map_err(|_| ApiError::internal())?;
                            }
                            if matches!(rule.kind, RuleKind::LowWorkers { .. }) {
                                rule.low_since = None;
                                rule.alerted = Some(false);
                            }
                            RulesRepository::put(transaction, &rule).map_err(store_api_error)?;
                        }
                        RuleWriteResponse::Record(Box::new(rule))
                    }
                }
                RuleAction::Save { data: body, .. } => {
                    let kind = body.kind.as_deref().unwrap_or("interval");
                    let command = body.command.as_deref().unwrap_or("");
                    let raw_path = body.path.as_deref();
                    let liveness = body.liveness_command.as_deref().unwrap_or("").trim();
                    if kind == "file" && raw_path.is_none_or(|path| path.trim().is_empty()) {
                        return Err(located("path"));
                    }
                    let interval = body
                        .interval_seconds
                        .as_ref()
                        .and_then(Number::as_u64)
                        .unwrap_or(60);
                    let stall = body
                        .stall_timeout_seconds
                        .as_ref()
                        .or(body.stall_alias.as_ref())
                        .and_then(Number::as_u64)
                        .unwrap_or(1800);
                    let text = body
                        .text
                        .as_deref()
                        .unwrap_or("Check this event and continue the original task.")
                        .trim();
                    let name = body.name.as_deref().map(str::trim).unwrap_or("");
                    let checked = rules::CheckedRuleSave::new(
                        rules::RuleSaveInput {
                            kind: kind.to_owned(),
                            minimum: Ok(Some(
                                body.minimum_workers.clone().unwrap_or_else(|| 8.into()),
                            )),
                            duration: Ok(Some(
                                body.duration_minutes.clone().unwrap_or_else(|| 30.into()),
                            )),
                            interval: Ok(interval as i64),
                            stall: Ok(stall as i64),
                            at: rules::RuleTimeInput::Checked(
                                checked_at.ok_or_else(ApiError::internal)?,
                            ),
                            event: body
                                .event
                                .as_ref()
                                .map_or("worker_completed", |event| event.as_str())
                                .to_owned(),
                            command: Ok(command.to_owned()),
                            liveness: Ok(liveness.to_owned()),
                            name: Ok(name.to_owned()),
                            text: Ok(text.to_owned()),
                        },
                        now,
                        agent_value.get("isLead").and_then(Value::as_bool) == Some(true),
                        rules::RuleSaveOrder::Http,
                    )
                    .map_err(rule_save_error)?;
                    let validated = checked;
                    let checked = validated.fields();
                    let kind = checked.kind.as_str();
                    let low_workers = kind == "low_workers";
                    let previous = old.as_ref();
                    let mut rule = rules::persisted_rule(
                        &validated,
                        rules::RuleRecordContext {
                            id: &rule_id,
                            actor: &agent,
                            previous,
                            path: prepared_path.as_deref(),
                            fingerprint: prepared_fingerprint,
                            now,
                        },
                    )
                    .map_err(rule_save_error)?;
                    if let Some(previous) =
                        previous.filter(|old| low_workers && low_worker_rule_unchanged(old, &rule))
                    {
                        rule = previous.clone();
                    } else {
                        if low_workers {
                            rule.next_at = number(now)?;
                        }
                        RulesRepository::put(transaction, &rule).map_err(store_api_error)?;
                    }
                    RuleWriteResponse::Record(Box::new(rule))
                }
            };
            Ok::<_, ApiError>((result, cancel_ids))
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
    Record(Box<RuleRecord>),
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
        error @ (StoreError::TranscriptFullTextUnavailable { .. }
        | StoreError::TranscriptInputsIncomplete { .. }) => ApiError::stored(error),
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

fn low_worker_rule_unchanged(old: &RuleRecord, new: &RuleRecord) -> bool {
    old.kind == new.kind && old.name == new.name && old.text == new.text && old.epoch == new.epoch
}
async fn file_fingerprint(path: &Path) -> Option<(i64, u64, u64)> {
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
    Some((i64::try_from(modified).ok()?, metadata.len(), inode))
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

fn number(value: f64) -> Result<Number, ApiError> {
    Number::from_f64(value).ok_or_else(ApiError::internal)
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
