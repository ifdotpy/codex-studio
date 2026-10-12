//! Read and update durable rule records.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value, json};
use studio_runtime_services::rules::{self, RulesError};
use studio_runtime_store::RulesRepository;

use crate::error::ApiError;
use crate::router::HttpState;

static RULE_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ACTIVE_CHECK_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("monitor-driver/active-checks")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/rules", OperationStatus::Implemented, list),
        post(
            "/api/rules",
            OperationStatus::partial(&ACTIVE_CHECK_DEPENDENCIES),
            update,
        ),
    ])
}

pub(crate) async fn list(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let records = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let (rules, agents) =
            RulesRepository::list(&connection).map_err(|_| ApiError::internal())?;
        let rules = rules
            .into_iter()
            .map(|mut rule| {
                let dto =
                    serde_json::from_str::<crate::generated::RuleRecordResponse>(&rule.record)
                        .map_err(|_| invalid_stored_rule(&rule.id))?;
                rule.record = serde_json::to_string(&dto).map_err(|_| ApiError::internal())?;
                Ok(rule)
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        rules::visible_rules(rules, agents).map_err(rules_error)
    })
    .await
    .map_err(|_| ApiError::internal())??;

    let output = records
        .into_iter()
        .map(|rule| serde_json::from_str::<Value>(&rule.record).map_err(|_| ApiError::internal()))
        .collect::<Result<Vec<_>, _>>()?;
    let response = json!({"rules": output});
    Ok(Json(generated_json::<crate::generated::RuleList>(
        response,
    )?))
}

async fn update(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = now_seconds()?;
    let action = string(&body, "action").unwrap_or("save").to_owned();
    let agent_id = string(&body, "agent")
        .ok_or_else(|| located("agent"))?
        .to_owned();
    let supplied_id = string(&body, "id").map(str::to_owned);
    let rule_id = supplied_id.unwrap_or_else(new_rule_id);
    let result = store
        .transaction(|transaction| {
            let result = (|| {
                let agent = RulesRepository::agent(transaction, &agent_id)
                    .map_err(store_api_error)?
                    .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent"))?;
                let agent_value = serde_json::from_str::<Value>(&agent.record)
                    .map_err(|_| invalid_agent(&agent.id))?;
                validate_agent(&agent.id, &agent_value)?;
                let old = RulesRepository::get(transaction, &rule_id).map_err(store_api_error)?;
                if let Some(old) = &old {
                    let old_value = serde_json::from_str::<Value>(&old.record)
                        .map_err(|_| invalid_rule(&old.id))?;
                    if string(&old_value, "id") != Some(old.id.as_str()) {
                        return Err(invalid_rule(&old.id));
                    }
                    if string(&old_value, "agent") != Some(agent_id.as_str()) {
                        return Err(ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "This rule belongs to another agent",
                        ));
                    }
                    if old_value.get("inFlight").and_then(Value::as_bool) == Some(true)
                        || RulesRepository::has_active_check(transaction, &rule_id)
                            .map_err(store_api_error)?
                    {
                        return Err(active_check_error());
                    }
                }

                match action.as_str() {
                    "delete" | "pause" | "resume" => {
                        let Some(old) = old else {
                            return Err(ApiError::new(StatusCode::BAD_REQUEST, "Unknown rule"));
                        };
                        let mut value = serde_json::from_str::<Value>(&old.record)
                            .map_err(|_| invalid_rule(&old.id))?;
                        if action == "delete" {
                            RulesRepository::delete(transaction, &rule_id)
                                .map_err(store_api_error)?;
                            return Ok(json!({"deleted": rule_id}));
                        }
                        if action == "resume"
                            && agent_value.get("autoWake").and_then(Value::as_bool) != Some(true)
                        {
                            return Err(ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "Resume the agent before its rules",
                            ));
                        }
                        let interval = value
                            .get("intervalSeconds")
                            .and_then(Value::as_f64)
                            .unwrap_or(60.0);
                        let low_workers = string(&value, "kind") == Some("low_workers");
                        if let Some(object) = value.as_object_mut() {
                            object.remove("restartCheck");
                            object.insert(
                                "status".to_owned(),
                                Value::String(if action == "resume" {
                                    "active".to_owned()
                                } else {
                                    "paused".to_owned()
                                }),
                            );
                            object.insert("epoch".to_owned(), agent_value["epoch"].clone());
                            object.insert("nextAt".to_owned(), number(now + interval)?);
                            object.insert("error".to_owned(), Value::Null);
                            if low_workers {
                                object.insert("lowSince".to_owned(), Value::Null);
                                object.insert("alerted".to_owned(), Value::Bool(false));
                            }
                        }
                        let stored =
                            serde_json::to_string(&value).map_err(|_| ApiError::internal())?;
                        RulesRepository::put(transaction, &rule_id, &stored)
                            .map_err(store_api_error)?;
                        Ok(value)
                    }
                    "save" => {
                        let kind = string(&body, "kind").unwrap_or("interval");
                        let command = string(&body, "command").unwrap_or("");
                        let path = string(&body, "path");
                        let liveness = string(&body, "livenessCommand").unwrap_or("");
                        if kind == "low_workers" && !command.is_empty() {
                            return Err(located("command"));
                        }
                        if kind == "file" && path.is_none_or(|path| path.trim().is_empty()) {
                            return Err(located("path"));
                        }
                        if !liveness.is_empty() && kind != "file" {
                            return Err(located("livenessCommand"));
                        }
                        if kind == "low_workers"
                            && agent_value.get("isLead").and_then(Value::as_bool) != Some(true)
                        {
                            return Err(ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "Only an orchestrator can own a low-worker alert",
                            ));
                        }
                        let interval = body
                            .get("intervalSeconds")
                            .and_then(Value::as_u64)
                            .unwrap_or(60);
                        let at = body
                            .get("at")
                            .and_then(Value::as_f64)
                            .unwrap_or(now + interval as f64);
                        if !(now - 1.0..=now + 31_536_000.0).contains(&at) {
                            return Err(located("at"));
                        }
                        let stall_timeout = body
                            .get("stallTimeoutSeconds")
                            .or_else(|| body.get("stall_timeout_seconds"))
                            .and_then(Value::as_u64)
                            .unwrap_or(1800);
                        let text = string(&body, "text")
                            .unwrap_or("Check this event and continue the original task.")
                            .trim();
                        if text.is_empty() {
                            return Err(located("text"));
                        }
                        let mut record = match &old {
                            Some(old) => serde_json::from_str::<Value>(&old.record)
                                .map_err(|_| invalid_rule(&old.id))?
                                .as_object()
                                .cloned()
                                .ok_or_else(|| invalid_rule(&old.id))?,
                            None => Map::new(),
                        };
                        record.insert("id".to_owned(), Value::String(rule_id.clone()));
                        record.insert("agent".to_owned(), Value::String(agent_id.clone()));
                        record.insert(
                            "rootId".to_owned(),
                            agent_value
                                .get("rootId")
                                .cloned()
                                .unwrap_or_else(|| Value::String(agent_id.clone())),
                        );
                        record.insert("epoch".to_owned(), agent_value["epoch"].clone());
                        record.insert("name".to_owned(), trimmed_text(&body, "name")?);
                        record.insert("kind".to_owned(), Value::String(kind.to_owned()));
                        record.insert("intervalSeconds".to_owned(), json!(interval));
                        record.insert("nextAt".to_owned(), number(at)?);
                        record.insert("at".to_owned(), number(at)?);
                        record.insert(
                            "path".to_owned(),
                            if kind == "file" {
                                path.map_or(Value::Null, |v| json!(v.trim()))
                            } else {
                                Value::Null
                            },
                        );
                        record.insert(
                            "event".to_owned(),
                            json!(string(&body, "event").unwrap_or("worker_completed")),
                        );
                        record.insert("command".to_owned(), json!(command.trim()));
                        record.insert(
                            "stallTimeoutSeconds".to_owned(),
                            json!(if kind == "file" { stall_timeout } else { 0 }),
                        );
                        record.insert("livenessCommand".to_owned(), json!(liveness.trim()));
                        record
                            .entry("fileActivityAt".to_owned())
                            .or_insert(number(now)?);
                        record
                            .entry("fileGeneration".to_owned())
                            .or_insert(json!(0));
                        record
                            .entry("stallWakeGeneration".to_owned())
                            .or_insert(json!(-1));
                        record.insert("text".to_owned(), json!(text));
                        record.insert(
                            "status".to_owned(),
                            json!(if agent_value.get("autoWake").and_then(Value::as_bool)
                                == Some(true)
                            {
                                "active"
                            } else {
                                "paused"
                            }),
                        );
                        record.entry("created".to_owned()).or_insert(number(now)?);
                        record.insert("inFlight".to_owned(), Value::Bool(false));
                        record.entry("checks".to_owned()).or_insert(json!(0));
                        record.entry("wakes".to_owned()).or_insert(json!(0));
                        record.insert("fingerprint".to_owned(), Value::Null);
                        if kind == "low_workers" {
                            record.insert(
                                "minimumWorkers".to_owned(),
                                body.get("minimumWorkers").cloned().unwrap_or(json!(8)),
                            );
                            record.insert(
                                "durationMinutes".to_owned(),
                                body.get("durationMinutes").cloned().unwrap_or(json!(30)),
                            );
                            record.insert("lowSince".to_owned(), Value::Null);
                            record.insert("alerted".to_owned(), Value::Bool(false));
                            record.insert("nextAt".to_owned(), number(now)?);
                        }
                        let value = Value::Object(record);
                        let stored =
                            serde_json::to_string(&value).map_err(|_| ApiError::internal())?;
                        RulesRepository::put(transaction, &rule_id, &stored)
                            .map_err(store_api_error)?;
                        Ok(value)
                    }
                    _ => Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "Unknown rule action",
                    )),
                }
            })();
            Ok(result)
        })
        .map_err(|_| ApiError::internal())??;
    if result.get("deleted").is_some() {
        return Ok(Json(
            generated_json::<crate::generated::DeletedRuleReceipt>(result)?,
        ));
    }
    Ok(Json(
        generated_json::<crate::generated::RuleRecordResponse>(result)?,
    ))
}

fn rules_error(error: RulesError) -> ApiError {
    match error {
        RulesError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
    }
}

fn invalid_stored_rule(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object rules/{id}"),
    )
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

fn active_check_error() -> ApiError {
    dependency_unavailable("POST", "/api/rules", &ACTIVE_CHECK_DEPENDENCIES)
        .unwrap_or_else(|_| ApiError::internal())
}

fn store_api_error(_error: impl std::fmt::Debug) -> ApiError {
    ApiError::internal()
}

fn invalid_agent(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object agents/{id}"),
    )
}

fn invalid_rule(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object rules/{id}"),
    )
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

fn generated_json<T>(value: Value) -> Result<Value, ApiError>
where
    T: DeserializeOwned + Serialize,
{
    let dto = serde_json::from_value::<T>(value).map_err(|_| ApiError::internal())?;
    serde_json::to_value(dto).map_err(|_| ApiError::internal())
}
