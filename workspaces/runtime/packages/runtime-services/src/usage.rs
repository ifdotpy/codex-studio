//! Local usage snapshot readers that do not require provider or turn drivers.

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

// Python owns the shared cost values in codex_costs.py:27-33: refresh
// intervals, history windows, the scanner/build timeouts, and report bytes.
// costs-contract.py checks the Rust and Swift consumers against those owners.
const COST_REFRESH_SECONDS: f64 = 120.0;
const CLAUDE_COST_REFRESH_SECONDS: f64 = 300.0;
const CLAUDE_COST_HISTORY_DAYS: i64 = 30;
const COST_REPORT_HISTORY_DAYS: i64 = 30;
const COST_SCAN_OUTPUT_LIMIT_BYTES: usize = 32 * 1024 * 1024;
// Python owns models.dev refresh policy in codex_pricing.py:17-18,120-128.
const PRICING_CATALOG_TTL_SECONDS: f64 = 24.0 * 60.0 * 60.0;
const PRICING_CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
// Rust-only capture bound for the build helper's diagnostic pipes. Python's
// helper publishes only the final 4000 diagnostic characters (build.py:122-123).
const COST_SCANNER_BUILD_STDERR_LIMIT_BYTES: usize = 64 * 1024;
const COST_SCANNER_BUILD_TIMEOUT_SECONDS: Duration = Duration::from_secs(120);
const COST_SCAN_TIMEOUT_SECONDS: Duration = Duration::from_secs(900);
const COST_SCAN_LOCK_RETRY_INTERVAL_MILLISECONDS: u64 = 50;
const COST_COMMAND_POLL_INTERVAL_MILLISECONDS: u64 = 10;
// Keep helper shutdown aligned with cost-scanner/build.py's compiler cleanup grace.
const COST_COMMAND_TERMINATION_GRACE_SECONDS: Duration = Duration::from_secs(5);
// Rust-only scheduling headroom after the helper's bounded compiler cleanup.
const COST_COMMAND_TERMINATION_HEADROOM_SECONDS: Duration = Duration::from_secs(2);
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
    #[error("Cannot prepare the local cost scanner.")]
    ScannerPrepare,
    #[error("Local cost scan timed out.")]
    ScannerTimedOut,
    #[error("Local cost scanner is busy.")]
    ScannerBusy,
    #[error("Local cost scanner was cancelled during shutdown.")]
    ScannerCancelled,
    #[error("Could not stop and reap the local cost scanner.")]
    ScannerCleanup,
    #[error("Local cost scan failed (exit {0}).")]
    ScannerFailed(i32),
    #[error("Local cost report exceeds 32 MiB.")]
    ScannerOutputLimit,
    #[error("CodexBar did not return local Codex costs.")]
    ScannerReport,
    #[error("CodexBar returned an unsupported currency.")]
    ScannerCurrency,
    #[error("The local cost scanner returned invalid coverage.")]
    ScannerCoverage,
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
    if account.get("provider").and_then(Value::as_str) == Some("claude")
        || account_key == "claude-local"
    {
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
    let build_script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src/cost-scanner/build.py");
    refresh_account_costs_report(state_dir, account_key, account, cancelled, &build_script).1
}

/// Refresh and report whether a cache value was durably saved, even if the
/// scanner itself failed and the saved value records that failure.
pub fn refresh_account_costs_report(
    state_dir: &Path,
    account_key: &str,
    account: &Value,
    cancelled: &AtomicBool,
    build_script: &Path,
) -> (bool, Result<(), UsageError>) {
    let build_script = build_script.to_owned();
    refresh_account_costs_with(
        state_dir,
        account_key,
        account,
        cancelled,
        unix_time_seconds,
        move |home, state_dir, scanner_cache, cancelled| {
            scan_codex_sessions(home, state_dir, scanner_cache, &build_script, cancelled)
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
    S: FnOnce(&Path, &Path, &Path, &AtomicBool) -> Result<Value, UsageError>,
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
    let is_claude = account.get("provider").and_then(Value::as_str) == Some("claude")
        || account_key == "claude-local";
    if is_claude {
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
        let scanner_cache = cache_root.join("scanner");
        create_private_dir(&scanner_cache)?;
        let data = scanner(&home, state_dir, &scanner_cache, cancelled)?;
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
        let temp = cache_root.join(format!(".local-costs-{}.tmp", std::process::id()));
        fs::write(&temp, bytes).map_err(|_| UsageError::Scan)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
                .map_err(|_| UsageError::Scan)?;
        }
        fs::rename(&temp, cache_root.join("local-costs.json")).map_err(|_| UsageError::Scan)?;
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
        let temporary = cache_root.join(format!(".local-costs-error-{}.tmp", std::process::id()));
        if let Ok(bytes) = serde_json::to_vec(&json!({"scope":scope,"state":state}))
            && fs::write(&temporary, bytes).is_ok()
        {
            match fs::rename(&temporary, path) {
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
    let temporary = root.join(format!(".claude-costs-{}.tmp", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec(&content).map_err(|_| UsageError::Scan)?,
    )
    .map_err(|_| UsageError::Scan)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|_| UsageError::Scan)?;
    }
    if cancelled.load(Ordering::Acquire) {
        if let Err(error) = fs::remove_file(&temporary) {
            eprintln!("Could not remove cancelled Claude cost scan file: {error}");
        }
        return Err(UsageError::ScannerCancelled);
    }
    fs::rename(&temporary, cache).map_err(|_| UsageError::Scan)?;
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
        let usage = json!({"inputTokens":row.input,"cachedInputTokens":row.cached,
            "cacheWriteInputTokens":row.write,"outputTokens":row.output});
        let context = row
            .input
            .zip(row.cached)
            .zip(row.write)
            .map(|((a, b), c)| a.saturating_add(b).saturating_add(c) as f64);
        let price = catalog
            .as_ref()
            .map(|catalog| catalog.price_usage("anthropic", &row.model, &usage, context, true));
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

/// Match the Python scanner runner: build the vendored scanner, then run it
/// against only this Codex profile and normalize its aggregate report.
/// Python owners: builder invocation and 120-second timeout are in
/// codex_costs.py:342-350; resolved account home, isolated cache and CODEX_HOME
/// are at :352-391; CodexBar normalization is at :61-132. The helper's
/// 110-second compiler timeout, 5-second termination grace and atomic
/// executable publication are in cost-scanner/build.py:17-19,76-125.
fn scan_codex_sessions(
    home: &Path,
    state_dir: &Path,
    scanner_cache: &Path,
    build_script: &Path,
    cancelled: &AtomicBool,
) -> Result<Value, UsageError> {
    let cost_root = state_dir.join("account-costs");
    let executable = cost_root.join("bin/codex-cost-scanner");
    create_private_dir(executable.parent().ok_or(UsageError::ScannerPrepare)?)?;
    let python = crate::python_tool::resolve_python_tool().ok_or(UsageError::ScannerPrepare)?;
    let mut build = Command::new(python);
    build.arg(build_script).arg("--output").arg(&executable);
    let build_output = run_cost_command(
        build,
        COST_SCANNER_BUILD_TIMEOUT_SECONDS,
        COST_SCAN_OUTPUT_LIMIT_BYTES,
        Some(COST_SCANNER_BUILD_STDERR_LIMIT_BYTES),
        cancelled,
    )
    .map_err(|error| match error {
        UsageError::ScannerCancelled => UsageError::ScannerCancelled,
        _ => UsageError::ScannerPrepare,
    })?;
    if !build_output.status.success() {
        return Err(UsageError::ScannerPrepare);
    }
    let _ = pricing_catalog_with_fetch(state_dir, unix_time_seconds(), fetch_models_dev_catalog);
    write_scanner_pricing_copy(state_dir, scanner_cache)?;
    let mut command = Command::new(&executable);
    command
        .arg("--home")
        .arg(home)
        .arg("--cache")
        .arg(scanner_cache)
        .env("CODEX_HOME", home)
        .stderr(Stdio::null());
    let output = run_cost_command(
        command,
        COST_SCAN_TIMEOUT_SECONDS,
        COST_SCAN_OUTPUT_LIMIT_BYTES,
        None,
        cancelled,
    )?;
    if !output.status.success() {
        return Err(UsageError::ScannerFailed(output.status.code().unwrap_or(1)));
    }
    if output.stdout_exceeded {
        return Err(UsageError::ScannerOutputLimit);
    }
    let payload: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| UsageError::ScannerReport)?;
    normalize_codex_costs(&payload)
}

/// Python waits up to the scan timeout for its shared scan lock and only then
/// raises the busy result (codex_costs.py:239-242); preserve that wait here.
fn acquire_codex_scan_lock(cancelled: &AtomicBool) -> Result<MutexGuard<'static, ()>, UsageError> {
    acquire_codex_scan_lock_for(COST_SCAN_TIMEOUT_SECONDS, Some(cancelled))
}

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

fn write_scanner_pricing_copy(state_dir: &Path, scanner_cache: &Path) -> Result<(), UsageError> {
    let Some(mut artifact) = read_json(&state_dir.join("model-pricing/models-dev-v1.json")) else {
        return Ok(());
    };
    if let Some(providers) = artifact
        .get_mut("catalog")
        .and_then(Value::as_object_mut)
        .and_then(|catalog| catalog.get_mut("providers"))
        .and_then(Value::as_object_mut)
    {
        for provider in providers.values_mut() {
            let Some(models) = provider.get_mut("models").and_then(Value::as_object_mut) else {
                continue;
            };
            for model in models.values_mut() {
                let Some(cost) = model.get_mut("cost").and_then(Value::as_object_mut) else {
                    continue;
                };
                let Some(tiers) = cost.get("tiers").and_then(Value::as_array) else {
                    continue;
                };
                if tiers.is_empty() {
                    continue;
                }
                let context_tiers = tiers
                    .iter()
                    .filter(|tier| {
                        tier.pointer("/tier/type").and_then(Value::as_str) == Some("context")
                    })
                    .collect::<Vec<_>>();
                if context_tiers.len() == 1
                    && cost_amount(context_tiers[0].pointer("/tier/size")) == Some(200_000.0)
                {
                    let mut context = serde_json::Map::new();
                    for key in ["input", "output", "cache_read", "cache_write"] {
                        if let Some(value) = context_tiers[0]
                            .get(key)
                            .filter(|value| cost_amount(Some(value)).is_some())
                        {
                            context.insert(key.to_owned(), value.clone());
                        }
                    }
                    cost.insert("context_over_200k".to_owned(), Value::Object(context));
                } else {
                    cost.remove("context_over_200k");
                }
            }
        }
    }
    let directory = scanner_cache.join("model-pricing");
    create_private_dir(&directory)?;
    let path = directory.join("models-dev-v1.json");
    let temporary = directory.join(".models-dev-v1.json.tmp");
    let bytes = serde_json::to_vec(&artifact).map_err(|_| UsageError::Scan)?;
    fs::write(&temporary, bytes).map_err(|_| UsageError::Scan)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|_| UsageError::Scan)?;
    }
    fs::rename(&temporary, path).map_err(|_| UsageError::Scan)
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
        if let Err(error) = write_private_json_atomic(&path, existing, ".models-dev-") {
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
    if write_private_json_atomic(&path, &artifact, ".models-dev-").is_err() {
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
    write_private_json_atomic(path, &json!({"lastAttemptAt":now}), ".refresh-state-")
}

fn write_private_json_atomic(path: &Path, value: &Value, prefix: &str) -> Result<(), UsageError> {
    let parent = path.parent().ok_or(UsageError::Scan)?;
    create_private_dir(parent)?;
    let temporary = parent.join(format!("{prefix}{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(value).map_err(|_| UsageError::Scan)?;
    fs::write(&temporary, bytes).map_err(|_| UsageError::Scan)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|_| UsageError::Scan)?;
    }
    fs::rename(&temporary, path).map_err(|_| UsageError::Scan)
}

struct CostCommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stdout_exceeded: bool,
}

fn run_cost_command(
    command: Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: Option<usize>,
    cancelled: &AtomicBool,
) -> Result<CostCommandOutput, UsageError> {
    let deadline = Instant::now() + timeout;
    let mut command = command;
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    command.stderr(if stderr_limit.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    let mut child = command.spawn().map_err(|_| UsageError::ScannerPrepare)?;
    let stdout = match child.stdout().take() {
        Some(stdout) => stdout,
        None => {
            terminate_cost_child(&mut *child)?;
            return Err(UsageError::ScannerPrepare);
        }
    };
    let stderr = if stderr_limit.is_some() {
        match child.stderr().take() {
            Some(stderr) => Some(stderr),
            None => {
                terminate_cost_child(&mut *child)?;
                return Err(UsageError::ScannerPrepare);
            }
        }
    } else {
        None
    };
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let stdout_reader = thread::spawn(move || {
        drop(stdout_sender.send(read_cost_pipe(stdout, stdout_limit)));
    });
    let stderr_reader = if let (Some(limit), Some(stderr)) = (stderr_limit, stderr) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || drop(sender.send(read_cost_pipe(stderr, limit))));
        Some((receiver, reader))
    } else {
        None
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if cancelled.load(Ordering::Acquire) => {
                terminate_cost_child(&mut *child)?;
                join_cost_reader(stdout_reader)?;
                if let Some((_, reader)) = stderr_reader {
                    join_cost_reader(reader)?;
                }
                return Err(UsageError::ScannerCancelled);
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(
                Duration::from_millis(COST_COMMAND_POLL_INTERVAL_MILLISECONDS)
                    .min(deadline.saturating_duration_since(Instant::now())),
            ),
            Ok(None) => {
                terminate_cost_child(&mut *child)?;
                join_cost_reader(stdout_reader)?;
                if let Some((_, reader)) = stderr_reader {
                    join_cost_reader(reader)?;
                }
                return Err(UsageError::ScannerTimedOut);
            }
            Err(_) => {
                terminate_cost_child(&mut *child)?;
                join_cost_reader(stdout_reader)?;
                if let Some((_, reader)) = stderr_reader {
                    join_cost_reader(reader)?;
                }
                return Err(UsageError::Scan);
            }
        }
    };
    child.wait().map_err(|_| UsageError::ScannerCleanup)?;
    let stdout_result = stdout_receiver.recv();
    join_cost_reader(stdout_reader)?;
    let stdout = stdout_result
        .map_err(|_| UsageError::Scan)?
        .map_err(|_| UsageError::Scan)?;
    if let Some((receiver, reader)) = stderr_reader {
        let stderr_result = receiver.recv();
        join_cost_reader(reader)?;
        let _stderr = stderr_result
            .map_err(|_| UsageError::Scan)?
            .map_err(|_| UsageError::Scan)?;
    }
    Ok(CostCommandOutput {
        status,
        stdout: stdout.bytes,
        stdout_exceeded: stdout.exceeded,
    })
}

struct CostPipeOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn read_cost_pipe(mut pipe: impl Read, limit: usize) -> std::io::Result<CostPipeOutput> {
    let mut bytes = Vec::with_capacity(limit.min(16_384));
    let mut buffer = [0_u8; 8192];
    let mut exceeded = false;
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let retained = count.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded |= retained < count;
    }
    Ok(CostPipeOutput { bytes, exceeded })
}

fn terminate_cost_child(child: &mut dyn ChildWrapper) -> Result<(), UsageError> {
    #[cfg(unix)]
    {
        let process_group = nix::unistd::Pid::from_raw(child.id() as i32);
        if nix::sys::signal::killpg(process_group, nix::sys::signal::Signal::SIGTERM).is_ok() {
            let deadline = Instant::now()
                + COST_COMMAND_TERMINATION_GRACE_SECONDS
                + COST_COMMAND_TERMINATION_HEADROOM_SECONDS
                + Duration::from_millis(COST_COMMAND_POLL_INTERVAL_MILLISECONDS);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        return child
                            .wait()
                            .map(|_| ())
                            .map_err(|_| UsageError::ScannerCleanup);
                    }
                    Ok(None) => thread::sleep(
                        Duration::from_millis(COST_COMMAND_POLL_INTERVAL_MILLISECONDS)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    ),
                    Err(_) => break,
                }
            }
        }
    }
    if child.start_kill().is_err()
        && child
            .try_wait()
            .map_err(|_| UsageError::ScannerCleanup)?
            .is_none()
    {
        return Err(UsageError::ScannerCleanup);
    }
    child.wait().map_err(|_| UsageError::ScannerCleanup)?;
    Ok(())
}

fn join_cost_reader(reader: thread::JoinHandle<()>) -> Result<(), UsageError> {
    reader.join().map_err(|_| UsageError::Scan)
}

fn normalize_codex_costs(payload: &Value) -> Result<Value, UsageError> {
    let row = payload
        .as_array()
        .and_then(|rows| {
            rows.iter().find(|row| {
                row.is_object() && row.get("provider").and_then(Value::as_str) == Some("codex")
            })
        })
        .ok_or(UsageError::ScannerReport)?;
    if !row.get("error").is_none_or(Value::is_null)
        || row.get("source").and_then(Value::as_str) != Some("local")
    {
        return Err(UsageError::ScannerReport);
    }
    if row.get("currencyCode").and_then(Value::as_str) != Some("USD") {
        return Err(UsageError::ScannerCurrency);
    }
    let coverage = row
        .get("coverage")
        .and_then(Value::as_object)
        .ok_or(UsageError::ScannerCoverage)?;
    let complete = row
        .get("historyCoverageIsEstablished")
        .and_then(Value::as_bool)
        .ok_or(UsageError::ScannerCoverage)?;
    let mut unknown = BTreeSet::new();
    if let Some(daily) = row.get("daily").and_then(Value::as_array) {
        for day in daily.iter().filter_map(Value::as_object) {
            let mut seen = BTreeSet::new();
            if let Some(breakdowns) = day.get("modelBreakdowns").and_then(Value::as_array) {
                for model in breakdowns.iter().filter_map(Value::as_object) {
                    let name = model
                        .get("modelName")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown model")
                        .chars()
                        .take(160)
                        .collect::<String>();
                    seen.insert(name.clone());
                    let cost = cost_amount(model.get("cost"));
                    let tokens = cost_amount(model.get("totalTokens"));
                    if cost.is_none() || (cost == Some(0.0) && tokens != Some(0.0)) {
                        unknown.insert(name);
                    }
                }
            }
            if let Some(models) = day.get("modelsUsed").and_then(Value::as_array) {
                for model in models {
                    let name = model
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| model.to_string())
                        .chars()
                        .take(160)
                        .collect::<String>();
                    if !seen.contains(&name) {
                        unknown.insert(name);
                    }
                }
            }
        }
    }
    let unpriced = coverage
        .iter()
        .filter(|(key, _)| key.to_ascii_lowercase().contains("unpriced"))
        .filter_map(|(_, value)| cost_amount(Some(value)))
        .sum::<f64>();
    let partial = !unknown.is_empty() || unpriced > 0.0 || !complete;
    let today = cost_amount(row.get("sessionCostUSD"));
    let month = cost_amount(row.get("last30DaysCostUSD"));
    let safe_today = if today == Some(0.0) && partial {
        Value::Null
    } else {
        cost_value(row.get("sessionCostUSD"))
    };
    let safe_month = if month == Some(0.0) && partial {
        Value::Null
    } else {
        cost_value(row.get("last30DaysCostUSD"))
    };
    let source_day = row
        .get("daily")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|day| day.get("date").and_then(Value::as_str))
        .filter(|day| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok())
        .max();
    Ok(json!({
        "source":"CodexBar local logs",
        "scope":"Local Codex sessions on this computer, including supported pi sessions. All projects and accounts in those logs.",
        "kind":"api_estimate",
        "currency":"USD",
        "billedUSD":null,
        "todayUSD":safe_today,
        "last30DaysUSD":safe_month,
        "todayTokens":cost_value(row.get("sessionTokens")),
        "last30DaysTokens":cost_value(row.get("last30DaysTokens")),
        "sourceUpdatedAt":row.get("updatedAt"),
        "sourceDay":source_day,
        "coverage":if partial {"partial"} else if !coverage.is_empty() {"reported"} else {"unverified"},
        "unknownModels":unknown,
        "historyDays":row.get("historyDays").cloned().unwrap_or(json!(COST_REPORT_HISTORY_DAYS)),
        "note":"API-rate estimate, not your ChatGPT bill. Base rates apply when the scanner cannot resolve a published context tier."
    }))
}

fn cost_amount(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|amount| amount.is_finite() && *amount >= 0.0)
}

fn cost_value(value: Option<&Value>) -> Value {
    value
        .filter(|value| cost_amount(Some(value)).is_some())
        .cloned()
        .unwrap_or(Value::Null)
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
mod tests {
    use super::{account_cost_snapshot, refresh_account_costs, scan_claude_sessions};
    use serde_json::{Value, json};
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

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
        let worker = tokio::task::spawn_blocking(move || {
            super::refresh_account_costs_with(
                &state_dir,
                "paused-long-scan",
                &account_for_worker,
                &AtomicBool::new(false),
                move || clock_for_worker.load(std::sync::atomic::Ordering::Relaxed) as f64,
                move |_, _, _, _| {
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
        });
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

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn shutdown_kills_and_reaps_fake_scanner_and_releases_refresh_guard() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let state_dir = directory.path().join("state");
        let home = directory.path().join("profile");
        let marker = directory.path().join("scanner-ready");
        let account =
            json!({"id":"cancelled-scan","accountId":"acct","home":home,"status":"ready"});
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let cancelled_for_worker = std::sync::Arc::clone(&cancelled);
        let account_for_worker = account.clone();
        let marker_for_worker = marker.clone();
        let state_dir_for_worker = state_dir.clone();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            super::refresh_account_costs_with(
                &state_dir_for_worker,
                "cancelled-scan",
                &account_for_worker,
                &cancelled_for_worker,
                || 1_000.0,
                move |_, _, _, cancellation| {
                    started_sender
                        .send(())
                        .map_err(|_| super::UsageError::Scan)?;
                    let mut command = std::process::Command::new("python3");
                    command
                        .arg("-c")
                        .arg("import os,pathlib,sys,time\nmarker=pathlib.Path(sys.argv[1])\ntemporary=marker.with_suffix('.tmp')\nwith temporary.open('w') as file: file.write(str(os.getpid()))\nos.replace(temporary, marker)\ntime.sleep(60)")
                        .arg(&marker_for_worker);
                    super::run_cost_command(
                        command,
                        Duration::from_secs(900),
                        1024,
                        None,
                        cancellation,
                    )?;
                    Ok(json!({}))
                },
            )
        });
        started_receiver.await.expect("fake scanner started");
        let ready_marker = marker.clone();
        let child_ready = tokio::task::spawn_blocking(move || {
            for _ in 0..500 {
                if ready_marker.is_file() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        })
        .await
        .expect("marker waiter");
        assert!(child_ready, "fake scanner should create its ready marker");
        let pid = std::fs::read_to_string(&marker)
            .expect("scanner PID")
            .parse::<u32>()
            .expect("valid scanner PID");
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        let (cache_saved, result) = worker.await.expect("refresh worker");
        assert!(!cache_saved);
        assert!(matches!(result, Err(super::UsageError::ScannerCancelled)));

        let probe = std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .expect("probe scanner process");
        assert!(!probe.success(), "cancelled scanner process must be reaped");
        assert!(!super::cost_refresh_active(&super::account_refresh_key(
            "cancelled-scan",
            &home
        )));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_scanner_build_reaps_its_detached_fake_compiler() {
        const READY_TIMEOUT: Duration = Duration::from_secs(5);

        let directory = tempfile::tempdir().expect("fixture directory");
        let build_script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/server/src/cost-scanner/build.py");
        let output = directory.path().join("scanner");
        let compiler = directory.path().join("fake-swiftc");
        let compiler_pid_path = directory.path().join("compiler.pid");
        std::fs::write(
            &compiler,
            "#!/usr/bin/env python3\nimport os,pathlib,sys,time\n".to_owned()
                + "marker=pathlib.Path(os.environ['FAKE_SWIFTC_PID'])\n"
                + "temporary=marker.with_suffix('.tmp')\n"
                + "temporary.write_text(str(os.getpid()))\n"
                + "os.replace(temporary, marker)\n"
                + "time.sleep(60)\n",
        )
        .expect("fake compiler script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o700))
                .expect("make fake compiler executable");
        }
        let helper_code = "import importlib.util,pathlib,sys; spec=importlib.util.spec_from_file_location('cost_scanner_build',sys.argv[1]); module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); module.compile_binary(pathlib.Path(sys.argv[2]),[],sys.argv[3])";
        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg(helper_code)
            .arg(&build_script)
            .arg(&output)
            .arg(&compiler)
            .env("FAKE_SWIFTC_PID", &compiler_pid_path);
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let worker_cancelled = std::sync::Arc::clone(&cancelled);
        let worker = tokio::task::spawn_blocking(move || {
            super::run_cost_command(
                command,
                Duration::from_secs(900),
                1024,
                Some(1024),
                &worker_cancelled,
            )
        });
        let ready_path = compiler_pid_path.clone();
        let compiler_pid = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + READY_TIMEOUT;
            while Instant::now() < deadline {
                if let Ok(value) = std::fs::read_to_string(&ready_path)
                    && let Ok(pid) = value.trim().parse::<u32>()
                {
                    return Some(pid);
                }
                thread::sleep(Duration::from_millis(10));
            }
            None
        })
        .await
        .expect("fake compiler PID waiter");
        let Some(compiler_pid) = compiler_pid else {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            drop(worker.await.expect("cancelled scanner worker"));
            panic!("fake compiler did not publish its PID");
        };

        cancelled.store(true, std::sync::atomic::Ordering::Release);
        let result = worker.await.expect("scanner build worker");
        assert!(matches!(result, Err(super::UsageError::ScannerCancelled)));
        let probe = Command::new("/bin/kill")
            .args(["-0", &compiler_pid.to_string()])
            .status()
            .expect("probe fake compiler process");
        assert!(
            !probe.success(),
            "cancelled compiler process must be reaped"
        );
        assert!(
            !output.exists(),
            "cancelled build must not publish the scanner"
        );
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
            |_, _, _, _| {
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
    fn codex_scanner_report_is_normalized_without_reparsing_session_logs() {
        let report = json!([{
            "provider":"codex",
            "source":"local",
            "currencyCode":"USD",
            "updatedAt":"2026-10-15T12:00:00Z",
            "historyDays":30,
            "historyCoverageIsEstablished":false,
            "coverage":{"priced":1,"unpriced":1},
            "sessionTokens":10,
            "sessionCostUSD":0,
            "last30DaysTokens":25,
            "last30DaysCostUSD":0,
            "daily":[{"date":"2026-10-15","modelsUsed":["gpt-unknown"],
                "modelBreakdowns":[{"modelName":"gpt-unknown","cost":null,"totalTokens":10}]}]
        }]);
        let normalized = super::normalize_codex_costs(&report).expect("valid scanner report");
        assert_eq!(normalized["sourceDay"], "2026-10-15");
        assert_eq!(normalized["todayTokens"], 10);
        assert_eq!(normalized["todayUSD"], Value::Null);
        assert_eq!(normalized["last30DaysUSD"], Value::Null);
        assert_eq!(normalized["unknownModels"], json!(["gpt-unknown"]));
        assert_eq!(normalized["coverage"], "partial");
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_uses_the_same_mac_only_scanner_builder_as_python() {
        let root = std::env::temp_dir().join(format!("usage-scanner-linux-{}", std::process::id()));
        let result = super::scan_codex_sessions(
            &root.join("profile"),
            &root.join("state"),
            &root.join("state/account-costs/test/scanner"),
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../apps/server/src/cost-scanner/build.py"),
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(super::UsageError::ScannerPrepare)));
        std::fs::remove_dir_all(root).expect("fixture cleanup");
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
