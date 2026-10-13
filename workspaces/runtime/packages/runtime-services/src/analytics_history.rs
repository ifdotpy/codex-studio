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
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
mod read_proof;
use read_proof::ReadProof;
mod discovery;
pub use discovery::DiscoveryRound;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// A failure aborts the batch before its analytics cursor is acknowledged.
/// The sink verifies current agent/account/epoch ownership in its own writer
/// transaction, then calls `budget_service::capture` with History source and
/// CURRENT durable agent ownership, not the supplied historical agent copy.
/// Historical response/turn IDs remain in params; collection time supplies now.
/// A replay must reuse the durable charge identity. The integrator must test
/// the real sink across lost acknowledgements, failures and stale batches;
/// analytics and budget databases cannot share a SQLite commit.
pub trait HistoryBudgetSink {
    fn capture(&self, agent: &Value, params: &Value, at: f64) -> Result<(), HistoryError>;
}
pub struct BudgetUnavailable;
impl HistoryBudgetSink for BudgetUnavailable {
    fn capture(&self, _: &Value, _: &Value, _: f64) -> Result<(), HistoryError> {
        Err(HistoryError::BudgetUnavailable)
    }
}
pub struct HistorySnapshot {
    agent: Value,
    previous: Option<Value>,
    state: Value,
    context: RolloutContext,
}
impl HistorySnapshot {
    pub fn from_records(agent: Value, previous: Option<Value>) -> Result<Self, HistoryError> {
        CaptureTable::Agents.validate(&agent)?;
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
    agent: Value,
    previous: Option<Value>,
    state: Value,
    captures: Vec<PreparedCapture>,
    budget: Vec<(Value, Value, f64)>,
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
    pub fn commit(
        mut self,
        store: &RuntimeStore,
        budget: &dyn HistoryBudgetSink,
        now: f64,
    ) -> Result<Vec<ResponseRateAssociation>, HistoryError> {
        // Re-read only before entering either writer or invoking budget effects.
        if let Some(proof) = &mut self.read_proof {
            proof.validate()?;
        }
        for (agent, params, at) in &self.budget {
            budget.capture(agent, params, *at)?;
        }
        store.analytics_transaction(|tx, canvas| {
            check_identity(canvas, &self.agent)?;
            let current = Repo::read(tx, CaptureTable::History, text(&self.state["id"]))?;
            if current != self.previous {
                return Err(HistoryError::StaleCheckpoint);
            }
            let mut effects = Vec::new();
            for prepared in self.captures {
                let decision = decide(CaptureSnapshot::load(tx, prepared)?)?;
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
pub fn collect_one(
    store: &RuntimeStore,
    agent: Value,
    profile: &Path,
    limits: BatchLimits,
    now: f64,
    cancel: &AtomicBool,
    budget: &dyn HistoryBudgetSink,
) -> Result<bool, HistoryError> {
    let path = if agent["provider"] == "claude" {
        RolloutPath::Missing
    } else {
        discover(profile, text(&agent["threadId"]), cancel)?
    };
    collect_path(store, agent, profile, (path, limits), now, cancel, budget)
}
fn collect_path(
    store: &RuntimeStore,
    agent: Value,
    profile: &Path,
    selection: (RolloutPath, BatchLimits),
    now: f64,
    cancel: &AtomicBool,
    budget: &dyn HistoryBudgetSink,
) -> Result<bool, HistoryError> {
    let key = format!(
        "{}:{}:{}",
        text(&agent["id"]),
        agent["accountKey"].as_str().unwrap_or("default"),
        text(&agent["threadId"])
    );
    crate::analytics_history_repair::repair_terminal_errors(store)?;
    let budget_advanced = crate::analytics_history_repair::migrate_budget(store, &agent, budget)?;
    let read = AnalyticsRepository::new(store.root())
        .open_read_only()
        .map_err(|_| HistoryError::Read)?;
    let previous = Repo::read(&read, CaptureTable::History, &key)?;
    drop(read);
    let snapshot = HistorySnapshot::from_records(agent, previous)?;
    let batch = prepare(snapshot, profile, selection.0, selection.1, now, cancel)?;
    let advanced = batch.advanced();
    batch.commit(store, budget, now)?;
    Ok(advanced || budget_advanced)
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
        let mut agent = snapshot.agent.clone();
        for key in ["turnId", "model", "effort"] {
            agent[key] = snapshot.context.record()[key].clone();
        }
        if captures.len().saturating_add(actions.len()) > 4096 {
            return Err(CaptureError::Invalid("/history/batch/actions").into());
        }
        for action in actions {
            match action {
                RolloutAction::Event { method, params, at } => {
                    if method == "thread/tokenUsage/updated" {
                        budget.push((agent.clone(), params.clone(), at));
                    }
                    captures.push(PreparedCapture::event(
                        &agent,
                        &method,
                        &params,
                        at,
                        now,
                        CaptureSource::Rollout,
                    )?);
                }
                RolloutAction::Payload { params, at } => {
                    captures.push(PreparedCapture::model_payload(&agent, &params, at, now)?)
                }
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
            .filter(|n| *n <= 9_007_199_254_740_991)
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

/// Timer/notification-loop consumer. The account worker supplies each current
/// roster and the selected account's profile; the scheduler owns no OS thread.
#[derive(Default)]
pub struct HistoryCollector {
    pub schedule: crate::analytics_history_schedule::HistorySchedule,
    discovery: DiscoveryRound,
    visited: std::collections::BTreeSet<String>,
}
impl HistoryCollector {
    /// Start each caller-owned timer/notification round with a fresh shared scan.
    /// An idle selection or revisiting an actor also closes the previous round.
    pub fn begin_round(&mut self) {
        self.discovery = DiscoveryRound::default();
        self.visited.clear();
    }
    pub fn step(
        &mut self,
        store: &RuntimeStore,
        actors: &[Value],
        profile: impl FnOnce(&str) -> Result<PathBuf, HistoryError>,
        clock: HistoryClock,
        cancel: &AtomicBool,
        budget: &dyn HistoryBudgetSink,
    ) -> Result<bool, HistoryError> {
        let Some(agent) = self.schedule.next(actors, clock.monotonic)?.cloned() else {
            self.begin_round();
            return Ok(false);
        };
        if self.visited.len() >= actors.len() || self.visited.contains(text(&agent["id"])) {
            self.begin_round();
        }
        self.visited.insert(text(&agent["id"]).to_owned());
        let home = profile(agent["accountKey"].as_str().unwrap_or("default"))?;
        let path = if agent["provider"] == "claude" {
            RolloutPath::Missing
        } else {
            self.discovery
                .resolve(&home, text(&agent["threadId"]), cancel)?
        };
        let advanced = collect_path(
            store,
            agent.clone(),
            &home,
            (path, BatchLimits::default()),
            clock.observed_at,
            cancel,
            budget,
        )?;
        let key = format!(
            "{}:{}:{}",
            text(&agent["id"]),
            agent["accountKey"].as_str().unwrap_or("default"),
            text(&agent["threadId"])
        );
        let db = AnalyticsRepository::new(store.root())
            .open_read_only()
            .map_err(|_| HistoryError::Read)?;
        if let Some(checkpoint) = Repo::read(&db, CaptureTable::History, &key)? {
            self.schedule
                .completed(&agent, &checkpoint, clock.monotonic, false, false)?;
        }
        Ok(advanced)
    }
}
