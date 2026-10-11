//! Evidence-based settlement decisions for durable tool requests.

use crate::{
    DecisionKind, Event, EventId, EventKind, FailureKind, NonExecutionProof, OperationState,
    Outcome, RequestId, State, TransitionError,
    reservation::{RequestStore, RequestWriteIntent},
    transition,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RequestTool {
    OrchestrationServers,
    #[cfg_attr(
        feature = "serde",
        serde(
            alias = "orchestration_peers",
            alias = "orchestration_search",
            alias = "orchestration_chat_read"
        )
    )]
    OrchestrationRead,
    OrchestrationUserTask,
    OrchestrationTask,
    OrchestrationMessage,
    OrchestrationAgentManage,
    OrchestrationMonitor,
    OrchestrationMonitorInput,
    OrchestrationWatch,
    OrchestrationContext,
    OrchestrationTitle,
    #[cfg_attr(feature = "serde", serde(other))]
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ResultEvidence {
    pub tool: RequestTool,
    pub success: Option<bool>,
    pub read_only: bool,
    pub first_error: Option<String>,
    pub server_outcome: Option<Outcome>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SettlementInput {
    pub request_id: RequestId,
    pub event_id: EventId,
    pub evidence: ResultEvidence,
    pub explicit_outcome: Option<Outcome>,
    pub proof: Option<NonExecutionProof>,
    pub operation_receipt_present: bool,
    pub result_changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SettlementKind {
    Persisted,
    NoChange,
}

#[must_use]
pub const fn missing_receipt_outcome() -> Outcome {
    Outcome::Unknown
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SettlementDecision {
    pub state: State,
    pub event: EventKind,
    pub kind: SettlementKind,
    pub intents: Vec<RequestWriteIntent>,
}

pub fn decide_settlement(
    state: &State,
    input: &SettlementInput,
) -> Result<SettlementDecision, TransitionError> {
    let operation = state
        .operation
        .as_ref()
        .ok_or(TransitionError::OperationMissing)?;
    let inferred = input.explicit_outcome.is_none();
    let outcome = match input.explicit_outcome {
        Some(outcome) => outcome,
        None => classify_result(&input.evidence),
    };
    let event_kind = if outcome == Outcome::Applied {
        EventKind::AppliedResponse
    } else if inferred && outcome == Outcome::NotApplied && input.operation_receipt_present {
        EventKind::CommittedOperationReceipt
    } else if input.proof == Some(NonExecutionProof::ReviewChildAbsent) {
        EventKind::DefinitiveRejection(NonExecutionProof::ReviewChildAbsent)
    } else if outcome == Outcome::NotApplied {
        EventKind::DefinitiveRejection(if input.evidence.read_only {
            NonExecutionProof::ReadOnlyRejection
        } else {
            NonExecutionProof::ExactPreWriteRejection
        })
    } else {
        EventKind::Failure(FailureKind::InvalidPostExecutionResponse)
    };
    let decision = transition(
        state,
        &Event {
            id: input.event_id.clone(),
            expected_revision: state.revision,
            identity: operation.identity.clone(),
            attempt: operation.attempt.clone(),
            kind: event_kind.clone(),
        },
    );
    if let DecisionKind::Rejected(error) = &decision.kind {
        return Err(*error);
    }
    let transitioned = decision.kind == DecisionKind::Transitioned;
    let update_unknown_result =
        !transitioned && operation.state == OperationState::Unknown && input.result_changed;
    let kind = if transitioned || update_unknown_result {
        SettlementKind::Persisted
    } else {
        SettlementKind::NoChange
    };
    let intents = if kind == SettlementKind::Persisted {
        vec![RequestWriteIntent::SaveRequest {
            state: Box::new(decision.next_state.clone()),
        }]
    } else {
        Vec::new()
    };
    Ok(SettlementDecision {
        state: decision.next_state,
        event: event_kind,
        kind,
        intents,
    })
}

pub fn settle_in_store<S: RequestStore>(
    store: &mut S,
    input: &SettlementInput,
) -> Result<SettlementDecision, S::Error>
where
    S::Error: From<TransitionError>,
{
    let snapshot = store.load_snapshot(&input.request_id)?;
    let state = snapshot
        .existing
        .ok_or_else(|| S::Error::from(TransitionError::OperationMissing))?;
    let decision = decide_settlement(&state, input).map_err(S::Error::from)?;
    store.apply_intents(&decision.intents)?;
    Ok(decision)
}

fn classify_result(evidence: &ResultEvidence) -> Outcome {
    match evidence.success {
        Some(true) => {
            if evidence.tool == RequestTool::OrchestrationServers {
                match evidence.server_outcome {
                    Some(outcome) => outcome,
                    None => Outcome::Applied,
                }
            } else {
                Outcome::Applied
            }
        }
        Some(false) if evidence.read_only || evidence.tool == RequestTool::OrchestrationRead => {
            Outcome::NotApplied
        }
        Some(false) if is_exact_rejection(evidence.tool, evidence.first_error.as_deref()) => {
            Outcome::NotApplied
        }
        _ => Outcome::Unknown,
    }
}

fn is_exact_rejection(tool: RequestTool, error: Option<&str>) -> bool {
    let Some(error) = error else {
        return false;
    };
    let errors: &[&str] = match tool {
        RequestTool::OrchestrationUserTask => {
            &["User tasks were removed. Send a message with orchestration_message target=user."]
        }
        RequestTool::OrchestrationTask => &[
            "limit must be 1 to 50",
            "List changed or cursor is invalid. Read the first page again.",
            "Unknown task in this team",
            "Unknown work item",
            "This work item changed. Reload before editing",
            "Only the lead can create work",
            "Only the lead can change work assignments",
            "Only the assigned worker can submit its result",
            "Only the lead can accept or reject a result",
            "This result is already accepted",
            "Submit a result before review",
            "This work item is not ready",
            "Supply up to 50 file paths",
            "Another agent owns this work item",
            "Dependencies have not been accepted",
            "Unknown managed agent",
            "The file is outside this agent workspace",
            "Supply a source revision with 1 to 200 characters",
            "Supply a result with 1 to 32000 characters",
            "Supply test evidence with 1 to 32000 characters",
            "Work under review cannot be reassigned; reject its result before editing",
            "Review or accepted work cannot be reassigned; reject a result before editing",
            "Supply a review decision with 1 to 32000 characters",
            "Supply result with 1 to 32000 characters for accept or reject",
        ],
        RequestTool::OrchestrationMessage => &[
            "A no-issue result requires the exact active review event and target",
            "Versioned progress requires a progress_key and nonnegative integer progress_version",
            "Unknown message importance",
            "Message must have 1 to 12000 characters",
            "Select another agent",
            "Unknown managed agent",
            "Recipient conversation was deleted",
            "Only the lead can contact the user. Send your message to your lead.",
            "This record belongs to another team",
        ],
        RequestTool::OrchestrationAgentManage => &[
            "You can manage only your own descendant workers",
            "Unknown managed agent",
        ],
        RequestTool::OrchestrationMonitor => &[
            "Command timeout must be 1 second to 24 hours",
            "Supply a command with 1 to 12000 characters",
            "success_exit_codes must list 1 to 16 exit codes from 0 to 255",
        ],
        RequestTool::OrchestrationMonitorInput => &["This interactive monitor is not active"],
        RequestTool::OrchestrationWatch => &[
            "The file is outside this agent workspace",
            "Choose a date within the next year",
            "Interval must be 10 seconds to one year",
        ],
        RequestTool::OrchestrationContext => {
            &["Unknown monitor in this team", "Unknown context topic"]
        }
        RequestTool::OrchestrationTitle => &["Only a lead can set a title of 1 to 80 characters"],
        RequestTool::OrchestrationServers | RequestTool::OrchestrationRead | RequestTool::Other => {
            &[]
        }
    };
    errors.contains(&error)
}
