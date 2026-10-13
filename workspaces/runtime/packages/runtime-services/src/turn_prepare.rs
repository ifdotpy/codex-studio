//! Pure submission compiler. The account worker gathers and revalidates the snapshot,
//! materializes artifact intents, persists the result, and submits exactly once.
//! No preparation result is evidence of delivery; only `accept` promotes knowledge.
//! Synthetic byte budgets are soft: reference-only summaries can exceed their share.
//! There is no finite total ceiling with unbounded IDs/paths, context, or bypass events.
//! The transcript and its input excerpts each have a hard 20,000-character ceiling.
use chrono::{DateTime, Datelike, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_store::{python_float_json, sha256_hex};
use thiserror::Error;

const TRANSCRIPT_CHARS: usize = 20_000;
const RESULT_BYTES: usize = 20_000;
const EVENT_BYTES: usize = 22_000;
const BATCH_BYTES: usize = 24_000;
const ATTACHMENTS: usize = 8;
const RESULT_EXCERPT: usize = 600;
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
    Active,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub root_id: String,
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
    pub actor: Actor,
    pub root: RootSettings,
    pub delivered_manifests: Vec<ContextManifest>,
    pub plan: Option<Plan>,
    pub complaints: Vec<Complaint>,
    pub workers: Vec<Worker>,
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
    Text { text: String },
    #[serde(rename = "localImage")]
    LocalImage { path: String },
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
}
#[derive(Error, Serialize)]
#[serde(tag = "kind", content = "artifacts")]
pub enum PrepareError {
    #[error("The submission batch is empty")]
    EmptyBatch,
    #[error("The submission batch repeats an event identity")]
    DuplicateEvent,
    #[error("Attach up to eight files")]
    TooManyAttachments,
    #[error("Unknown attachment")]
    UnknownAttachment,
    #[error("Attachment belongs to another conversation")]
    AttachmentOwnership,
    #[error("Invalid stored event evidence metadata")]
    InvalidEvidence,
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

fn packed(value: &Value) -> String {
    value.to_string()
}
/// Explicit recursive key sorting keeps hashes independent of serde feature unification.
pub fn context_version(value: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let keys: BTreeSet<_> = map.keys().collect();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), canonical(&map[k])))
                        .collect(),
                )
            }
            Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    sha256_hex(packed(&canonical(value)).as_bytes())[..24].into()
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
fn context(s: &PreparationSnapshot) -> Result<(String, ContextManifest), PrepareError> {
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
                Some(p) if !p.steps.is_empty() || !p.explanation.is_empty() => packed(&value),
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
        blocks.push(format!("[Complaint messages requiring a response]\n{}", packed(&json!({"complaints":changed,"required":"Respond to each complaint with orchestration_complaint action=respond before finishing. The full complaint is in this message. No separate read call is required."}))));
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
    let text = if blocks.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", blocks.join("\n\n"))
    };
    Ok((
        text,
        ContextManifest {
            epoch: s.actor.epoch.clone(),
            versions,
            sequence,
        },
    ))
}

// Retain producer order only when rewritten payload bytes become summary evidence.
// serde owns tokenization and validation; duplicate keys keep their first position.
struct OrderedJson(Value, Vec<(String, OrderedJson)>, Vec<OrderedJson>);
impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJson(Value::Null, vec![], vec![]))
            }
            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(OrderedJson(json!(v), vec![], vec![]))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(OrderedJson(json!(v), vec![], vec![]))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(OrderedJson(json!(v), vec![], vec![]))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(OrderedJson(json!(v), vec![], vec![]))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(OrderedJson(json!(v), vec![], vec![]))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element()? {
                    values.push(v);
                }
                Ok(OrderedJson(json!([]), vec![], values))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values: Vec<(String, OrderedJson)> = Vec::new();
                while let Some((k, v)) = map.next_entry()? {
                    if let Some(existing) = values.iter_mut().find(|(key, _)| *key == k) {
                        existing.1 = v;
                    } else {
                        values.push((k, v));
                    }
                }
                Ok(OrderedJson(json!({}), values, vec![]))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
impl Serialize for OrderedJson {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        if self.0.is_object() {
            let mut map = serializer.serialize_map(Some(self.1.len()))?;
            for (key, value) in &self.1 {
                map.serialize_entry(key, value)?;
            }
            map.end()
        } else if self.0.is_array() {
            let mut seq = serializer.serialize_seq(Some(self.2.len()))?;
            for value in &self.2 {
                seq.serialize_element(value)?;
            }
            seq.end()
        } else if let Some(value) = self.0.as_f64().filter(|_| self.0.is_f64()) {
            serde_json::value::RawValue::from_string(python_float_json(value))
                .map_err(serde::ser::Error::custom)?
                .serialize(serializer)
        } else {
            self.0.serialize(serializer)
        }
    }
}
impl OrderedJson {
    fn update(&mut self, value: &Value) {
        if let Value::Object(map) = value {
            self.1.retain(|(k, _)| map.contains_key(k));
            for (k, v) in &mut self.1 {
                v.update(&map[k]);
            }
            // Order of fields appended by Python's three rewriting operations.
            for key in [
                "textTruncated",
                "checks",
                "checksTruncated",
                "filesTruncated",
                "resultFile",
                "resultTruncated",
                "read",
                "earlierProgressUpdates",
                "history",
                "tool",
                "output_ref",
            ]
            .into_iter()
            .chain(map.keys().map(String::as_str))
            {
                if let Some(v) = map.get(key)
                    && !self.1.iter().any(|(k, _)| k == key)
                {
                    let mut ordered = Self(Value::Null, vec![], vec![]);
                    ordered.update(v);
                    self.1.push((key.into(), ordered));
                }
            }
        } else if let Value::Array(values) = value {
            self.2.truncate(values.len());
            for (i, v) in values.iter().enumerate() {
                if i == self.2.len() {
                    self.2.push(Self(Value::Null, vec![], vec![]));
                }
                self.2[i].update(v);
            }
        }
        self.0 = match value {
            Value::Object(_) => json!({}),
            Value::Array(_) => json!([]),
            v => v.clone(),
        };
    }
}
fn rewritten(text: &str, value: &Value, replace_read: bool) -> String {
    // Called only for successfully parsed event JSON.
    match serde_json::from_str::<OrderedJson>(text) {
        Ok(mut ordered) => {
            if replace_read && let Some((_, read)) = ordered.1.iter_mut().find(|(k, _)| k == "read")
            {
                *read = OrderedJson(Value::Null, vec![], vec![]);
            }
            ordered.update(value);
            serde_json::to_string(&ordered).unwrap_or_else(|_| packed(value))
        }
        Err(_) => packed(value),
    }
}

fn truncate_review(result: &mut Value) {
    let checks = result["checks"].as_str().unwrap_or("");
    let truncated = checks.chars().count() > CHECKS_CHARS;
    result["checks"] = json!(chars(checks, CHECKS_CHARS));
    result["checksTruncated"] = json!(truncated);
    if let Some(files) = result.get("files").and_then(Value::as_array)
        && packed(&json!(files)).len() > FILES_BYTES
    {
        let names: Vec<_> = files
            .iter()
            .take(ATTACHMENTS)
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
            text = rewritten(&text, &value, replace_read);
        } else if row.kind == "work_review"
            && result.is_object()
            && path.is_some()
            && packed(&value).len() > limit
        {
            let result = &mut value["result"];
            truncate_review(result);
            text = rewritten(&text, &value, false);
        }
    }
    if text.len() <= limit {
        return text;
    }
    let mut summary = json!({"eventId":row.id,"kind":row.kind,"truncated":true,"textBytes":row.text.len(),"read":{"tool":"orchestration_read","output_ref":format!("event:{}",row.id)}});
    if let Ok(value) = serde_json::from_str::<Value>(&text)
        && value.is_object()
    {
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
        if packed(&summary).len() > limit
            && let Some(map) = summary.as_object_mut()
        {
            map.remove(key);
        }
    }
    let remaining = limit.saturating_sub(packed(&summary).len() + SUMMARY_RESERVE);
    let mut excerpt = byte_prefix(&text, remaining / 2).to_owned();
    let mut tail = byte_tail(&text, remaining / 2).to_owned();
    loop {
        summary["excerpt"] = json!(excerpt);
        summary["tail"] = json!(tail);
        if packed(&summary).len() <= limit || (excerpt.is_empty() && tail.is_empty()) {
            break;
        }
        excerpt = chars(&excerpt, excerpt.chars().count() / 2);
        tail = if tail.chars().count() > 1 {
            tail.chars().skip(tail.chars().count() / 2).collect()
        } else {
            String::new()
        };
    }
    packed(&summary)
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
fn event_text(
    s: &PreparationSnapshot,
    rows: &[Event],
) -> Result<(String, Vec<ArtifactIntent>), PrepareError> {
    let parsed: Vec<_> = rows
        .iter()
        .map(|r| serde_json::from_str::<Value>(&r.text).ok())
        .collect();
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
    let synthetic = rows
        .iter()
        .filter(|r| !matches!(r.kind.as_str(), "user" | "followup"))
        .count();
    let limit = EVENT_BYTES.min(BATCH_BYTES / synthetic.max(1));
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
            && let Some(mut value) = parsed.clone()
            && let Some(body) = value["result"].as_str().filter(|v| v.len() > RESULT_BYTES)
            && !value["resultFile"].as_str().is_some_and(|p| !p.is_empty())
        {
            let intent = artifact(row, &value, body);
            if let Some(path) = s.artifact_paths.get(&row.id).filter(|p| !p.is_empty()) {
                value["resultFile"] = json!(path);
                text = rewritten(&text, &value, false);
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
                text = rewritten(&text, &value, false);
            }
        }
        if !matches!(
            row.kind.as_str(),
            "user" | "followup" | "work_decision" | "radio_turn"
        ) {
            text = bound_event(row, &text, limit);
        }
        parts.push(if row.kind == "user" {
            text
        } else {
            format!("[Orchestration event: {}]\n{text}", row.kind)
        });
    }
    if !missing.is_empty() {
        return Err(PrepareError::ArtifactsRequired(missing));
    }
    Ok((parts.join("\n\n"), artifacts))
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
    if rows.iter().map(|r| r.metadata.assets.len()).sum::<usize>() > ATTACHMENTS {
        return Err(PrepareError::TooManyAttachments);
    }
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
                }
            } else {
                ProviderInput::Text {
                    text: format!(
                        "Attached file: {}\nRead the file at {} ({} bytes).",
                        a.name, a.path, a.size
                    ),
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
    let (mut text, artifact_intents) = event_text(s, rows)?;
    let (context, manifest) = context(s)?;
    text.push_str(&context);
    if text.is_empty() {
        text = "[Complaint update] No complaints require a response.".into();
    }
    let item_id = format!("{}:{}", s.actor.id, first.id);
    let transcript_text = chars(&text, TRANSCRIPT_CHARS);
    let truncated = transcript_text.len() < text.len();
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
    let fulltext = truncated.then(|| text.clone());
    if !clocks.is_empty() {
        text.push_str("\n\n[Time awareness, message receipt time]\n");
        text.push_str(&clocks.join("\n"));
    }
    let mut provider_inputs = vec![ProviderInput::Text { text }];
    provider_inputs.extend(provider_assets);
    let mut metadata_patches = BTreeMap::new();
    for row in rows {
        let mut patch = Map::from_iter([("modelEventProjection".into(), json!(1))]);
        if !row.id.is_empty() {
            patch.insert("transcriptItemId".into(), json!(item_id));
        }
        if row.id == first.id {
            patch.insert("contextManifest".into(), json!(manifest));
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
