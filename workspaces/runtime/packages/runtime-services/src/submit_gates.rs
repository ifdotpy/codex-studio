//! Pre-submit decisions only. The account worker must re-read these snapshots and
//! apply a freeze patch atomically with its exact attempt/epoch check. Refusals
//! never consume queued input. Paths below are already resolved by the caller.

use crate::event_write::truthy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct AgentAuthority {
    pub id: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub is_lead: bool,
    pub deleted: bool,
    pub resolved_cwd: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RoomAuthority {
    Broadcast {
        root_id: Option<String>,
    },
    Private {
        members: Vec<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ComplaintAuthority {
    pub author: Option<String>,
    pub lead_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AuthorizationSnapshot {
    recipient_id: String,
    agents: BTreeMap<String, AgentAuthority>,
    rooms: BTreeMap<String, RoomAuthority>,
    complaints: BTreeMap<String, ComplaintAuthority>,
    projects: BTreeMap<String, Vec<Vec<String>>>,
    events: Vec<EventSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EventSnapshot {
    AgentMessage {
        sender: Option<String>,
        room: Option<String>,
    },
    Followup {
        sender_present: bool,
        sender: Option<String>,
    },
    ComplaintResponse {
        complaint_id: Option<String>,
        responder: Option<String>,
        response_author: Option<String>,
    },
    ChatReview,
    RadioTurn,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, Error, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EventDenial {
    #[error("team-isolation-policy/recipient-unavailable")]
    RecipientUnavailable,
    #[error("team-isolation-policy/room-unavailable")]
    RoomUnavailable,
    #[error("team-isolation-policy/broadcast-disabled")]
    BroadcastDisabled,
    #[error("team-isolation-policy/room-boundary")]
    RoomBoundary,
    #[error("team-isolation-policy/complaint-unavailable")]
    ComplaintUnavailable,
    #[error("team-isolation-policy/complaint-recipient")]
    ComplaintRecipient,
    #[error("team-isolation-policy/complaint-responder-unavailable")]
    ComplaintResponderUnavailable,
    #[error("team-isolation-policy/complaint-responder")]
    ComplaintResponder,
    #[error("team-isolation-policy/team-boundary")]
    TeamBoundary,
    #[error("team-isolation-policy/chat-review-removed")]
    ChatReviewRemoved,
    #[error("radio-policy/unsupported")]
    RadioUnsupported,
}

impl AuthorizationSnapshot {
    fn same_team(&self, root: &str, sender: Option<&str>) -> bool {
        sender
            .and_then(|id| self.agents.get(id))
            .is_some_and(|agent| agent.root_id.as_deref() == Some(root))
    }

    fn peer_pair(&self, left: &str, right: &str) -> bool {
        if left == right {
            return false;
        }
        let (Some(left), Some(right)) = (self.agents.get(left), self.agents.get(right)) else {
            return false;
        };
        let Some(path) = left.resolved_cwd.as_deref().filter(|p| !p.is_empty()) else {
            return false;
        };
        let lead = |agent: &AgentAuthority| {
            agent.is_lead
                && !agent.deleted
                && agent.parent_id.is_none()
                && agent.root_id.as_ref() == Some(&agent.id)
                && agent.resolved_cwd.as_deref() == Some(path)
        };
        lead(left)
            && lead(right)
            && self.projects.get(path).is_some_and(|teams| {
                teams
                    .iter()
                    .any(|team| team.contains(&left.id) && team.contains(&right.id))
            })
    }

    fn authorize(&self, event: &EventSnapshot) -> Result<(), EventDenial> {
        use EventDenial as D;
        match event {
            EventSnapshot::Other => return Ok(()),
            EventSnapshot::ChatReview => return Err(D::ChatReviewRemoved),
            EventSnapshot::RadioTurn => return Err(D::RadioUnsupported),
            _ => {}
        }
        let recipient = self
            .agents
            .get(&self.recipient_id)
            .ok_or(D::RecipientUnavailable)?;
        let root = recipient
            .root_id
            .as_deref()
            .filter(|r| !r.is_empty())
            .ok_or(D::RecipientUnavailable)?;
        let sender = match event {
            EventSnapshot::Followup {
                sender_present,
                sender,
            } => {
                if !sender_present {
                    return Ok(());
                }
                sender.as_deref()
            }
            EventSnapshot::AgentMessage { sender, room } => {
                let room = room
                    .as_ref()
                    .and_then(|id| self.rooms.get(id))
                    .ok_or(D::RoomUnavailable)?;
                match room {
                    RoomAuthority::Broadcast { root_id } => {
                        if root_id.as_deref() != Some(root) || root == "all" {
                            return Err(D::BroadcastDisabled);
                        }
                    }
                    RoomAuthority::Private { members } => {
                        if !sender.as_ref().is_some_and(|s| members.contains(s))
                            || !members.contains(&recipient.id)
                            || !(members.iter().all(|id| self.same_team(root, Some(id)))
                                || (members.len() == 2 && self.peer_pair(&members[0], &members[1])))
                        {
                            return Err(D::RoomBoundary);
                        }
                    }
                    RoomAuthority::Other => return Err(D::RoomUnavailable),
                }
                sender.as_deref()
            }
            EventSnapshot::ComplaintResponse {
                complaint_id,
                responder,
                response_author,
            } => {
                let id = complaint_id.as_ref().ok_or(D::ComplaintUnavailable)?;
                let complaint = self
                    .complaints
                    .get(id)
                    .filter(|c| c.author.as_ref() == Some(&recipient.id))
                    .ok_or(D::ComplaintRecipient)?;
                if response_author.is_none() || response_author != responder {
                    return Err(D::ComplaintResponderUnavailable);
                }
                if responder.as_deref() == Some("user") {
                    return Ok(());
                }
                if complaint.lead_id != *responder {
                    return Err(D::ComplaintResponder);
                }
                responder.as_deref()
            }
            _ => return Ok(()),
        };
        if self.same_team(root, sender)
            || (matches!(event, EventSnapshot::AgentMessage { .. })
                && sender.is_some_and(|sender| self.peer_pair(&recipient.id, sender)))
        {
            Ok(())
        } else {
            Err(D::TeamBoundary)
        }
    }
}

/// All-or-nothing, in event order. Re-run against fresh authority before submission.
pub fn authorize_events(snapshot: &AuthorizationSnapshot) -> Result<(), EventDenial> {
    snapshot
        .events
        .iter()
        .try_for_each(|event| snapshot.authorize(event))
}

/// Persisted SQLite JSON text, including malformed rows, crosses this boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthorizationRecords {
    pub recipient_id: String,
    pub agents: BTreeMap<String, String>,
    pub rooms: BTreeMap<String, String>,
    pub complaints: BTreeMap<String, String>,
    pub projects: BTreeMap<String, String>,
    pub event_meta: BTreeMap<String, String>,
    pub events: Vec<StoredEvent>,
    /// Results of the caller's expanduser/resolve step, keyed by original path.
    /// Canonicalization is an observation, not filesystem I/O inside a gate.
    pub resolved_paths: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredEvent {
    pub id: String,
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("invalid stored gate input at {location}")]
pub struct SnapshotError {
    pub location: String,
}

fn invalid(location: impl Into<String>) -> SnapshotError {
    SnapshotError {
        location: location.into(),
    }
}
fn object(raw: &str, location: &str) -> Result<Value, SnapshotError> {
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid(location))?;
    if !value.is_object() {
        return Err(invalid(location));
    }
    Ok(value)
}
fn optional_string(value: Option<&Value>, location: &str) -> Result<Option<String>, SnapshotError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(invalid(location)),
    }
}
fn strings(value: Option<&Value>, location: &str) -> Result<Vec<String>, SnapshotError> {
    serde_json::from_value(value.cloned().unwrap_or(Value::Null)).map_err(|_| invalid(location))
}
impl AuthorizationSnapshot {
    pub fn from_records(rows: &AuthorizationRecords) -> Result<Self, SnapshotError> {
        if rows.recipient_id.is_empty() {
            return Err(invalid("recipient_id"));
        }
        let resolve =
            |value: Option<&Value>, location: &str| -> Result<Option<String>, SnapshotError> {
                let Some(path) = optional_string(value, location)?.filter(|p| !p.trim().is_empty())
                else {
                    return Ok(None);
                };
                let resolved = rows
                    .resolved_paths
                    .get(&path)
                    .filter(|p| {
                        let path = std::path::Path::new(p);
                        path.is_absolute()
                            && path
                                .components()
                                .collect::<std::path::PathBuf>()
                                .as_os_str()
                                == p.as_str()
                            && !path
                                .components()
                                .any(|c| matches!(c, std::path::Component::ParentDir))
                    })
                    .ok_or_else(|| invalid(location))?;
                Ok(Some(resolved.clone()))
            };
        let mut snapshot = Self {
            recipient_id: rows.recipient_id.clone(),
            agents: BTreeMap::new(),
            rooms: BTreeMap::new(),
            complaints: BTreeMap::new(),
            projects: BTreeMap::new(),
            events: Vec::new(),
        };
        for (key, raw) in &rows.agents {
            let location = format!("agents/{key}");
            let row = object(raw, &location)?;
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id == key)
                .ok_or_else(|| invalid(format!("{location}/id")))?;
            snapshot.agents.insert(
                key.clone(),
                AgentAuthority {
                    id: id.into(),
                    root_id: optional_string(row.get("rootId"), &format!("{location}/rootId"))?,
                    parent_id: optional_string(
                        row.get("parentId"),
                        &format!("{location}/parentId"),
                    )?,
                    is_lead: row.get("isLead") == Some(&Value::Bool(true)),
                    deleted: truthy(row.get("deletedAt")),
                    resolved_cwd: resolve(row.get("cwd"), &format!("{location}/cwd"))?,
                },
            );
        }
        for (key, raw) in &rows.rooms {
            let location = format!("rooms/{key}");
            let row = object(raw, &location)?;
            let room = match row.get("kind").and_then(Value::as_str) {
                Some("broadcast") => RoomAuthority::Broadcast {
                    root_id: optional_string(row.get("rootId"), &format!("{location}/rootId"))?,
                },
                Some("private") => RoomAuthority::Private {
                    members: strings(row.get("members"), &format!("{location}/members"))?,
                },
                _ => RoomAuthority::Other,
            };
            snapshot.rooms.insert(key.clone(), room);
        }
        for (key, raw) in &rows.complaints {
            let location = format!("complaints/{key}");
            let row = object(raw, &location)?;
            snapshot.complaints.insert(
                key.clone(),
                ComplaintAuthority {
                    author: optional_string(row.get("author"), &format!("{location}/author"))?,
                    lead_id: optional_string(row.get("leadId"), &format!("{location}/leadId"))?,
                },
            );
        }
        for (key, raw) in &rows.projects {
            let location = format!("projects/{key}");
            let row = object(raw, &location)?;
            let path = resolve(row.get("path"), &format!("{location}/path"))?
                .ok_or_else(|| invalid(&location))?;
            let mut teams = Vec::new();
            if let Some(value) = row.get("peerTeams") {
                for (index, team) in value
                    .as_array()
                    .ok_or_else(|| invalid(format!("{location}/peerTeams")))?
                    .iter()
                    .enumerate()
                {
                    let at = format!("{location}/peerTeams/{index}");
                    for field in ["id", "name"] {
                        if !team
                            .get(field)
                            .and_then(Value::as_str)
                            .is_some_and(|v| !v.is_empty())
                        {
                            return Err(invalid(format!("{at}/{field}")));
                        }
                    }
                    let members = strings(team.get("members"), &format!("{at}/members"))?;
                    if members.len() < 2
                        || members.iter().any(String::is_empty)
                        || members
                            .iter()
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            != members.len()
                    {
                        return Err(invalid(format!("{at}/members")));
                    }
                    teams.push(members);
                }
            }
            if path == *key {
                snapshot.projects.insert(key.clone(), teams);
            }
        }
        for event in &rows.events {
            // Python treats invalid/non-object event payloads as empty objects.
            let payload = serde_json::from_str::<Value>(&event.text)
                .ok()
                .filter(Value::is_object)
                .unwrap_or(json!({}));
            let string = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
            let projected = match event.kind.as_str() {
                "agent_message" => EventSnapshot::AgentMessage {
                    sender: string("sender"),
                    room: string("room"),
                },
                "followup" => {
                    let metadata = rows
                        .event_meta
                        .get(&event.id)
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .filter(Value::is_object)
                        .unwrap_or(json!({}));
                    EventSnapshot::Followup {
                        sender_present: metadata.get("senderId").is_some(),
                        sender: metadata
                            .get("senderId")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    }
                }
                "complaint_response" => EventSnapshot::ComplaintResponse {
                    complaint_id: string("complaint_id"),
                    responder: payload
                        .get("responder")
                        .or_else(|| payload.get("lead"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    response_author: payload
                        .get("response")
                        .and_then(|v| v.get("author"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                "chat_review" => EventSnapshot::ChatReview,
                "radio_turn" => EventSnapshot::RadioTurn,
                _ => EventSnapshot::Other,
            };
            snapshot.events.push(projected);
        }
        Ok(snapshot)
    }
}

/// Missing and explicit null are distinct persisted states (notably account pins).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StoredField<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}
impl<T> StoredField<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
    fn value_or<'a>(&'a self, default: Option<&'a T>) -> Option<&'a T> {
        match self {
            Self::Missing => default,
            Self::Null => None,
            Self::Value(value) => Some(value),
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for StoredField<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<T>::deserialize(deserializer)?.map_or(Self::Null, Self::Value))
    }
}
impl<T: Serialize> Serialize for StoredField<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Value(value) => value.serialize(serializer),
            _ => serializer.serialize_none(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionSettings {
    #[serde(default)]
    pub model: StoredField<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "StoredField::is_missing")]
    pub native_effort: StoredField<String>,
    #[serde(default = "default_false")]
    pub fast_mode: Option<bool>,
    #[serde(default = "default_false")]
    pub daybreak_enabled: Option<bool>,
}

fn default_false() -> Option<bool> {
    Some(false)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CatalogIdentity {
    pub account_key: String,
    pub home: String,
    pub provider: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSettings {
    stored: serde_json::Map<String, Value>,
    settings: ExecutionSettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionSnapshot {
    pub account: CatalogIdentity,
    pub execution_settings_account_key: StoredField<String>,
    pub settings: ExecutionSettings,
    pub pending: Option<PendingSettings>,
    pub pending_account_key: StoredField<String>,
    pub settings_fixed: bool,
}
impl ExecutionSnapshot {
    /// Checked projection of the persisted fields used by the freeze decision.
    pub fn from_record(record: &Value, account: CatalogIdentity) -> Result<Self, SnapshotError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Record {
            #[serde(default = "default_account")]
            account_key: String,
            #[serde(default)]
            execution_settings_account_key: StoredField<String>,
            #[serde(flatten)]
            settings: ExecutionSettings,
            #[serde(default)]
            pending_settings: Option<Value>,
            #[serde(default)]
            pending_settings_account_key: StoredField<String>,
        }
        fn default_account() -> String {
            "default".into()
        }
        let row: Record = serde_json::from_value(record.clone())
            .map_err(|_| invalid("agent/executionSettings"))?;
        if row.account_key != account.account_key {
            return Err(invalid("agent/accountKey"));
        }
        let pending = row
            .pending_settings
            .filter(|v| truthy(Some(v)))
            .map(|v| {
                let stored = v
                    .as_object()
                    .ok_or_else(|| invalid("agent/pendingSettings"))?
                    .clone();
                let mut merged = record.clone();
                merged
                    .as_object_mut()
                    .ok_or_else(|| invalid("agent"))?
                    .extend(stored.clone());
                let settings =
                    serde_json::from_value(merged).map_err(|_| invalid("agent/pendingSettings"))?;
                Ok(PendingSettings { stored, settings })
            })
            .transpose()?;
        let settings_fixed = record
            .get("startAttempt")
            .and_then(|v| v.get("settingsFixed"));
        Ok(Self {
            account,
            execution_settings_account_key: row.execution_settings_account_key,
            settings: row.settings,
            pending,
            pending_account_key: row.pending_settings_account_key,
            settings_fixed: truthy(settings_fixed),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub model: String,
    pub resolved_model: Option<String>,
    pub hidden: bool,
    pub supported_efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub priority: bool,
}

/// Preserve the full read identity through projection and the commit check.
#[derive(Clone, Debug)]
pub struct CatalogObservation {
    pub identity: CatalogIdentity,
    pub expires_at: Instant,
    pub models: Vec<ModelCapabilities>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreezePatch {
    pub execution_settings_account_key: StoredField<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionSelection {
    pub expected: ExecutionSnapshot,
    pub settings: ExecutionSettings,
    pub freeze: Option<FreezePatch>,
    /// Must still match the cache/profile identity at commit, even on fixed retries.
    pub catalog_precondition: Option<(CatalogIdentity, Instant)>,
}

impl ExecutionSelection {
    /// Apply to a cloned stored record, never a database. The caller still owns
    /// the attempt/epoch check and atomic write. Removal and settingsFixed follow
    /// from the patch; duplicated flags cannot contradict the selected settings.
    pub fn freeze_record(
        &self,
        record: &Value,
        account: &CatalogIdentity,
        now: Instant,
    ) -> Result<Value, ExecutionDenial> {
        if self
            .catalog_precondition
            .as_ref()
            .is_some_and(|(expected, _)| expected != account)
        {
            return Err(ExecutionDenial::CatalogIdentityMismatch);
        }
        if self
            .catalog_precondition
            .as_ref()
            .is_some_and(|(_, expires)| *expires <= now)
        {
            return Err(ExecutionDenial::CatalogExpired);
        }
        let mut current = ExecutionSnapshot::from_record(record, account.clone())
            .map_err(|_| ExecutionDenial::SettingsChanged)?;
        if self.catalog_precondition.is_none() {
            current.account.generation = self.expected.account.generation;
        }
        if current != self.expected {
            return Err(ExecutionDenial::SettingsChanged);
        }
        let Some(patch) = &self.freeze else {
            return Ok(record.clone());
        };
        let mut result = record.clone();
        let object = result
            .as_object_mut()
            .ok_or(ExecutionDenial::SettingsChanged)?;
        if let Some(pending) = &self.expected.pending {
            object.extend(pending.stored.clone());
            object.remove("pendingSettings");
            object.remove("pendingSettingsAccountKey");
        } else if patch.execution_settings_account_key
            != self.expected.execution_settings_account_key
        {
            object.insert("effort".into(), json!(self.settings.effort));
            object.insert("nativeEffort".into(), json!(self.settings.native_effort));
        }
        match &patch.execution_settings_account_key {
            StoredField::Missing => {}
            StoredField::Null => {
                object.insert("executionSettingsAccountKey".into(), Value::Null);
            }
            StoredField::Value(key) => {
                object.insert("executionSettingsAccountKey".into(), json!(key));
            }
        }
        let attempt = object.entry("startAttempt").or_insert_with(|| json!({}));
        attempt
            .as_object_mut()
            .ok_or(ExecutionDenial::SettingsChanged)?
            .insert("settingsFixed".into(), Value::Bool(true));
        Ok(result)
    }

    /// Immediate frozen-record projection. Preparation must resolve a missing
    /// native effort before the consumer computes the final provider parameters.
    pub fn provider_settings(&self) -> Value {
        let mut params = json!({"serviceTier": if self.settings.fast_mode == Some(true) { "priority" } else { "default" }});
        if let Some(effort) = self
            .settings
            .native_effort
            .value_or(self.settings.effort.as_ref())
        {
            params["effort"] = json!(effort);
        }
        params
    }
}

#[derive(Clone, Copy, Debug, Error, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionDenial {
    #[error("execution-settings/account-changed")]
    AccountChanged,
    #[error("execution-settings/settings-changed")]
    SettingsChanged,
    #[error("model-catalog/fresh-observation-required")]
    CatalogRequired,
    #[error("model-catalog/identity-mismatch")]
    CatalogIdentityMismatch,
    #[error("model-catalog/expired")]
    CatalogExpired,
    #[error("execution-settings/model-unavailable")]
    ModelUnavailable,
    #[error("execution-settings/effort-unsupported")]
    EffortUnsupported,
    #[error("execution-settings/fast-mode-unsupported")]
    FastModeUnsupported,
    #[error("daybreak-policy/unsupported")]
    DaybreakUnsupported,
}

/// The same validation used when saving settings; model-change callers may opt
/// into falling back to the provider default for an unsupported explicit effort.
pub fn validate_execution(
    settings: &ExecutionSettings,
    catalog: &CatalogObservation,
    fallback_effort: bool,
) -> Result<(Option<String>, Option<String>), ExecutionDenial> {
    use ExecutionDenial as D;
    let model = settings
        .model
        .value_or(None)
        .map(String::as_str)
        .ok_or(D::ModelUnavailable)?;
    let info = catalog
        .models
        .iter()
        .find(|row| {
            !row.hidden
                && !model.trim().is_empty()
                && (row.model == model || row.resolved_model.as_deref() == Some(model))
        })
        .ok_or(D::ModelUnavailable)?;
    let mut effort = settings.effort.clone();
    if effort.as_ref().is_some_and(|v| v.trim().is_empty()) {
        return Err(D::EffortUnsupported);
    }
    if effort
        .as_ref()
        .is_some_and(|v| !info.supported_efforts.contains(v))
    {
        if fallback_effort {
            effort = None;
        } else {
            return Err(D::EffortUnsupported);
        }
    }
    if settings.fast_mode == Some(true) && !info.priority {
        return Err(D::FastModeUnsupported);
    }
    Ok((
        effort.clone(),
        effort.or_else(|| info.default_effort.clone()),
    ))
}

/// Pending settings were validated when saved: start applies them without a
/// catalog. Only account changes without pending settings require a fresh read.
pub fn select_execution(
    current: &ExecutionSnapshot,
    observed: &ExecutionSnapshot,
    catalog: Option<&CatalogObservation>,
    now: Instant,
) -> Result<ExecutionSelection, ExecutionDenial> {
    use ExecutionDenial as D;
    let mut settings = current.settings.clone();
    let mut account = current.execution_settings_account_key.clone();
    let mut catalog_precondition = None;
    let revalidated = if observed
        .execution_settings_account_key
        .value_or(Some(&observed.account.account_key))
        != Some(&observed.account.account_key)
        && observed.pending.is_none()
    {
        let catalog = catalog.ok_or(D::CatalogRequired)?;
        if catalog.identity != observed.account || current.account != observed.account {
            return Err(D::CatalogIdentityMismatch);
        }
        if catalog.expires_at <= now {
            return Err(D::CatalogExpired);
        }
        catalog_precondition = Some((catalog.identity.clone(), catalog.expires_at));
        Some(validate_execution(&observed.settings, catalog, false)?)
    } else {
        None
    };
    if !current.settings_fixed {
        if let Some(pending) = &current.pending {
            if current
                .pending_account_key
                .value_or(Some(&current.account.account_key))
                != Some(&current.account.account_key)
            {
                return Err(D::AccountChanged);
            }
            settings = pending.settings.clone();
            account = StoredField::Value(current.account.account_key.clone());
        } else if account.value_or(Some(&current.account.account_key))
            != Some(&current.account.account_key)
        {
            if observed.account != current.account
                || observed.settings.model != current.settings.model
                || observed.settings.effort != current.settings.effort
                || observed.settings.fast_mode != current.settings.fast_mode
            {
                return Err(D::SettingsChanged);
            }
            let (effort, native_effort) = revalidated.ok_or(D::SettingsChanged)?;
            settings.effort = effort;
            settings.native_effort = native_effort.map_or(StoredField::Null, StoredField::Value);
            account = StoredField::Value(current.account.account_key.clone());
        }
    }
    if settings.model.is_missing() {
        return Err(D::ModelUnavailable);
    }
    if settings.daybreak_enabled != Some(false) {
        return Err(D::DaybreakUnsupported);
    }
    let freeze = (!current.settings_fixed).then_some(FreezePatch {
        execution_settings_account_key: account,
    });
    Ok(ExecutionSelection {
        expected: current.clone(),
        settings,
        freeze,
        catalog_precondition,
    })
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BudgetDenial {
    #[error("budget-policy/coverage-unavailable")]
    CoverageUnavailable,
    #[error("invalid stored gate input at agent/tokenBudget")]
    InvalidBudget,
}

/// Missing/null is unlimited. Every valid present budget is held; every invalid
/// present value is refused, never projected to the unlimited state.
pub fn budget_admission(token_budget: Option<&Value>) -> Result<(), BudgetDenial> {
    match token_budget {
        None | Some(Value::Null) => Ok(()),
        Some(value) if value.as_u64().is_some() => Err(BudgetDenial::CoverageUnavailable),
        Some(_) => Err(BudgetDenial::InvalidBudget),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peer_pair_requires_distinct_checked_leads() {
        let records: AuthorizationRecords = serde_json::from_value(json!({
            "recipient_id":"left", "agents": {
                "left":json!({"id":"left","rootId":"left","isLead":true,"cwd":"/project"}).to_string(),
                "right":json!({"id":"right","rootId":"right","isLead":true,"cwd":"/project"}).to_string()
            }, "projects":{"/project":json!({"path":"/project","peerTeams":[{"id":"peers","name":"Peers","members":["left","right"]}]}).to_string()},
            "rooms":{},"complaints":{},"event_meta":{},"events":[],"resolved_paths":{"/project":"/project"}
        })).expect("raw rows");
        let snapshot = AuthorizationSnapshot::from_records(&records).expect("checked rows");
        assert!(snapshot.peer_pair("left", "right"));
        assert!(!snapshot.peer_pair("left", "left"));
    }
}
