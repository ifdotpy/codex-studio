//! Read-only session cost estimates over the shared analytics store.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_store::{
    AnalyticsRepository, AnalyticsStoreError, CostUsageRepositoryError, RuntimeStore, analytics,
    read_cost_usage,
};
use thiserror::Error;

use crate::cost_usage::{CostUsageError, CostUsageSnapshot};
use crate::file_derived::FileDerived;
use crate::pricing::TokenUsage;

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
    #[error("Claude session state is invalid")]
    InvalidSessionState,
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
    let local_claude = read_local_claude_usage(store.root(), &members, accounts)?;
    let receipts = local_claude
        .iter()
        .map(|row| {
            (
                row.account_key.clone(),
                row.thread_id.clone(),
                row.usage.id.clone(),
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

    let loaded = crate::pricing::load_pricing(store.root())
        .ok_or(CostsError::Dependency("costs/pricing-refresh"))?;
    let catalog = loaded.catalog.as_ref().ok_or(CostsError::InvalidPricing)?;

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
        let usage = TokenUsage {
            input_tokens: group.input_tokens,
            cached_input_tokens: group.cached_tokens,
            cache_write_input_tokens: group.write_tokens,
            output_tokens: group.output_tokens,
        };
        let context = group.input_tokens.map(|input| {
            if group.input_uncached {
                input + group.cached_tokens.unwrap_or(0.0) + group.write_tokens.unwrap_or(0.0)
            } else {
                input
            }
        });
        let price = catalog.price_usage(provider, model, usage, context, group.input_uncached);
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
    for row in local_claude.iter().map(|row| &row.usage) {
        let usage = TokenUsage {
            input_tokens: row.input_tokens,
            cached_input_tokens: row.cached_tokens,
            cache_write_input_tokens: row.write_tokens,
            output_tokens: row.output_tokens,
        };
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
            usage,
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

/// One assistant response of a Claude session log.
#[derive(Clone, Debug)]
struct LogUsage {
    id: String,
    model: String,
    input_tokens: Option<f64>,
    cached_tokens: Option<f64>,
    write_tokens: Option<f64>,
    output_tokens: Option<f64>,
    input_tokens_are_uncached: bool,
}

#[derive(Clone, Debug)]
struct LocalClaudeUsage {
    account_key: String,
    thread_id: String,
    usage: LogUsage,
}

/// Usage rows per session log, parsed again only after the log changed.
static CLAUDE_LOGS: LazyLock<FileDerived<Vec<LogUsage>>> = LazyLock::new(FileDerived::default);
/// Native session ids per Studio session state file, read again only after it changed.
static SESSION_NATIVE_IDS: LazyLock<FileDerived<Result<Vec<String>, ()>>> =
    LazyLock::new(FileDerived::default);

fn read_local_claude_usage(
    state_root: &Path,
    members: &[(String, Value)],
    accounts: &BTreeMap<String, Value>,
) -> Result<Vec<LocalClaudeUsage>, CostsError> {
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
        let server_root = crate::account_driver::account_server_root(state_root, &account_key);
        let state_path = server_root
            .join("sessions")
            .join(format!("{thread_id}.json"));
        if let Some(native) = SESSION_NATIVE_IDS.get_or_load(&state_path, read_session_native_ids) {
            let native = native
                .as_ref()
                .as_ref()
                .map_err(|()| CostsError::InvalidSessionState)?;
            ids.extend(native.iter().cloned());
        }
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
                let Some(log) = CLAUDE_LOGS.get_or_load(&path, parse_claude_log) else {
                    continue;
                };
                for usage in log.iter() {
                    rows.entry((account_key.clone(), thread_id.clone(), usage.id.clone()))
                        .or_insert_with(|| LocalClaudeUsage {
                            account_key: account_key.clone(),
                            thread_id: thread_id.clone(),
                            usage: usage.clone(),
                        });
                }
            }
        }
    }
    Ok(rows.into_values().collect())
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

/// The native ids a Studio session state file names. Studio writes this file, so a
/// file of another shape is an error, not something to skip.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionState {
    native_id: Option<String>,
    #[serde(default)]
    history_branches: Vec<NativeIdOf>,
    #[serde(default)]
    control_requests: BTreeMap<String, ControlRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeIdOf {
    native_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlRequest {
    source_native_id: Option<String>,
    target_native_id: Option<String>,
}

/// The valid native ids of one session state file; `Err` when the file does not decode.
fn read_session_native_ids(path: &Path) -> Option<Result<Vec<String>, ()>> {
    if fs::metadata(path).ok()?.len() > MAX_SESSION_STATE_BYTES {
        return Some(Err(()));
    }
    let Ok(state) = serde_json::from_slice::<SessionState>(&fs::read(path).ok()?) else {
        return Some(Err(()));
    };
    let ids = std::iter::once(state.native_id)
        .chain(
            state
                .history_branches
                .into_iter()
                .map(|branch| branch.native_id),
        )
        .chain(
            state
                .control_requests
                .into_values()
                .flat_map(|control| [control.source_native_id, control.target_native_id]),
        )
        .flatten()
        .filter(|id| valid_session_id(id))
        .collect::<BTreeSet<_>>();
    Some(Ok(ids.into_iter().collect()))
}

/// Just the line kind: Claude writes many kinds of line and only assistant responses matter.
#[derive(Deserialize)]
struct LogLineKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<&'a str>,
}

/// An assistant line of a Claude session log (a format Claude owns, not Studio).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssistantLine {
    timestamp: Option<String>,
    request_id: Option<String>,
    uuid: Option<String>,
    message: Option<LogMessage>,
}

#[derive(Deserialize)]
struct LogMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<LogTokens>,
}

#[derive(Deserialize)]
struct LogTokens {
    input_tokens: Option<f64>,
    cache_read_input_tokens: Option<f64>,
    cache_creation_input_tokens: Option<f64>,
    output_tokens: Option<f64>,
}

/// Session log lines that did not decode and were skipped, since process start. A line
/// is skipped whole; surfaced in diagnostics as `claudeLogLinesSkipped`.
static CLAUDE_LOG_LINES_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Count of Claude session log lines skipped because they did not decode.
pub fn claude_log_lines_skipped() -> u64 {
    CLAUDE_LOG_LINES_SKIPPED.load(Ordering::Relaxed)
}

/// The first usage row of every response id in one session log, in file order.
fn parse_claude_log(path: &Path) -> Option<Vec<LogUsage>> {
    let file = File::open(path).ok()?;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let skipped = || {
            CLAUDE_LOG_LINES_SKIPPED.fetch_add(1, Ordering::Relaxed);
        };
        match serde_json::from_slice::<LogLineKind>(&line) {
            Ok(LogLineKind {
                kind: Some("assistant"),
            }) => {}
            Ok(_) => continue,
            Err(_) => {
                skipped();
                continue;
            }
        }
        let Ok(row) = serde_json::from_slice::<AssistantLine>(&line) else {
            skipped();
            continue;
        };
        let Some(message) = row.message else {
            continue;
        };
        let Some(model) = message
            .model
            .filter(|model| !model.is_empty() && model != "<synthetic>")
        else {
            continue;
        };
        if !valid_claude_timestamp(row.timestamp.as_deref()) {
            continue;
        }
        let Some(usage) = message.usage else {
            continue;
        };
        let Some(id) = [message.id, row.request_id, row.uuid]
            .into_iter()
            .flatten()
            .find(|id| !id.is_empty())
        else {
            continue;
        };
        // An absent count is zero; a negative count voids all three input counts.
        let count = |value: Option<f64>| match value {
            None => Some(0.0),
            Some(number) => (number >= 0.0).then_some(number),
        };
        let input_values = [
            count(usage.input_tokens),
            count(usage.cache_read_input_tokens),
            count(usage.cache_creation_input_tokens),
        ];
        let (input_tokens, cached_tokens, write_tokens) =
            if input_values.iter().all(Option::is_some) {
                (input_values[0], input_values[1], input_values[2])
            } else {
                (None, None, None)
            };
        if !seen.insert(id.clone()) {
            continue;
        }
        rows.push(LogUsage {
            id,
            model,
            input_tokens,
            cached_tokens,
            write_tokens,
            output_tokens: usage.output_tokens,
            input_tokens_are_uncached: input_tokens.is_some(),
        });
    }
    Some(rows)
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

        let skipped_before = super::claude_log_lines_skipped();
        let rows = read_local_claude_usage(&state, &members, &accounts).expect("local usage");
        // The previous account's row has a boolean token count: that line is skipped whole
        // and counted, as is the line whose response id is a number.
        assert_eq!(rows.len(), 2);
        assert_eq!(super::claude_log_lines_skipped() - skipped_before, 2);
        assert!(!rows.iter().any(|row| row.usage.id == "must-not-be-used"));
        assert!(rows.iter().any(|row| row.usage.id == "response-naive-time"));
        let current = rows
            .iter()
            .find(|row| row.account_key == "current")
            .expect("current session row");
        assert_eq!(current.thread_id, "thread-current");
        assert_eq!(current.usage.id, "response-1");
        assert_eq!(current.usage.input_tokens, Some(100.0));
        assert_eq!(current.usage.cached_tokens, Some(20.0));
        assert_eq!(current.usage.write_tokens, Some(5.0));
        assert_eq!(current.usage.output_tokens, Some(10.0));
        assert!(current.usage.input_tokens_are_uncached);
        assert!(!rows.iter().any(|row| row.account_key == "previous"));
    }

    /// A Claude log line that does not decode is skipped whole and counted; one that merely is
    /// not an assistant response is not an error. A Studio state file of another shape fails.
    #[test]
    fn malformed_log_lines_are_skipped_and_counted_and_bad_state_files_fail() {
        let temp = tempdir().expect("temporary fixture directory");
        let state = temp.path().join("state");
        let profile = temp.path().join("claude");
        let project = profile.join("projects/project");
        fs::create_dir_all(&project).expect("create project");
        fs::create_dir_all(state.join("account-servers/acct/sessions")).expect("sessions");
        let good = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z",
            "message":{"id":"r1","model":"claude-sonnet-4-5","usage":{"input_tokens":1,"output_tokens":2}}});
        let wrong_type = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z",
            "message":{"id":7,"model":"claude-sonnet-4-5","usage":{}}});
        fs::write(
            project.join("thread.jsonl"),
            format!(
                "{good}\nnot json\n{wrong_type}\n{}\n",
                json!({"type":"summary","message":"free text"})
            ),
        )
        .expect("write log");
        let accounts = BTreeMap::from([(
            "acct".to_owned(),
            json!({"provider":"claude","claudeOptions":{"configDir":profile}}),
        )]);
        let members = vec![(
            "agent".to_owned(),
            json!({"accountKey":"acct","threadId":"thread"}),
        )];
        let before = super::claude_log_lines_skipped();
        let rows = read_local_claude_usage(&state, &members, &accounts).expect("usage");
        assert_eq!(rows.len(), 1);
        assert_eq!(super::claude_log_lines_skipped() - before, 2);
        // Parsed once per file version: a second read does not count them again.
        read_local_claude_usage(&state, &members, &accounts).expect("usage again");
        assert_eq!(super::claude_log_lines_skipped() - before, 2);

        fs::write(
            state.join("account-servers/acct/sessions/thread.json"),
            r#"{"nativeId": 5}"#,
        )
        .expect("write bad state");
        assert!(matches!(
            read_local_claude_usage(&state, &members, &accounts),
            Err(CostsError::InvalidSessionState)
        ));
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
        assert!(
            read_local_claude_usage(temp.path(), &members, &accounts)
                .expect("local usage")
                .is_empty()
        );
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

        // Cached log parses and the cached catalog follow the files: a second answer
        // in the log is priced, and new rates replace the old ones.
        let second = json!({
            "type":"assistant",
            "timestamp":"2026-09-01T12:02:00Z",
            "message":{
                "id":"response-3",
                "model":"claude-sonnet-4-5",
                "usage":{
                    "input_tokens":100,
                    "cache_read_input_tokens":20,
                    "cache_creation_input_tokens":5,
                    "output_tokens":10
                }
            }
        });
        let log = temp
            .path()
            .join("claude-profile/projects/project/thread.jsonl");
        let mut text = fs::read_to_string(&log).expect("read log");
        text.push_str(&format!("{second}\n"));
        fs::write(&log, text).expect("append a response");
        let grown = session_cost_with_account_profiles(&store, "agent", &accounts)
            .expect("session cost after the log grew");
        assert_eq!(grown["pricedSamples"], 2);
        assert_eq!(grown["totalUSD"], json!(0.00027));
        fs::write(
            temp.path().join("model-pricing/models-dev-v1.json"),
            json!({
                "version":1,
                "catalog":{"providers":{
                    "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":1}}}},
                    "anthropic":{"models":{"claude-sonnet-4-5":{"cost":{
                        "input":2,"output":4,"cache_read":1,"cache_write":2
                    }}}}
                }}
            })
            .to_string(),
        )
        .expect("rewrite pricing catalog");
        let repriced = session_cost_with_account_profiles(&store, "agent", &accounts)
            .expect("session cost after the rates changed");
        assert_eq!(repriced["totalUSD"], json!(0.00054));
    }
}
