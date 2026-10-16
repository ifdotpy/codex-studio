//! Read-only session cost estimates over the shared analytics store.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use studio_runtime_store::{
    AnalyticsRepository, AnalyticsStoreError, CostUsageRepositoryError, RuntimeStore, analytics,
    read_cost_usage,
};
use thiserror::Error;

use crate::cost_usage::{CostUsageError, CostUsageSnapshot};
use crate::pricing::PricingCatalog;

#[derive(Debug, Error)]
pub enum CostsError {
    #[error("costs require dependency {0}")]
    Dependency(&'static str),
    #[error(transparent)]
    Analytics(#[from] analytics::SessionCostError),
    #[error(transparent)]
    Repository(#[from] AnalyticsStoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Usage(#[from] CostUsageRepositoryError),
    #[error(transparent)]
    Grouping(#[from] CostUsageError),
    #[error("pricing artifact is invalid")]
    InvalidPricing,
    #[error("cost computation failed")]
    Computation,
    #[error("Claude session costs require account profile snapshots")]
    AccountProfilesRequired,
}

/// Return a current estimate, using the existing Python-published cache when valid.
pub fn session_cost(store: &RuntimeStore, agent_id: &str) -> Result<Value, CostsError> {
    session_cost_with_accounts(store, agent_id, &BTreeMap::new())
}

pub fn session_cost_with_accounts(
    store: &RuntimeStore,
    agent_id: &str,
    account_providers: &BTreeMap<String, String>,
) -> Result<Value, CostsError> {
    let accounts = account_providers
        .iter()
        .map(|(key, provider)| (key.clone(), json!({"provider": provider})))
        .collect::<BTreeMap<_, _>>();
    session_cost_inner(store, agent_id, &accounts, false)
}

/// Return a session estimate with account snapshots used to locate local Claude profiles.
pub fn session_cost_with_account_profiles(
    store: &RuntimeStore,
    agent_id: &str,
    accounts: &BTreeMap<String, Value>,
) -> Result<Value, CostsError> {
    session_cost_inner(store, agent_id, accounts, true)
}

fn session_cost_inner(
    store: &RuntimeStore,
    agent_id: &str,
    accounts: &BTreeMap<String, Value>,
    has_account_profiles: bool,
) -> Result<Value, CostsError> {
    let account_providers = accounts
        .iter()
        .filter_map(|(key, account)| {
            account
                .get("provider")
                .and_then(Value::as_str)
                .map(|provider| (key.clone(), provider.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    if let Some(value) =
        crate::analytics::persisted_session_cost_with_accounts(store, agent_id, &account_providers)?
    {
        return Ok(value);
    }
    let repository = AnalyticsRepository::new(store.root());
    let mut db = repository.open_read_only()?;
    let tx = db.transaction()?;
    let root = analytics::cost_root(&tx, agent_id)?;
    let generation = analytics::usage_signature(&tx, &root)?.generation();
    let members = analytics::cost_agents(&tx, agent_id, &root)?;
    let mut providers_by_account = BTreeMap::new();
    let mut has_claude_history = false;
    for (_, record) in &members {
        let provider = record
            .get("provider")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        has_claude_history |= provider == Some("claude");
        let account_key = record
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let resolved_provider = provider.or_else(|| {
            account_providers
                .get(account_key)
                .map(String::as_str)
                .filter(|value| !value.is_empty())
        });
        has_claude_history |=
            crate::analytics::current_account_is_claude(record, &account_providers);
        if let Some(resolved) = resolved_provider.and_then(pricing_provider) {
            providers_by_account.insert(account_key.to_owned(), resolved.to_owned());
        }
        if let (Some(key), Some(provider)) =
            (record.get("accountKey").and_then(Value::as_str), provider)
            && let Some(provider) = pricing_provider(provider)
        {
            providers_by_account.insert(key.to_owned(), provider.to_owned());
        }
        if let Some(history) = record.get("accountHistory").and_then(Value::as_array) {
            for entry in history {
                let (Some(key), Some(provider)) = (
                    entry.get("accountKey").and_then(Value::as_str),
                    entry.get("provider").and_then(Value::as_str),
                ) else {
                    continue;
                };
                has_claude_history |= provider == "claude";
                if let Some(provider) = pricing_provider(provider) {
                    providers_by_account.insert(key.to_owned(), provider.to_owned());
                }
            }
        }
    }
    if has_claude_history && !has_account_profiles {
        return Err(CostsError::AccountProfilesRequired);
    }
    let rows = read_cost_usage(&tx, &root)?;
    tx.commit()?;
    let local_claude = read_local_claude_usage(store.root(), &members, accounts);
    let receipts = local_claude
        .iter()
        .map(|row| {
            (
                row.account_key.clone(),
                row.thread_id.clone(),
                row.id.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    let snapshot = CostUsageSnapshot::from_records(rows.into_iter().filter_map(|row| {
        if let (Some(account_key), Some(thread_id), Some(response_id)) = (
            row.record.get("accountKey").and_then(Value::as_str),
            row.thread.as_deref(),
            row.record.get("responseId").and_then(Value::as_str),
        ) && receipts.contains(&(
            account_key.to_owned(),
            thread_id.to_owned(),
            response_id.to_owned(),
        )) {
            return None;
        }
        Some((
            row.sequence,
            row.agent,
            row.thread,
            row.turn,
            row.at,
            row.record,
        ))
    }))?;

    let pricing_path = store.root().join("model-pricing/models-dev-v1.json");
    let raw =
        fs::read(pricing_path).map_err(|_| CostsError::Dependency("costs/pricing-refresh"))?;
    let artifact: Value = serde_json::from_slice(&raw).map_err(|_| CostsError::InvalidPricing)?;
    if artifact.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(CostsError::InvalidPricing);
    }
    let catalog_value = artifact.get("catalog").ok_or(CostsError::InvalidPricing)?;
    let catalog =
        PricingCatalog::from_value(catalog_value).map_err(|_| CostsError::InvalidPricing)?;

    let mut provider_totals = BTreeMap::<String, f64>::new();
    let mut model_totals = BTreeMap::<String, f64>::new();
    let mut unknown = BTreeSet::new();
    let mut priced_count = 0_u64;
    let mut total = 0.0;
    let mut tier_used = false;
    for group in snapshot.groups() {
        let model = group.model.as_deref().unwrap_or("Unknown model");
        if model == "<synthetic>" {
            continue;
        }
        let provider = provider_for(model).or_else(|| {
            providers_by_account
                .get(&group.account_key)
                .map(String::as_str)
        });
        let Some(provider) = provider else {
            unknown.insert(model.to_owned());
            continue;
        };
        let usage = json!({
            "inputTokens": group.input_tokens,
            "cachedInputTokens": group.cached_tokens,
            "cacheWriteInputTokens": group.write_tokens,
            "outputTokens": group.output_tokens,
        });
        let context = group.input_tokens.map(|input| {
            if group.input_uncached {
                input + group.cached_tokens.unwrap_or(0.0) + group.write_tokens.unwrap_or(0.0)
            } else {
                input
            }
        });
        let price = catalog.price_usage(provider, model, &usage, context, group.input_uncached);
        let Some(amount) = price.amount_usd.filter(|amount| amount.is_finite()) else {
            unknown.insert(model.to_owned());
            continue;
        };
        let count = group.count as f64;
        let subtotal = amount * count;
        total += subtotal;
        priced_count = priced_count.saturating_add(group.count as u64);
        *provider_totals.entry(provider.to_owned()).or_default() += subtotal;
        *model_totals.entry(model.to_owned()).or_default() += subtotal;
        tier_used |= price.context_tier_used;
    }
    for row in &local_claude {
        let usage = json!({
            "inputTokens": row.input_tokens,
            "cachedInputTokens": row.cached_tokens,
            "cacheWriteInputTokens": row.write_tokens,
            "outputTokens": row.output_tokens,
        });
        let context = row.input_tokens.map(|input| {
            if row.input_tokens_are_uncached {
                input + row.cached_tokens.unwrap_or(0.0) + row.write_tokens.unwrap_or(0.0)
            } else {
                input
            }
        });
        let price = catalog.price_usage(
            "anthropic",
            &row.model,
            &usage,
            context,
            row.input_tokens_are_uncached,
        );
        let Some(amount) = price.amount_usd.filter(|amount| amount.is_finite()) else {
            unknown.insert(row.model.clone());
            continue;
        };
        total += amount;
        priced_count = priced_count.saturating_add(1);
        *provider_totals.entry("anthropic".to_owned()).or_default() += amount;
        *model_totals.entry(row.model.clone()).or_default() += amount;
        tier_used |= price.context_tier_used;
    }
    if !total.is_finite() {
        return Err(CostsError::Computation);
    }
    Ok(json!({
        "rootId": root,
        "generation": generation,
        "totalUSD": if priced_count == 0 { Value::Null } else { json!(total) },
        "pricedSamples": priced_count,
        "breakdown": {"providers": provider_totals, "models": model_totals},
        "unknownModels": unknown,
        "estimated": true,
        "pricingState": "ready",
        "cacheAgeSeconds": 0,
        "refreshing": false,
        "claudeHistoryIncomplete": has_claude_history,
        "method": format!("API prices from models.dev; cached input rates are applied when reported.{}{}",
            if has_claude_history { " Earlier Claude totals can be incomplete because earlier responses may not have separate saved usage rows." } else { "" },
            if tier_used { " Published context tier applied where request size matched its threshold." }
            else { " Base rates used when request size was unavailable or below the published threshold." }),
    }))
}

// Python contract owner: codex_session_costs.py:_thread_ids.
const MAX_SESSION_ID_BYTES: usize = 100;
const MAX_SESSION_STATE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
struct LocalClaudeUsage {
    account_key: String,
    thread_id: String,
    id: String,
    model: String,
    input_tokens: Option<f64>,
    cached_tokens: Option<f64>,
    write_tokens: Option<f64>,
    output_tokens: Option<f64>,
    input_tokens_are_uncached: bool,
}

fn read_local_claude_usage(
    state_root: &Path,
    members: &[(String, Value)],
    accounts: &BTreeMap<String, Value>,
) -> Vec<LocalClaudeUsage> {
    let mut sessions = BTreeSet::new();
    for (_, member) in members {
        let current_key = member
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        if account_claude_config(current_key, accounts).is_some()
            && let Some(thread_id) = member.get("threadId").and_then(Value::as_str)
        {
            sessions.insert((current_key.to_owned(), thread_id.to_owned()));
        }
        if let Some(history) = member.get("accountHistory").and_then(Value::as_array) {
            for entry in history {
                if entry.get("provider").and_then(Value::as_str) != Some("claude") {
                    continue;
                }
                let (Some(account_key), Some(thread_id)) = (
                    entry.get("accountKey").and_then(Value::as_str),
                    entry.get("threadId").and_then(Value::as_str),
                ) else {
                    continue;
                };
                if account_claude_config(account_key, accounts).is_some() {
                    sessions.insert((account_key.to_owned(), thread_id.to_owned()));
                }
            }
        }
    }

    let mut rows = BTreeMap::new();
    for (account_key, thread_id) in sessions {
        if !valid_session_id(&account_key) || !valid_session_id(&thread_id) {
            continue;
        }
        let Some(config) = account_claude_config(&account_key, accounts) else {
            continue;
        };
        let mut ids = BTreeSet::from([thread_id.clone()]);
        let server_root = if account_key == "default" {
            state_root.to_path_buf()
        } else {
            state_root.join("account-servers").join(&account_key)
        };
        read_session_native_ids(
            &server_root
                .join("sessions")
                .join(format!("{thread_id}.json")),
            &mut ids,
        );
        let projects = config.join("projects");
        let Ok(project_entries) = fs::read_dir(projects) else {
            continue;
        };
        for project in project_entries.flatten() {
            let project_path = project.path();
            if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            for id in &ids {
                let path = project_path.join(format!("{id}.jsonl"));
                read_claude_log(&path, &account_key, &thread_id, &mut rows);
            }
        }
    }
    rows.into_values().collect()
}

fn account_claude_config(account_key: &str, accounts: &BTreeMap<String, Value>) -> Option<PathBuf> {
    let account = accounts.get(account_key)?;
    if account.get("provider").and_then(Value::as_str) != Some("claude") {
        return None;
    }
    let config_dir = account
        .get("claudeOptions")
        .and_then(|options| options.get("configDir"))
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    let home = account
        .get("home")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    let path = config_dir
        .or(home)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))?;
    Some(expand_home(path))
}

fn expand_home(path: PathBuf) -> PathBuf {
    if path == Path::new("~") {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
    }
    if let Ok(suffix) = path.strip_prefix("~/") {
        return std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(suffix))
            .unwrap_or(path);
    }
    path
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn read_session_native_ids(path: &Path, ids: &mut BTreeSet<String>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() > MAX_SESSION_STATE_BYTES {
        return;
    }
    let Ok(raw) = fs::read(path) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
        return;
    };
    let mut add = |value: Option<&Value>| {
        if let Some(value) = value
            .and_then(Value::as_str)
            .filter(|id| valid_session_id(id))
        {
            ids.insert(value.to_owned());
        }
    };
    add(value.get("nativeId"));
    if let Some(branches) = value.get("historyBranches").and_then(Value::as_array) {
        for branch in branches {
            add(branch.get("nativeId"));
        }
    }
    if let Some(controls) = value.get("controlRequests").and_then(Value::as_object) {
        for control in controls.values() {
            add(control.get("sourceNativeId"));
            add(control.get("targetNativeId"));
        }
    }
}

fn read_claude_log(
    path: &Path,
    account_key: &str,
    thread_id: &str,
    rows: &mut BTreeMap<(String, String, String), LocalClaudeUsage>,
) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&line);
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message) = row.get("message") else {
            continue;
        };
        let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty() && *model != "<synthetic>")
        else {
            continue;
        };
        if !valid_claude_timestamp(row.get("timestamp").and_then(Value::as_str)) {
            continue;
        }
        let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) else {
            continue;
        };
        let identity = [message.get("id"), row.get("requestId"), row.get("uuid")]
            .into_iter()
            .flatten()
            .find(|value| is_json_truthy(value));
        let Some(id) = identity
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let input_values = [
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .map(|name| usage.get(name).map_or(Some(0.0), nonnegative_number));
        let (input_tokens, cached_tokens, write_tokens) =
            if input_values.iter().all(Option::is_some) {
                (input_values[0], input_values[1], input_values[2])
            } else {
                (None, None, None)
            };
        let record = LocalClaudeUsage {
            account_key: account_key.to_owned(),
            thread_id: thread_id.to_owned(),
            id: id.to_owned(),
            model: model.to_owned(),
            input_tokens,
            cached_tokens,
            write_tokens,
            output_tokens: usage.get("output_tokens").and_then(Value::as_f64),
            input_tokens_are_uncached: input_tokens.is_some(),
        };
        rows.entry((account_key.to_owned(), thread_id.to_owned(), id.to_owned()))
            .or_insert(record);
    }
}

fn nonnegative_number(value: &Value) -> Option<f64> {
    let number = value.as_f64()?;
    (number >= 0.0).then_some(number)
}

fn is_json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn valid_claude_timestamp(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        chrono::DateTime::parse_from_rfc3339(value).is_ok()
            || chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
            || chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f").is_ok()
            || chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
    })
}

fn provider_for(model: &str) -> Option<&'static str> {
    if model.starts_with("claude-")
        || matches!(
            model.strip_suffix("[1m]").unwrap_or(model),
            "opus" | "sonnet" | "haiku" | "fable"
        )
    {
        Some("anthropic")
    } else if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        Some("openai")
    } else {
        None
    }
}

fn pricing_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" | "codex" => Some("openai"),
        "anthropic" | "claude" => Some("anthropic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CostsError, read_local_claude_usage, session_cost, session_cost_with_account_profiles,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::fs;
    use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository, CaptureTable};
    use studio_runtime_store::{RuntimeStore, StoreError};
    use tempfile::tempdir;

    #[test]
    fn local_claude_session_usage_is_thread_scoped_deduplicated_and_uses_account_history() {
        let temp = tempdir().expect("temporary fixture directory");
        let state = temp.path().join("state");
        let current_profile = temp.path().join("current-claude");
        let previous_profile = temp.path().join("previous-claude");
        let current_project = current_profile.join("projects/project");
        let previous_project = previous_profile.join("projects/project");
        fs::create_dir_all(&current_project).expect("create current project");
        fs::create_dir_all(&previous_project).expect("create previous project");
        fs::create_dir_all(state.join("account-servers/current/sessions"))
            .expect("create account session state");
        fs::write(
            state.join("account-servers/current/sessions/thread-current.json"),
            json!({
                "nativeId": "native-current",
                "historyBranches": [{"nativeId": "branch-current"}],
                "controlRequests": {"control-1": {
                    "sourceNativeId": "source-current",
                    "targetNativeId": "target-current"
                }}
            })
            .to_string(),
        )
        .expect("write current session");
        let valid_row = json!({
            "type": "assistant",
            "timestamp": "2026-09-01T12:00:00Z",
            "message": {
                "id": "response-1",
                "model": "claude-sonnet-4-5",
                "usage": {
                    "input_tokens": 100,
                    "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 5,
                    "output_tokens": 10
                }
            }
        });
        let invalid_identity = json!({
            "type": "assistant",
            "timestamp": "2026-09-01T12:00:00Z",
            "requestId": "must-not-be-used",
            "message": {"id": 7, "model": "claude-sonnet-4-5", "usage": {}}
        });
        let naive_timestamp = json!({
            "type": "assistant",
            "timestamp": "2026-09-01T12:00:00",
            "message": {
                "id": "response-naive-time",
                "model": "claude-sonnet-4-5",
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "output_tokens": 2
                }
            }
        });
        fs::write(
            current_project.join("thread-current.jsonl"),
            format!("{valid_row}\n{invalid_identity}\n{naive_timestamp}\n"),
        )
        .expect("write primary current history rows");
        for id in [
            "native-current",
            "branch-current",
            "source-current",
            "target-current",
        ] {
            fs::write(
                current_project.join(format!("{id}.jsonl")),
                format!("{}\n", valid_row),
            )
            .expect("write current history row");
        }
        fs::write(
            current_project.join("unrelated-thread.jsonl"),
            format!("{}\n", valid_row),
        )
        .expect("write unrelated history row");
        let mut malformed_tokens = valid_row.clone();
        malformed_tokens["message"]["id"] = json!("response-2");
        malformed_tokens["message"]["usage"]["input_tokens"] = json!(true);
        fs::write(
            previous_project.join("thread-previous.jsonl"),
            format!("{}\n", malformed_tokens),
        )
        .expect("write previous history row");
        let accounts = BTreeMap::from([
            (
                "current".to_owned(),
                json!({"provider":"claude", "claudeOptions":{"configDir":current_profile}}),
            ),
            (
                "previous".to_owned(),
                json!({"provider":"claude", "claudeOptions":{"configDir":previous_profile}}),
            ),
        ]);
        let members = vec![(
            "agent".to_owned(),
            json!({
                "accountKey":"current",
                "threadId":"thread-current",
                "accountHistory":[{
                    "accountKey":"previous",
                    "threadId":"thread-previous",
                    "provider":"claude"
                }]
            }),
        )];

        let rows = read_local_claude_usage(&state, &members, &accounts);
        assert_eq!(rows.len(), 3);
        assert!(!rows.iter().any(|row| row.id == "must-not-be-used"));
        assert!(rows.iter().any(|row| row.id == "response-naive-time"));
        let current = rows
            .iter()
            .find(|row| row.account_key == "current")
            .expect("current session row");
        assert_eq!(current.thread_id, "thread-current");
        assert_eq!(current.id, "response-1");
        assert_eq!(current.input_tokens, Some(100.0));
        assert_eq!(current.cached_tokens, Some(20.0));
        assert_eq!(current.write_tokens, Some(5.0));
        assert_eq!(current.output_tokens, Some(10.0));
        assert!(current.input_tokens_are_uncached);
        let previous = rows
            .iter()
            .find(|row| row.account_key == "previous")
            .expect("historical session row");
        assert_eq!(previous.thread_id, "thread-previous");
        assert_eq!(previous.input_tokens, None);
        assert_eq!(previous.cached_tokens, None);
        assert_eq!(previous.write_tokens, None);
        assert!(!previous.input_tokens_are_uncached);
    }

    #[test]
    fn non_claude_accounts_do_not_read_local_project_logs() {
        let temp = tempdir().expect("temporary fixture directory");
        let config = temp.path().join("profile");
        fs::create_dir_all(config.join("projects/project")).expect("create project");
        fs::write(
            config.join("projects/project/thread.jsonl"),
            format!("{}\n", json!({"type":"assistant"})),
        )
        .expect("write ignored history");
        let accounts = BTreeMap::from([(
            "account".to_owned(),
            json!({"provider":"openai", "claudeOptions":{"configDir":config}}),
        )]);
        let members: Vec<(String, Value)> = vec![(
            "agent".to_owned(),
            json!({"accountKey":"account", "threadId":"thread"}),
        )];
        assert!(read_local_claude_usage(temp.path(), &members, &accounts).is_empty());
    }

    #[tokio::test]
    async fn session_cost_prices_claude_history_and_suppresses_analytics_receipts() {
        let temp = tempdir().expect("temporary fixture directory");
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("runtime store opens");
        let profile = temp.path().join("claude-profile");
        let project = profile.join("projects/project");
        fs::create_dir_all(project).expect("create Claude project");
        fs::create_dir_all(temp.path().join("account-servers/claude/sessions"))
            .expect("create account session directory");
        fs::write(
            temp.path()
                .join("account-servers/claude/sessions/thread.json"),
            json!({"nativeId":"native-thread"}).to_string(),
        )
        .expect("write session metadata");
        let local_row = json!({
            "type":"assistant",
            "timestamp":"2026-09-01T12:00:00Z",
            "message":{
                "id":"response-1",
                "model":"claude-sonnet-4-5",
                "usage":{
                    "input_tokens":100,
                    "cache_read_input_tokens":20,
                    "cache_creation_input_tokens":5,
                    "output_tokens":10
                }
            }
        });
        let incomplete_row = json!({
            "type":"assistant",
            "timestamp":"2026-09-01T12:01:00Z",
            "message":{
                "id":"response-2",
                "model":"claude-sonnet-4-5",
                "usage":{"input_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}
            }
        });
        for session_id in ["thread", "native-thread"] {
            fs::write(
                temp.path()
                    .join("claude-profile/projects/project")
                    .join(format!("{session_id}.jsonl")),
                format!("{local_row}\n{incomplete_row}\n"),
            )
            .expect("write local cost rows");
        }
        fs::create_dir_all(temp.path().join("model-pricing")).expect("create pricing directory");
        fs::write(
            temp.path().join("model-pricing/models-dev-v1.json"),
            json!({
                "version":1,
                "catalog":{"providers":{
                    "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":1}}}},
                    "anthropic":{"models":{"claude-sonnet-4-5":{"cost":{
                        "input":1,"output":2,"cache_read":0.5,"cache_write":1
                    }}}}
                }}
            })
            .to_string(),
        )
        .expect("write pricing catalog");
        let agent = json!({
            "id":"agent",
            "rootId":"agent",
            "accountKey":"claude",
            "provider":"claude",
            "threadId":"thread",
            "accountHistory":[]
        });
        let receipt = json!({
            "id":"usage-receipt",
            "agentId":"agent",
            "rootId":"agent",
            "threadId":"thread",
            "turnId":"turn-1",
            "at":1,
            "accountKey":"claude",
            "responseId":"response-1",
            "model":"claude-sonnet-4-5",
            "last":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":5,"outputTokens":10},
            "total":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":5,"outputTokens":10,"totalTokens":135}
        });
        store
            .analytics_transaction::<_, StoreError>(move |tx, _| {
                AnalyticsCaptureRepository::write(tx, CaptureTable::Agents, &agent)?;
                AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &receipt)?;
                Ok(())
            })
            .await
            .expect("write analytics fixtures");
        assert!(matches!(
            session_cost(&store, "agent"),
            Err(CostsError::AccountProfilesRequired)
        ));
        let accounts = BTreeMap::from([(
            "claude".to_owned(),
            json!({"provider":"claude", "claudeOptions":{"configDir":profile}}),
        )]);

        let result = session_cost_with_account_profiles(&store, "agent", &accounts)
            .expect("session cost is available");
        assert_eq!(result["pricedSamples"], 1);
        assert_eq!(result["totalUSD"], json!(0.000135));
        assert_eq!(result["claudeHistoryIncomplete"], true);
        assert_eq!(result["unknownModels"], json!(["claude-sonnet-4-5"]));
    }
}
