//! Local usage snapshot readers that do not require provider or turn drivers.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

// Python owns the shared cost values in codex_costs.py:27-33: refresh
// intervals, history windows, and the scan timeout. costs-contract.py checks
// the Rust consumers against those owners.
const COST_REFRESH_SECONDS: f64 = 120.0;
const CLAUDE_COST_REFRESH_SECONDS: f64 = 300.0;
const CLAUDE_COST_HISTORY_DAYS: i64 = 30;
const COST_REPORT_HISTORY_DAYS: i64 = 30;
// Python owns models.dev refresh policy in codex_pricing.py:17-18,120-128.
const PRICING_CATALOG_TTL_SECONDS: f64 = 24.0 * 60.0 * 60.0;
const PRICING_CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const COST_SCAN_TIMEOUT_SECONDS: Duration = Duration::from_secs(900);
const COST_SCAN_LOCK_RETRY_INTERVAL_MILLISECONDS: u64 = 50;
static COST_REFRESH_ATTEMPTS: LazyLock<Mutex<BTreeMap<String, tokio::time::Instant>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static ACTIVE_COST_REFRESHES: LazyLock<Mutex<BTreeSet<String>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));
static CODEX_SCAN_LOCK: Mutex<()> = Mutex::new(());
static PRICING_CATALOG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Error, Eq, PartialEq)]
pub enum UsageError {
    #[error("This account's local cost history is unavailable.")]
    AccountUnavailable,
    #[error("The account has no local profile path.")]
    MissingProfile,
    #[error("Local cost cache could not be read.")]
    CacheRead,
    #[error("Local cost scan failed.")]
    Scan,
    #[error("Local cost scanner is busy.")]
    ScannerBusy,
    #[error("Local cost scanner was cancelled during shutdown.")]
    ScannerCancelled,
}

/// Claude accounts scan Claude JSONL transcripts; every other account scans Codex rollouts.
fn is_claude_account(account: &Value) -> bool {
    studio_runtime_providers::ProviderKind::of_record(account)
        == Ok(studio_runtime_providers::ProviderKind::Claude)
}

/// Read an account's last locally scanned cost snapshot.
///
/// The response reports stale caches as stale and never invokes a scanner,
/// provider session, or billing endpoint. Python's reader interval/timeout and
/// CODEX_HOME cache scope are in codex_costs.py:165-199; stale refresh
/// scheduling is at :201-232. Account home resolution and account-scoped
/// cache/environment construction are at :352-391.
pub fn account_cost_snapshot(
    state_dir: &Path,
    account_key: &str,
    account: &Value,
    now: f64,
) -> Result<Value, UsageError> {
    if account.get("status").and_then(Value::as_str) != Some("ready") {
        return Err(UsageError::AccountUnavailable);
    }
    let home = account
        .get("home")
        .and_then(Value::as_str)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or(UsageError::MissingProfile)?;
    if is_claude_account(account) {
        return claude_cost_snapshot(state_dir, account_key, &home, now);
    }
    let account_id = account.get("accountId").cloned().unwrap_or(Value::Null);
    let home_text = home.to_string_lossy().into_owned();
    let account_identity =
        crate::event_write::python_json(&json!([account_key, account_id, home_text]));
    let account_root = state_dir
        .join("account-costs")
        .join(digest(&account_identity));
    let cache_identity = crate::event_write::python_json(&json!([
        home_text,
        account_root.to_string_lossy().as_ref()
    ]));
    let cache_scope = digest(&cache_identity);
    let cache_path = account_root.join("local-costs.json");
    let saved = read_json(&cache_path);
    let mut state = saved
        .filter(|saved| saved.get("scope").and_then(Value::as_str) == Some(&cache_scope))
        .and_then(|saved| saved.get("state").cloned())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"at": null, "error": null, "data": null}));
    let checked_at = state
        .get("checkedAt")
        .and_then(Value::as_f64)
        .or_else(|| state.get("at").and_then(Value::as_f64));
    let mut stale = checked_at.is_none_or(|at| now - at >= COST_REFRESH_SECONDS)
        || state.get("error").is_some_and(|error| !error.is_null());
    let refreshing = cost_refresh_active(&account_refresh_key(account_key, &home));
    let refresh_due = checked_at.is_none_or(|at| now - at >= COST_REFRESH_SECONDS);
    if let Some(data) = state.get_mut("data").and_then(Value::as_object_mut) {
        let current_day = local_day(now);
        if data.get("sourceDay").and_then(Value::as_str) != Some(&current_day) {
            data.insert("todayUSD".to_owned(), Value::Null);
            data.insert("todayTokens".to_owned(), Value::Null);
            stale = true;
        }
        data.insert("accountId".to_owned(), account_id);
        data.insert(
            "source".to_owned(),
            Value::String("Codex profile logs".to_owned()),
        );
        data.insert("scope".to_owned(), Value::String("account".to_owned()));
        data.insert(
            "note".to_owned(),
            Value::String("API-rate estimate".to_owned()),
        );
        data.insert("kind".to_owned(), Value::String("api_estimate".to_owned()));
    }
    let mut response = state.as_object().cloned().unwrap_or_default();
    response.insert("refreshing".to_owned(), Value::Bool(refreshing));
    response.insert("_refreshDue".to_owned(), Value::Bool(refresh_due));
    response.insert("stale".to_owned(), Value::Bool(stale));
    response.insert(
        "accountKey".to_owned(),
        Value::String(account_key.to_owned()),
    );
    Ok(Value::Object(response))
}

/// Reserve an in-process refresh attempt so a failed cache write cannot cause
/// every following GET to start another scan. The interval remains owned by
/// `COST_REFRESH_SECONDS` above.
pub fn claim_cost_refresh_attempt(account_key: &str) -> bool {
    let now = tokio::time::Instant::now();
    let Ok(mut attempts) = COST_REFRESH_ATTEMPTS.lock() else {
        return false;
    };
    attempts.retain(|_, started| {
        now.duration_since(*started) < Duration::from_secs_f64(COST_REFRESH_SECONDS)
    });
    if attempts.get(account_key).is_some_and(|started| {
        now.duration_since(*started) < Duration::from_secs_f64(COST_REFRESH_SECONDS)
    }) {
        return false;
    }
    attempts.insert(account_key.to_owned(), now);
    true
}

/// Refresh local Codex session logs into the same scoped cache used by Python.
/// Called only from the measured blocking pool; GET handlers return their cache
/// before scheduling this work. Python's lock wait/busy result is at
/// codex_costs.py:234-242; retry interval is at :216-219; completion-time
/// checkedAt and cache save are at :280-292; failure timestamping is at :294-306.
#[cfg(test)]
fn refresh_account_costs(
    state_dir: &Path,
    account_key: &str,
    account: &Value,
    cancelled: &AtomicBool,
) -> Result<(), UsageError> {
    refresh_account_costs_report(state_dir, account_key, account, cancelled).1
}

/// Refresh and report whether a cache value was durably saved, even if the
/// scanner itself failed and the saved value records that failure.
pub fn refresh_account_costs_report(
    state_dir: &Path,
    account_key: &str,
    account: &Value,
    cancelled: &AtomicBool,
) -> (bool, Result<(), UsageError>) {
    refresh_account_costs_with(
        state_dir,
        account_key,
        account,
        cancelled,
        unix_time_seconds,
        |home, state_dir, cancelled| {
            scan_codex_sessions(home, state_dir, unix_time_seconds(), cancelled)
        },
    )
}

fn refresh_account_costs_with<C, S>(
    state_dir: &Path,
    account_key: &str,
    account: &Value,
    cancelled: &AtomicBool,
    clock: C,
    scanner: S,
) -> (bool, Result<(), UsageError>)
where
    C: Fn() -> f64,
    S: FnOnce(&Path, &Path, &AtomicBool) -> Result<Value, UsageError>,
{
    let home = match account
        .get("home")
        .and_then(Value::as_str)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
    {
        Some(home) => home,
        None => return (false, Err(UsageError::MissingProfile)),
    };
    let refresh_key = account_refresh_key(account_key, &home);
    let Some(_refresh) = begin_cost_refresh(refresh_key) else {
        return (false, Ok(()));
    };
    if is_claude_account(account) {
        if cancelled.load(Ordering::Acquire) {
            return (false, Err(UsageError::ScannerCancelled));
        }
        let scan_started_at = clock();
        let result = refresh_claude_costs(
            state_dir,
            account_key,
            &home,
            scan_started_at,
            cancelled,
            clock,
        );
        return (result.is_ok(), result);
    }
    let account_id = account.get("accountId").cloned().unwrap_or(Value::Null);
    let home_text = home.to_string_lossy().into_owned();
    let account_identity =
        crate::event_write::python_json(&json!([account_key, account_id, home_text]));
    let cache_root = state_dir
        .join("account-costs")
        .join(digest(&account_identity));
    let cache_identity =
        crate::event_write::python_json(&json!([home_text, cache_root.to_string_lossy().as_ref()]));
    let scope = digest(&cache_identity);
    if let Err(error) = create_private_dir(&cache_root) {
        return (false, Err(error));
    }
    if cancelled.load(Ordering::Acquire) {
        return (false, Err(UsageError::ScannerCancelled));
    }
    let _scan_guard = match acquire_codex_scan_lock(cancelled) {
        Ok(guard) => guard,
        Err(error) => return (false, Err(error)),
    };
    let result: Result<(), UsageError> = (|| {
        let data = scanner(&home, state_dir, cancelled)?;
        if cancelled.load(Ordering::Acquire) {
            return Err(UsageError::ScannerCancelled);
        }
        let checked_at = clock();
        let previous = read_json(&cache_root.join("local-costs.json"));
        let fingerprint = cost_fingerprint(&data);
        let old = previous
            .as_ref()
            .and_then(|v| v.get("state"))
            .and_then(|v| v.get("data"));
        let old_fingerprint = previous
            .as_ref()
            .and_then(|v| v.pointer("/state/sourceFingerprint"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| old.map(cost_fingerprint));
        let changed = old.is_none() || old_fingerprint.as_deref() != Some(fingerprint.as_str());
        let data = if changed {
            data
        } else {
            old.cloned().unwrap_or(data)
        };
        let at = if changed {
            checked_at
        } else {
            previous
                .as_ref()
                .and_then(|v| v.pointer("/state/at"))
                .and_then(Value::as_f64)
                .unwrap_or(checked_at)
        };
        let state = json!({"at":at,"checkedAt":checked_at,"error":null,"data":data,"sourceFingerprint":fingerprint});
        let bytes = serde_json::to_vec(&json!({"scope":scope,"state":state}))
            .map_err(|_| UsageError::Scan)?;
        crate::atomic_file::replace(
            &cache_root.join("local-costs.json"),
            &bytes,
            Some(0o600),
            false,
        )
        .map_err(|_| UsageError::Scan)?;
        Ok(())
    })();
    let mut cache_saved = result.is_ok();
    if let Err(error) = &result {
        if matches!(error, UsageError::ScannerCancelled) {
            return (false, result);
        }
        let checked_at = clock();
        let path = cache_root.join("local-costs.json");
        let mut state = read_json(&path)
            .and_then(|cache| cache.get("state").cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"at":null,"data":null}));
        if let Some(fields) = state.as_object_mut() {
            fields.insert("error".to_owned(), Value::String(error.to_string()));
            fields.insert("checkedAt".to_owned(), Value::from(checked_at));
        }
        if let Ok(bytes) = serde_json::to_vec(&json!({"scope":scope,"state":state})) {
            match crate::atomic_file::replace(&path, &bytes, Some(0o600), false) {
                Ok(()) => cache_saved = true,
                Err(error) => {
                    eprintln!("Could not persist local cost scan failure: {error}");
                }
            }
        }
    }
    (cache_saved, result)
}

fn unix_time_seconds() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

fn cost_fingerprint(data: &Value) -> String {
    let content = [
        "todayUSD",
        "last30DaysUSD",
        "todayTokens",
        "last30DaysTokens",
        "sourceDay",
        "coverage",
        "unknownModels",
        "historyDays",
    ]
    .into_iter()
    .map(|field| {
        (
            field.to_owned(),
            data.get(field).cloned().unwrap_or(Value::Null),
        )
    })
    .collect::<serde_json::Map<_, _>>();
    crate::event_write::python_json(&Value::Object(content))
}

fn refresh_claude_costs(
    state_dir: &Path,
    account_key: &str,
    home: &Path,
    now: f64,
    cancelled: &AtomicBool,
    clock: impl Fn() -> f64,
) -> Result<(), UsageError> {
    let identity = format!("{account_key}{}", home.to_string_lossy());
    let root = state_dir
        .join("account-costs/claude")
        .join(digest(&identity));
    create_private_dir(&root)?;
    let cache = root.join("claude-costs.json");
    let previous = read_json(&cache);
    let (data, error) = match scan_claude_sessions(home, state_dir, now, cancelled) {
        Ok(data) => (Some(data), Value::Null),
        Err(UsageError::ScannerCancelled) => return Err(UsageError::ScannerCancelled),
        Err(_) => (
            previous.as_ref().and_then(|v| v.get("data")).cloned(),
            Value::String("Claude account logs are unavailable".to_owned()),
        ),
    };
    if cancelled.load(Ordering::Acquire) {
        return Err(UsageError::ScannerCancelled);
    }
    let content = json!({"config":home,"data":data,"checked":clock(),"error":error});
    let bytes = serde_json::to_vec(&content).map_err(|_| UsageError::Scan)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(UsageError::ScannerCancelled);
    }
    crate::atomic_file::replace(&cache, &bytes, Some(0o600), false)
        .map_err(|_| UsageError::Scan)?;
    Ok(())
}

struct CostRefreshGuard(String);

impl Drop for CostRefreshGuard {
    fn drop(&mut self) {
        ACTIVE_COST_REFRESHES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

fn account_refresh_key(account_key: &str, home: &Path) -> String {
    digest(&format!("{account_key}{}", home.to_string_lossy()))
}

fn begin_cost_refresh(key: String) -> Option<CostRefreshGuard> {
    let mut active = ACTIVE_COST_REFRESHES.lock().ok()?;
    if active.insert(key.clone()) {
        Some(CostRefreshGuard(key))
    } else {
        None
    }
}

fn cost_refresh_active(key: &str) -> bool {
    ACTIVE_COST_REFRESHES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(key)
}

#[derive(Clone)]
struct ClaudeUsage {
    model: String,
    at: f64,
    input: Option<u64>,
    cached: Option<u64>,
    write: Option<u64>,
    output: Option<u64>,
}

/// Read deduplicated Claude Code assistant usage rows under the chosen profile.
/// Python owner: codex_claude_costs.py:6-34 and codex_costs.py:391-478.
fn scan_claude_sessions(
    config: &Path,
    state_dir: &Path,
    now: f64,
    cancelled: &AtomicBool,
) -> Result<Value, UsageError> {
    scan_claude_sessions_with_catalog(config, now, cancelled, || {
        pricing_catalog_with_fetch(state_dir, now, fetch_models_dev_catalog)
    })
}

fn scan_claude_sessions_with_catalog<F>(
    config: &Path,
    now: f64,
    cancelled: &AtomicBool,
    fetch_catalog: F,
) -> Result<Value, UsageError>
where
    F: FnOnce() -> Option<crate::pricing::PricingCatalog>,
{
    let started = Instant::now();
    let projects = config.join("projects");
    if !projects.is_dir() {
        return Err(UsageError::Scan);
    }
    let mut stack = vec![projects];
    let mut rows = BTreeMap::<String, ClaudeUsage>::new();
    while let Some(directory) = stack.pop() {
        if cancelled.load(Ordering::Acquire) {
            return Err(UsageError::ScannerCancelled);
        }
        check_scan_timeout(started)?;
        for entry in fs::read_dir(directory).map_err(|_| UsageError::Scan)? {
            if cancelled.load(Ordering::Acquire) {
                return Err(UsageError::ScannerCancelled);
            }
            let entry = entry.map_err(|_| UsageError::Scan)?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|_| UsageError::Scan)?;
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() || path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                continue;
            }
            let file = File::open(path).map_err(|_| UsageError::Scan)?;
            for line in BufReader::new(file).lines() {
                if cancelled.load(Ordering::Acquire) {
                    return Err(UsageError::ScannerCancelled);
                }
                check_scan_timeout(started)?;
                let line = line.map_err(|_| UsageError::Scan)?;
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
                let Some(at) = row
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                else {
                    continue;
                };
                let Some(usage) = message.get("usage") else {
                    continue;
                };
                let Some(id) = message
                    .get("id")
                    .or_else(|| row.get("requestId"))
                    .or_else(|| row.get("uuid"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                else {
                    continue;
                };
                let value = |key| match usage.get(key) {
                    None => Some(0),
                    Some(value) => value.as_u64(),
                };
                rows.entry(id.to_owned()).or_insert_with(|| ClaudeUsage {
                    model: model.to_owned(),
                    at: at.timestamp() as f64,
                    input: value("input_tokens"),
                    cached: value("cache_read_input_tokens"),
                    write: value("cache_creation_input_tokens"),
                    // Python keeps an absent output_tokens field as None
                    // (codex_claude_costs.py:37), so pricing marks the row
                    // incomplete instead of treating it as zero usage.
                    output: usage.get("output_tokens").and_then(Value::as_u64),
                });
            }
        }
    }

    if cancelled.load(Ordering::Acquire) {
        return Err(UsageError::ScannerCancelled);
    }
    let catalog = fetch_catalog();
    let today = local_day(now);
    let mut today_cost = 0.0;
    let mut month_cost = 0.0;
    let mut today_priced = false;
    let mut month_priced = false;
    let mut unknown = BTreeSet::new();
    let mut groups = BTreeMap::<String, f64>::new();
    for row in rows.values() {
        if cancelled.load(Ordering::Acquire) {
            return Err(UsageError::ScannerCancelled);
        }
        check_scan_timeout(started)?;
        let day = chrono::DateTime::from_timestamp(row.at as i64, 0).map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        });
        let is_today = day.as_deref() == Some(today.as_str());
        let is_month = now - row.at <= CLAUDE_COST_HISTORY_DAYS as f64 * 86_400.0;
        if !is_today && !is_month {
            continue;
        }
        let usage = crate::pricing::TokenUsage {
            input_tokens: row.input.map(|tokens| tokens as f64),
            cached_input_tokens: row.cached.map(|tokens| tokens as f64),
            cache_write_input_tokens: row.write.map(|tokens| tokens as f64),
            output_tokens: row.output.map(|tokens| tokens as f64),
        };
        let context = row
            .input
            .zip(row.cached)
            .zip(row.write)
            .map(|((a, b), c)| a.saturating_add(b).saturating_add(c) as f64);
        let price = catalog
            .as_ref()
            .map(|catalog| catalog.price_usage("anthropic", &row.model, usage, context, true));
        let Some(cost) = price.and_then(|price| price.amount_usd) else {
            unknown.insert(row.model.clone());
            continue;
        };
        *groups.entry(row.model.clone()).or_default() += cost;
        if is_today {
            today_cost += cost;
            today_priced = true;
        } else if is_month {
            month_cost += cost;
            month_priced = true;
        }
    }
    let has_rows = !rows.is_empty();
    let today = if today_priced || (has_rows && unknown.is_empty()) {
        Value::from(today_cost)
    } else {
        Value::Null
    };
    let month = if today_priced || month_priced || (has_rows && unknown.is_empty()) {
        Value::from(today_cost + month_cost)
    } else {
        Value::Null
    };
    let coverage = if unknown.is_empty() {
        if has_rows { "reported" } else { "unverified" }
    } else {
        "partial"
    };
    Ok(
        json!({"source":"Claude Code local logs","scope":"account","kind":"api_estimate",
        "currency":"USD","billedUSD":null,"todayUSD":today,"last30DaysUSD":month,
        "coverage":coverage,"unknownModels":unknown,"modelBreakdown":groups,
        "note":"API-rate estimate. Uses base rates when request size is unavailable."}),
    )
}

/// Token counts of one Codex `token_count` usage object: input (including
/// cached), cached input, and output.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct CodexTokens {
    input: u64,
    cached: u64,
    output: u64,
}

impl CodexTokens {
    fn from_usage(usage: Option<&Value>) -> Option<Self> {
        let usage = usage?;
        let count = |keys: &[&str]| {
            keys.iter()
                .find_map(|key| usage.get(*key).and_then(Value::as_u64))
                .unwrap_or(0)
        };
        Some(Self {
            input: count(&["input_tokens"]),
            cached: count(&["cached_input_tokens", "cache_read_input_tokens"]),
            output: count(&["output_tokens"]),
        })
    }

    fn at_least(self, other: Self) -> bool {
        self.input >= other.input && self.cached >= other.cached && self.output >= other.output
    }

    fn minus(self, other: Self) -> Self {
        Self {
            input: self.input - other.input,
            cached: self.cached - other.cached,
            output: self.output - other.output,
        }
    }
}

/// Estimated usage of one model on one local day.
#[derive(Default)]
struct CodexModelDay {
    tokens: u64,
    cost_usd: f64,
    unpriced: bool,
}

/// Scan the Codex session logs of one profile and build the aggregate cost
/// report. Reads `sessions` and the sibling `archived_sessions` under `home`;
/// a fork's replayed parent history only advances the running totals.
/// History completeness is never verified, so coverage is always `partial`.
fn scan_codex_sessions(
    home: &Path,
    state_dir: &Path,
    now: f64,
    cancelled: &AtomicBool,
) -> Result<Value, UsageError> {
    scan_codex_sessions_with_catalog(home, now, cancelled, || {
        pricing_catalog_with_fetch(state_dir, now, fetch_models_dev_catalog)
    })
}

fn scan_codex_sessions_with_catalog<F>(
    home: &Path,
    now: f64,
    cancelled: &AtomicBool,
    fetch_catalog: F,
) -> Result<Value, UsageError>
where
    F: FnOnce() -> Option<crate::pricing::PricingCatalog>,
{
    let started = Instant::now();
    let oldest_mtime = SystemTime::UNIX_EPOCH
        + Duration::from_secs_f64((now - COST_REPORT_HISTORY_DAYS as f64 * 86_400.0).max(0.0));
    let since_day = local_day(now - (COST_REPORT_HISTORY_DAYS - 1) as f64 * 86_400.0);
    let mut files = BTreeMap::<std::ffi::OsString, PathBuf>::new();
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        let mut stack = vec![root];
        while let Some(directory) = stack.pop() {
            check_codex_scan(started, cancelled)?;
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for entry in entries {
                let entry = entry.map_err(|_| UsageError::Scan)?;
                let path = entry.path();
                let kind = entry.file_type().map_err(|_| UsageError::Scan)?;
                if kind.is_dir() {
                    stack.push(path);
                } else if kind.is_file()
                    && path.extension().and_then(|v| v.to_str()) == Some("jsonl")
                    && entry
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .is_ok_and(|modified| modified >= oldest_mtime)
                {
                    files.entry(entry.file_name()).or_insert(path);
                }
            }
        }
    }
    let catalog = fetch_catalog();
    let mut days = BTreeMap::<(String, String), CodexModelDay>::new();
    for path in files.values() {
        let file = File::open(path).map_err(|_| UsageError::Scan)?;
        let mut model = None::<String>;
        let mut fork_at = None::<chrono::DateTime<chrono::FixedOffset>>;
        let mut previous = CodexTokens::default();
        for line in BufReader::new(file).lines() {
            check_codex_scan(started, cancelled)?;
            let Ok(row) = serde_json::from_str::<Value>(&line.map_err(|_| UsageError::Scan)?)
            else {
                continue;
            };
            let payload = row.get("payload");
            let at = row
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
            match row.get("type").and_then(Value::as_str) {
                Some("session_meta") => {
                    if payload.is_some_and(|meta| meta.get("forked_from_id").is_some()) {
                        fork_at = payload
                            .and_then(|meta| meta.get("timestamp"))
                            .and_then(Value::as_str)
                            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                            .or(at);
                    }
                }
                Some("turn_context") => {
                    let named = |value: Option<&Value>| {
                        value
                            .and_then(|v| v.get("model"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    };
                    model = named(payload).or_else(|| named(payload.and_then(|p| p.get("info"))));
                }
                Some("event_msg")
                    if payload.and_then(|p| p.get("type")).and_then(Value::as_str)
                        == Some("token_count") =>
                {
                    let info = payload.and_then(|p| p.get("info"));
                    let last =
                        CodexTokens::from_usage(info.and_then(|i| i.get("last_token_usage")));
                    let total =
                        CodexTokens::from_usage(info.and_then(|i| i.get("total_token_usage")));
                    let delta = match (total, last) {
                        (Some(total), last) => {
                            let delta = if total.at_least(previous) {
                                total.minus(previous)
                            } else {
                                last.unwrap_or(total)
                            };
                            previous = total;
                            delta
                        }
                        (None, Some(last)) => last,
                        (None, None) => continue,
                    };
                    let Some(at) = at else { continue };
                    if fork_at.is_some_and(|forked| at < forked) || delta == CodexTokens::default()
                    {
                        continue;
                    }
                    let day = at
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d")
                        .to_string();
                    if day < since_day {
                        continue;
                    }
                    let name = codex_model_name(model.as_deref().unwrap_or("gpt-5"));
                    let cached = delta.cached.min(delta.input);
                    let usage = crate::pricing::TokenUsage {
                        input_tokens: Some(delta.input as f64),
                        cached_input_tokens: Some(cached as f64),
                        cache_write_input_tokens: None,
                        output_tokens: Some(delta.output as f64),
                    };
                    let cost = catalog
                        .as_ref()
                        .map(|catalog| {
                            catalog.price_usage(
                                "openai",
                                &name,
                                usage,
                                Some(delta.input as f64),
                                false,
                            )
                        })
                        .and_then(|price| price.amount_usd);
                    let entry = days.entry((day, name)).or_default();
                    entry.tokens += delta.input + delta.output;
                    match cost {
                        Some(cost) => entry.cost_usd += cost,
                        None => entry.unpriced = true,
                    }
                }
                _ => {}
            }
        }
    }
    let today = local_day(now);
    let mut unknown = BTreeSet::new();
    let (mut today_tokens, mut today_cost, mut month_tokens, mut month_cost) = (0, 0.0, 0, 0.0);
    for ((day, name), entry) in &days {
        // A zero with tokens is not evidence of free inference.
        if entry.unpriced || (entry.cost_usd == 0.0 && entry.tokens > 0) {
            unknown.insert(name.chars().take(160).collect::<String>());
        }
        month_tokens += entry.tokens;
        month_cost += entry.cost_usd;
        if *day == today {
            today_tokens += entry.tokens;
            today_cost += entry.cost_usd;
        }
    }
    // A claimed zero must not conceal missing rates or incomplete history.
    let usd = |cost: f64| if cost > 0.0 { json!(cost) } else { Value::Null };
    let tokens = |count: u64, present: bool| if present { json!(count) } else { Value::Null };
    let has_today = days.keys().any(|(day, _)| *day == today);
    Ok(json!({
        "source":"CodexBar local logs",
        "scope":"Local Codex sessions on this computer, including supported pi sessions. All projects and accounts in those logs.",
        "kind":"api_estimate",
        "currency":"USD",
        "billedUSD":null,
        "todayUSD":usd(today_cost),
        "last30DaysUSD":usd(month_cost),
        "todayTokens":tokens(today_tokens, has_today),
        "last30DaysTokens":tokens(month_tokens, !days.is_empty()),
        "sourceUpdatedAt":chrono::DateTime::from_timestamp(now as i64, 0)
            .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        "sourceDay":days.keys().map(|(day, _)| day).max(),
        "coverage":"partial",
        "unknownModels":unknown,
        "historyDays":COST_REPORT_HISTORY_DAYS,
        "note":"API-rate estimate, not your ChatGPT bill. Base rates apply when the scanner cannot resolve a published context tier."
    }))
}

fn check_codex_scan(started: Instant, cancelled: &AtomicBool) -> Result<(), UsageError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(UsageError::ScannerCancelled);
    }
    check_scan_timeout(started)
}

/// Drop the provider prefix and a trailing `-YYYY-MM-DD` snapshot date.
fn codex_model_name(raw: &str) -> String {
    let name = raw.trim().strip_prefix("openai/").unwrap_or(raw.trim());
    let dated = name.len() > 11
        && name.is_char_boundary(name.len() - 11)
        && chrono::NaiveDate::parse_from_str(&name[name.len() - 10..], "%Y-%m-%d").is_ok()
        && name.as_bytes()[name.len() - 11] == b'-';
    if dated {
        &name[..name.len() - 11]
    } else {
        name
    }
    .to_owned()
}

/// Python waits up to the scan timeout for its shared scan lock and only then
/// raises the busy result (codex_costs.py:239-242); preserve that wait here.
fn acquire_codex_scan_lock(cancelled: &AtomicBool) -> Result<MutexGuard<'static, ()>, UsageError> {
    acquire_codex_scan_lock_for(COST_SCAN_TIMEOUT_SECONDS, Some(cancelled))
}

#[allow(
    clippy::disallowed_methods,
    reason = "usage scans execute on the labeled account-cost blocking worker"
)]
fn acquire_codex_scan_lock_for(
    timeout: Duration,
    cancelled: Option<&AtomicBool>,
) -> Result<MutexGuard<'static, ()>, UsageError> {
    let deadline = Instant::now() + timeout;
    loop {
        match CODEX_SCAN_LOCK.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(TryLockError::WouldBlock)
                if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) =>
            {
                return Err(UsageError::ScannerCancelled);
            }
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(
                    COST_SCAN_LOCK_RETRY_INTERVAL_MILLISECONDS,
                ));
            }
            Err(TryLockError::WouldBlock) => return Err(UsageError::ScannerBusy),
        }
    }
}

fn pricing_catalog_with_fetch<F>(
    state_dir: &Path,
    now: f64,
    fetch: F,
) -> Option<crate::pricing::PricingCatalog>
where
    F: FnOnce() -> Option<Value>,
{
    let _catalog_guard = PRICING_CATALOG_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = state_dir.join("model-pricing/models-dev-v1.json");
    let attempt_path = state_dir.join("model-pricing/refresh-state.json");
    let mut artifact = read_json(&path).filter(|artifact| {
        artifact.get("version").and_then(Value::as_u64) == Some(1)
            && artifact
                .get("catalog")
                .and_then(|catalog| crate::pricing::PricingCatalog::from_value(catalog).ok())
                .is_some()
    });
    let current_catalog = artifact
        .as_ref()
        .and_then(|artifact| artifact.get("catalog"))
        .and_then(|catalog| crate::pricing::PricingCatalog::from_value(catalog).ok());
    let last_attempt = artifact
        .as_ref()
        .and_then(|artifact| {
            artifact
                .get("lastAttemptAt")
                .and_then(Value::as_f64)
                .or_else(|| artifact.get("fetchedAtEpoch").and_then(Value::as_f64))
                .or_else(|| {
                    artifact
                        .get("fetchedAt")
                        .and_then(Value::as_str)
                        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
                        .map(|stamp| stamp.timestamp() as f64)
                })
        })
        .unwrap_or_default()
        .max(
            read_json(&attempt_path)
                .and_then(|value| value.get("lastAttemptAt").and_then(Value::as_f64))
                .unwrap_or_default(),
        );
    if now - last_attempt < PRICING_CATALOG_TTL_SECONDS {
        return current_catalog;
    }

    if write_pricing_attempt(&attempt_path, now).is_err() {
        return current_catalog;
    }
    if let Some(existing) = artifact.as_mut() {
        existing["lastAttemptAt"] = json!(now);
        if let Err(error) = write_private_json_atomic(&path, existing) {
            eprintln!("Could not persist models.dev pricing attempt: {error}");
        }
    }
    let Some(providers) = fetch() else {
        return current_catalog;
    };
    let catalog_value = json!({"providers": providers});
    let Ok(catalog) = crate::pricing::PricingCatalog::from_value(&catalog_value) else {
        return current_catalog;
    };
    let Some(fetched_at) = chrono::DateTime::from_timestamp(now as i64, 0) else {
        return current_catalog;
    };
    let fetched_at = fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let artifact = json!({
        "version": 1,
        "fetchedAt": fetched_at,
        "fetchedAtEpoch": now,
        "lastAttemptAt": now,
        "catalog": catalog_value,
    });
    if write_private_json_atomic(&path, &artifact).is_err() {
        return current_catalog;
    }
    if let Err(error) = fs::remove_file(attempt_path) {
        eprintln!("Could not remove successful models.dev attempt marker: {error}");
    }
    Some(catalog)
}

fn fetch_models_dev_catalog() -> Option<Value> {
    fetch_models_dev_catalog_with(|agent, url| {
        let response = agent
            .get(url)
            .header("Accept", "application/json")
            .header("User-Agent", "Codex-Studio-pricing-cache/1")
            .call()
            .ok()?;
        if response.status().as_u16() != 200 {
            return None;
        }
        let body: Value = serde_json::from_reader(response.into_body().into_reader()).ok()?;
        body.get("providers").cloned().or(Some(body))
    })
}

fn fetch_models_dev_catalog_with<F>(request: F) -> Option<Value>
where
    F: FnOnce(&ureq::Agent, &str) -> Option<Value>,
{
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .max_redirects(0)
        .tls_config(tls)
        .timeout_global(Some(PRICING_CATALOG_REQUEST_TIMEOUT))
        .build();
    request(
        &ureq::Agent::new_with_config(agent),
        "https://models.dev/api.json",
    )
}

fn write_pricing_attempt(path: &Path, now: f64) -> Result<(), UsageError> {
    write_private_json_atomic(path, &json!({"lastAttemptAt":now}))
}

fn write_private_json_atomic(path: &Path, value: &Value) -> Result<(), UsageError> {
    let parent = path.parent().ok_or(UsageError::Scan)?;
    create_private_dir(parent)?;
    let bytes = serde_json::to_vec(value).map_err(|_| UsageError::Scan)?;
    crate::atomic_file::replace(path, &bytes, Some(0o600), false).map_err(|_| UsageError::Scan)
}

fn check_scan_timeout(started: Instant) -> Result<(), UsageError> {
    if started.elapsed() >= COST_SCAN_TIMEOUT_SECONDS {
        Err(UsageError::Scan)
    } else {
        Ok(())
    }
}

fn claude_cost_snapshot(
    state_dir: &Path,
    account_key: &str,
    home: &Path,
    now: f64,
) -> Result<Value, UsageError> {
    let identity = format!("{account_key}{}", home.to_string_lossy());
    let path = state_dir
        .join("account-costs/claude")
        .join(digest(&identity))
        .join("claude-costs.json");
    let saved = read_json(&path);
    let data = saved
        .as_ref()
        .filter(|saved| {
            saved.get("config").and_then(Value::as_str) == Some(home.to_string_lossy().as_ref())
        })
        .and_then(|saved| saved.get("data"))
        .filter(|value| value.is_object())
        .cloned();
    let checked = saved
        .as_ref()
        .and_then(|saved| saved.get("checked"))
        .cloned()
        .unwrap_or(Value::Null);
    let stale = data.is_none()
        || saved
            .as_ref()
            .and_then(|saved| saved.get("error"))
            .is_some_and(|error| !error.is_null());
    let refreshing = cost_refresh_active(&account_refresh_key(account_key, home));
    let refresh_due = checked
        .as_f64()
        .is_none_or(|checked| now - checked >= CLAUDE_COST_REFRESH_SECONDS);
    let error = saved
        .as_ref()
        .and_then(|saved| saved.get("error"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({
        "at": checked,
        "error": error,
        "data": data,
        "refreshing": refreshing,
        "_refreshDue": refresh_due,
        "stale": stale,
        "accountKey": account_key,
    }))
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn create_private_dir(path: &Path) -> Result<(), UsageError> {
    fs::create_dir_all(path).map_err(|_| UsageError::Scan)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| UsageError::Scan)?;
    }
    Ok(())
}

fn digest(value: &str) -> String {
    studio_runtime_store::sha256_hex(value.as_bytes())
}

fn local_day(timestamp: f64) -> String {
    chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::{account_cost_snapshot, refresh_account_costs, scan_claude_sessions};
    use serde_json::{Value, json};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[test]
    fn concurrent_refresh_for_same_account_is_rejected_until_guard_drops() {
        let key = "same-account-profile".to_owned();
        let first = super::begin_cost_refresh(key.clone()).expect("first refresh claims key");
        assert!(super::cost_refresh_active(&key));
        assert!(super::begin_cost_refresh(key.clone()).is_none());
        drop(first);
        assert!(!super::cost_refresh_active(&key));
        assert!(super::begin_cost_refresh(key).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn long_fake_scan_records_checked_at_when_the_scan_finishes() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let state_dir = directory.path().join("state");
        let home = directory.path().join("profile");
        let account =
            json!({"id":"paused-long-scan","accountId":"acct","home":home,"status":"ready"});
        let clock_value = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1_000));
        let clock_for_worker = std::sync::Arc::clone(&clock_value);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (finish_sender, finish_receiver) = tokio::sync::oneshot::channel();
        let account_for_worker = account.clone();
        let worker = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/usage",
            move || {
                super::refresh_account_costs_with(
                    &state_dir,
                    "paused-long-scan",
                    &account_for_worker,
                    &AtomicBool::new(false),
                    move || clock_for_worker.load(std::sync::atomic::Ordering::Relaxed) as f64,
                    move |_, _, _| {
                        started_sender
                            .send(())
                            .map_err(|_| super::UsageError::Scan)?;
                        finish_receiver
                            .blocking_recv()
                            .map_err(|_| super::UsageError::Scan)?;
                        Ok(json!({"todayUSD":2.0,"last30DaysUSD":3.0,"todayTokens":4,
                        "last30DaysTokens":5,"sourceDay":"2026-10-15","coverage":"reported",
                        "unknownModels":[],"historyDays":30}))
                    },
                )
            },
        );
        started_receiver.await.expect("scanner starts");
        tokio::time::advance(Duration::from_secs(121)).await;
        clock_value.store(1_121, std::sync::atomic::Ordering::Relaxed);
        finish_sender.send(()).expect("scanner finishes");
        let (cache_saved, result) = worker.await.expect("refresh worker");
        assert!(cache_saved);
        result.expect("refresh succeeds");

        let snapshot = account_cost_snapshot(
            &directory.path().join("state"),
            "paused-long-scan",
            &account,
            1_122.0,
        )
        .expect("read completed cache");
        assert_eq!(snapshot["checkedAt"], 1_121.0);
        assert_eq!(snapshot["_refreshDue"], false);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_cache_save_has_a_normal_refresh_backoff() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let state_dir = directory.path().join("state");
        let home = directory.path().join("profile");
        let key = "failed-save-backoff";
        let account = json!({"id":key,"accountId":"acct","home":home,"status":"ready"});
        let account_identity =
            crate::event_write::python_json(&json!([key, "acct", home.to_string_lossy().as_ref()]));
        let cache_root = state_dir
            .join("account-costs")
            .join(super::digest(&account_identity));
        std::fs::create_dir_all(cache_root.join("local-costs.json")).expect("make cache save fail");
        assert!(super::claim_cost_refresh_attempt(key));

        let (cache_saved, result) = super::refresh_account_costs_with(
            &state_dir,
            key,
            &account,
            &AtomicBool::new(false),
            || 1_000.0,
            |_, _, _| {
                Ok(json!({"todayUSD":2.0,"last30DaysUSD":3.0,"todayTokens":4,
                    "last30DaysTokens":5,"sourceDay":"2026-10-15","coverage":"reported",
                    "unknownModels":[],"historyDays":30}))
            },
        );
        assert!(!cache_saved);
        assert!(matches!(result, Err(super::UsageError::Scan)));
        assert!(!super::claim_cost_refresh_attempt(key));
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(super::claim_cost_refresh_attempt(key));
    }

    #[test]
    fn reads_python_shaped_local_cache_without_starting_a_refresh() {
        let root = std::env::temp_dir().join(format!("usage-costs-{}", std::process::id()));
        let home = root.join("profile");
        let account_root = root.join("state/account-costs");
        let key = "default";
        let account_id = "account-雪";
        assert!(crate::event_write::python_json(&json!(account_id)).contains("\\u96ea"));
        let account_identity = crate::event_write::python_json(&json!([
            key,
            account_id,
            home.to_string_lossy().as_ref()
        ]));
        let account_hash = super::digest(&account_identity);
        let cache = account_root.join(account_hash);
        let cache_identity = crate::event_write::python_json(&json!([
            home.to_string_lossy().as_ref(),
            cache.to_string_lossy().as_ref()
        ]));
        let scope = super::digest(&cache_identity);
        std::fs::create_dir_all(&cache).expect("fixture directory");
        std::fs::write(
            cache.join("local-costs.json"),
            json!({"scope":scope,"state":{"at":120,"checkedAt":120,"error":null,
                "data":{"todayUSD":2.5,"todayTokens":7,"sourceDay":"1900-01-01"}}})
            .to_string(),
        )
        .expect("fixture cache");

        let response = account_cost_snapshot(
            &root.join("state"),
            key,
            &json!({"id":key,"accountId":account_id,"home":home,"status":"ready"}),
            122.0,
        )
        .expect("cache read");
        assert_eq!(response["data"]["todayUSD"], Value::Null);
        assert_eq!(response["data"]["todayTokens"], Value::Null);
        assert_eq!(response["data"]["source"], "Codex profile logs");
        assert_eq!(response["refreshing"], false);
        assert_eq!(response["stale"], true);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[test]
    fn missing_source_day_clears_daily_totals_and_marks_cache_stale() {
        let root = std::env::temp_dir().join(format!("usage-cost-day-{}", std::process::id()));
        let home = root.join("profile");
        let key = "default";
        let account_id = "account-1";
        let account_root = root.join("state/account-costs");
        let account_identity = crate::event_write::python_json(&json!([
            key,
            account_id,
            home.to_string_lossy().as_ref()
        ]));
        let cache = account_root.join(super::digest(&account_identity));
        let scope_identity = crate::event_write::python_json(&json!([
            home.to_string_lossy().as_ref(),
            cache.to_string_lossy().as_ref()
        ]));
        let scope = super::digest(&scope_identity);
        std::fs::create_dir_all(&cache).expect("fixture directory");
        std::fs::write(
            cache.join("local-costs.json"),
            json!({
                "scope": scope,
                "state": {"at": 100.0, "checkedAt": 100.0, "error": null,
                    "data": {"todayUSD": 3.0, "todayTokens": 8}}
            })
            .to_string(),
        )
        .expect("fixture cache");
        let response = account_cost_snapshot(
            &root.join("state"),
            key,
            &json!({"id":key,"accountId":account_id,"home":home,"status":"ready"}),
            101.0,
        )
        .expect("cache read");
        assert_eq!(response["stale"], true);
        assert_eq!(response["data"]["todayUSD"], Value::Null);
        assert_eq!(response["data"]["todayTokens"], Value::Null);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[test]
    fn scanner_lock_contention_matches_python_busy_error() {
        let _guard = super::CODEX_SCAN_LOCK
            .lock()
            .expect("the scanner lock is not poisoned");
        assert!(matches!(
            super::acquire_codex_scan_lock_for(Duration::ZERO, None),
            Err(super::UsageError::ScannerBusy)
        ));
    }

    #[test]
    fn codex_session_fixture_counts_total_deltas_skips_fork_replay_and_flags_unpriced_models() {
        let root = tempfile::tempdir().expect("fixture directory");
        let home = root.path();
        let day = home.join("sessions/2026/10/15");
        std::fs::create_dir_all(&day).expect("fixture sessions");
        std::fs::create_dir_all(home.join("archived_sessions")).expect("fixture archive");
        let now = chrono::Utc::now();
        let at = |minutes: i64| (now - chrono::Duration::minutes(minutes)).to_rfc3339();
        let count = |minutes: i64, input: u64, cached: u64, output: u64| {
            json!({"timestamp":at(minutes),"type":"event_msg","payload":{"type":"token_count",
                "info":{"total_token_usage":{"input_tokens":input,"cached_input_tokens":cached,
                "output_tokens":output}}}})
            .to_string()
        };
        let context = |minutes: i64, model: &str| {
            json!({"timestamp":at(minutes),"type":"turn_context","payload":{"model":model}})
                .to_string()
        };
        // Repeated totals add nothing: 1_000_000 input (200_000 cached) and 100_000 output.
        let parent = [
            context(60, "openai/gpt-test-2026-01-01"),
            count(59, 400_000, 100_000, 40_000),
            count(58, 400_000, 100_000, 40_000),
            count(57, 1_000_000, 200_000, 100_000),
        ];
        std::fs::write(day.join("rollout-parent.jsonl"), parent.join("\n")).expect("parent log");
        // The fork replays the parent history, then adds 1_000 input and 2_000 output tokens.
        let fork = [
            json!({"timestamp":at(30),"type":"session_meta","payload":{"id":"fork",
                "forked_from_id":"parent","timestamp":at(30)}})
            .to_string(),
            context(40, "gpt-test"),
            count(39, 1_000_000, 200_000, 100_000),
            count(20, 1_001_000, 200_000, 102_000),
        ];
        std::fs::write(
            home.join("archived_sessions/rollout-fork.jsonl"),
            fork.join("\n"),
        )
        .expect("fork log");
        let unknown = [context(10, "gpt-unpriced"), count(9, 10, 0, 5)];
        std::fs::write(day.join("rollout-unknown.jsonl"), unknown.join("\n")).expect("unknown log");
        let catalog = crate::pricing::PricingCatalog::from_value(&json!({"providers":{
            "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":2,"cache_read":0.5}}}},
            "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":2}}}}}}))
        .expect("fixture catalog");

        let report = super::scan_codex_sessions_with_catalog(
            home,
            now.timestamp() as f64,
            &AtomicBool::new(false),
            || Some(catalog),
        )
        .expect("scan fixture logs");
        // Parent: 800_000 uncached input * 1 + 200_000 cached * 0.5 + 100_000 output * 2 = 1.1 USD.
        // Fork: 1_000 input * 1 + 2_000 output * 2 = 0.005 USD.
        let month = report["last30DaysUSD"].as_f64().expect("priced month");
        assert!((month - 1.105).abs() < 1e-9, "{month}");
        assert_eq!(report["last30DaysTokens"], 1_100_000 + 3_000 + 15);
        assert_eq!(report["unknownModels"], json!(["gpt-unpriced"]));
        assert_eq!(report["coverage"], "partial");
        assert_eq!(report["historyDays"], 30);
        let latest = (now - chrono::Duration::minutes(9)).with_timezone(&chrono::Local);
        assert_eq!(report["sourceDay"], latest.format("%Y-%m-%d").to_string());
    }

    #[test]
    fn local_claude_session_fixture_is_deduplicated_and_priced() {
        let root = std::env::temp_dir().join(format!("usage-claude-{}", std::process::id()));
        let home = root.join("profile");
        let logs = home.join("projects/sample");
        std::fs::create_dir_all(&logs).expect("fixture sessions");
        std::fs::create_dir_all(root.join("state/model-pricing")).expect("fixture pricing");
        std::fs::write(
            root.join("state/model-pricing/models-dev-v1.json"),
            json!({
                "version":1,"fetchedAtEpoch":chrono::Utc::now().timestamp(),
                "catalog":{"providers":{
                    "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":2}}}},
                    "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":2}}}}
                }}
            })
            .to_string(),
        )
        .expect("fixture catalog");
        let now = chrono::Utc::now();
        let stamp = now.to_rfc3339();
        let row = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{stamp}\",\"message\":{{\"id\":\"message-1\",\"model\":\"claude-test\",\"usage\":{{\"input_tokens\":100000,\"output_tokens\":100000}}}}}}\n"
        );
        let log_path = logs.join("session.jsonl");
        std::fs::write(&log_path, format!("{row}{row}")).expect("fixture log");
        let result = scan_claude_sessions(
            &home,
            &root.join("state"),
            now.timestamp() as f64,
            &AtomicBool::new(false),
        )
        .expect("scan fixture logs");
        let oracle = std::process::Command::new("python3")
            .arg("-B")
            .arg("-c")
            .arg("import json,sys; from codex_claude_costs import parse_claude_usage; from codex_pricing import price_usage; rows=parse_claude_usage(sys.argv[1]); row=rows[0]; print(json.dumps(price_usage(json.load(open(sys.argv[2]))['catalog'],'anthropic',row['model'],row['usage'],context_tokens=100000,input_tokens_are_uncached=True)[0]))")
            .arg(&log_path)
            .arg(root.join("state/model-pricing/models-dev-v1.json"))
            .env_remove("CODEX_AGENTS_PYTHON")
            .env("PYTHONPATH", env!("CARGO_MANIFEST_DIR").replace("/packages/runtime-services", "/apps/server/src"))
            .output()
            .expect("run Python pricing oracle");
        assert!(
            oracle.status.success(),
            "Python cost oracle failed: {}",
            String::from_utf8_lossy(&oracle.stderr)
        );
        let expected: f64 =
            serde_json::from_slice(&oracle.stdout).expect("Python cost oracle result");
        assert_eq!(result["todayUSD"], 0.3);
        assert_eq!(result["todayUSD"].as_f64(), Some(expected));
        assert_eq!(result["last30DaysUSD"], 0.3);
        assert_eq!(result["coverage"], "reported");
        let account = json!({"id":"claude-local","provider":"claude","home":home,
            "status":"ready"});
        let state = root.join("state");
        refresh_account_costs(&state, "claude-local", &account, &AtomicBool::new(false))
            .expect("refresh local Claude cache");
        let cached =
            account_cost_snapshot(&state, "claude-local", &account, now.timestamp() as f64)
                .expect("read refreshed Claude cache");
        assert_eq!(cached["stale"], false);
        assert_eq!(cached["refreshing"], false);
        assert_eq!(cached["data"]["todayUSD"], 0.3);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[test]
    fn missing_claude_output_tokens_remain_incomplete_like_python() {
        let root = std::env::temp_dir().join(format!(
            "usage-claude-missing-output-{}",
            std::process::id()
        ));
        let home = root.join("profile");
        let logs = home.join("projects/sample");
        let pricing = root.join("state/model-pricing");
        std::fs::create_dir_all(&logs).expect("fixture sessions");
        std::fs::create_dir_all(&pricing).expect("fixture pricing");
        std::fs::write(
            pricing.join("models-dev-v1.json"),
            json!({"version":1,"fetchedAtEpoch":chrono::Utc::now().timestamp(),
                "catalog":{"providers":{
                "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":2}}}},
                "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":2}}}}
            }}})
            .to_string(),
        )
        .expect("write fixture catalog");
        let now = chrono::Utc::now();
        std::fs::write(
            logs.join("session.jsonl"),
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"id\":\"message-1\",\"model\":\"claude-test\",\"usage\":{{\"input_tokens\":100000}}}}}}\n",
                now.to_rfc3339()
            ),
        )
        .expect("write session row without output token count");

        let result = scan_claude_sessions(
            &home,
            &root.join("state"),
            now.timestamp() as f64,
            &AtomicBool::new(false),
        )
        .expect("scan fixture sessions");
        assert_eq!(result["coverage"], "partial");
        assert_eq!(result["todayUSD"], Value::Null);
        assert_eq!(result["unknownModels"], json!(["claude-test"]));
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }

    #[test]
    fn missing_pricing_artifact_fetches_and_persists_catalog_before_cost_scan() {
        let root = tempfile::tempdir().expect("fixture state directory");
        let fetched = std::sync::atomic::AtomicBool::new(false);
        let providers = json!({
            "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":2}}}},
            "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":2}}}}
        });
        let catalog = super::pricing_catalog_with_fetch(root.path(), 2_000_000.0, || {
            fetched.store(true, std::sync::atomic::Ordering::Relaxed);
            Some(providers.clone())
        });
        assert!(catalog.is_some(), "wait-ready returns the fetched catalog");
        assert!(fetched.load(std::sync::atomic::Ordering::Relaxed));
        let artifact =
            std::fs::read_to_string(root.path().join("model-pricing/models-dev-v1.json"))
                .expect("pricing artifact was persisted");
        assert!(artifact.contains("claude-test"));

        let reused = super::pricing_catalog_with_fetch(root.path(), 2_000_001.0, || {
            panic!("fresh pricing catalog must not be fetched for each request")
        });
        assert!(reused.is_some());

        let mut refreshed = false;
        let catalog = super::pricing_catalog_with_fetch(
            root.path(),
            2_000_000.0 + super::PRICING_CATALOG_TTL_SECONDS,
            || {
                refreshed = true;
                Some(providers)
            },
        );
        assert!(catalog.is_some());
        assert!(refreshed, "expired catalog schedules a new refresh");
    }

    #[test]
    fn models_dev_fetch_builds_platform_tls_and_uses_injected_response() {
        let expected = json!({"openai":{"models":{}},"anthropic":{"models":{}}});
        let fetched = super::fetch_models_dev_catalog_with(|_agent, url| {
            assert_eq!(url, "https://models.dev/api.json");
            Some(expected.clone())
        });
        assert_eq!(fetched, Some(expected));
    }

    #[test]
    fn claude_refresh_uses_completion_time_and_does_not_publish_after_cancellation() {
        let root = tempfile::tempdir().expect("fixture state directory");
        let home = root.path().join("profile");
        std::fs::create_dir_all(home.join("projects")).expect("fixture projects");
        let state = root.path().join("state");
        let cancelled = AtomicBool::new(false);
        super::refresh_claude_costs(&state, "claude-local", &home, 200.0, &cancelled, || 321.0)
            .expect("refresh completes");
        let identity = format!("claude-local{}", home.to_string_lossy());
        let cache = state
            .join("account-costs/claude")
            .join(super::digest(&identity))
            .join("claude-costs.json");
        let saved: Value = serde_json::from_slice(&std::fs::read(&cache).expect("cache exists"))
            .expect("valid cache");
        assert_eq!(saved["checked"], 321.0);

        let cancelled_state = root.path().join("cancelled-state");
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            super::refresh_claude_costs(
                &cancelled_state,
                "claude-local",
                &home,
                200.0,
                &cancelled,
                || 321.0,
            ),
            Err(super::UsageError::ScannerCancelled)
        ));
        let cancelled_cache = cancelled_state
            .join("account-costs/claude")
            .join(super::digest(&identity))
            .join("claude-costs.json");
        assert!(!cancelled_cache.exists(), "cancelled scan is not published");
    }

    #[test]
    fn claude_pricing_loop_observes_shutdown_after_catalog_fetch() {
        let root = tempfile::tempdir().expect("fixture state directory");
        let home = root.path().join("profile");
        let logs = home.join("projects/sample");
        std::fs::create_dir_all(&logs).expect("fixture sessions");
        std::fs::write(
            logs.join("session.jsonl"),
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"id\":\"message-1\",\"model\":\"claude-test\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n",
                chrono::Utc::now().to_rfc3339()
            ),
        )
        .expect("write usage row");
        let cancelled = AtomicBool::new(false);
        let result = super::scan_claude_sessions_with_catalog(
            &home,
            chrono::Utc::now().timestamp() as f64,
            &cancelled,
            || {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                None
            },
        );
        assert!(matches!(result, Err(super::UsageError::ScannerCancelled)));
    }

    #[test]
    fn failed_pricing_fetch_uses_python_daily_attempt_ttl() {
        let root = tempfile::tempdir().expect("fixture state directory");
        let now = 2_000_000.0;
        assert!(super::pricing_catalog_with_fetch(root.path(), now, || None).is_none());
        let attempt = std::fs::read_to_string(root.path().join("model-pricing/refresh-state.json"))
            .expect("attempt time is persisted separately from successful catalog");
        assert_eq!(
            serde_json::from_str::<Value>(&attempt).expect("attempt state remains valid JSON")["lastAttemptAt"],
            now
        );

        let mut fetched = false;
        assert!(
            super::pricing_catalog_with_fetch(
                root.path(),
                now + super::PRICING_CATALOG_TTL_SECONDS - 1.0,
                || {
                    fetched = true;
                    Some(json!({}))
                },
            )
            .is_none()
        );
        assert!(
            !fetched,
            "failed fetch follows Python's 24-hour attempt TTL"
        );

        let providers = json!({
            "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":2}}}},
            "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":2}}}}
        });
        let catalog = super::pricing_catalog_with_fetch(
            root.path(),
            now + super::PRICING_CATALOG_TTL_SECONDS,
            || {
                fetched = true;
                Some(providers)
            },
        );
        assert!(catalog.is_some());
        assert!(fetched);
        assert!(
            !root
                .path()
                .join("model-pricing/refresh-state.json")
                .exists()
        );
    }
}
