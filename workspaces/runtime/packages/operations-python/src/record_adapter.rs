//! Durable Studio request records at the boundary to the pure lifecycle core.

use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use studio_operations::{
    AccountId, AccountThreadEpoch, AgentId, AttemptIdentity, ContentFingerprint, Event, EventId,
    EventKind, IdentityScope, Operation, OperationIdentity, OperationState, ProcessGeneration,
    RequestId, State, StateRevision, ThreadId, TransitionError, transition,
};

const DIFFERENT_CONTENT: &str = "This request id has different content";
const CONFLICTING_EVIDENCE: &str = "Conflicting evidence for tool request";
const STALE_REVISION: &str = "This request receipt changed while applying its lifecycle transition";
const STALE_EPOCH: &str = "This request belongs to an older account/thread epoch";
const STALE_GENERATION: &str = "This request belongs to an older process generation";
const GENERATION_UNAVAILABLE: &str = "The request has no captured process generation";

fn deserialize_nonnegative_integer<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_u64())
}

fn deserialize_supervisor<'de, D>(deserializer: D) -> Result<Option<SupervisorRecord>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

#[derive(Clone, Debug)]
enum StoredStage {
    Queued,
    Running,
    Cancelled,
    Interrupted,
    Failed,
    Unknown,
    Completed,
    Other(String),
}

impl StoredStage {
    fn name(&self) -> &str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Completed => "completed",
            Self::Other(value) => value,
        }
    }
}

impl<'de> Deserialize<'de> for StoredStage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            "failed" => Self::Failed,
            "unknown" => Self::Unknown,
            "completed" => Self::Completed,
            _ => Self::Other(value),
        })
    }
}

#[derive(Clone, Debug)]
enum StoredOutcome {
    Pending,
    NotApplied,
    Applied,
    Unknown,
    Other(String),
}

impl StoredOutcome {
    fn name(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::NotApplied => "not_applied",
            Self::Applied => "applied",
            Self::Unknown => "unknown",
            Self::Other(value) => value,
        }
    }
}

impl<'de> Deserialize<'de> for StoredOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "pending" => Self::Pending,
            "not_applied" => Self::NotApplied,
            "applied" => Self::Applied,
            "unknown" => Self::Unknown,
            _ => Self::Other(value),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolRequestRecord {
    #[serde(default)]
    stage: Option<StoredStage>,
    #[serde(default)]
    outcome: Option<StoredOutcome>,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    account_key: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nonnegative_integer")]
    epoch: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_supervisor")]
    supervisor: Option<SupervisorRecord>,
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Deserialize)]
struct SupervisorRecord {
    #[serde(default, deserialize_with = "deserialize_nonnegative_integer")]
    generation: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestEvent {
    Admit,
    ClaimDispatch,
    Cancel,
    ServerRestarted,
    CommittedOperationReceipt,
    AppliedResponse,
    DefinitiveRejection,
    Failure,
    ReceiptMissing,
}

impl RequestEvent {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "Admit" => Ok(Self::Admit),
            "ClaimDispatch" => Ok(Self::ClaimDispatch),
            "Cancel" => Ok(Self::Cancel),
            "ServerRestarted" => Ok(Self::ServerRestarted),
            "CommittedOperationReceipt" => Ok(Self::CommittedOperationReceipt),
            "AppliedResponse" => Ok(Self::AppliedResponse),
            "DefinitiveRejection" => Ok(Self::DefinitiveRejection),
            "Failure" => Ok(Self::Failure),
            "ReceiptMissing" => Ok(Self::ReceiptMissing),
            _ => Err(format!("Unknown tool request event: {value}")),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Admit => "Admit",
            Self::ClaimDispatch => "ClaimDispatch",
            Self::Cancel => "Cancel",
            Self::ServerRestarted => "ServerRestarted",
            Self::CommittedOperationReceipt => "CommittedOperationReceipt",
            Self::AppliedResponse => "AppliedResponse",
            Self::DefinitiveRejection => "DefinitiveRejection",
            Self::Failure => "Failure",
            Self::ReceiptMissing => "ReceiptMissing",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum RejectionProof {
    QueuedCancellation,
    ReadOnlyRejection,
    ExactPreWriteRejection,
    ReviewChildAbsent,
}

impl RejectionProof {
    fn parse(value: Option<&str>) -> Result<Option<Self>, String> {
        match value {
            None => Ok(None),
            Some("QueuedCancellation") => Ok(Some(Self::QueuedCancellation)),
            Some("ReadOnlyRejection") => Ok(Some(Self::ReadOnlyRejection)),
            Some("ExactPreWriteRejection") => Ok(Some(Self::ExactPreWriteRejection)),
            Some("ReviewChildAbsent") => Ok(Some(Self::ReviewChildAbsent)),
            Some(other) => Err(format!("Unknown tool request rejection proof: {other}")),
        }
    }

    const fn core(self) -> studio_operations::NonExecutionProof {
        match self {
            Self::QueuedCancellation => studio_operations::NonExecutionProof::QueuedCancellation,
            Self::ReadOnlyRejection => studio_operations::NonExecutionProof::ReadOnlyRejection,
            Self::ExactPreWriteRejection => {
                studio_operations::NonExecutionProof::ExactPreWriteRejection
            }
            Self::ReviewChildAbsent => studio_operations::NonExecutionProof::ReviewChildAbsent,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ResultKind {
    Transitioned,
    ReturnExisting,
    NoEffect,
    ReceiptMissingUnknown,
    Rejected,
}

impl ResultKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Transitioned => "Transitioned",
            Self::ReturnExisting => "ReturnExisting",
            Self::NoEffect => "NoEffect",
            Self::ReceiptMissingUnknown => "ReceiptMissingUnknown",
            Self::Rejected => "Rejected",
        }
    }
}

pub(crate) struct AdapterResult {
    pub(crate) kind: &'static str,
    pub(crate) rejection_code: Option<String>,
    pub(crate) rejection_message: Option<String>,
    pub(crate) stage: Option<&'static str>,
    pub(crate) outcome: Option<&'static str>,
    pub(crate) cancel_requested: Option<bool>,
}

pub(crate) fn transition_record(
    record_json: &str,
    event_name: &str,
    proof_name: Option<&str>,
    empty: bool,
    generation: Option<u64>,
    admission_signature: Option<&str>,
    admission_agent: Option<&str>,
) -> Result<AdapterResult, String> {
    let stored: ToolRequestRecord = serde_json::from_str(record_json)
        .map_err(|error| format!("Invalid stored tool request record: {error}"))?;
    let legacy_state = if empty {
        State::empty(StateRevision(0))
    } else {
        state_for_record(&stored)?
    };
    let event_kind = RequestEvent::parse(event_name)?;
    let proof = match RejectionProof::parse(proof_name)? {
        Some(proof) => Some(proof),
        None if event_kind == RequestEvent::DefinitiveRejection => Some(if stored.read_only {
            RejectionProof::ReadOnlyRejection
        } else {
            RejectionProof::ExactPreWriteRejection
        }),
        None => None,
    };
    let revision = legacy_state.revision.0;
    let identity = identity_for_record(&stored);
    let attempt = attempt_for_record(&stored, generation);
    let state = legacy_state;
    let kind = match event_kind {
        RequestEvent::Admit => EventKind::Admit {
            content: content_for_record(
                admission_signature
                    .unwrap_or_else(|| stored.signature.as_deref().unwrap_or_default()),
                admission_agent.unwrap_or_else(|| stored.agent.as_deref().unwrap_or_default()),
            )?,
        },
        RequestEvent::ClaimDispatch => EventKind::ClaimDispatch,
        RequestEvent::Cancel => EventKind::Cancel,
        RequestEvent::ServerRestarted => EventKind::ServerRestarted,
        RequestEvent::CommittedOperationReceipt => EventKind::CommittedOperationReceipt,
        RequestEvent::AppliedResponse => EventKind::AppliedResponse,
        RequestEvent::DefinitiveRejection => EventKind::DefinitiveRejection(
            proof
                .ok_or_else(|| "DefinitiveRejection requires a rejection proof".to_owned())?
                .core(),
        ),
        RequestEvent::Failure => {
            EventKind::Failure(studio_operations::FailureKind::InvalidPostExecutionResponse)
        }
        RequestEvent::ReceiptMissing => EventKind::ReceiptMissing,
    };
    let generated_id = format!(
        "tool-request:{}:{}:{}",
        stored.id.as_deref().unwrap_or_default(),
        revision,
        event_kind.label()
    );
    let event = Event {
        id: EventId::from(generated_id),
        expected_revision: StateRevision(revision),
        identity,
        attempt,
        kind,
    };
    let decision = transition(&state, &event);
    Ok(result_for_decision(
        &stored,
        &decision.kind,
        &decision.next_state,
    ))
}

fn unsupported(stage: &str, outcome: &str) -> String {
    format!("Unsupported stored tool request lifecycle: {stage}/{outcome}")
}

fn state_for_record(record: &ToolRequestRecord) -> Result<State, String> {
    let missing_stage = StoredStage::Other("None".to_owned());
    let missing_outcome = StoredOutcome::Other("None".to_owned());
    let stage = record.stage.as_ref().unwrap_or(&missing_stage);
    let outcome = record.outcome.as_ref().unwrap_or(&missing_outcome);
    let (revision, state) = match (stage, outcome) {
        (StoredStage::Queued, StoredOutcome::Pending) => (0, OperationState::Reserved),
        (StoredStage::Running, StoredOutcome::Pending) if record.cancel_requested => {
            (2, OperationState::CancelRequested)
        }
        (StoredStage::Running, StoredOutcome::Pending) => (1, OperationState::Dispatched),
        (StoredStage::Cancelled, StoredOutcome::NotApplied) => {
            (3, OperationState::CancelledBeforeDispatch)
        }
        (
            StoredStage::Queued
            | StoredStage::Running
            | StoredStage::Interrupted
            | StoredStage::Failed
            | StoredStage::Unknown,
            StoredOutcome::Unknown,
        ) => (3, OperationState::Unknown),
        (StoredStage::Completed, StoredOutcome::Applied) => (3, OperationState::Applied),
        (StoredStage::Failed, StoredOutcome::NotApplied) => (3, OperationState::NotApplied),
        _ => return Err(unsupported(stage.name(), outcome.name())),
    };
    Ok(State {
        revision: StateRevision(revision),
        operation: Some(Operation {
            identity: identity_for_record(record),
            content: content_for_record(
                record.signature.as_deref().unwrap_or_default(),
                record.agent.as_deref().unwrap_or_default(),
            )?,
            attempt: attempt_for_record(record, None),
            state,
        }),
        tombstone: None,
    })
}

fn identity_for_record(record: &ToolRequestRecord) -> OperationIdentity {
    let account = record.account_key.as_deref().unwrap_or("default");
    let thread = record.thread_id.as_deref().unwrap_or_default();
    let prefix = if account == "default" {
        format!("{thread}:")
    } else {
        format!("{account}:{thread}:")
    };
    let request_id = record
        .id
        .as_deref()
        .unwrap_or_default()
        .strip_prefix(&prefix)
        .unwrap_or_else(|| record.call_id.as_deref().unwrap_or_default());
    OperationIdentity {
        scope: IdentityScope::ToolRequest {
            account: AccountId::from(account),
            thread: ThreadId::from(thread),
        },
        request_id: RequestId::from(request_id),
    }
}

fn attempt_for_record(record: &ToolRequestRecord, generation: Option<u64>) -> AttemptIdentity {
    let account = record.account_key.as_deref().unwrap_or("default");
    let thread = record.thread_id.as_deref().unwrap_or_default();
    let saved_generation = record
        .supervisor
        .as_ref()
        .and_then(|supervisor| supervisor.generation);
    AttemptIdentity {
        agent: AgentId::from(record.agent.as_deref().unwrap_or_default()),
        account: AccountId::from(account),
        thread: ThreadId::from(thread),
        epoch: AccountThreadEpoch(record.epoch.unwrap_or(0)),
        generation: generation.or(saved_generation).map(ProcessGeneration),
    }
}

fn content_for_record(signature: &str, agent: &str) -> Result<ContentFingerprint, String> {
    let canonical = serde_json::to_vec(&(signature, agent))
        .map_err(|error| format!("Could not encode tool request content: {error}"))?;
    Ok(ContentFingerprint(Sha256::digest(canonical).into()))
}

fn result_for_decision(
    record: &ToolRequestRecord,
    kind: &studio_operations::DecisionKind,
    next_state: &State,
) -> AdapterResult {
    use studio_operations::DecisionKind;
    match kind {
        DecisionKind::Rejected(error) => {
            let code = format!("{error:?}");
            AdapterResult {
                kind: ResultKind::Rejected.as_str(),
                rejection_message: Some(rejection_message(*error)),
                rejection_code: Some(code),
                stage: None,
                outcome: None,
                cancel_requested: None,
            }
        }
        other => {
            let result_kind = match other {
                DecisionKind::Transitioned => ResultKind::Transitioned,
                DecisionKind::ReturnExisting => ResultKind::ReturnExisting,
                DecisionKind::NoEffect => ResultKind::NoEffect,
                DecisionKind::ReceiptMissingUnknown => ResultKind::ReceiptMissingUnknown,
                DecisionKind::Rejected(_) => ResultKind::Rejected,
            };
            let persisted = next_state
                .operation
                .as_ref()
                .map(|operation| match operation.state {
                    OperationState::Reserved => ("queued", "pending", false),
                    OperationState::Dispatched => ("running", "pending", false),
                    OperationState::CancelRequested => ("running", "pending", true),
                    OperationState::CancelledBeforeDispatch => {
                        ("cancelled", "not_applied", record.cancel_requested)
                    }
                    OperationState::NotApplied => {
                        ("failed", "not_applied", record.cancel_requested)
                    }
                    OperationState::Applied => ("completed", "applied", record.cancel_requested),
                    OperationState::Unknown => ("failed", "unknown", record.cancel_requested),
                });
            AdapterResult {
                kind: result_kind.as_str(),
                rejection_code: None,
                rejection_message: None,
                stage: persisted.map(|value| value.0),
                outcome: persisted.map(|value| value.1),
                cancel_requested: persisted.map(|value| value.2),
            }
        }
    }
}

fn rejection_message(error: TransitionError) -> String {
    match error {
        TransitionError::DifferentContent | TransitionError::IdentityMismatch => {
            DIFFERENT_CONTENT.to_owned()
        }
        TransitionError::ConflictingEvidence => CONFLICTING_EVIDENCE.to_owned(),
        TransitionError::StaleRevision => STALE_REVISION.to_owned(),
        TransitionError::StaleEpoch => STALE_EPOCH.to_owned(),
        TransitionError::StaleGeneration => STALE_GENERATION.to_owned(),
        TransitionError::GenerationUnavailable => GENERATION_UNAVAILABLE.to_owned(),
        other => format!("Invalid tool request lifecycle transition: {other:?}"),
    }
}
