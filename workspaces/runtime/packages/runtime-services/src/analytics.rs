//! Analytics policy over one repository snapshot; generated wire DTOs stay at the HTTP edge.
use crate::event_write::python_json_compact;
pub use repo::{AnalyticsError, AnalyticsOptions, SessionCostError};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use studio_runtime_schema::provider_kind::ProviderKind;
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
    provider: Option<ProviderKind>,
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
            && let Ok(raw) = fs::read(root.join("accounts/registry.json"))
            && let Ok(registry) = serde_json::from_slice::<Value>(&raw)
            && let Some(account) = registry.pointer(&format!("/accounts/{key}"))
        {
            result.account_label = text(account, "label");
            result.provider = ProviderKind::of_record(account).ok();
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
            let observation = if response.get("studioUsage").is_some() {
                studio_runtime_schema::usage::ledger::UsageLedgerRow::from_typed_analytics(
                    &response,
                )
                .map(|ledger| ledger.into_observation())
                .ok()
            } else {
                result.provider.and_then(|provider| {
                    studio_runtime_schema::usage::parse_analytics_usage_row(provider, &response)
                        .ok()
                })
            };
            if let Some(studio_runtime_schema::usage::UsageObservation::Response {
                tokens, ..
            }) = observation
            {
                result.tokens = Some(crate::analytics_capture::analytics_tokens(&tokens));
            }
        }
    }
    Ok(serde_json::to_value(result)?)
}

fn signature(value: impl serde::Serialize) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(
        python_json_compact(&serde_json::to_value(value)?).as_bytes(),
    ))
}

#[cfg(test)]
mod provider_identity_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn provider_identity_message_info_preserves_typed_facts_and_canonical_account_metadata() {
        let root = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        let agent = json!({"id":"agent","name":"Agent","rootId":"agent","provider":"claude","accountKey":"account","threadId":"thread","turnId":"turn","status":"running","epoch":0});
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('agent',?1)",
                    [seed.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("agent");
        let usage = json!({"threadId":"thread","turnId":"turn","studioUsage":{"kind":"response","responseId":"response","model":"claude-test","evidence":"live","tokens":{"uncachedInput":12,"cacheReadInput":3,"cacheWriteInput":2,"output":7}}});
        for (method, params) in [
            (
                "turn/started",
                json!({"threadId":"thread","turn":{"id":"turn"}}),
            ),
            (
                "item/completed",
                json!({"threadId":"thread","turnId":"turn","item":{"id":"response","type":"agentMessage","text":"Answer"}}),
            ),
            ("thread/tokenUsage/updated", usage),
        ] {
            let prepared = crate::analytics_capture::PreparedCapture::event(
                &agent,
                method,
                &params,
                1.,
                2.,
                crate::analytics_capture::CaptureSource::Live,
                Default::default(),
            )
            .expect("capture input");
            crate::analytics_capture::capture(&store, prepared)
                .await
                .expect("capture");
        }
        fs::create_dir_all(root.path().join("accounts")).expect("accounts");
        let registry = root.path().join("accounts/registry.json");
        fs::write(
            &registry,
            json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"accounts":{"account":{"provider":"claude","label":"Account"}}}).to_string(),
        )
        .expect("registry");
        let options = AnalyticsOptions {
            agent: Some("agent".into()),
            item: Some("response".into()),
            view: Some("message-info".into()),
            ..Default::default()
        };
        let known = read(&store, &options).expect("message info");
        assert_eq!(known["provider"], "claude");
        assert_eq!(known["accountLabel"], "Account");
        assert_eq!(known["tokens"]["totalTokens"], 24);
        fs::write(
            &registry,
            json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"accounts":{"account":{"provider":"future"}}}).to_string(),
        )
        .expect("unknown registry provider");
        let unknown = read(&store, &options).expect("provider-neutral message info");
        assert_eq!(unknown["tokens"], known["tokens"]);
        assert!(unknown.get("provider").is_none());
        store.analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
            tx.execute("UPDATE analytics_usage SET record=json_remove(record,'$.studioUsage','$.usageIdentity','$.receiptKey')", [])?;
            Ok(())
        }).await.expect("legacy evidence");
        assert!(
            read(&store, &options)
                .expect("unknown legacy metadata")
                .get("tokens")
                .is_none()
        );
    }
}
