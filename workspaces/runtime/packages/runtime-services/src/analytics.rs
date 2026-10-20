//! Analytics policy over one repository snapshot; generated wire DTOs stay at the HTTP edge.
use crate::event_write::python_json_compact;
pub use repo::{AnalyticsError, AnalyticsOptions, SessionCostError};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use studio_runtime_store::analytics::{self as repo, AnalyticsRepository};
use studio_runtime_store::sha256_hex;
use studio_runtime_store::{Connection, RuntimeStore};

/// Produce the route's JSON representation using one read-only SQLite snapshot.
pub fn read(store: &RuntimeStore, options: &AnalyticsOptions) -> Result<Value, AnalyticsError> {
    let scope = options.scope.as_deref().unwrap_or("agent");
    if !matches!(scope, "agent" | "team" | "all") {
        return Err(AnalyticsError::InvalidRequest("Unknown analytics scope"));
    }
    if let (Some(start), Some(end)) = (options.from, options.to)
        && start > end
    {
        return Err(AnalyticsError::InvalidRequest(
            "Analytics start is after end",
        ));
    }
    let repo = AnalyticsRepository::new(store.root());
    let mut db = repo.open_read_only()?;
    let transaction = db.transaction()?;
    let value = match options.view.as_deref() {
        Some("detail") => repo::detail(&transaction, options, scope)?,
        Some("turn-errors") => turn_errors(&transaction, options, scope)?,
        Some("message-info") if scope != "agent" => {
            return Err(AnalyticsError::InvalidRequest(
                "Select one agent for message metadata",
            ));
        }
        Some("message-info") => message_info(&transaction, store.root(), options)?,
        Some(_) => return Err(AnalyticsError::InvalidRequest("Unknown analytics view")),
        None => {
            let mut report = repo::rollup(&transaction, options, scope)?;
            report.chart_buckets = compact_chart(report.chart_buckets, options.export);
            serde_json::to_value(report)?
        }
    };
    transaction.commit()?;
    Ok(value)
}

fn compact_chart(points: Vec<Value>, export: bool) -> Vec<Value> {
    let mut buckets: HashMap<i64, Value> = HashMap::new();
    let mut width = 3600.0;
    for point in points {
        let bucket = (number_at(&point, "at").unwrap_or_default() / width) as i64;
        if buckets
            .get(&bucket)
            .is_none_or(|old| context_percent(old) < context_percent(&point))
        {
            buckets.insert(bucket, point);
        }
        if !export && buckets.len() > 500 {
            width *= 2.0;
            let mut compacted: HashMap<i64, Value> = HashMap::new();
            for point in buckets.into_values() {
                let key = (number_at(&point, "at").unwrap_or_default() / width) as i64;
                if compacted
                    .get(&key)
                    .is_none_or(|old| context_percent(old) < context_percent(&point))
                {
                    compacted.insert(key, point);
                }
            }
            buckets = compacted;
        }
    }
    let mut points: Vec<_> = buckets.into_values().collect();
    points.sort_by(|a, b| {
        number_at(a, "at")
            .partial_cmp(&number_at(b, "at"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    points
}
fn number_at(row: &Value, key: &str) -> Option<f64> {
    row.get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn context_percent(row: &Value) -> Option<f64> {
    Some(row.pointer("/last/totalTokens")?.as_f64()? / row.get("modelContextWindow")?.as_f64()?)
        .filter(|n| n.is_finite())
}
fn turn_errors(
    db: &Connection,
    options: &AnalyticsOptions,
    scope: &str,
) -> Result<Value, AnalyticsError> {
    if scope != "agent" || options.thread.as_deref().unwrap_or_default().is_empty() {
        return Err(AnalyticsError::InvalidRequest(
            "Select one thread and up to 120 turns",
        ));
    }
    let agent = options.agent.as_deref().unwrap_or_default();
    repo::require_agent(db, agent)?;
    let list: Vec<_> = options
        .turns
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if list.is_empty() || list.len() > 120 || list.iter().any(|s| s.is_empty()) {
        return Err(AnalyticsError::InvalidRequest(
            "Select one thread and up to 120 turns",
        ));
    }
    let keys: Vec<_> = list
        .iter()
        .map(|turn| {
            format!(
                "{}:{}:{}",
                options.agent.as_deref().unwrap_or_default(),
                options.thread.as_deref().unwrap_or_default(),
                turn
            )
        })
        .collect();
    repo::turn_errors(db, keys)
}
/// Cache signature of a models.dev catalog document.
pub(crate) fn catalog_signature(catalog: &Value) -> Option<String> {
    signature(catalog).ok()
}

#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_duration_ms: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<Value>,
}
fn text(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn unique(mut rows: Vec<Value>) -> Option<Value> {
    if rows.len() == 1 { rows.pop() } else { None }
}
fn message_info(
    db: &Connection,
    root: &Path,
    options: &AnalyticsOptions,
) -> Result<Value, AnalyticsError> {
    let agent = options.agent.as_deref().unwrap_or_default();
    let item_id = options.item.as_deref().unwrap_or_default();
    if item_id.is_empty() || item_id.len() > 512 {
        return Err(AnalyticsError::InvalidRequest("Select a message item"));
    }
    repo::require_agent(db, agent)?;
    let item = repo::message_item(db, agent, item_id, options.thread.as_deref())?.filter(|i| {
        i["type"] == "agentMessage" && options.turn.as_deref().is_none_or(|t| i["turnId"] == t)
    });
    let mut result = MessageInfo::default();
    let (thread, turn_id) = if let Some(item) = &item {
        result.at = item
            .get("finishedAt")
            .filter(|v| !v.is_null())
            .or_else(|| item.get("startedAt"))
            .or_else(|| item.get("at"))
            .and_then(Value::as_number)
            .cloned();
        (text(item, "threadId"), text(item, "turnId"))
    } else {
        (options.thread.clone(), options.turn.clone())
    };
    if let Some(turn_id) = turn_id
        && let Some(turn) = repo::message_turn(db, agent, &turn_id, thread.as_deref())?
    {
        let thread = turn["threadId"].as_str();
        if let Some(run) = repo::message_run(db, agent, &turn_id, thread)? {
            result.run_id = text(&run, "id");
            result.attempt_id = text(&run, "rootAttemptId");
        }
        result.model = text(&turn, "model").filter(|s| !s.is_empty());
        result.effort = text(&turn, "effort").filter(|s| !s.is_empty());
        result.account_key = text(&turn, "accountKey").filter(|s| !s.is_empty());
        if let Some(key) = &result.account_key
            && let Ok(raw) = fs::read(root.join("registry.json"))
            && let Ok(registry) = serde_json::from_slice::<Value>(&raw)
            && let Some(account) = registry.pointer(&format!("/accounts/{key}"))
        {
            result.account_label = text(account, "label");
            if account.is_object() {
                result.provider = if account.get("provider").is_some() {
                    text(account, "provider")
                } else {
                    Some("codex".into())
                };
            }
        }
        result.turn_duration_ms = turn
            .get("nativeDurationMs")
            .or_else(|| turn.get("durationMs"))
            .and_then(Value::as_number)
            .cloned();
        let usage = repo::message_usage(db, agent, thread, &turn_id, None)?;
        let exact = unique(repo::message_usage(
            db,
            agent,
            thread,
            &turn_id,
            Some(item_id),
        )?);
        let response = if exact.is_some() {
            exact
        } else if item.is_some()
            && usage.len() == 1
            && matches!(
                turn["status"].as_str(),
                Some("completed" | "failed" | "interrupted")
            )
            && repo::unambiguous_message(db, agent, thread, &turn_id, item_id)?
        {
            unique(usage)
        } else {
            None
        };
        if let Some(response) = response {
            result.model = text(&response, "model")
                .filter(|s| !s.is_empty())
                .or(result.model);
            result.tokens = response
                .get("requestUsage")
                .or_else(|| response.get("last"))
                .filter(|v| !v.is_null())
                .cloned();
        }
    }
    Ok(serde_json::to_value(result)?)
}

fn signature(value: impl serde::Serialize) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(
        python_json_compact(&serde_json::to_value(value)?).as_bytes(),
    ))
}
