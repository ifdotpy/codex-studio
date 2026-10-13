//! Analytics policy over one repository snapshot; generated wire DTOs stay at the HTTP edge.
use crate::event_write::python_json_compact;
pub use repo::{AnalyticsError, AnalyticsOptions, SessionCostError};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
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
fn valid_cost_result(v: &Value, root: &str) -> bool {
    v.is_object()
        && v.get("rootId").and_then(Value::as_str) == Some(root)
        && v.get("pricingState").and_then(Value::as_str) == Some("ready")
        && v.get("unknownModels").is_some_and(Value::is_array)
        && v.get("pricedSamples").and_then(Value::as_i64).is_some()
        && v.get("estimated").and_then(Value::as_bool).is_some()
        && v.pointer("/breakdown/providers")
            .is_some_and(Value::is_object)
        && v.pointer("/breakdown/models").is_some_and(Value::is_object)
        && v.get("totalUSD")
            .is_some_and(|x| x.is_null() || x.is_number())
}
fn has_claude_history(record: &Value) -> bool {
    record.get("provider").and_then(Value::as_str) == Some("claude")
        || record
            .get("accountHistory")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .any(|h| h.get("provider").and_then(Value::as_str) == Some("claude"))
            })
}
pub(crate) fn current_account_is_claude(
    record: &Value,
    account_providers: &BTreeMap<String, String>,
) -> bool {
    let embedded = record
        .get("provider")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    embedded.is_none()
        && account_providers
            .get(
                record
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default"),
            )
            .is_some_and(|provider| provider == "claude")
}
fn pricing_signature(root: &Path) -> Option<String> {
    let path = root.join("model-pricing/models-dev-v1.json");
    let Ok(raw) = fs::read(path) else {
        return None;
    };
    let Ok(artifact) = serde_json::from_slice::<Value>(&raw) else {
        return None;
    };
    if artifact.get("version").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    let catalog = artifact
        .get("catalog")
        .filter(|value| valid_catalog(value))?;
    signature(catalog).ok()
}
fn valid_catalog(value: &Value) -> bool {
    let Some(providers) = value.get("providers").and_then(Value::as_object) else {
        return false;
    };
    ["openai", "anthropic"].into_iter().all(|provider| {
        providers
            .get(provider)
            .and_then(|value| value.get("models"))
            .and_then(Value::as_object)
            .is_some_and(|models| {
                models.values().any(|model| {
                    let Some(cost) = model.get("cost") else {
                        return false;
                    };
                    ["input", "output"].into_iter().all(|field| {
                        cost.get(field)
                            .and_then(Value::as_f64)
                            .is_some_and(|amount| amount.is_finite() && amount >= 0.0)
                    })
                })
            })
    })
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

type CostProjection = (String, i64, Option<(u64, u64)>, i64);
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CostSource {
    usage: repo::UsageState,
    pricing_signature: Option<String>,
    agents_signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_providers_signature: Option<String>,
    claude_signature: String,
    cost_projection: Option<CostProjection>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedCost {
    version: i64,
    root_id: String,
    source: Value,
    cached_at: f64,
    result: Value,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CachedCost {
    #[serde(flatten)]
    result: Value,
    cache_age_seconds: f64,
    refreshing: bool,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSignature<'a> {
    root_id: &'a Value,
    account_key: &'a Value,
    provider: &'a Value,
    thread_id: &'a Value,
    account_history: &'a Value,
}
#[derive(serde::Serialize)]
struct ClaudeSignature {
    configs: [(); 0],
    files: [(); 0],
}
fn signature(value: impl serde::Serialize) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(
        python_json_compact(&serde_json::to_value(value)?).as_bytes(),
    ))
}
fn account_providers_signature(
    providers: &BTreeMap<String, String>,
) -> Result<String, serde_json::Error> {
    signature(providers)
}
pub fn persisted_session_cost(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<Option<Value>, SessionCostError> {
    persisted_session_cost_with_accounts(store, agent_id, &BTreeMap::new())
}

pub fn persisted_session_cost_with_accounts(
    store: &RuntimeStore,
    agent_id: &str,
    account_providers: &BTreeMap<String, String>,
) -> Result<Option<Value>, SessionCostError> {
    if !store.root().join("analytics.sqlite3").is_file() {
        return Err(SessionCostError::RefreshRequired);
    }
    let mut db = AnalyticsRepository::new(store.root()).open_read_only()?;
    let tx = db.transaction()?;
    let root = repo::cost_root(&tx, agent_id)?;
    let members = repo::cost_agents(&tx, agent_id, &root)?;
    if members
        .iter()
        .any(|(_, r)| has_claude_history(r) || current_account_is_claude(r, account_providers))
    {
        return Ok(None);
    }
    let cost_projection = repo::cost_projection(&tx, &root)?
        .map(|(token, revision, floor)| (token, revision, file_identity(store.root()), floor));
    let source = serde_json::to_value(CostSource {
        usage: repo::usage_signature(&tx, &root)?,
        pricing_signature: pricing_signature(store.root()),
        cost_projection,
        agents_signature: signature(
            members
                .iter()
                .map(|(id, row)| {
                    (
                        id,
                        AgentSignature {
                            root_id: &row["rootId"],
                            account_key: &row["accountKey"],
                            provider: &row["provider"],
                            thread_id: &row["threadId"],
                            account_history: &row["accountHistory"],
                        },
                    )
                })
                .collect::<Vec<_>>(),
        )?,
        account_providers_signature: if account_providers.is_empty() {
            None
        } else {
            Some(account_providers_signature(account_providers)?)
        },
        claude_signature: signature(ClaudeSignature {
            configs: [],
            files: [],
        })?,
    })?;
    tx.commit()?;
    let path = store
        .root()
        .join("session-costs")
        .join(format!("cost-{}.json", sha256_hex(root.as_bytes())));
    let saved = fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<SavedCost>(&raw).ok());
    let Some(saved) = saved.filter(|v| {
        v.version == 2
            && v.root_id == root
            && v.source == source
            && valid_cost_result(&v.result, &root)
    }) else {
        return Ok(None);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Ok(Some(serde_json::to_value(CachedCost {
        result: saved.result,
        cache_age_seconds: ((now - saved.cached_at).max(0.0) * 10.0).round() / 10.0,
        refreshing: false,
    })?))
}
#[cfg(unix)]
fn file_identity(root: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(root.join("analytics.sqlite3"))
        .ok()
        .map(|m| (m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn file_identity(_: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod costs_account_tests {
    use super::{account_providers_signature, current_account_is_claude};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn current_account_provider_fills_missing_embedded_provider() {
        let providers = BTreeMap::from([("claude-account".to_owned(), "claude".to_owned())]);
        assert!(current_account_is_claude(
            &json!({"accountKey":"claude-account"}),
            &providers
        ));
        assert!(!current_account_is_claude(
            &json!({"accountKey":"claude-account","provider":"openai"}),
            &providers
        ));
    }

    #[test]
    fn account_provider_cache_signature_tracks_provider_changes() {
        let first = BTreeMap::from([("account".to_owned(), "openai".to_owned())]);
        let second = BTreeMap::from([("account".to_owned(), "claude".to_owned())]);
        assert_ne!(
            account_providers_signature(&first).expect("serializable account map"),
            account_providers_signature(&second).expect("serializable account map")
        );
        assert_ne!(
            account_providers_signature(&first).expect("serializable account map"),
            account_providers_signature(&BTreeMap::new()).expect("serializable account map")
        );
    }
}
