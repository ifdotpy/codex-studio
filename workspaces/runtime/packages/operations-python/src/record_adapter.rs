//! Durable Studio request records at the boundary to the pure lifecycle core.

use serde::Deserialize;
use studio_operations::{
    AccountId, AccountThreadEpoch, AgentId, AttemptIdentity, ContentFingerprint, Event, EventId,
    EventKind, IdentityScope, Operation, OperationIdentity, OperationState, ProcessGeneration,
    RequestAlias, RequestId, ReservationDecision, ReservationInput, ReservationSnapshot, State,
    StateRevision, ThreadId, TransitionError, transition,
};

const DIFFERENT_CONTENT: &str = "This request id has different content";
const CONFLICTING_EVIDENCE: &str = "Conflicting evidence for tool request";
const STALE_REVISION: &str = "This request receipt changed while applying its lifecycle transition";
const STALE_EPOCH: &str = "This request belongs to an older account/thread epoch";
const STALE_GENERATION: &str = "This request belongs to an older process generation";
const GENERATION_UNAVAILABLE: &str = "The request has no captured process generation";

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize)]
        pub(crate) enum $name {
            $(#[serde(rename = $label)] $variant),+
        }

        impl $name {
            pub(crate) const NAMES: &'static [&'static str] = &[$($label),+];

            pub(crate) const fn label(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }
        }
    };
}

macro_rules! wire_enum_without_label {
    ($name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize)]
        pub(crate) enum $name {
            $(#[serde(rename = $label)] $variant),+
        }

        impl $name {
            pub(crate) const NAMES: &'static [&'static str] = &[$($label),+];
        }
    };
}

wire_enum!(RequestEvent {
    Admit => "Admit",
    ClaimDispatch => "ClaimDispatch",
    Cancel => "Cancel",
    ServerRestarted => "ServerRestarted",
    CommittedOperationReceipt => "CommittedOperationReceipt",
    AppliedResponse => "AppliedResponse",
    DefinitiveRejection => "DefinitiveRejection",
    Failure => "Failure",
    ReceiptMissing => "ReceiptMissing",
});
wire_enum_without_label!(RejectionProof {
    QueuedCancellation => "QueuedCancellation",
    ReadOnlyRejection => "ReadOnlyRejection",
    ExactPreWriteRejection => "ExactPreWriteRejection",
    ReviewChildAbsent => "ReviewChildAbsent",
});
wire_enum!(ResultKind {
    Transitioned => "Transitioned",
    ReturnExisting => "ReturnExisting",
    NoEffect => "NoEffect",
    ReceiptMissingUnknown => "ReceiptMissingUnknown",
    Rejected => "Rejected",
});
wire_enum!(PersistedStage {
    Queued => "queued",
    Running => "running",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
    Failed => "failed",
    Unknown => "unknown",
    Completed => "completed",
});
wire_enum!(PersistedOutcome {
    Pending => "pending",
    NotApplied => "not_applied",
    Applied => "applied",
    Unknown => "unknown",
});

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolRequestRecord {
    #[serde(default)]
    stage: Option<PersistedStage>,
    #[serde(default)]
    outcome: Option<PersistedOutcome>,
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
    epoch: Option<u64>,
    #[serde(default)]
    supervisor: Option<SupervisorRecord>,
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Deserialize)]
struct SupervisorRecord {
    #[serde(default)]
    generation: Option<u64>,
}
impl RejectionProof {
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

impl ResultKind {
    const fn as_str(self) -> &'static str {
        self.label()
    }
}

pub(crate) struct AdapterResult {
    pub(crate) kind: &'static str,
    pub(crate) rejection_code: Option<String>,
    pub(crate) rejection_message: Option<String>,
    pub(crate) stage: Option<PersistedStage>,
    pub(crate) outcome: Option<PersistedOutcome>,
    pub(crate) cancel_requested: Option<bool>,
}

pub(crate) const EVENT_NAMES: &[&str] = RequestEvent::NAMES;
pub(crate) const PROOF_NAMES: &[&str] = RejectionProof::NAMES;
pub(crate) const RESULT_NAMES: &[&str] = ResultKind::NAMES;
pub(crate) const STAGE_NAMES: &[&str] = PersistedStage::NAMES;
pub(crate) const OUTCOME_NAMES: &[&str] = PersistedOutcome::NAMES;

pub(crate) fn transition_record(
    record_json: &str,
    event_name: &str,
    fingerprint: Vec<u8>,
    proof_name: Option<&str>,
    empty: bool,
    generation: Option<u64>,
    admission_fingerprint: Option<Vec<u8>>,
) -> Result<AdapterResult, String> {
    let stored: ToolRequestRecord = serde_json::from_str(record_json)
        .map_err(|error| format!("Invalid stored tool request record: {error}"))?;
    let fingerprint: [u8; 32] = fingerprint
        .try_into()
        .map_err(|_| "Tool request content fingerprint must contain 32 bytes".to_owned())?;
    let legacy_state = if empty {
        State::empty(StateRevision(0))
    } else {
        state_for_record(&stored, ContentFingerprint(fingerprint))?
    };
    let encoded_event = serde_json::to_string(event_name)
        .map_err(|error| format!("Could not encode tool request event: {error}"))?;
    let event_kind: RequestEvent = serde_json::from_str(&encoded_event)
        .map_err(|_| format!("Unknown tool request event: {event_name}"))?;
    let proof = match proof_name.map(|name| {
        serde_json::to_string(name)
            .map_err(|error| format!("Could not encode tool request rejection proof: {error}"))
            .and_then(|encoded| {
                serde_json::from_str(&encoded)
                    .map_err(|_| format!("Unknown tool request rejection proof: {name}"))
            })
    }) {
        Some(Ok(proof)) => Some(proof),
        Some(Err(error)) => return Err(error),
        None => match event_kind {
            RequestEvent::DefinitiveRejection => Some(if stored.read_only {
                RejectionProof::ReadOnlyRejection
            } else {
                RejectionProof::ExactPreWriteRejection
            }),
            _ => None,
        },
    };
    let revision = legacy_state.revision.0;
    let identity = identity_for_record(&stored);
    let attempt = attempt_for_record(&stored, generation);
    let state = legacy_state;
    let kind = match event_kind {
        RequestEvent::Admit => EventKind::Admit {
            content: ContentFingerprint(
                admission_fingerprint
                    .map(TryInto::try_into)
                    .transpose()
                    .map_err(|_| "Admission fingerprint must contain 32 bytes".to_owned())?
                    .unwrap_or(fingerprint),
            ),
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

#[derive(Debug, Deserialize)]
struct ReservationCommand {
    candidate: ReservationRecord,
    existing: Option<ReservationRecord>,
    aliases: Vec<RequestAlias>,
}

#[derive(Debug, Deserialize)]
struct ReservationRecord {
    record: ToolRequestRecord,
    fingerprint: Vec<u8>,
}

pub(crate) fn reserve_tool_request(command_json: &str) -> Result<String, String> {
    let command: ReservationCommand = serde_json::from_str(command_json)
        .map_err(|error| format!("Invalid tool request reservation snapshot: {error}"))?;
    let candidate_fingerprint = fingerprint(command.candidate.fingerprint)?;
    let identity = identity_for_record(&command.candidate.record);
    let attempt = attempt_for_record(&command.candidate.record, None);
    let existing = command
        .existing
        .map(|stored| {
            let content = fingerprint(stored.fingerprint)?;
            let state = state_for_record(&stored.record, ContentFingerprint(content))?;
            Ok::<State, String>(state)
        })
        .transpose()?;
    let input = ReservationInput {
        identity,
        content: ContentFingerprint(candidate_fingerprint),
        attempt,
        aliases: command.aliases,
    };
    let decision = studio_operations::decide_reservation(&ReservationSnapshot { existing }, &input);
    let message = match &decision {
        ReservationDecision::Rejected { error, .. } => Some(rejection_message(*error)),
        ReservationDecision::Admitted { .. } | ReservationDecision::ReturnExisting { .. } => None,
    };
    serde_json::to_string(&(decision, message))
        .map_err(|error| format!("Could not encode tool request reservation decision: {error}"))
}

fn fingerprint(value: Vec<u8>) -> Result<[u8; 32], String> {
    value
        .try_into()
        .map_err(|_| "Tool request content fingerprint must contain 32 bytes".to_owned())
}

fn state_for_record(
    record: &ToolRequestRecord,
    content: ContentFingerprint,
) -> Result<State, String> {
    let stage = record.stage;
    let outcome = record.outcome;
    let (revision, state) = match (stage, outcome) {
        (Some(PersistedStage::Queued), Some(PersistedOutcome::Pending)) => {
            (0, OperationState::Reserved)
        }
        (Some(PersistedStage::Running), Some(PersistedOutcome::Pending))
            if record.cancel_requested =>
        {
            (2, OperationState::CancelRequested)
        }
        (Some(PersistedStage::Running), Some(PersistedOutcome::Pending)) => {
            (1, OperationState::Dispatched)
        }
        (Some(PersistedStage::Cancelled), Some(PersistedOutcome::NotApplied)) => {
            (3, OperationState::CancelledBeforeDispatch)
        }
        (
            Some(
                PersistedStage::Queued
                | PersistedStage::Running
                | PersistedStage::Interrupted
                | PersistedStage::Failed
                | PersistedStage::Unknown,
            ),
            Some(PersistedOutcome::Unknown),
        ) => (3, OperationState::Unknown),
        (Some(PersistedStage::Completed), Some(PersistedOutcome::Applied)) => {
            (3, OperationState::Applied)
        }
        (Some(PersistedStage::Failed), Some(PersistedOutcome::NotApplied)) => {
            (3, OperationState::NotApplied)
        }
        _ => {
            return Err(format!(
                "Unsupported stored tool request lifecycle: {}/{}",
                stage.map(PersistedStage::label).unwrap_or("None"),
                outcome.map(PersistedOutcome::label).unwrap_or("None")
            ));
        }
    };
    Ok(State {
        revision: StateRevision(revision),
        operation: Some(Operation {
            identity: identity_for_record(record),
            content,
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
                    OperationState::Reserved => {
                        (PersistedStage::Queued, PersistedOutcome::Pending, false)
                    }
                    OperationState::Dispatched => {
                        (PersistedStage::Running, PersistedOutcome::Pending, false)
                    }
                    OperationState::CancelRequested => {
                        (PersistedStage::Running, PersistedOutcome::Pending, true)
                    }
                    OperationState::CancelledBeforeDispatch => (
                        PersistedStage::Cancelled,
                        PersistedOutcome::NotApplied,
                        record.cancel_requested,
                    ),
                    OperationState::NotApplied => (
                        PersistedStage::Failed,
                        PersistedOutcome::NotApplied,
                        record.cancel_requested,
                    ),
                    OperationState::Applied => (
                        PersistedStage::Completed,
                        PersistedOutcome::Applied,
                        record.cancel_requested,
                    ),
                    OperationState::Unknown => (
                        PersistedStage::Failed,
                        PersistedOutcome::Unknown,
                        record.cancel_requested,
                    ),
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
