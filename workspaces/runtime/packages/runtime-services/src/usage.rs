//! Local usage snapshot readers that do not require provider or turn drivers.

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

const COST_REFRESH_SECONDS: f64 = 120.0;
const CLAUDE_COST_REFRESH_SECONDS: f64 = 300.0;
const COST_SCAN_HISTORY_DAYS: i64 = 30;
const COST_SCAN_OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
const COST_SCANNER_BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const COST_SCANNER_BUILD_SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../apps/server/src/cost-scanner/build.py"
);
const COST_SCAN_TIMEOUT: Duration = Duration::from_secs(900);
static CODEX_SCAN_LOCK: Mutex<()> = Mutex::new(());

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
/// provider session, or billing endpoint.
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
    let lock_path = account_root.join(".refresh.lock");
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
    let refreshing = refresh_lock_active(&lock_path);
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

/// Refresh local Codex session logs into the same scoped cache used by Python.
/// Called only from the measured blocking pool; GET handlers return their cache
/// before scheduling this work. Python owner: codex_costs.py:153-306, 313-385.
pub fn refresh_account_costs(
    state_dir: &Path,
    account_key: &str,
    account: &Value,
    now: f64,
) -> Result<(), UsageError> {
    let home = account
        .get("home")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .ok_or(UsageError::MissingProfile)?;
    if account.get("provider").and_then(Value::as_str) == Some("claude")
        || account_key == "claude-local"
    {
        return refresh_claude_costs(state_dir, account_key, &home, now);
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
    create_private_dir(&cache_root)?;
    let lock_path = cache_root.join(".refresh.lock");
    let Some(_lock) = claim_refresh_lock(&lock_path)? else {
        return Ok(());
    };
    let result: Result<(), UsageError> = (|| {
        let scanner_cache = cache_root.join("scanner");
        create_private_dir(&scanner_cache)?;
        let _scan_guard = acquire_codex_scan_lock()?;
        let data = scan_codex_sessions(&home, state_dir, &scanner_cache)?;
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
            now
        } else {
            previous
                .as_ref()
                .and_then(|v| v.pointer("/state/at"))
                .and_then(Value::as_f64)
                .unwrap_or(now)
        };
        let state = json!({"at":at,"checkedAt":now,"error":null,"data":data,"sourceFingerprint":fingerprint});
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
    if let Err(error) = &result {
        let path = cache_root.join("local-costs.json");
        let mut state = read_json(&path)
            .and_then(|cache| cache.get("state").cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"at":null,"data":null}));
        if let Some(fields) = state.as_object_mut() {
            fields.insert("error".to_owned(), Value::String(error.to_string()));
            fields.insert("checkedAt".to_owned(), Value::from(now));
        }
        let temporary = cache_root.join(format!(".local-costs-error-{}.tmp", std::process::id()));
        if let Ok(bytes) = serde_json::to_vec(&json!({"scope":scope,"state":state})) {
            if fs::write(&temporary, bytes).is_ok() {
                if let Err(error) = fs::rename(&temporary, path) {
                    eprintln!("Could not persist local cost scan failure: {error}");
                }
            }
        }
    }
    drop(_lock);
    remove_refresh_lock(&lock_path);
    result
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
) -> Result<(), UsageError> {
    let identity = format!("{account_key}{}", home.to_string_lossy());
    let root = state_dir
        .join("account-costs/claude")
        .join(digest(&identity));
    create_private_dir(&root)?;
    let cache = root.join("claude-costs.json");
    let lock_path = cache.with_extension("refresh.lock");
    let Some(lock) = claim_refresh_lock(&lock_path)? else {
        return Ok(());
    };
    let previous = read_json(&cache);
    let (data, error) = match scan_claude_sessions(home, state_dir, now) {
        Ok(data) => (Some(data), Value::Null),
        Err(_) => (
            previous.as_ref().and_then(|v| v.get("data")).cloned(),
            Value::String("Claude account logs are unavailable".to_owned()),
        ),
    };
    let content = json!({"config":home,"data":data,"checked":now,"error":error});
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
    fs::rename(&temporary, cache).map_err(|_| UsageError::Scan)?;
    drop(lock);
    remove_refresh_lock(&lock_path);
    Ok(())
}

fn remove_refresh_lock(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("Could not remove local cost refresh lock: {error}"),
    }
}

fn refresh_lock_active(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        < COST_SCAN_TIMEOUT
}

fn claim_refresh_lock(path: &Path) -> Result<Option<File>, UsageError> {
    loop {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => return Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if refresh_lock_active(path) {
                    return Ok(None);
                }
                remove_refresh_lock(path);
            }
            Err(_) => return Err(UsageError::Scan),
        }
    }
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
fn scan_claude_sessions(config: &Path, state_dir: &Path, now: f64) -> Result<Value, UsageError> {
    let started = Instant::now();
    let projects = config.join("projects");
    if !projects.is_dir() {
        return Err(UsageError::Scan);
    }
    let mut stack = vec![projects];
    let mut rows = BTreeMap::<String, ClaudeUsage>::new();
    while let Some(directory) = stack.pop() {
        check_scan_timeout(started)?;
        for entry in fs::read_dir(directory).map_err(|_| UsageError::Scan)? {
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
                    output: value("output_tokens"),
                });
            }
        }
    }

    let artifact = read_json(&state_dir.join("model-pricing/models-dev-v1.json"));
    let catalog = artifact
        .as_ref()
        .and_then(|value| value.get("catalog"))
        .and_then(|value| crate::pricing::PricingCatalog::from_value(value).ok());
    let today = local_day(now);
    let mut today_cost = 0.0;
    let mut month_cost = 0.0;
    let mut today_priced = false;
    let mut month_priced = false;
    let mut unknown = BTreeSet::new();
    let mut groups = BTreeMap::<String, f64>::new();
    for row in rows.values() {
        let day = chrono::DateTime::from_timestamp(row.at as i64, 0).map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        });
        let is_today = day.as_deref() == Some(today.as_str());
        let is_month = now - row.at <= COST_SCAN_HISTORY_DAYS as f64 * 86_400.0;
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
/// Python owners: codex_costs.py:49-120, 313-385, 391-478.
fn scan_codex_sessions(
    home: &Path,
    state_dir: &Path,
    scanner_cache: &Path,
) -> Result<Value, UsageError> {
    let cost_root = state_dir.join("account-costs");
    let executable = cost_root.join("bin/codex-cost-scanner");
    create_private_dir(executable.parent().ok_or(UsageError::ScannerPrepare)?)?;
    let python = crate::python_tool::resolve_python_tool().ok_or(UsageError::ScannerPrepare)?;
    let mut build = Command::new(python);
    build
        .arg(COST_SCANNER_BUILD_SCRIPT)
        .arg("--output")
        .arg(&executable);
    let build_output = run_cost_command(
        build,
        COST_SCANNER_BUILD_TIMEOUT,
        COST_SCAN_OUTPUT_LIMIT,
        Some(64 * 1024),
    )
    .map_err(|_| UsageError::ScannerPrepare)?;
    if !build_output.status.success() {
        return Err(UsageError::ScannerPrepare);
    }
    write_scanner_pricing_copy(state_dir, scanner_cache)?;
    let mut command = Command::new(&executable);
    command
        .arg("--home")
        .arg(home)
        .arg("--cache")
        .arg(scanner_cache)
        .env("CODEX_HOME", home)
        .stderr(Stdio::null());
    let output = run_cost_command(command, COST_SCAN_TIMEOUT, COST_SCAN_OUTPUT_LIMIT, None)?;
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

fn acquire_codex_scan_lock() -> Result<MutexGuard<'static, ()>, UsageError> {
    acquire_codex_scan_lock_for(COST_SCAN_TIMEOUT)
}

fn acquire_codex_scan_lock_for(timeout: Duration) -> Result<MutexGuard<'static, ()>, UsageError> {
    let deadline = Instant::now() + timeout;
    loop {
        match CODEX_SCAN_LOCK.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
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
    let stdout = child.stdout().take().ok_or(UsageError::ScannerPrepare)?;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let stdout_reader = thread::spawn(move || {
        drop(stdout_sender.send(read_cost_pipe(stdout, stdout_limit)));
    });
    let stderr_reader = if let Some(limit) = stderr_limit {
        let stderr = child.stderr().take().ok_or(UsageError::ScannerPrepare)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || drop(sender.send(read_cost_pipe(stderr, limit))));
        Some((receiver, reader))
    } else {
        None
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            ),
            Ok(None) => {
                terminate_cost_child(&mut *child);
                drop(stdout_reader);
                if let Some((_, reader)) = stderr_reader {
                    drop(reader);
                }
                return Err(UsageError::ScannerTimedOut);
            }
            Err(_) => {
                terminate_cost_child(&mut *child);
                drop(stdout_reader);
                if let Some((_, reader)) = stderr_reader {
                    drop(reader);
                }
                return Err(UsageError::Scan);
            }
        }
    };
    terminate_cost_child(&mut *child);
    let stdout = stdout_receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| UsageError::ScannerTimedOut)?
        .map_err(|_| UsageError::Scan)?;
    if let Some((receiver, reader)) = stderr_reader {
        let _stderr = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| UsageError::ScannerTimedOut)?
            .map_err(|_| UsageError::Scan)?;
        drop(reader);
    }
    drop(stdout_reader);
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

fn terminate_cost_child(child: &mut dyn ChildWrapper) {
    drop(child.start_kill());
    drop(child.try_wait());
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
        "historyDays":row.get("historyDays").cloned().unwrap_or(json!(30)),
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
    if started.elapsed() >= COST_SCAN_TIMEOUT {
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
    let refreshing = refresh_lock_active(&path.with_extension("refresh.lock"));
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
    use std::time::Duration;

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
            super::acquire_codex_scan_lock_for(Duration::ZERO),
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
                "version":1,"catalog":{"providers":{
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
        let result = scan_claude_sessions(&home, &root.join("state"), now.timestamp() as f64)
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
        refresh_account_costs(&state, "claude-local", &account, now.timestamp() as f64)
            .expect("refresh local Claude cache");
        let cached =
            account_cost_snapshot(&state, "claude-local", &account, now.timestamp() as f64)
                .expect("read refreshed Claude cache");
        assert_eq!(cached["stale"], false);
        assert_eq!(cached["refreshing"], false);
        assert_eq!(cached["data"]["todayUSD"], 0.3);
        std::fs::remove_dir_all(root).expect("fixture cleanup");
    }
}
