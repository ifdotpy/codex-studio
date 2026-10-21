//! Pure submission compiler. The account worker gathers and revalidates the snapshot,
//! materializes artifact intents, persists the result, and submits exactly once.
//! No preparation result is evidence of delivery; only `accept` promotes knowledge.
//! Studio additions share one hard UTF-8 byte budget. User text and delegated
//! task text are preserved in full, outside that budget.
//! Omitted additions remain available through a durable, agent-owned output reference.
//! The transcript and its input excerpts each have a hard 20,000-character ceiling.
//! JSON evidence admits at most 64 nested containers, finite floats and i64/u64
//! integers; unsupported numbers, negative integer zero and invalid structured JSON
//! return a located InvalidJsonEvidence. Caller/bypass text is not parsed; progress
//! revisions retain the explicitly registered negative-zero/u64-overflow exception.
use chrono::{DateTime, Datelike, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::provider_input::ProviderInputOrigin;
use studio_runtime_store::{python_float_json, sha256_hex};
use thiserror::Error;

const TRANSCRIPT_CHARS: usize = 20_000;
const RESULT_BYTES: usize = 20_000;
const EVENT_BYTES: usize = 22_000;
/// Maximum Studio-authored text in one provider input, including wrappers,
/// context, receipt clocks, and attachment descriptions. Unit: UTF-8 bytes.
pub const STUDIO_INPUT_BYTES: usize = 24_000;
/// Maximum share reserved for the post-compaction roster. Unit: UTF-8 bytes.
const TEAM_RESTORE_BYTES: usize = 8_000;
use studio_runtime_schema::shared_values::MAX_TURN_ATTACHMENTS;
const RESULT_EXCERPT: usize = 600;
/// File names retained in a bounded review excerpt; independent of the number
/// of attached files admitted to a provider turn.
const REVIEW_FILE_EXCERPT_COUNT: usize = 8;
const CHECKS_CHARS: usize = 800;
const FILES_BYTES: usize = 1_000;
const FILE_NAME_CHARS: usize = 160;
const SUMMARY_RESERVE: usize = 100;
const CLEANUP_COUNT: usize = 3;

type Versions = BTreeMap<String, String>;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Epoch(pub Option<String>, pub u64);

#[derive(Clone, Serialize, Deserialize)]
pub struct ContextManifest {
    pub epoch: Epoch,
    pub versions: Versions,
    pub sequence: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct DeveloperContext {
    pub epoch: Epoch,
    pub versions: Versions,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct DeliveredMode {
    pub epoch: Epoch,
    pub version: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub root_id: String,
    pub epoch: Epoch,
    pub is_lead: bool,
    pub forked_from: Option<String>,
    pub prepared_context: Option<DeveloperContext>,
    pub delivered_mode: Option<DeliveredMode>,
}
/// The adapter normalizes legacy mode-only records with the stored schema.
#[derive(Clone, Serialize, Deserialize)]
pub struct RootSettings {
    pub concurrency: u16,
    pub mode_revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct GuidanceBundle {
    /// Includes the frozen native role when one exists.
    pub role: String,
    pub progress: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    #[serde(rename = "rootId")]
    pub root_id: String,
    pub steps: Vec<PlanStep>,
    pub explanation: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplaintStatus {
    Open,
    InProgress,
    Resolved,
    Declined,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    Lead,
    User,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Complaint {
    pub id: String,
    pub lead_id: String,
    pub recipient: Recipient,
    pub status: ComplaintStatus,
    pub legacy_user_task: bool,
    pub response_authors: Vec<String>,
    pub author: String,
    pub author_name: Option<String>,
    pub text: String,
    pub version: Option<u64>,
    pub created: f64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Completed,
    Failed,
    Interrupted,
    Paused,
    Queued,
    /// Starting, running, waiting, or awaiting approval.
    Running,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub root_id: String,
    pub parent_id: Option<String>,
    pub worktree_ready: bool,
    pub deleted: bool,
    pub status: WorkerStatus,
    pub auto_wake: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    pub owner_id: String,
    pub owner_root_id: String,
    pub owner_deleted: bool,
    pub image: bool,
    pub path: String,
    pub name: String,
    pub size: u64,
    /// Additional public metadata; typed identity fields override conflicting entries.
    pub view: Map<String, Value>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct EventMetadata {
    pub assets: Vec<String>,
    pub accepted_at: Option<f64>,
    /// Fields not owned by preparation; metadata patches never replace these.
    pub extra: Map<String, Value>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    /// Open set: unknown synthetic events still receive a bounded projection.
    pub kind: String,
    pub text: String,
    pub created: f64,
    pub metadata: EventMetadata,
    pub preserve_progress: bool,
    pub preserve_complaint: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PreparationSnapshot {
    /// The provider controls only the input wire projection, never transcript text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    pub actor: Actor,
    pub root: RootSettings,
    pub delivered_manifests: Vec<ContextManifest>,
    pub plan: Option<Plan>,
    pub complaints: Vec<Complaint>,
    pub workers: Vec<Worker>,
    /// Authoritative store projection, gathered only for post-compaction delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_state: Option<Value>,
    pub delivered_reminders: BTreeSet<String>,
    pub assets: Vec<Asset>,
    pub guidance: GuidanceBundle,
    /// Verified paths returned by the adapter after materializing artifact intents.
    pub artifact_paths: BTreeMap<String, String>,
    pub prepared_at: f64,
    pub operation_id: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProviderInput {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(
            default,
            rename = "inputOrigin",
            skip_serializing_if = "Option::is_none"
        )]
        origin: Option<ProviderInputOrigin>,
    },
    #[serde(rename = "localImage")]
    LocalImage {
        path: String,
        #[serde(
            default,
            rename = "inputOrigin",
            skip_serializing_if = "Option::is_none"
        )]
        origin: Option<ProviderInputOrigin>,
    },
}
#[derive(Clone, Serialize, Deserialize)]
pub struct TranscriptInput {
    pub id: String,
    pub at: f64,
    pub kind: String,
    pub text: String,
    pub truncated: bool,
    pub assets: Vec<Map<String, Value>>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct TranscriptItem {
    pub id: String,
    pub role: String,
    pub title: String,
    pub text: String,
    pub truncated: bool,
    pub at: f64,
    pub assets: Vec<Map<String, Value>>,
    pub inputs: Vec<TranscriptInput>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactIntent {
    pub event_id: String,
    pub relative_path: String,
    /// Create exclusively, or verify identical bytes if this location already exists.
    pub body: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct AgentPatch {
    #[serde(rename = "complaintsPresented")]
    pub complaints_presented: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedSubmission {
    pub operation_id: String,
    pub actor_id: String,
    pub epoch: Epoch,
    pub event_ids: Vec<String>,
    pub provider_inputs: Vec<ProviderInput>,
    pub transcript: TranscriptItem,
    pub fulltext: Option<String>,
    pub metadata_patches: BTreeMap<String, Map<String, Value>>,
    pub agent_patch: AgentPatch,
    pub manifest: ContextManifest,
    /// Delivered sequence observed in this epoch; acceptance is a compare-and-set.
    pub base_sequence: u64,
    pub artifact_intents: Vec<ArtifactIntent>,
    /// Full Studio additions when their provider projection is bounded. The adapter
    /// writes this through payload_blobs before it commits the output reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio_input: Option<String>,
}
#[derive(Error, Serialize)]
#[serde(tag = "kind", content = "artifacts")]
pub enum PrepareError {
    #[error("The submission batch is empty")]
    EmptyBatch,
    #[error("The submission batch repeats an event identity")]
    DuplicateEvent,
    #[error("Attach up to {MAX_TURN_ATTACHMENTS} files")]
    TooManyAttachments,
    #[error("Unknown attachment")]
    UnknownAttachment,
    #[error("Attachment belongs to another conversation")]
    AttachmentOwnership,
    #[error("Invalid stored event evidence metadata")]
    InvalidEvidence,
    #[error("Invalid stored JSON evidence at {location}")]
    InvalidJsonEvidence { location: String },
    #[error("Invalid message receipt time")]
    InvalidClock,
    #[error("Context sequence exhausted")]
    SequenceExhausted,
    #[error("Child result artifacts must be materialized before submission")]
    ArtifactsRequired(Vec<ArtifactIntent>),
}
// Error diagnostics must never print stored text or artifact bodies.
impl std::fmt::Debug for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// serde_json objects are sorted; workspace preserve_order is forbidden.
pub fn context_version(value: &Value) -> String {
    sha256_hex(value.to_string().as_bytes())[..24].into()
}
fn chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
fn byte_prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn byte_tail(text: &str, limit: usize) -> &str {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
fn mode_guidance(root: &RootSettings) -> String {
    let limit = root.concurrency;
    let text = if limit == 0 {
        "Single agent mode (subagent concurrency 0): the lead completes new work itself. Do not delegate or start new worker turns. Existing active turns finish; queued work stays queued until the user raises the limit. Do not interrupt or stop workers.".into()
    } else {
        format!(
            "Multi agent mode (subagent concurrency {limit}): delegate sensibly up to {limit} simultaneous descendant turns across all providers and reviewer roles. The lead does not use a subagent slot. Excess worker turns queue. Do not poll unchanged status."
        )
    };
    format!(
        "[Studio subagent concurrency, revision {}] {text}",
        root.mode_revision
    )
}
fn outstanding(s: &PreparationSnapshot) -> Vec<&Complaint> {
    s.complaints
        .iter()
        .filter(|c| {
            c.lead_id == s.actor.id
                && c.recipient == Recipient::Lead
                && !matches!(
                    c.status,
                    ComplaintStatus::Resolved | ComplaintStatus::Declined
                )
                && ((c.legacy_user_task && matches!(c.status, ComplaintStatus::Open))
                    || !c.response_authors.contains(&c.lead_id))
        })
        .collect()
}
/// Only positive provider receipts consume this epoch's restoration. Rejected or
/// uncertain submissions retain it for recovery; a new thread is a distinct epoch.
pub(crate) fn needs_team_restore(epoch: &Epoch, manifests: &[ContextManifest]) -> bool {
    if epoch.1 == 0 {
        return false;
    }
    let same_thread = manifests
        .iter()
        .filter(|m| m.epoch.0 == epoch.0)
        .collect::<Vec<_>>();
    if same_thread.is_empty() {
        // The counter survives a thread replacement. A new thread alone must
        // not look like another compaction of the previous thread.
        manifests.iter().all(|m| m.epoch.1 < epoch.1)
    } else {
        same_thread.iter().all(|m| m.epoch.1 < epoch.1)
    }
}

fn context(s: &PreparationSnapshot) -> Result<(Vec<String>, ContextManifest), PrepareError> {
    let old = s
        .delivered_manifests
        .iter()
        .max_by_key(|m| (m.sequence, Reverse(&m.epoch.0), Reverse(m.epoch.1)));
    let mut known = old
        .filter(|m| m.epoch == s.actor.epoch)
        .map(|m| m.versions.clone())
        .unwrap_or_default();
    if let Some(prepared) = &s.actor.prepared_context
        && prepared.epoch.0 == s.actor.epoch.0
    {
        for (key, version) in &prepared.versions {
            known.entry(key.clone()).or_insert_with(|| version.clone());
        }
    }
    let mode = mode_guidance(&s.root);
    let mode_version = context_version(&json!(mode));
    if let Some(delivered) = &s.actor.delivered_mode
        && delivered.epoch == s.actor.epoch
        && delivered.version == mode_version
    {
        known.insert("agentMode".into(), mode_version);
    }
    let mut versions = Versions::new();
    let mut blocks = Vec::new();
    if needs_team_restore(&s.actor.epoch, &s.delivered_manifests)
        && let Some(state) = &s.team_state
    {
        let count = |key: &str| state[key].as_array().map_or(0, Vec::len);
        blocks.push(format!(
            "[Studio team state after compaction]\nAuthoritative current state. Do not respawn these workers or rerun these commands. Review pending results. Use orchestration_status and orchestration_task for full lists and details.\nChildren: {}; monitors: {}; open tasks: {}; pending result events: {}.\n{state}",
            count("children"), count("monitors"), count("tasks"), count("pendingResults")
        ));
    }
    for (key, text, prefix) in [
        ("agentMode", &mode, ""),
        (
            "progressFile",
            &s.guidance.progress,
            "[Studio progress file]\n",
        ),
        ("roleSkill", &s.guidance.role, ""),
    ] {
        if key == "progressFile" && !s.actor.is_lead {
            continue;
        }
        let version = context_version(&json!(text));
        if known.get(key) != Some(&version) {
            blocks.push(format!("{prefix}{text}"));
        }
        versions.insert(key.into(), version);
    }
    let plan = s.plan.as_ref().filter(|p| p.id == s.actor.root_id);
    if plan.is_some() || known.contains_key("plan") {
        let value = json!(plan);
        let version = context_version(&value);
        if known.get("plan") != Some(&version) {
            let content = match plan {
                Some(p) if !p.steps.is_empty() || !p.explanation.is_empty() => value.to_string(),
                _ => "The agent plan is empty. Discard the previous agent plan.".into(),
            };
            blocks.push(format!("[Agent plan version {version}]\n{content}"));
        }
        versions.insert("plan".into(), version);
    }
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();
    for c in outstanding(s) {
        let key = format!("complaint:{}", c.id);
        let version = context_version(&json!([c.text, c.status, c.version]));
        if known.get(&key) == Some(&version) {
            unchanged.push(c.id.as_str());
        } else {
            changed.push(json!({"complaint_id":c.id,"author":c.author,"author_name":c.author_name.as_ref().unwrap_or(&c.author),"text":c.text,"status":c.status,"created":c.created}));
        }
        versions.insert(key, version);
    }
    if !changed.is_empty() {
        blocks.push(format!("[Complaint messages requiring a response]\n{}", json!({"complaints":changed,"required":"Respond to each complaint with orchestration_complaint action=respond before finishing. The full complaint is in this message. No separate read call is required."})));
    }
    if !unchanged.is_empty() {
        blocks.push(format!("[Complaints still requiring a response] {}. The full text was delivered earlier. Read orchestration_context topic=complaints if needed.", unchanged.join(", ")));
    }
    if s.actor.is_lead {
        let mut waiting: Vec<_> = s
            .workers
            .iter()
            .filter(|w| {
                w.root_id == s.actor.id
                    && w.id != s.actor.id
                    && w.worktree_ready
                    && !w.deleted
                    && (matches!(
                        w.status,
                        WorkerStatus::Completed | WorkerStatus::Failed | WorkerStatus::Interrupted
                    ) || (matches!(w.status, WorkerStatus::Paused) && !w.auto_wake))
            })
            .map(|w| &w.id)
            .collect();
        waiting.sort();
        if waiting.len() >= CLEANUP_COUNT {
            let version = context_version(&json!(waiting));
            if known.get("worktreeReminder") != Some(&version)
                && !s.delivered_reminders.contains(&version)
            {
                blocks.push(format!("Studio: {} finished workers keep worktrees. Run orchestration_agent_manage action=archive_finished.", waiting.len()));
            }
            versions.insert("worktreeReminder".into(), version);
        }
    }
    let sequence = old
        .map_or(0, |m| m.sequence)
        .checked_add(1)
        .ok_or(PrepareError::SequenceExhausted)?;
    Ok((
        blocks,
        ContextManifest {
            epoch: s.actor.epoch.clone(),
            versions,
            sequence,
        },
    ))
}

// Retain producer order only when rewritten payload bytes become summary evidence.
// serde owns tokenization and validation; duplicate keys keep their first position.
pub(crate) enum OrderedJson {
    Scalar(Value),
    Object(Vec<(String, OrderedJson)>),
    Array(Vec<OrderedJson>),
}
impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        Self::parse(&raw, 0, true).map_err(serde::de::Error::custom)
    }
}
impl OrderedJson {
    fn parse(
        raw: &serde_json::value::RawValue,
        depth: usize,
        progress: bool,
    ) -> Result<Self, serde_json::Error> {
        use serde::de::{Deserializer, Error};
        let text = raw.get();
        if !text.starts_with(['{', '[']) {
            let value: Value = serde_json::from_str(text)?;
            if value.is_number()
                && !text.contains(['.', 'e', 'E'])
                && (text == "-0" || (text.parse::<i64>().is_err() && text.parse::<u64>().is_err()))
                && !(progress && (text == "-0" || !text.starts_with('-')))
            {
                return Err(serde_json::Error::custom("unsupported integer"));
            }
            return Ok(Self::Scalar(value));
        }
        if depth >= 64 {
            return Err(serde_json::Error::custom("JSON depth exceeds 64"));
        }
        struct Visitor {
            depth: usize,
            progress: bool,
        }
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON container")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(raw) = seq.next_element::<Box<serde_json::value::RawValue>>()? {
                    values.push(
                        OrderedJson::parse(&raw, self.depth + 1, false)
                            .map_err(A::Error::custom)?,
                    );
                }
                Ok(OrderedJson::Array(values))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values: Vec<(String, OrderedJson)> = Vec::new();
                let mut positions = BTreeMap::new();
                while let Some((key, raw)) =
                    map.next_entry::<String, Box<serde_json::value::RawValue>>()?
                {
                    let value = OrderedJson::parse(
                        &raw,
                        self.depth + 1,
                        self.progress && self.depth == 0 && key == "progress_version",
                    )
                    .map_err(A::Error::custom)?;
                    let index = *positions.entry(key.clone()).or_insert(values.len());
                    if index == values.len() {
                        values.push((key, value));
                    } else {
                        values[index].1 = value;
                    }
                }
                Ok(OrderedJson::Object(values))
            }
        }
        serde_json::Deserializer::from_str(text).deserialize_any(Visitor { depth, progress })
    }
}
// The raw-token checks share the ordered parse; there is no validation-only walk.
fn event_json(row: &Event, index: usize) -> Result<Option<Value>, PrepareError> {
    if matches!(
        row.kind.as_str(),
        "user" | "followup" | "work_decision" | "radio_turn"
    ) {
        return Ok(None);
    }
    let invalid = || PrepareError::InvalidJsonEvidence {
        location: format!("events[{index}].text"),
    };
    let text = row.text.trim();
    let raw = match serde_json::from_str::<Box<serde_json::value::RawValue>>(text) {
        Ok(raw) => raw,
        Err(_)
            if text.starts_with(['{', '[']) || matches!(text, "NaN" | "Infinity" | "-Infinity") =>
        {
            return Err(invalid());
        }
        Err(_) => return Ok(None),
    };
    let ordered =
        OrderedJson::parse(&raw, 0, row.kind == "agent_message").map_err(|_| invalid())?;
    serde_json::to_value(ordered)
        .map(Some)
        .map_err(|_| invalid())
}
impl Serialize for OrderedJson {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Self::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
            Self::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::Scalar(v) => {
                if let Some(value) = v.as_f64().filter(|_| v.is_f64()) {
                    serde_json::value::RawValue::from_string(python_float_json(value))
                        .map_err(serde::ser::Error::custom)?
                        .serialize(serializer)
                } else {
                    v.serialize(serializer)
                }
            }
        }
    }
}
impl OrderedJson {
    // Only a fixed number of known fields are edited per event, so these scans
    // remain linear in payload width. Parsing uses an index for duplicate keys.
    fn set(&mut self, key: &str, value: Self) {
        if let Self::Object(fields) = self {
            if let Some((_, v)) = fields.iter_mut().find(|(k, _)| k == key) {
                *v = value;
            } else {
                fields.push((key.into(), value));
            }
        }
    }
    fn fields(&mut self, value: &Value, keys: &[&str]) {
        for key in keys {
            if let Some(v) = value.get(key) {
                self.set(key, Self::Scalar(v.clone()));
            }
        }
    }
}
fn rewritten(text: &str, edit: impl FnOnce(&mut OrderedJson)) -> String {
    // Parsing already passed the explicit JSON evidence boundary.
    match serde_json::from_str::<OrderedJson>(text) {
        Ok(mut ordered) => {
            edit(&mut ordered);
            serde_json::to_string(&ordered).unwrap_or_else(|_| text.into())
        }
        Err(_) => text.into(),
    }
}

fn truncate_review(result: &mut Value) {
    let checks = result["checks"].as_str().unwrap_or("");
    let truncated = checks.chars().count() > CHECKS_CHARS;
    result["checks"] = json!(chars(checks, CHECKS_CHARS));
    result["checksTruncated"] = json!(truncated);
    if let Some(files) = result.get("files").and_then(Value::as_array)
        && json!(files).to_string().len() > FILES_BYTES
    {
        let names: Vec<_> = files
            .iter()
            .take(REVIEW_FILE_EXCERPT_COUNT)
            .map(|v| chars(v.as_str().unwrap_or(""), FILE_NAME_CHARS))
            .collect();
        result["files"] = json!(names);
        result["filesTruncated"] = json!(true);
    }
}
fn bound_event(row: &Event, text: &str, limit: usize) -> String {
    let mut text = text.to_owned();
    if matches!(row.kind.as_str(), "work_review" | "child_result")
        && let Ok(mut value) = serde_json::from_str::<Value>(&text)
    {
        let result = &value["result"];
        let body = if result.is_object() {
            result["text"].as_str()
        } else {
            result.as_str()
        };
        let path = if result.is_object() {
            &result["resultFile"]
        } else {
            &value["resultFile"]
        };
        let path = path.as_str().filter(|p| !p.is_empty()).map(str::to_owned);
        if let Some(body) = body.filter(|b| b.len() > RESULT_BYTES) {
            let replace_read = path.is_some();
            let excerpt = chars(body, RESULT_EXCERPT);
            if let Some(path) = path {
                if row.kind == "work_review" && result.is_object() {
                    let result = &mut value["result"];
                    result["text"] = json!(excerpt);
                    result["textTruncated"] = json!(true);
                    truncate_review(result);
                } else {
                    value["result"] = json!(excerpt);
                    value["resultTruncated"] = json!(true);
                    value["resultFile"] = json!(path);
                }
                value["read"] =
                    json!({"tool":"orchestration_read","output_ref":format!("event:{}",row.id)});
            }
            text = rewritten(&text, |ordered| {
                if row.kind == "work_review" && value["result"].is_object() {
                    if let OrderedJson::Object(fields) = ordered
                        && let Some((_, result)) = fields.iter_mut().find(|(k, _)| k == "result")
                    {
                        result.fields(
                            &value["result"],
                            &[
                                "text",
                                "textTruncated",
                                "checks",
                                "checksTruncated",
                                "files",
                                "filesTruncated",
                            ],
                        );
                    }
                } else {
                    ordered.fields(&value, &["result", "resultTruncated", "resultFile"]);
                }
                if replace_read && let Some(read) = value.get("read") {
                    ordered.set(
                        "read",
                        OrderedJson::Object(
                            ["tool", "output_ref"]
                                .map(|key| (key.into(), OrderedJson::Scalar(read[key].clone())))
                                .into(),
                        ),
                    );
                }
            });
        } else if row.kind == "work_review"
            && result.is_object()
            && path.is_some()
            && value.to_string().len() > limit
        {
            let result = &mut value["result"];
            truncate_review(result);
            text = rewritten(&text, |ordered| {
                if let OrderedJson::Object(fields) = ordered
                    && let Some((_, result)) = fields.iter_mut().find(|(k, _)| k == "result")
                {
                    result.fields(
                        &value["result"],
                        &["checks", "checksTruncated", "files", "filesTruncated"],
                    );
                }
            });
        }
    }
    if text.len() <= limit {
        return text;
    }
    let mut summary = json!({"eventId":row.id,"kind":row.kind,"truncated":true,"textBytes":row.text.len(),"read":{"tool":"orchestration_read","output_ref":format!("event:{}",row.id)}});
    if let Ok(value) = serde_json::from_str::<Value>(&text)
        && value.is_object()
    {
        // A bounded review must retain the acceptance outcome even when its log is large.
        if row.kind == "work_review" && value["result"]["gate"].is_object() {
            summary["task"] = value["task"].clone();
            summary["gate"] = json!({
                "status":value["result"]["gate"]["status"],
                "exitCode":value["result"]["gate"]["exitCode"],
                "monitorId":value["result"]["gate"]["monitorId"],
            });
        }
        let path = if value["result"].is_object() {
            &value["result"]["resultFile"]
        } else {
            &value["resultFile"]
        };
        if path.as_str().is_some_and(|p| !p.is_empty()) {
            summary["resultFile"] = path.clone();
        }
        let keys = [
            "id",
            "agent_id",
            "sender",
            "room",
            "message_id",
            "turnId",
            "turn_id",
            "status",
            "exitCode",
            "importance",
            "progress_key",
            "progress_version",
            "earlierProgressUpdates",
            "complaint_id",
            "lead",
            "log",
            "bytes",
        ];
        summary["identity"] = Value::Object(
            keys.into_iter()
                .filter_map(|k| value.get(k).map(|v| (k.to_owned(), v.clone())))
                .collect(),
        );
    }
    for key in ["identity", "eventId"] {
        if summary.to_string().len() > limit
            && let Some(map) = summary.as_object_mut()
        {
            map.remove(key);
        }
    }
    let remaining = limit.saturating_sub(summary.to_string().len() + SUMMARY_RESERVE);
    let mut excerpt = byte_prefix(&text, remaining / 2).to_owned();
    let mut tail = byte_tail(&text, remaining / 2).to_owned();
    loop {
        summary["excerpt"] = json!(excerpt);
        summary["tail"] = json!(tail);
        if summary.to_string().len() <= limit || (excerpt.is_empty() && tail.is_empty()) {
            break;
        }
        excerpt = chars(&excerpt, excerpt.chars().count() / 2);
        tail = if tail.chars().count() > 1 {
            tail.chars().skip(tail.chars().count() / 2).collect()
        } else {
            String::new()
        };
    }
    summary.to_string()
}

type ProgressKey = (String, String, String);
type ProgressGroup<'a> = (BTreeSet<u64>, usize, (u64, &'a str));
fn progress(row: &Event, value: &Value) -> Option<(ProgressKey, u64)> {
    if row.kind != "agent_message" || row.preserve_progress {
        return None;
    }
    if value["importance"] != "progress" {
        return None;
    }
    let field = |key| {
        value[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    Some((
        (field("sender")?, field("room")?, field("progress_key")?),
        value["progress_version"].as_u64()?,
    ))
}

fn validate_evidence(row: &Event, value: Option<&Value>) -> Result<(), PrepareError> {
    if row.kind == "work_review"
        && let Some(result) = value.and_then(|v| v.get("result"))
        && (result.get("checks").is_some_and(|v| !v.is_string())
            || result
                .get("files")
                .and_then(Value::as_array)
                .is_some_and(|files| files.iter().any(|v| !v.is_string())))
    {
        return Err(PrepareError::InvalidEvidence);
    }
    if row.kind == "child_result"
        && value.is_some_and(|v| {
            ["agent_id", "name"]
                .iter()
                .any(|k| v.get(k).is_some_and(|v| v.is_array() || v.is_object()))
        })
    {
        return Err(PrepareError::InvalidEvidence);
    }
    Ok(())
}
/// Locate invalid notification evidence with the same parser and validator used
/// by compilation. A bad notification can be held visibly without withholding
/// unrelated user input. User text is never classified as structured evidence.
pub(crate) fn invalid_notification_evidence(rows: &[Event]) -> BTreeMap<String, String> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            event_json(row, index)
                .and_then(|value| validate_evidence(row, value.as_ref()))
                .err()
                .map(|error| (row.id.clone(), error.to_string()))
        })
        .collect()
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(v) if v.is_f64() => {
            v.as_f64().map_or_else(|| v.to_string(), python_float_json)
        }
        _ => value.to_string(),
    }
}
fn artifact(row: &Event, value: &Value, body: &str) -> ArtifactIntent {
    let raw = value.get("agent_id");
    let child = raw.map(scalar_text).unwrap_or_default();
    let child = if raw.is_some_and(Value::is_string)
        && !child.is_empty()
        && child.len() <= 200
        && child
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        child.to_owned()
    } else {
        sha256_hex(child.as_bytes())
    };
    let name = value
        .get("name")
        .map(scalar_text)
        .unwrap_or_else(|| child.clone());
    ArtifactIntent {
        event_id: row.id.clone(),
        relative_path: format!("results/{child}/{}.md", sha256_hex(row.id.as_bytes())),
        body: format!(
            "# Worker result\n\nAgent: `{name}`\n\nEvent ID: `{}`\n\n## Result\n\n{body}\n",
            row.id
        ),
    }
}
/// Authoritative child counts of one parent, computed from the store at delivery
/// time so a parent never has to count `child_result` events. The one owner of the
/// team progress line; both providers consume it through `compile`. Counts are
/// agents. A finished child is completed, failed, interrupted, or paused without
/// auto-wake (the same definition as `AgentIndexedReads::ACTIVE_TEAM_MEMBERS_SQL`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TeamProgress {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub interrupted: usize,
    pub running: usize,
    pub queued: usize,
}
impl TeamProgress {
    pub fn of(s: &PreparationSnapshot) -> Self {
        let mut progress = Self::default();
        for w in s
            .workers
            .iter()
            .filter(|w| !w.deleted && w.parent_id.as_deref() == Some(s.actor.id.as_str()))
        {
            progress.total += 1;
            match w.status {
                WorkerStatus::Completed => progress.completed += 1,
                WorkerStatus::Paused if !w.auto_wake => progress.completed += 1,
                WorkerStatus::Failed => progress.failed += 1,
                WorkerStatus::Interrupted => progress.interrupted += 1,
                WorkerStatus::Queued => progress.queued += 1,
                WorkerStatus::Paused | WorkerStatus::Running => progress.running += 1,
            }
        }
        progress
    }
    pub fn finished(&self) -> usize {
        self.completed + self.failed + self.interrupted
    }
    /// The delivery line, or `None` when the parent has no children.
    pub fn line(&self) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        let mut line = format!(
            "Children: {} total, {} finished ({} completed, {} failed, {} interrupted), {} running, {} queued.",
            self.total,
            self.finished(),
            self.completed,
            self.failed,
            self.interrupted,
            self.running,
            self.queued
        );
        if self.finished() == self.total {
            line.push_str(" All children have finished.");
        }
        Some(line)
    }
}
fn event_text(
    s: &PreparationSnapshot,
    rows: &[Event],
) -> Result<(Vec<InputPart>, Vec<ArtifactIntent>), PrepareError> {
    let parsed: Vec<_> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| event_json(row, index))
        .collect::<Result<_, _>>()?;
    let progress: Vec<_> = rows
        .iter()
        .zip(&parsed)
        .map(|(r, v)| v.as_ref().and_then(|v| progress(r, v)))
        .collect();
    let mut groups: BTreeMap<&ProgressKey, ProgressGroup<'_>> = BTreeMap::new();
    for (row, update) in rows.iter().zip(&progress) {
        if let Some((key, version)) = update {
            let group = groups.entry(key).or_default();
            group.0.insert(*version);
            group.1 += 1;
            if *version >= group.2.0 {
                group.2 = (*version, &row.id);
            }
        }
    }
    let synthetic = rows.iter().filter(|row| !preserves_task_text(row)).count();
    let limit = EVENT_BYTES.min(STUDIO_INPUT_BYTES / synthetic.max(1));
    let mut parts = Vec::new();
    let mut artifacts = Vec::new();
    let mut missing = Vec::new();
    for ((row, parsed), update) in rows.iter().zip(parsed).zip(progress.iter()) {
        let mut text = row.text.clone();
        validate_evidence(row, parsed.as_ref())?;
        if row.kind == "complaint"
            && !row.preserve_complaint
            && parsed
                .as_ref()
                .and_then(|v| v["complaints"].as_array())
                .is_some_and(|cs| {
                    !cs.is_empty() && cs.iter().all(|c| c["complaint_id"].is_string())
                })
        {
            continue;
        }
        if row.kind == "child_result"
            && let Some(value) = parsed.clone()
            && let Some(body) = value["result"].as_str().filter(|v| v.len() > RESULT_BYTES)
            && !value["resultFile"].as_str().is_some_and(|p| !p.is_empty())
        {
            let intent = artifact(row, &value, body);
            if let Some(path) = s.artifact_paths.get(&row.id).filter(|p| !p.is_empty()) {
                text = rewritten(&text, |ordered| {
                    ordered.set("resultFile", OrderedJson::Scalar(json!(path)))
                });
            } else {
                missing.push(intent.clone());
            }
            artifacts.push(intent);
        }
        if let Some((key, _)) = update
            && let Some((seen, count, latest)) = groups.get(key)
            && seen.len() == *count
        {
            if latest.1 != row.id {
                continue;
            }
            if *count > 1
                && let Some(mut value) = parsed
            {
                value["earlierProgressUpdates"] = json!(count - 1);
                value["history"] =
                    json!("Read earlier progress with orchestration_chat_read in this room.");
                text = rewritten(&text, |ordered| {
                    ordered.fields(&value, &["earlierProgressUpdates", "history"])
                });
            }
        }
        let preserved = preserves_task_text(row);
        if !preserved {
            text = bound_event(row, &text, limit);
        }
        let origin = event_origin(row);
        if !parts.is_empty() {
            parts.push(InputPart::event("\n\n".into(), origin));
        }
        if preserved && row.kind == "followup" {
            parts.push(InputPart::event(
                "[Orchestration event: followup]\n".into(),
                origin,
            ));
        }
        parts.push(InputPart {
            text: if preserved {
                text
            } else {
                format!("[Orchestration event: {}]\n{text}", row.kind)
            },
            injected: !preserved,
            origin,
        });
    }
    if !missing.is_empty() {
        return Err(PrepareError::ArtifactsRequired(missing));
    }
    if rows
        .iter()
        .any(|r| matches!(r.kind.as_str(), "child_result" | "child_stopped"))
        && let Some(line) = TeamProgress::of(s).line()
    {
        parts.push(InputPart::studio(format!("\n\n[Team progress]\n{line}")));
    }
    Ok((parts, artifacts))
}

/// A sender-stamped followup is the parent's complete task instruction.
/// Its Human origin is shared with decisions, but only its body is exempt.
fn preserves_task_text(event: &Event) -> bool {
    event.kind == "user"
        || (event.kind == "followup" && event_origin(event) == ProviderInputOrigin::Human)
}

fn event_origin(event: &Event) -> ProviderInputOrigin {
    // Followup senderId is stamped by coordination::send_followup after the
    // caller verifies descendant ancestry. System followups have no sender.
    let delegated_followup = event.kind == "followup"
        && event
            .metadata
            .extra
            .get("senderId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
    if matches!(event.kind.as_str(), "user" | "work_decision" | "work_ready") || delegated_followup
    {
        ProviderInputOrigin::Human
    } else {
        ProviderInputOrigin::StudioEvent
    }
}

#[derive(Clone)]
struct InputPart {
    text: String,
    injected: bool,
    origin: ProviderInputOrigin,
}
impl InputPart {
    fn studio(text: String) -> Self {
        Self {
            text,
            injected: true,
            origin: ProviderInputOrigin::StudioEvent,
        }
    }
    fn event(text: String, origin: ProviderInputOrigin) -> Self {
        Self {
            text,
            injected: true,
            origin,
        }
    }
}

/// The final authority for the aggregate budget, after all per-event projections.
/// Fixed-size content hashes keep the recovery pointer bounded even with long IDs.
fn bound_input(parts: &[InputPart]) -> (Vec<InputPart>, Option<String>) {
    let bytes: usize = parts
        .iter()
        .filter(|p| p.injected)
        .map(|p| p.text.len())
        .sum();
    let roster = parts.iter().position(|p| {
        p.injected
            && p.text
                .starts_with("\n\n[Studio team state after compaction]")
    });
    if bytes <= STUDIO_INPUT_BYTES
        && roster.is_none_or(|i| parts[i].text.len() <= TEAM_RESTORE_BYTES)
    {
        return (parts.to_vec(), None);
    }
    let full: String = parts
        .iter()
        .filter(|p| p.injected)
        .map(|p| p.text.as_str())
        .collect();
    let reference = format!("studio-input:{}", sha256_hex(full.as_bytes()));
    let notice = format!(
        "\n\n[Studio input truncated] Read the omitted context before acting. outputRef: {reference}. Use orchestration_read with output_ref={reference}.\n"
    );
    let mut remaining = STUDIO_INPUT_BYTES - notice.len();
    let mut shares = vec![0; parts.len()];
    if let Some(index) = roster {
        shares[index] = remaining
            .min(TEAM_RESTORE_BYTES)
            .min(parts[index].text.len());
        remaining -= shares[index];
    }
    let mut count = parts
        .iter()
        .enumerate()
        .filter(|(i, p)| p.injected && Some(*i) != roster)
        .count();
    for (index, part) in parts.iter().enumerate() {
        if part.injected && Some(index) != roster {
            shares[index] = part.text.len().min(remaining / count);
            remaining -= shares[index];
            count -= 1;
        }
    }
    let mut projected = vec![InputPart::studio(notice)];
    for (index, part) in parts.iter().enumerate() {
        projected.push(InputPart {
            text: if part.injected {
                byte_prefix(&part.text, shares[index]).into()
            } else {
                part.text.clone()
            },
            injected: part.injected,
            origin: part.origin,
        });
    }
    (projected, Some(full))
}

/// Compile only an admitted, authorized, ordered batch. Those effectful gates belong
/// to the caller; compilation does not grant permission to submit.
pub fn compile(
    s: &PreparationSnapshot,
    rows: &[Event],
) -> Result<PreparedSubmission, PrepareError> {
    let first = rows.first().ok_or(PrepareError::EmptyBatch)?;
    if rows.iter().map(|r| &r.id).collect::<BTreeSet<_>>().len() != rows.len() {
        return Err(PrepareError::DuplicateEvent);
    }
    if rows.iter().map(|r| r.metadata.assets.len()).sum::<usize>() > MAX_TURN_ATTACHMENTS {
        return Err(PrepareError::TooManyAttachments);
    }
    let attributed = s.provider == Some(ProviderKind::Claude);
    let mut provider_assets = Vec::new();
    let mut all_assets = Vec::new();
    let mut inputs = Vec::new();
    let mut remaining = TRANSCRIPT_CHARS;
    let mut clocks = Vec::new();
    for row in rows {
        let mut assets = Vec::new();
        for key in &row.metadata.assets {
            let a = s
                .assets
                .iter()
                .find(|a| &a.id == key && !a.owner_deleted)
                .ok_or(PrepareError::UnknownAttachment)?;
            if a.owner_root_id != s.actor.root_id
                && s.actor.forked_from.as_ref() != Some(&a.owner_id)
            {
                return Err(PrepareError::AttachmentOwnership);
            }
            provider_assets.push(if a.image {
                ProviderInput::LocalImage {
                    path: a.path.clone(),
                    origin: attributed.then(|| event_origin(row)),
                }
            } else {
                ProviderInput::Text {
                    text: format!(
                        "Attached file: {}\nRead the file at {} ({} bytes).",
                        a.name, a.path, a.size
                    ),
                    origin: attributed.then(|| event_origin(row)),
                }
            });
            let mut view = a.view.clone();
            view.remove("path");
            view.extend(Map::from_iter([
                ("id".into(), json!(a.id)),
                ("agent".into(), json!(a.owner_id)),
                ("image".into(), json!(a.image)),
                ("name".into(), json!(a.name)),
                ("size".into(), json!(a.size)),
            ]));
            assets.push(view);
        }
        all_assets.extend(assets.clone());
        let text = chars(&row.text, remaining);
        remaining -= text.chars().count();
        inputs.push(TranscriptInput {
            id: row.id.clone(),
            at: row.created,
            kind: row.kind.clone(),
            truncated: text.len() < row.text.len(),
            text,
            assets,
        });
        if matches!(row.kind.as_str(), "user" | "followup")
            && let Some(at) = row.metadata.accepted_at
        {
            // Python datetime rounds to microseconds before truncating to milliseconds.
            if !at.is_finite() {
                return Err(PrepareError::InvalidClock);
            }
            // Split before scaling: multiplying the full epoch loses fractional bits.
            let seconds = at.trunc();
            let micros = (at.fract() * 1_000_000.0).round_ties_even();
            if seconds < -62_135_596_801.0 || seconds > 253_402_300_800.0 {
                return Err(PrepareError::InvalidClock);
            }
            let time = DateTime::from_timestamp_micros(seconds as i64 * 1_000_000 + micros as i64)
                .ok_or(PrepareError::InvalidClock)?;
            if !(1..=9999).contains(&time.year()) {
                return Err(PrepareError::InvalidClock);
            }
            clocks.push(format!(
                "Message {} accepted at {}",
                row.id,
                time.to_rfc3339_opts(SecondsFormat::Millis, true)
            ));
        }
    }
    let (mut parts, artifact_intents) = event_text(s, rows)?;
    let (context, manifest) = context(s)?;
    for block in context {
        parts.push(InputPart::studio(format!("\n\n{block}")));
    }
    if parts.iter().all(|part| part.text.is_empty()) {
        parts.push(InputPart::studio(
            "[Complaint update] No complaints require a response.".into(),
        ));
    }
    let original_transcript: String = parts.iter().map(|part| part.text.as_str()).collect();
    if !clocks.is_empty() {
        parts.push(InputPart::studio(format!(
            "\n\n[Time awareness, message receipt time]\n{}",
            clocks.join("\n")
        )));
    }
    let before_attachments = parts.len();
    // Text attachment descriptions share this input's budget. Images stay native.
    for input in &provider_assets {
        if let ProviderInput::Text { text, origin } = input {
            parts.push(InputPart::event(
                format!("\n\n{text}"),
                origin.unwrap_or(ProviderInputOrigin::StudioEvent),
            ));
        }
    }
    let (mut projected, studio_input) = bound_input(&parts);
    if studio_input.is_none() {
        // Preserve native attachments when the aggregate budget admits them.
        projected.truncate(before_attachments);
    }
    let text: String = projected.iter().map(|part| part.text.as_str()).collect();
    let transcript_source = if studio_input.is_some() {
        &text
    } else {
        &original_transcript
    };
    let item_id = format!("{}:{}", s.actor.id, first.id);
    let transcript_text = chars(transcript_source, TRANSCRIPT_CHARS);
    let truncated = transcript_text.len() < transcript_source.len();
    let transcript = TranscriptItem {
        id: item_id.clone(),
        role: "user".into(),
        title: "User".into(),
        text: transcript_text,
        truncated,
        at: s.prepared_at,
        assets: all_assets,
        inputs,
    };
    let fulltext = truncated.then(|| transcript_source.clone());
    let mut provider_inputs = if attributed {
        // Budget parts can split wrappers from exempt task bodies. Provider
        // items change only at an authority boundary, never at that split.
        let mut inputs = Vec::new();
        for part in projected.into_iter().filter(|part| !part.text.is_empty()) {
            if let Some(ProviderInput::Text { text, origin }) = inputs.last_mut()
                && *origin == Some(part.origin)
            {
                text.push_str(&part.text);
                continue;
            }
            inputs.push(ProviderInput::Text {
                text: part.text,
                origin: Some(part.origin),
            });
        }
        inputs
    } else {
        vec![ProviderInput::Text { text, origin: None }]
    };
    provider_inputs.extend(provider_assets.into_iter().filter(|input| {
        studio_input.is_none() || matches!(input, ProviderInput::LocalImage { .. })
    }));
    let mut metadata_patches = BTreeMap::new();
    for row in rows {
        let mut patch = Map::from_iter([("modelEventProjection".into(), json!(1))]);
        if !row.id.is_empty() {
            patch.insert("transcriptItemId".into(), json!(item_id));
        }
        if row.id == first.id {
            patch.insert("contextManifest".into(), json!(manifest));
            if let Some(body) = &studio_input {
                patch.insert(
                    "studioInput".into(),
                    json!({"sha256":sha256_hex(body.as_bytes()),"size":body.len()}),
                );
            }
        }
        metadata_patches.insert(row.id.clone(), patch);
    }
    Ok(PreparedSubmission {
        operation_id: s.operation_id.clone(),
        actor_id: s.actor.id.clone(),
        epoch: s.actor.epoch.clone(),
        event_ids: rows.iter().map(|r| r.id.clone()).collect(),
        provider_inputs,
        transcript,
        fulltext,
        metadata_patches,
        agent_patch: AgentPatch {
            complaints_presented: outstanding(s).iter().map(|c| c.id.clone()).collect(),
        },
        base_sequence: s
            .delivered_manifests
            .iter()
            .filter(|m| m.epoch == s.actor.epoch)
            .map(|m| m.sequence)
            .max()
            .unwrap_or(0),
        manifest,
        artifact_intents,
        studio_input,
    })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReceiptOutcome {
    Accepted { turn_id: String },
    Rejected,
    Unknown,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct NativeReceipt {
    pub operation_id: String,
    pub actor_id: String,
    pub epoch: Epoch,
    pub event_ids: Vec<String>,
    pub outcome: ReceiptOutcome,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    #[default]
    Unconfirmed,
    Stale,
    Accepted,
}
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct DeliveryTransition {
    pub status: DeliveryStatus,
    pub manifest_upsert: Option<ContextManifest>,
    pub reminder_insert: Option<String>,
    pub transcript_turn_id: Option<String>,
}
/// `current` is the delivered manifest for this exact epoch, read in the same
/// transaction that applies this transition. A receipt compiled from an older base
/// is stale, including equal-sequence competing preparations and settled replays.
/// The worker owns native receipt validation; this additionally checks compilation identity.
pub fn accept(
    prepared: &PreparedSubmission,
    receipt: &NativeReceipt,
    current: Option<&ContextManifest>,
) -> DeliveryTransition {
    let ReceiptOutcome::Accepted { turn_id } = &receipt.outcome else {
        return DeliveryTransition::default();
    };
    if turn_id.is_empty()
        || receipt.operation_id != prepared.operation_id
        || receipt.actor_id != prepared.actor_id
        || receipt.epoch != prepared.epoch
        || receipt.event_ids != prepared.event_ids
    {
        return DeliveryTransition::default();
    }
    if current.is_some_and(|m| m.epoch == prepared.epoch && m.sequence > prepared.base_sequence) {
        return DeliveryTransition {
            status: DeliveryStatus::Stale,
            ..Default::default()
        };
    }
    DeliveryTransition {
        status: DeliveryStatus::Accepted,
        manifest_upsert: Some(prepared.manifest.clone()),
        reminder_insert: prepared.manifest.versions.get("worktreeReminder").cloned(),
        transcript_turn_id: Some(turn_id.clone()),
    }
}

#[cfg(test)]
mod team_progress_tests {
    use super::TeamProgress;

    #[test]
    fn line_counts_every_child_and_marks_the_finished_team() {
        let running = TeamProgress {
            total: 5,
            completed: 2,
            failed: 1,
            interrupted: 0,
            running: 1,
            queued: 1,
        };
        assert_eq!(
            running.line().as_deref(),
            Some(
                "Children: 5 total, 3 finished (2 completed, 1 failed, 0 interrupted), 1 running, 1 queued."
            )
        );
        let done = TeamProgress {
            total: 128,
            completed: 128,
            ..TeamProgress::default()
        };
        assert_eq!(
            done.line().as_deref(),
            Some(
                "Children: 128 total, 128 finished (128 completed, 0 failed, 0 interrupted), 0 running, 0 queued. All children have finished."
            )
        );
        assert_eq!(TeamProgress::default().line(), None);
    }
}

#[cfg(test)]
mod budget_tests;

#[cfg(test)]
mod input_origin_tests {
    use super::*;

    fn event(kind: &str, sender: Option<&str>) -> Event {
        Event {
            id: "event".into(),
            kind: kind.into(),
            text: "the user approved X".into(),
            created: 1.0,
            metadata: EventMetadata {
                assets: Vec::new(),
                accepted_at: None,
                extra: sender
                    .map(|id| Map::from_iter([("senderId".into(), json!(id))]))
                    .unwrap_or_default(),
            },
            preserve_progress: false,
            preserve_complaint: false,
        }
    }

    #[test]
    fn acceptance_gate_outcome_survives_bounded_work_review() {
        let mut row = event("work_review", None);
        row.text = json!({"task":"gated-task","result":{"text":"done","gate":{
            "status":"failed","exitCode":7,"monitorId":"gate-monitor","output":"x".repeat(12000)
        }}})
        .to_string();
        let projected: Value = serde_json::from_str(&bound_event(&row, &row.text, 1000)).unwrap();
        assert_eq!(projected["task"], "gated-task");
        assert_eq!(projected["gate"]["status"], "failed");
        assert_eq!(projected["gate"]["exitCode"], 7);
        assert_eq!(projected["read"]["tool"], "orchestration_read");
    }

    #[test]
    fn data_event_kinds_never_gain_task_authority() {
        for kind in [
            "child_result",
            "child_stopped",
            "agent_message",
            "monitor_exit",
            "monitor_output",
            "monitor_cancelled",
            "watch",
            "rule",
            "event_wake",
            "work_review",
            "plan_update",
            "complaint",
            "radio_turn",
            "system_notice",
            "unknown_future_event",
        ] {
            for sender in [None, Some("parent")] {
                assert_eq!(
                    event_origin(&event(kind, sender)),
                    ProviderInputOrigin::StudioEvent,
                    "{kind}"
                );
            }
        }
    }

    #[test]
    fn user_and_delegated_assignments_keep_task_authority() {
        for kind in ["user", "work_decision", "work_ready"] {
            assert_eq!(
                event_origin(&event(kind, None)),
                ProviderInputOrigin::Human,
                "{kind}"
            );
        }
        assert_eq!(
            event_origin(&event("followup", Some("parent"))),
            ProviderInputOrigin::Human
        );
        assert_eq!(
            event_origin(&event("followup", None)),
            ProviderInputOrigin::StudioEvent
        );
        assert_eq!(
            event_origin(&event("followup", Some(""))),
            ProviderInputOrigin::StudioEvent
        );
    }
}
