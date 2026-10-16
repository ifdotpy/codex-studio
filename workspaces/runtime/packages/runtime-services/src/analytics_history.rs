//! Bounded rollout tails and atomic analytics/checkpoint commits.
//!
//! The caller supplies an account-resolved profile and persisted agent. File
//! reads and budget receipts precede the analytics writer lock. A checkpoint
//! compare-and-swap prevents two readers from acknowledging the same batch.
use crate::analytics_capture::{
    CaptureError, CaptureSnapshot, CaptureSource, PreparedCapture, ResponseRateAssociation,
    check_identity, decide, text,
};
use crate::analytics_rollout::{RolloutAction, RolloutContext};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
mod read_proof;
use read_proof::ReadProof;
mod discovery;
pub use discovery::DiscoveryRound;
use std::fs::{self, File};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use studio_runtime_schema::shared_values::MAX_SAFE_INTEGER;
use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository as Repo, CaptureTable};
use studio_runtime_store::{AnalyticsRepository, RuntimeStore, sha256_hex};
use thiserror::Error;

pub const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_RECORDS: usize = 128;
const ANCHOR_BYTES: u64 = 256;

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Budget(#[from] crate::budget::BudgetError),
    #[error("analytics history read failed")]
    Read,
    #[error("analytics history checkpoint advanced concurrently")]
    StaleCheckpoint,
    #[error("analytics history file changed while reading; checkpoint retained")]
    ChangedDuringRead,
    #[error("analytics history collection cancelled")]
    Cancelled,
    #[error("Not implemented: requires analytics-history/budget-capture")]
    BudgetUnavailable,
    #[error("analytics history discovery exceeded its bounded entry budget")]
    DiscoveryLimit,
    #[error("analytics history discovery reached the current fairness-round budget")]
    DiscoveryRoundLimit,
    #[error("analytics history filesystem watcher failed")]
    Watcher(#[from] notify::Error),
    #[error("account profile snapshot could not be read")]
    AccountDriver(#[from] crate::account_driver::AccountDriverError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryRetryPolicy {
    Backoff,
    ContinueAfterRoundBudget,
    WaitForProfileInput,
    WaitForBudgetChange,
}

impl HistoryError {
    pub fn retry_policy(&self) -> HistoryRetryPolicy {
        match self {
            Self::Budget(crate::budget::BudgetError::Invalid(_)) => {
                HistoryRetryPolicy::WaitForBudgetChange
            }
            Self::DiscoveryLimit => HistoryRetryPolicy::WaitForProfileInput,
            Self::DiscoveryRoundLimit => HistoryRetryPolicy::ContinueAfterRoundBudget,
            _ => HistoryRetryPolicy::Backoff,
        }
    }
}

#[cfg(test)]
mod retry_policy_tests {
    use super::{HistoryError, HistoryRetryPolicy};

    #[test]
    fn permanent_input_failures_wait_for_input_but_transient_reads_back_off() {
        assert_eq!(
            HistoryError::Budget(crate::budget::BudgetError::Invalid(
                "runtime_budget/tokenBudget".to_owned(),
            ))
            .retry_policy(),
            HistoryRetryPolicy::WaitForBudgetChange
        );
        assert_eq!(
            HistoryError::DiscoveryLimit.retry_policy(),
            HistoryRetryPolicy::WaitForProfileInput
        );
        assert_eq!(
            HistoryError::DiscoveryRoundLimit.retry_policy(),
            HistoryRetryPolicy::ContinueAfterRoundBudget
        );
        assert_eq!(
            HistoryError::Read.retry_policy(),
            HistoryRetryPolicy::Backoff
        );
    }
}
#[derive(Clone, Copy)]
pub struct BatchLimits {
    bytes: usize,
    records: usize,
}
impl BatchLimits {
    pub fn new(bytes: usize, records: usize) -> Result<Self, HistoryError> {
        if bytes == 0 || bytes > MAX_LINE_BYTES || records == 0 || records > MAX_BATCH_RECORDS {
            return Err(CaptureError::Invalid("/history/limits").into());
        }
        Ok(Self { bytes, records })
    }
}
impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            bytes: DEFAULT_BATCH_BYTES,
            records: MAX_BATCH_RECORDS,
        }
    }
}
/// Budget receipts must be idempotent by provider response/counter identity.
/// The sink applies every capture for this bounded page in one writer
/// transaction. A failure aborts the page before its analytics cursor is
/// acknowledged. Historical response/turn IDs remain in params; collection
/// time supplies `now`. Analytics and budget databases commit separately.
pub trait HistoryBudgetSink: Send + Sync {
    fn capture_page<'a>(
        &'a self,
        captures: Vec<(Arc<Value>, Value, f64)>,
        now: f64,
    ) -> Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>>;
}

/// The production page sink. Identity and budget state are re-read through
/// `budget_service::capture` while holding one admitted canvas writer.
pub struct StoreHistoryBudgetSink<'a> {
    store: &'a RuntimeStore,
}

impl<'a> StoreHistoryBudgetSink<'a> {
    pub fn new(store: &'a RuntimeStore) -> Self {
        Self { store }
    }
}

impl HistoryBudgetSink for StoreHistoryBudgetSink<'_> {
    fn capture_page<'a>(
        &'a self,
        captures: Vec<(Arc<Value>, Value, f64)>,
        now: f64,
    ) -> Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>> {
        Box::pin(async move {
            if captures.is_empty() {
                return Ok(());
            }
            self.store
                .transaction_with::<_, HistoryError>(move |tx| {
                    let mut page_agent = captures
                        .first()
                        .map(|(agent, _, _)| (**agent).clone())
                        .ok_or(HistoryError::BudgetUnavailable)?;
                    for (_, params, at) in captures {
                        crate::budget_service::capture(
                            tx,
                            &mut page_agent,
                            &params,
                            at,
                            crate::budget::CaptureSource::History,
                            now,
                        )?;
                    }
                    Ok(())
                })
                .await?;
            Ok(())
        })
    }
}

pub struct BudgetUnavailable;
impl HistoryBudgetSink for BudgetUnavailable {
    fn capture_page<'a>(
        &'a self,
        _: Vec<(Arc<Value>, Value, f64)>,
        _: f64,
    ) -> Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>> {
        Box::pin(async { Err(HistoryError::BudgetUnavailable) })
    }
}
pub struct HistorySnapshot {
    agent: Arc<Value>,
    previous: Option<Value>,
    state: Value,
    context: RolloutContext,
}
impl HistorySnapshot {
    pub fn from_records(agent: Value, previous: Option<Value>) -> Result<Self, HistoryError> {
        CaptureTable::Agents.validate(&agent)?;
        let agent = Arc::new(agent);
        let thread = agent["threadId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(CaptureError::Invalid("/agent/threadId"))?;
        let account = agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let key = format!("{}:{account}:{thread}", text(&agent["id"]));
        let state=previous.clone().unwrap_or_else(||json!({"id":key,"agent":agent["id"],"accountKey":account,"threadId":thread,"deletedAt":agent["deletedAt"],"offset":0,"importedRecords":0,"malformedLines":0,"context":{"threadId":thread}}));
        CaptureTable::History.validate(&state)?;
        if state["id"] != key
            || state["agent"] != agent["id"]
            || state["threadId"] != thread
            || state["accountKey"] != account
            || state["context"]["threadId"] != thread
        {
            return Err(CaptureError::Invalid("/history/identity").into());
        }
        let context = RolloutContext::from_record(state["context"].clone())?;
        Ok(Self {
            agent,
            previous,
            state,
            context,
        })
    }
}
pub struct HistoryBatch {
    agent: Arc<Value>,
    previous: Option<Value>,
    state: Value,
    captures: Vec<PreparedCapture>,
    budget: Vec<(Arc<Value>, Value, f64)>,
    advanced: bool,
    read_proof: Option<ReadProof>,
}
impl HistoryBatch {
    pub fn checkpoint(&self) -> &Value {
        &self.state
    }
    pub fn advanced(&self) -> bool {
        self.advanced
    }
    pub async fn commit(
        mut self,
        store: &RuntimeStore,
        budget: &dyn HistoryBudgetSink,
        now: f64,
    ) -> Result<Vec<ResponseRateAssociation>, HistoryError> {
        // Re-read only before entering either writer or invoking budget effects.
        if let Some(mut proof) = self.read_proof.take() {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/analytics_history_proof",
                move || proof.validate(),
            )
            .await
            .map_err(|_| HistoryError::Read)??;
        }
        if !self.budget.is_empty() {
            budget
                .capture_page(std::mem::take(&mut self.budget), now)
                .await?;
        }
        store
            .analytics_transaction(move |tx, canvas| {
                check_identity(canvas, &self.agent)?;
                let current = Repo::read(tx, CaptureTable::History, text(&self.state["id"]))?;
                if current != self.previous {
                    return Err(HistoryError::StaleCheckpoint);
                }
                let mut effects = Vec::new();
                for prepared in self.captures {
                    let decision = decide(CaptureSnapshot::load(tx, Arc::new(prepared))?)?;
                    decision.apply(tx)?;
                    if let Some(effect) = decision.response_rate {
                        effects.push(effect)
                    }
                }
                let mut state = self.state;
                if without_updated(current.as_ref()) != without_updated(Some(&state)) {
                    state["updated"] = json!(now);
                    Repo::write(tx, CaptureTable::History, &state)?;
                }
                Ok(effects)
            })
            .await
    }
}
fn without_updated(v: Option<&Value>) -> Option<Value> {
    v.map(|v| {
        let mut v = v.clone();
        if let Some(m) = v.as_object_mut() {
            m.remove("updated");
        }
        v
    })
}
/// Actual collector entry point. This does not start a runtime or a provider.
pub async fn collect_one(
    store: &RuntimeStore,
    agent: Value,
    profile: PathBuf,
    limits: BatchLimits,
    now: f64,
    cancel: Arc<AtomicBool>,
    budget: &dyn HistoryBudgetSink,
) -> Result<bool, HistoryError> {
    let path_profile = profile.clone();
    let thread = text(&agent["threadId"]).to_owned();
    let is_live_only = agent["provider"] == "claude";
    let path_cancel = Arc::clone(&cancel);
    let path = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/analytics_history_discovery",
        move || {
            if is_live_only {
                Ok(RolloutPath::Missing)
            } else {
                discover(&path_profile, &thread, &path_cancel)
            }
        },
    )
    .await
    .map_err(|_| HistoryError::Read)??;
    collect_path(store, agent, profile, (path, limits), now, cancel, budget).await
}
async fn collect_path(
    store: &RuntimeStore,
    agent: Value,
    profile: PathBuf,
    selection: (RolloutPath, BatchLimits),
    now: f64,
    cancel: Arc<AtomicBool>,
    budget: &dyn HistoryBudgetSink,
) -> Result<bool, HistoryError> {
    let key = format!(
        "{}:{}:{}",
        text(&agent["id"]),
        agent["accountKey"].as_str().unwrap_or("default"),
        text(&agent["threadId"])
    );
    crate::analytics_history_repair::repair_terminal_errors(store).await?;
    let budget_advanced =
        crate::analytics_history_repair::migrate_budget(store, agent.clone(), budget, now).await?;
    let previous = read_history_checkpoint(store, &key, &agent).await?;
    let prepare_cancel = Arc::clone(&cancel);
    let batch = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/analytics_history_prepare",
        move || {
            let snapshot = HistorySnapshot::from_records(agent, previous)?;
            prepare(
                snapshot,
                &profile,
                selection.0,
                selection.1,
                now,
                &prepare_cancel,
            )
        },
    )
    .await
    .map_err(|_| HistoryError::Read)??;
    let advanced = batch.advanced();
    batch.commit(store, budget, now).await?;
    Ok(advanced || budget_advanced)
}

/// Read one checkpoint without allowing one malformed stored row to stop the
/// account-wide history importer. Invalid rows are replaced with a safe initial
/// checkpoint that retains a value-free diagnostic for repair and display.
/// Python reads the keyed row with `json.loads` in
/// `apps/server/src/codex_analytics_history.py:627-638`; it does not validate
/// the persisted checkpoint against the AnalyticsHistory schema at read time.
async fn read_history_checkpoint(
    store: &RuntimeStore,
    key: &str,
    agent: &Value,
) -> Result<Option<Value>, HistoryError> {
    let root = store.root().to_owned();
    let key_owned = key.to_owned();
    let agent = agent.clone();
    let (previous, repair) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/analytics_history_checkpoint",
        move || read_history_checkpoint_stage(&root, &key_owned, &agent),
    )
    .await
    .map_err(|_| HistoryError::Read)??;
    if let Some((key, previous_raw, replacement)) = repair {
        quarantine_history_checkpoint(store, key, previous_raw, replacement.clone()).await?;
        return Ok(Some(replacement));
    }
    Ok(previous)
}

type HistoryCheckpointRepair = (String, String, Value);
type HistoryCheckpointRead = (Option<Value>, Option<HistoryCheckpointRepair>);

fn read_history_checkpoint_stage(
    root: &Path,
    key: &str,
    agent: &Value,
) -> Result<HistoryCheckpointRead, HistoryError> {
    let read = AnalyticsRepository::new(root)
        .open_read_only()
        .map_err(|_| HistoryError::Read)?;
    let raw = read
        .query_row(
            "SELECT record FROM analytics_history WHERE id=?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(studio_runtime_store::StoreError::from)?;
    drop(read);
    let Some(raw) = raw else {
        return Ok((None, None));
    };
    if let Ok(value) = serde_json::from_str::<Value>(&raw) {
        let valid = CaptureTable::History.validate(&value).is_ok()
            && value.get("id").and_then(Value::as_str) == Some(key);
        if valid {
            return Ok((Some(value), None));
        }
        let (diagnostic, invalid_pointers) = if value.get("id").and_then(Value::as_str) != Some(key)
        {
            (
                analytics_field_diagnostic(key, "/id", "row_id_mismatch", &value),
                vec!["/id".to_owned()],
            )
        } else {
            let errors = studio_runtime_schema::validation_errors("AnalyticsHistory", &value)
                .map_err(studio_runtime_store::StoreError::SchemaValidation)?;
            let details = errors
                .iter()
                .map(|(pointer, reason)| analytics_field_diagnostic(key, pointer, reason, &value))
                .collect::<Vec<_>>();
            let pointers = errors
                .into_iter()
                .map(|(pointer, _)| pointer)
                .collect::<Vec<_>>();
            let diagnostic = if details.is_empty() {
                format!("analytics_history/{key}: schema_invalid")
            } else {
                details.join("; ")
            };
            (diagnostic, pointers)
        };
        let replacement = if let Some(mut retained) = value.as_object().cloned().map(Value::Object)
        {
            for pointer in &invalid_pointers {
                remove_json_pointer(&mut retained, pointer);
            }
            retained["status"] = json!("invalidRecord");
            retained["error"] = json!(diagnostic);
            retained["invalidRecordDiagnostic"] = json!(diagnostic);
            if retained.get("id").and_then(Value::as_str) == Some(key)
                && CaptureTable::History.validate(&retained).is_ok()
            {
                retained
            } else {
                initial_history_checkpoint(key, agent, &diagnostic)
            }
        } else {
            initial_history_checkpoint(key, agent, &diagnostic)
        };
        return Ok((None, Some((key.to_owned(), raw, replacement))));
    }
    let diagnostic = format!(
        "analytics_history/{key} at : invalid_json; record length {} bytes",
        raw.len()
    );
    let replacement = initial_history_checkpoint(key, agent, &diagnostic);
    Ok((None, Some((key.to_owned(), raw, replacement))))
}

fn remove_json_pointer(value: &mut Value, pointer: &str) {
    if pointer.is_empty() || pointer == "/" {
        return;
    }
    let parts = pointer
        .strip_prefix('/')
        .unwrap_or(pointer)
        .split('/')
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut parent = value;
    for part in parents {
        let Some(next) = (if let Ok(index) = part.parse::<usize>() {
            parent.get_mut(index)
        } else {
            parent.get_mut(part.as_str())
        }) else {
            return;
        };
        parent = next;
    }
    if let Some(object) = parent.as_object_mut() {
        object.remove(last);
    } else if let (Ok(index), Some(array)) = (last.parse::<usize>(), parent.as_array_mut())
        && index < array.len()
    {
        array.remove(index);
    }
}

fn analytics_value_shape(value: Option<&Value>) -> (&'static str, String) {
    match value {
        Some(Value::String(value)) => ("string", format!("{} bytes", value.len())),
        Some(Value::Array(value)) => ("array", format!("{} items", value.len())),
        Some(Value::Object(value)) => ("object", format!("{} properties", value.len())),
        Some(Value::Number(_)) => ("number", "not applicable".to_owned()),
        Some(Value::Bool(_)) => ("boolean", "not applicable".to_owned()),
        Some(Value::Null) => ("null", "not applicable".to_owned()),
        None => ("missing", "not applicable".to_owned()),
    }
}

fn analytics_field_diagnostic(key: &str, pointer: &str, reason: &str, value: &Value) -> String {
    let (kind, length) = analytics_value_shape(value.pointer(pointer));
    format!("analytics_history/{key} at {pointer}: {reason}; value type {kind}, length {length}")
}

fn initial_history_checkpoint(key: &str, agent: &Value, diagnostic: &str) -> Value {
    let account = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let thread = text(&agent["threadId"]);
    json!({
        "id": key,
        "agent": agent["id"],
        "accountKey": account,
        "threadId": thread,
        "deletedAt": agent.get("deletedAt"),
        "offset": 0,
        "importedRecords": 0,
        "malformedLines": 0,
        "context": {"threadId": thread},
        "status": "invalidRecord",
        "error": diagnostic,
        "invalidRecordDiagnostic": diagnostic
    })
}

async fn quarantine_history_checkpoint(
    store: &RuntimeStore,
    key: String,
    previous_raw: String,
    replacement: Value,
) -> Result<(), HistoryError> {
    store
        .analytics_transaction(move |tx, _| {
            let current: Option<String> = tx
                .query_row(
                    "SELECT record FROM analytics_history WHERE id=?1",
                    [key.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(studio_runtime_store::StoreError::from)?;
            if current.as_deref() != Some(previous_raw.as_str()) {
                return Err(HistoryError::StaleCheckpoint);
            }
            tx.execute(
                "UPDATE analytics_history SET agent=?2,record=?3 WHERE id=?1",
                rusqlite::params![
                    key.as_str(),
                    replacement["agent"].as_str(),
                    replacement.to_string()
                ],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            Ok(())
        })
        .await?;
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RolloutPath {
    Found(PathBuf),
    Missing,
    Ambiguous,
    OutsideProfile,
    Unreadable,
}
/// Discovery is bounded and skips symlink directories. The opened file is
/// independently checked against its canonical profile and filesystem identity.
pub fn discover(
    profile: &Path,
    thread: &str,
    cancel: &AtomicBool,
) -> Result<RolloutPath, HistoryError> {
    DiscoveryRound::default().resolve(profile, thread, cancel)
}

pub fn prepare(
    mut snapshot: HistorySnapshot,
    profile: &Path,
    path: RolloutPath,
    limits: BatchLimits,
    now: f64,
    cancel: &AtomicBool,
) -> Result<HistoryBatch, HistoryError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(HistoryError::Cancelled);
    }
    snapshot.state["deletedAt"] = snapshot.agent["deletedAt"].clone();
    if snapshot.agent["provider"] == "claude" {
        return Ok(refused(snapshot, "liveOnly", None));
    }
    let path = match path {
        RolloutPath::Found(p) => p,
        RolloutPath::Missing => {
            return Ok(refused(snapshot, "missing", Some("Native rollout missing")));
        }
        RolloutPath::Ambiguous => {
            return Ok(refused(
                snapshot,
                "ambiguous",
                Some("Native rollout ambiguous"),
            ));
        }
        RolloutPath::OutsideProfile => {
            return Ok(refused(
                snapshot,
                "outsideProfile",
                Some("Native rollout outsideProfile"),
            ));
        }
        RolloutPath::Unreadable => {
            return Ok(refused(
                snapshot,
                "unreadable",
                Some("Native rollout unreadable"),
            ));
        }
    };
    let resolved = fs::canonicalize(&path).map_err(|_| HistoryError::Read)?;
    let root = fs::canonicalize(profile).map_err(|_| HistoryError::Read)?;
    if !resolved.starts_with(root) {
        return Ok(refused(
            snapshot,
            "outsideProfile",
            Some("Native rollout outsideProfile"),
        ));
    }
    let before = fs::metadata(&resolved).map_err(|_| HistoryError::Read)?;
    let file = File::open(&resolved).map_err(|_| HistoryError::Read)?;
    let info = file.metadata().map_err(|_| HistoryError::Read)?;
    if filesystem_identity(&before) != filesystem_identity(&info)
        || fs::canonicalize(&path).map_err(|_| HistoryError::Read)? != resolved
    {
        return Ok(refused(
            snapshot,
            "identityChanged",
            Some("Native rollout changed while opening"),
        ));
    }
    if !info.is_file() {
        return Ok(refused(
            snapshot,
            "unreadable",
            Some("Native rollout unreadable"),
        ));
    }
    let identity = filesystem_identity(&info);
    let offset = snapshot.state["offset"]
        .as_u64()
        .ok_or(CaptureError::Invalid("/history/offset"))?;
    let old = snapshot
        .state
        .get("filesystemIdentity")
        .or_else(|| snapshot.state.get("identity"))
        .filter(|v| !v.is_null())
        .cloned();
    let remap = old.as_ref().is_some_and(|v| {
        *v != identity
            && v[1] == identity[1]
            && snapshot.state["path"] == path.to_string_lossy().as_ref()
            && snapshot.state["anchor"].is_string()
            && offset > 0
            && info.len() >= offset
    });
    if old.as_ref().is_some_and(|v| *v != identity && !remap) || info.len() < offset {
        return Ok(refused(
            snapshot,
            "identityChanged",
            Some("Native rollout was replaced or truncated; previous checkpoint retained"),
        ));
    }
    snapshot.state["path"] = json!(path);
    snapshot.state["fileBytes"] = json!(info.len());
    if snapshot.state.get("identity").is_none() {
        snapshot.state["identity"] = identity.clone();
    }
    let mut reader = BufReader::new(file);
    if remap || snapshot.state["validated"] != true {
        let header = bounded_line(&mut reader)?;
        let valid = serde_json::from_slice::<Value>(&header)
            .ok()
            .is_some_and(|v| {
                v["type"] == "session_meta"
                    && crate::analytics_capture::first(&[
                        &v["payload"]["id"],
                        &v["payload"]["session_id"],
                    ]) == snapshot.agent["threadId"]
            });
        if !valid {
            return Ok(refused(
                snapshot,
                if remap {
                    "identityChanged"
                } else {
                    "wrongThread"
                },
                Some("Native rollout header does not match the managed thread"),
            ));
        }
        snapshot.state["validated"] = json!(true);
    }
    if let Some(anchor) = snapshot.state["anchor"].as_str()
        && anchor_at(&mut reader, offset)? != anchor
    {
        return Ok(refused(
            snapshot,
            "identityChanged",
            Some("Native rollout checkpoint bytes changed"),
        ));
    }
    if remap {
        snapshot.state["filesystemRemap"] = json!({"previous":old,"current":identity,"offset":offset,"at":now,"proof":"samePathInodeHeaderAnchor"});
        increment(&mut snapshot.state, "filesystemRemapCount", 1)?;
    }
    snapshot.state["filesystemIdentity"] = identity;
    let range_start = offset.saturating_sub(ANCHOR_BYTES);
    reader
        .seek(SeekFrom::Start(range_start))
        .map_err(|_| HistoryError::Read)?;
    let mut prefix = vec![0; (offset - range_start) as usize];
    reader
        .read_exact(&mut prefix)
        .map_err(|_| HistoryError::Read)?;
    if let Some(anchor) = snapshot.state["anchor"].as_str()
        && sha256_hex(&prefix) != anchor
    {
        return Err(HistoryError::ChangedDuringRead);
    }
    let mut digest = Sha256::new();
    digest.update(&prefix);
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|_| HistoryError::Read)?;
    let mut consumed = 0u64;
    let mut count = 0u64;
    let mut partial = false;
    let mut oversized = false;
    let mut captures = Vec::new();
    let mut budget = Vec::new();
    // Proven inherited source threads are passed through the checked repair chain.
    snapshot
        .context
        .allow_sources(inherited_threads(&snapshot.agent));
    let fallback = info
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(now);
    while count < limits.records as u64 && consumed < limits.bytes as u64 {
        if cancel.load(Ordering::Relaxed) {
            return Err(HistoryError::Cancelled);
        }
        let line = bounded_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            oversized = true;
            break;
        }
        if !line.ends_with(b"\n") {
            partial = true;
            break;
        }
        let start = offset + consumed;
        digest.update(&line);
        consumed += line.len() as u64;
        count += 1;
        let record = match serde_json::from_slice::<Value>(&line) {
            Ok(v) if v.is_object() => v,
            _ => {
                increment(&mut snapshot.state, "malformedLines", 1)?;
                continue;
            }
        };
        let identity = &snapshot.state["identity"];
        let identity_key = sha256_hex(
            format!(
                "{}:[{}, {}]:{start}",
                text(&snapshot.state["id"]),
                identity[0],
                identity[1]
            )
            .as_bytes(),
        );
        let actions = snapshot.context.actions(&record, &identity_key, fallback)?;
        let mut agent = (*snapshot.agent).clone();
        for key in ["turnId", "model", "effort"] {
            agent[key] = snapshot.context.record()[key].clone();
        }
        let agent = Arc::new(agent);
        if captures.len().saturating_add(actions.len()) > 4096 {
            return Err(CaptureError::Invalid("/history/batch/actions").into());
        }
        for action in actions {
            match action {
                RolloutAction::Event { method, params, at } => {
                    if method == "thread/tokenUsage/updated" {
                        budget.push((Arc::clone(&agent), params.clone(), at));
                    }
                    captures.push(PreparedCapture::event_shared(
                        Arc::clone(&agent),
                        &method,
                        &params,
                        at,
                        now,
                        CaptureSource::Rollout,
                    )?);
                }
                RolloutAction::Payload { params, at } => captures.push(
                    PreparedCapture::model_payload_shared(Arc::clone(&agent), &params, at, now)?,
                ),
                RolloutAction::WrongThread => {
                    increment(&mut snapshot.state, "wrongThreadRecord", 1)?
                }
            }
        }
    }
    snapshot.state["anchor"] = json!(anchor_at(&mut reader, offset + consumed)?);
    snapshot.state["offset"] = json!(offset + consumed);
    increment(&mut snapshot.state, "importedRecords", count)?;
    snapshot.state["context"] = snapshot.context.record().clone();
    snapshot.state["status"] = json!(if oversized {
        "oversizedLine"
    } else if partial {
        "partialLine"
    } else if offset + consumed < info.len() {
        "catchingUp"
    } else {
        "current"
    });
    snapshot.state["error"] = if oversized {
        json!("Native rollout line exceeds 64 MiB; checkpoint retained")
    } else {
        Value::Null
    };
    snapshot.state["coverage"] = json!(
        if snapshot.state["malformedLines"].as_u64().unwrap_or(0) > 0
            || snapshot.state["wrongThreadRecord"].as_u64().unwrap_or(0) > 0
        {
            "partial"
        } else {
            "availableRecords"
        }
    );
    Ok(HistoryBatch {
        agent: snapshot.agent,
        previous: snapshot.previous,
        state: snapshot.state,
        captures,
        budget,
        advanced: consumed > 0,
        read_proof: Some(ReadProof::new(
            reader.into_inner(),
            path,
            range_start,
            offset + consumed - range_start,
            digest.finalize().into(),
        )),
    })
}
fn increment(state: &mut Value, key: &str, count: u64) -> Result<(), HistoryError> {
    let current = state
        .get(key)
        .map(|v| v.as_u64().ok_or(CaptureError::Invalid("/history/counter")))
        .transpose()?
        .unwrap_or(0);
    state[key] = json!(
        current
            .checked_add(count)
            .filter(|n| *n <= MAX_SAFE_INTEGER as u64)
            .ok_or(CaptureError::Invalid("/history/counter"))?
    );
    Ok(())
}
fn refused(mut snapshot: HistorySnapshot, status: &str, error: Option<&str>) -> HistoryBatch {
    snapshot.state["status"] = json!(status);
    snapshot.state["error"] = json!(error);
    HistoryBatch {
        agent: snapshot.agent,
        previous: snapshot.previous,
        state: snapshot.state,
        captures: vec![],
        budget: vec![],
        advanced: false,
        read_proof: None,
    }
}
fn bounded_line(reader: &mut BufReader<File>) -> Result<Vec<u8>, HistoryError> {
    let mut line = Vec::new();
    reader
        .take((MAX_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| HistoryError::Read)?;
    Ok(line)
}
fn anchor_at(reader: &mut BufReader<File>, offset: u64) -> Result<String, HistoryError> {
    reader
        .seek(SeekFrom::Start(offset.saturating_sub(ANCHOR_BYTES)))
        .map_err(|_| HistoryError::Read)?;
    let mut bytes = vec![0; offset.min(ANCHOR_BYTES) as usize];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| HistoryError::Read)?;
    Ok(sha256_hex(&bytes))
}
#[cfg(unix)]
pub(super) fn filesystem_identity(info: &fs::Metadata) -> Value {
    use std::os::unix::fs::MetadataExt;
    json!([info.dev(), info.ino()])
}
#[cfg(not(unix))]
pub(super) fn filesystem_identity(_: &fs::Metadata) -> Value {
    Value::Null
}
fn inherited_threads(agent: &Value) -> Vec<String> {
    let mut receipts = agent["contextRepairHistory"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    receipts.insert(0, agent["contextRepair"].clone());
    let mut thread = agent["threadId"].clone();
    let mut seen = std::collections::BTreeSet::new();
    let mut allowed = std::collections::BTreeSet::new();
    while let Some(id) = thread.as_str().filter(|s| !s.is_empty()) {
        if !seen.insert(id.to_owned()) {
            break;
        }
        let Some(receipt) = receipts.iter().find(|r| {
            r["phase"] == "completed"
                && r["newThreadId"] == thread
                && r["agent"] == agent["id"]
                && r["source"]["id"] == agent["id"]
                && r["source"]["accountKey"]
                    == agent.get("accountKey").cloned().unwrap_or(json!("default"))
        }) else {
            break;
        };
        for v in [
            &receipt["source"]["threadId"],
            &receipt["snapshot"]["importThreadId"],
        ] {
            if let Some(id) = v.as_str().filter(|s| !s.is_empty()) {
                allowed.insert(id.into());
            }
        }
        if let Some(ids) = ancestry_threads(receipt) {
            allowed.extend(ids);
        }
        thread = receipt["source"]["threadId"].clone();
    }
    allowed.into_iter().collect()
}

fn ancestry_threads(receipt: &Value) -> Option<Vec<String>> {
    let snapshot = &receipt["snapshot"];
    let ancestry = snapshot["ancestry"].as_array()?;
    if !(2..=32).contains(&ancestry.len()) {
        return None;
    }
    let last = ancestry.last()?;
    if snapshot["sourcePath"] != last["path"]
        || snapshot["sourceSha256"] != last["sha256"]
        || snapshot["sourceBytes"] != last["endByteOffset"]
        || last["threadId"] != receipt["source"]["threadId"]
        || !nonempty(&snapshot["copyPath"])
        || !nonempty(&snapshot["terminalTurnId"])
        || !digest(&snapshot["copySha256"])
    {
        return None;
    }
    let mut ids = std::collections::BTreeSet::new();
    for (index, segment) in ancestry.iter().enumerate() {
        let id = segment["threadId"].as_str()?;
        if uuid::Uuid::parse_str(id).ok()?.to_string() != id
            || !ids.insert(id.to_owned())
            || !nonempty(&segment["path"])
            || !segment["endByteOffset"].as_u64().is_some_and(|n| n > 0)
            || !digest(&segment["sha256"])
        {
            return None;
        }
        if index == ancestry.len() - 1 {
            if !segment["endOrdinalExclusive"].is_null() {
                return None;
            }
        } else if !segment["endOrdinalExclusive"]
            .as_u64()
            .is_some_and(|n| n > 0)
        {
            return None;
        }
    }
    Some(ids.into_iter().collect())
}
fn nonempty(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.is_empty())
}
fn digest(v: &Value) -> bool {
    v.as_str().is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Clone, Copy)]
pub struct HistoryClock {
    pub observed_at: f64,
    pub monotonic: f64,
}

/// Timer/notification-loop consumer. Each step commits at most one rollout
/// batch and returns; its caller schedules the next step after yielding to
/// other work. The scheduler owns no OS thread.
#[derive(Default)]
pub struct HistoryCollector {
    pub schedule: crate::analytics_history_schedule::HistorySchedule,
    discovery: Arc<std::sync::Mutex<DiscoveryRound>>,
    visited: std::collections::BTreeSet<String>,
}
impl HistoryCollector {
    /// Start a fresh fairness round while retaining watcher-maintained profile
    /// indexes. An idle selection or revisiting an actor closes the prior round.
    pub fn begin_round(&mut self) {
        self.discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin_round();
        self.visited.clear();
    }

    /// Apply one watcher event to the persistent profile discovery cache.
    pub fn file_changed(
        &mut self,
        profile: &Path,
        path: &Path,
        removed: bool,
        structural: bool,
        account: &str,
    ) {
        self.discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .file_changed(profile, path, removed, structural);
        if structural {
            self.schedule.discovery_changed(Some(account));
        }
    }

    /// Force a one-time profile rescan after directory changes or watcher overflow.
    pub fn profile_changed(&mut self, profile: &Path) {
        self.discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate(profile);
    }
    pub async fn step(
        &mut self,
        store: &RuntimeStore,
        actors: &[Value],
        profile: impl FnOnce(&str) -> Result<PathBuf, HistoryError> + Send + 'static,
        clock: HistoryClock,
        cancel: Arc<AtomicBool>,
        budget: &dyn HistoryBudgetSink,
    ) -> Result<bool, HistoryError> {
        let Some(agent) = self.schedule.next(actors, clock.monotonic)?.cloned() else {
            self.begin_round();
            return Ok(false);
        };
        let result = self
            .step_selected(
                store,
                agent.clone(),
                actors.len(),
                profile,
                clock,
                cancel,
                budget,
            )
            .await;
        if matches!(
            &result,
            Err(error) if matches!(
                error.retry_policy(),
                HistoryRetryPolicy::WaitForProfileInput | HistoryRetryPolicy::WaitForBudgetChange
            )
        ) {
            match &result {
                Err(error) if error.retry_policy() == HistoryRetryPolicy::WaitForBudgetChange => {
                    self.schedule
                        .block_for_budget_change(&agent, clock.monotonic)?;
                }
                _ => self
                    .schedule
                    .block_for_input_change(&agent, clock.monotonic)?,
            }
        }
        result
    }

    async fn step_selected(
        &mut self,
        store: &RuntimeStore,
        agent: Value,
        actor_count: usize,
        profile: impl FnOnce(&str) -> Result<PathBuf, HistoryError> + Send + 'static,
        clock: HistoryClock,
        cancel: Arc<AtomicBool>,
        budget: &dyn HistoryBudgetSink,
    ) -> Result<bool, HistoryError> {
        if self.visited.len() >= actor_count || self.visited.contains(text(&agent["id"])) {
            self.begin_round();
        }
        self.visited.insert(text(&agent["id"]).to_owned());
        let discovery = Arc::clone(&self.discovery);
        let cancel_discovery = Arc::clone(&cancel);
        let account_key = agent["accountKey"].as_str().unwrap_or("default").to_owned();
        let thread = text(&agent["threadId"]).to_owned();
        let live_only = agent["provider"] == "claude";
        let (home, path) = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/analytics_history_discovery",
            move || {
                let home = profile(&account_key)?;
                let path = if live_only {
                    RolloutPath::Missing
                } else {
                    discovery
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .resolve(&home, &thread, &cancel_discovery)?
                };
                Ok::<_, HistoryError>((home, path))
            },
        )
        .await
        .map_err(|_| HistoryError::Read)??;
        crate::analytics_history_repair::repair_terminal_errors(store).await?;
        let budget_advanced = crate::analytics_history_repair::migrate_budget(
            store,
            agent.clone(),
            budget,
            clock.observed_at,
        )
        .await?;
        let key = format!(
            "{}:{}:{}",
            text(&agent["id"]),
            agent["accountKey"].as_str().unwrap_or("default"),
            text(&agent["threadId"])
        );
        let prepare_agent = agent.clone();
        let prepare_cancel = Arc::clone(&cancel);
        let previous = read_history_checkpoint(store, &key, &agent).await?;
        let batch = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/analytics_history_prepare",
            move || {
                let snapshot = HistorySnapshot::from_records(prepare_agent, previous)?;
                prepare(
                    snapshot,
                    &home,
                    path,
                    BatchLimits::default(),
                    clock.observed_at,
                    &prepare_cancel,
                )
            },
        )
        .await
        .map_err(|_| HistoryError::Read)??;
        let advanced = batch.advanced();
        batch.commit(store, budget, clock.observed_at).await?;
        let root = store.root().to_owned();
        let checkpoint_key = key;
        let checkpoint = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/analytics_history_checkpoint",
            move || {
                let db = AnalyticsRepository::new(&root)
                    .open_read_only()
                    .map_err(|_| HistoryError::Read)?;
                Repo::read(&db, CaptureTable::History, &checkpoint_key).map_err(HistoryError::from)
            },
        )
        .await
        .map_err(|_| HistoryError::Read)??;
        if let Some(checkpoint) = checkpoint {
            self.schedule
                .completed(&agent, &checkpoint, clock.monotonic)?;
        }
        Ok(advanced || budget_advanced)
    }
}

#[cfg(test)]
mod invalid_record_tests {
    use super::*;

    #[tokio::test]
    async fn invalid_history_checkpoint_is_quarantined_without_blocking_valid_row() {
        let root = tempfile::tempdir().expect("temporary runtime");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("runtime store");
        let db = rusqlite::Connection::open(root.path().join("analytics.sqlite3"))
            .expect("analytics database");
        let invalid_key = "bad:default:bad-thread";
        let valid_key = "good:default:good-thread";
        let invalid = json!({
            "id": invalid_key,
            "agent": "bad",
            "accountKey": "default",
            "threadId": "bad-thread",
            "path": "/profile/rollout.jsonl",
            "filesystemIdentity": [12, 34],
            "anchor": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "validated": true,
            "offset": "private offset",
            "importedRecords": 32,
            "malformedLines": 0,
            "context": {"threadId": "bad-thread", "window": "private value"}
        });
        let valid = json!({
            "id": valid_key,
            "agent": "good",
            "accountKey": "default",
            "threadId": "good-thread",
            "offset": 0,
            "importedRecords": 0,
            "malformedLines": 0,
            "context": {"threadId": "good-thread", "window": 128000}
        });
        for (key, agent_id, record) in [
            (invalid_key, "bad", invalid),
            (valid_key, "good", valid.clone()),
        ] {
            db.execute(
                "INSERT OR REPLACE INTO analytics_history(id,agent,record) VALUES (?1,?2,?3)",
                rusqlite::params![key, agent_id, record.to_string()],
            )
            .expect("seed history row");
        }
        drop(db);
        let bad_agent = json!({"id":"bad","accountKey":"default","threadId":"bad-thread"});
        let recovered = read_history_checkpoint(&store, invalid_key, &bad_agent)
            .await
            .expect("quarantine invalid history row")
            .expect("replacement checkpoint");
        let diagnostic = recovered["invalidRecordDiagnostic"]
            .as_str()
            .expect("recorded diagnostic");
        assert!(diagnostic.contains("analytics_history/bad:default:bad-thread"));
        assert!(diagnostic.contains("/context/window"));
        assert!(diagnostic.contains("/offset"));
        assert!(diagnostic.contains("anyOf"));
        assert!(diagnostic.contains("value type string"));
        assert!(!diagnostic.contains("private value"));
        assert!(!diagnostic.contains("private offset"));
        assert_eq!(recovered["offset"], 0);
        assert_eq!(recovered["importedRecords"], 0);
        assert!(recovered["context"].get("window").is_none());
        assert_eq!(recovered["status"], "invalidRecord");
        let good_agent = json!({"id":"good","accountKey":"default","threadId":"good-thread"});
        let still_valid = read_history_checkpoint(&store, valid_key, &good_agent)
            .await
            .expect("continue importing valid row")
            .expect("valid checkpoint");
        assert_eq!(still_valid, valid);
    }

    #[tokio::test]
    async fn non_object_history_checkpoints_reset_to_initial_values_without_panicking() {
        let root = tempfile::tempdir().expect("temporary runtime");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("runtime store");
        let db = rusqlite::Connection::open(root.path().join("analytics.sqlite3"))
            .expect("analytics database");
        let agent = json!({"id":"agent","accountKey":"default","threadId":"thread"});

        for (index, value) in [json!([]), json!(23), json!("x")].into_iter().enumerate() {
            let key = format!("agent:default:thread-{index}");
            db.execute(
                "INSERT INTO analytics_history(id,agent,record) VALUES (?1,?2,?3)",
                rusqlite::params![key, "agent", value.to_string()],
            )
            .expect("seed non-object checkpoint");
            let replacement = read_history_checkpoint(&store, &key, &agent)
                .await
                .expect("quarantine non-object checkpoint")
                .expect("initial checkpoint");
            assert_eq!(replacement["id"], key);
            assert_eq!(replacement["status"], "invalidRecord");
            assert_eq!(replacement["offset"], 0);
            assert_eq!(replacement["importedRecords"], 0);
            assert!(
                replacement["invalidRecordDiagnostic"]
                    .as_str()
                    .is_some_and(|diagnostic| diagnostic.contains("analytics_history/"))
            );
        }
    }
}
