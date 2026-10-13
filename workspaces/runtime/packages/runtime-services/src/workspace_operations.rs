//! Pure decisions for committed-turn checkpoints and recoverable workspace operations.
//! Constructors accept stored records; decisions never read a clock, database or path.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_store::truthy;

use studio_runtime_store::workspace_operations::MAX_CHECKPOINT_LINEAGE;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceOperationError {
    #[error("Invalid stored object {object}/{id} at {pointer}")]
    Invalid {
        object: &'static str,
        id: String,
        pointer: String,
    },
    #[error("Checkpoint history lineage contains a cycle")]
    HistoryCycle,
    #[error("Checkpoint history parent is missing")]
    MissingParent,
    #[error("Checkpoint history exceeds the supported bound")]
    HistoryLimit,
    #[error("Checkpoint history payload requires payload-store/checkpoint-history")]
    HistoryPayload,
    #[error("Workspace operation source changed")]
    SourceChanged,
    #[error("This request id has different content")]
    RequestChanged,
    #[error("Workspace recovery is required before retrying this operation")]
    RecoveryRequired,
    #[error("Workspace provider receipt does not match the reserved request")]
    ForeignReceipt,
}

type Result<T> = std::result::Result<T, WorkspaceOperationError>;

fn decode<T: serde::de::DeserializeOwned>(schema: &'static str, value: &Value) -> Result<T> {
    let invalid = |pointer: String| WorkspaceOperationError::Invalid {
        object: schema,
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .into(),
        pointer,
    };
    if let Some((pointer, _)) =
        studio_runtime_schema::first_error(schema, value).map_err(|_| invalid(String::new()))?
    {
        return Err(invalid(pointer));
    }
    serde_json::from_value(value.clone()).map_err(|_| invalid(String::new()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompletionIdentity {
    id: String,
    epoch: u64,
    cwd: String,
    #[serde(default = "default_account")]
    account_key: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    last_completed_turn: Option<String>,
    #[serde(default)]
    start_attempt: Option<Value>,
}
fn default_account() -> String {
    "default".into()
}

/// Captured by the turn consumer before releasing its committed completion.
/// Private fields prevent a caller from substituting just the current epoch.
#[derive(Clone, Debug)]
pub struct CompletedTurn {
    identity: CompletionIdentity,
    attempt_id: Option<Value>,
}
impl CompletedTurn {
    pub fn from_record(agent: &Value) -> Result<Self> {
        let identity: CompletionIdentity = decode("WorkspaceCheckpointAgentRecord", agent)?;
        let attempt_id = identity
            .start_attempt
            .as_ref()
            .and_then(|v| v.get("id"))
            .cloned();
        Ok(Self {
            identity,
            attempt_id,
        })
    }
    pub fn agent_id(&self) -> &str {
        &self.identity.id
    }
    pub fn turn_id(&self) -> Option<&str> {
        self.identity.last_completed_turn.as_deref()
    }
    pub fn checkpoint_id(&self) -> String {
        // One completion has one durable checkpoint identity, including its source generation.
        let identity = json!([
            self.identity.id,
            self.identity.epoch,
            self.identity.cwd,
            self.identity.account_key,
            self.identity.thread_id,
            self.identity.last_completed_turn,
            self.attempt_id
        ]);
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.to_string().as_bytes()).to_string()
    }
}

#[derive(Clone, Debug)]
pub struct CompletionSnapshot {
    identity: CompletionIdentity,
    record: Value,
    committed: bool,
}
impl CompletionSnapshot {
    pub fn from_record(record: &Value, committed: bool) -> Result<Self> {
        Ok(Self {
            identity: decode("WorkspaceCheckpointAgentRecord", record)?,
            record: record.clone(),
            committed,
        })
    }
    pub fn eligible(&self, request: &CompletedTurn) -> bool {
        let a = &self.record;
        let identity = &self.identity;
        self.committed
            && identity.id == request.identity.id
            && identity.epoch == request.identity.epoch
            && identity.cwd == request.identity.cwd
            && identity.account_key == request.identity.account_key
            && identity.thread_id == request.identity.thread_id
            && identity.last_completed_turn == request.identity.last_completed_turn
            && identity.start_attempt.as_ref().and_then(|v| v.get("id"))
                == request.attempt_id.as_ref()
            && !truthy(a.get("inFlight"))
            && !truthy(a.get("turnId"))
            && !truthy(a.get("workspaceOperation"))
            && truthy(a.get("autoWake"))
            && !matches!(
                a.get("status").and_then(Value::as_str),
                Some("paused" | "starting")
            )
            && !truthy(a.get("deletedAt"))
            && !truthy(a.get("agentArchive"))
            && !truthy(a.get("imageWorkspace"))
            && truthy(a.get("worktreeReady"))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord {
    id: String,
    agent: String,
    #[serde(default)]
    created: f64,
    #[serde(default)]
    history_parent: Option<String>,
    items: Option<Vec<String>>,
    history_delta: Option<Vec<String>>,
}

/// A resolved lineage and the legacy created-time fallback supplied by the repository.
pub struct CheckpointHistory {
    selected: HistoryRecord,
    records: BTreeMap<String, HistoryRecord>,
    fallback: Vec<(String, f64)>,
}
impl CheckpointHistory {
    pub fn from_records(
        selected: &Value,
        parents: &[Value],
        fallback: Vec<(String, f64)>,
    ) -> Result<Self> {
        let mut records = BTreeMap::new();
        for raw in std::iter::once(selected).chain(parents) {
            if raw
                .get("_payloadBlobs")
                .and_then(Value::as_object)
                .is_some_and(|v| v.contains_key("items") || v.contains_key("historyDelta"))
            {
                return Err(WorkspaceOperationError::HistoryPayload);
            }
            let record: HistoryRecord = decode("WorkspaceCheckpointHistoryRecord", raw)?;
            if records.insert(record.id.clone(), record).is_some() {
                return Err(WorkspaceOperationError::HistoryCycle);
            }
        }
        Ok(Self {
            selected: decode("WorkspaceCheckpointHistoryRecord", selected)?,
            records,
            fallback,
        })
    }
    pub fn visible_ids(&self) -> Result<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut current = &self.selected;
        loop {
            if !seen.insert(&current.id) {
                return Err(WorkspaceOperationError::HistoryCycle);
            }
            if seen.len() > MAX_CHECKPOINT_LINEAGE {
                return Err(WorkspaceOperationError::HistoryLimit);
            }
            if current.agent != self.selected.agent {
                return Err(WorkspaceOperationError::SourceChanged);
            }
            if let Some(delta) = current.items.as_ref().or(current.history_delta.as_ref()) {
                ids.extend(delta.iter().cloned());
            } else {
                ids.extend(
                    self.fallback
                        .iter()
                        .filter(|(_, created)| *created <= self.selected.created)
                        .map(|(id, _)| id.clone()),
                );
            }
            let Some(parent) = current.history_parent.as_ref().filter(|s| !s.is_empty()) else {
                break;
            };
            current = self
                .records
                .get(parent)
                .ok_or(WorkspaceOperationError::MissingParent)?;
        }
        Ok(ids)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Capture,
    Checkpoint,
    Restore,
    Branch,
    Provision,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    CapturePending,
    CaptureRunning,
    ProviderPending,
    ProviderReady,
    LocalMutation,
    RecoveryRequired,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize)]
struct OperationRecord {
    id: String,
    kind: OperationKind,
    agent: String,
    phase: OperationPhase,
    signature: Option<String>,
    source: Option<Value>,
    provider: Option<Value>,
    result: Option<Value>,
}

pub struct OperationSnapshot {
    operation: OperationRecord,
    raw: Value,
    agent: Value,
}
#[derive(Clone, Debug, PartialEq)]
pub enum RetryDecision {
    Completed(Value),
    Dispatch,
    ResumeLocal(Value),
    InspectWorkspace,
}
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryWrites {
    pub operation: Value,
    pub agent: Value,
}

impl OperationSnapshot {
    pub fn from_records(operation: &Value, agent: &Value) -> Result<Self> {
        let decoded: OperationRecord = decode("PersistedWorkspaceOperationRecord", operation)?;
        let actor: CompletionIdentity = decode("WorkspaceCheckpointAgentRecord", agent)?;
        if decoded.agent != actor.id {
            return Err(WorkspaceOperationError::SourceChanged);
        }
        Ok(Self {
            operation: decoded,
            raw: operation.clone(),
            agent: agent.clone(),
        })
    }
    pub fn retry(&self, signature: &str) -> Result<RetryDecision> {
        if self.operation.signature.as_deref() != Some(signature) {
            return Err(WorkspaceOperationError::RequestChanged);
        }
        if self.operation.phase == OperationPhase::Completed {
            return self
                .operation
                .result
                .clone()
                .map(RetryDecision::Completed)
                .ok_or(WorkspaceOperationError::RecoveryRequired);
        }
        let source = json!({"accountKey":self.agent.get("accountKey").cloned().unwrap_or(json!("default")),
            "threadId":self.agent.get("threadId"), "cwd":self.agent.get("cwd")});
        if self.operation.source.as_ref() != Some(&source) {
            return Err(WorkspaceOperationError::SourceChanged);
        }
        Ok(match self.operation.phase {
            OperationPhase::ProviderReady => match &self.operation.provider {
                Some(provider) => RetryDecision::ResumeLocal(provider.clone()),
                None => RetryDecision::InspectWorkspace,
            },
            OperationPhase::Failed => RetryDecision::Dispatch,
            _ => RetryDecision::InspectWorkspace,
        })
    }
    /// Restart never resends a provider write. Capture can release its own reservation;
    /// restore and branch keep an observable hold until an explicit recovery.
    pub fn recover(&self, now: f64) -> RecoveryWrites {
        let mut operation = self.raw.clone();
        let mut agent = self.agent.clone();
        if matches!(
            self.operation.phase,
            OperationPhase::Completed | OperationPhase::Failed
        ) {
            return RecoveryWrites { operation, agent };
        }
        if matches!(
            self.operation.kind,
            OperationKind::Capture | OperationKind::Checkpoint
        ) {
            operation["phase"] = json!("failed");
            operation["updated"] = json!(now);
            operation["error"] = json!("Server restarted before checkpoint capture finished");
            if agent.get("workspaceReservationId") == Some(&json!(self.operation.id)) {
                agent["workspaceOperation"] = Value::Null;
                if let Some(object) = agent.as_object_mut() {
                    object.remove("workspaceReservationId");
                }
                agent["checkpointError"] = operation["error"].clone();
            }
        } else {
            operation["restartHold"] = json!(true);
            if self.operation.kind == OperationKind::Restore
                || self.operation.phase == OperationPhase::ProviderPending
            {
                operation["phase"] = json!("recovery_required");
                operation["updated"] = json!(now);
                operation["error"] = json!(
                    "Server restarted during a workspace operation. Inspect the workspace before retrying."
                );
            }
            agent["workspaceOperation"] = json!(if self.operation.kind == OperationKind::Restore {
                "restore_recovery"
            } else {
                "branch"
            });
            agent["autoWake"] = json!(false);
            agent["status"] = json!("interrupted");
            agent["error"] = operation
                .get("error")
                .filter(|v| truthy(Some(v)))
                .cloned()
                .unwrap_or(json!(
                    "Workspace operation interrupted. Inspect files before continuing."
                ));
        }
        RecoveryWrites { operation, agent }
    }
}

/// Durable transport identity; response contents alone never authorize local mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTicket {
    pub operation: String,
    pub agent: String,
    pub epoch: u64,
    pub account: String,
    pub thread: Option<String>,
    pub connection: String,
    pub generation: u64,
    pub request: String,
}

pub fn accept_provider_result(
    expected: &ProviderTicket,
    receipt: &ProviderTicket,
    response: &Value,
) -> Result<Value> {
    if expected != receipt {
        return Err(WorkspaceOperationError::ForeignReceipt);
    }
    let thread = response
        .get("thread")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(WorkspaceOperationError::ForeignReceipt)?;
    if Some(thread) == expected.thread.as_deref() {
        return Err(WorkspaceOperationError::ForeignReceipt);
    }
    let mut result = json!({"thread":{"id":thread}});
    for field in [
        "sandbox",
        "approvalPolicy",
        "activePermissionProfile",
        "model",
    ] {
        if let Some(value) = response.get(field) {
            result[field] = value.clone();
        }
    }
    Ok(result)
}
