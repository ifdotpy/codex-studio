use std::collections::HashMap;

use studio_operations::{
    AccountId, AccountThreadEpoch, AgentId, AttemptIdentity, ContentFingerprint, DecisionKind,
    Event, EventId, EventKind, IdentityScope, NonExecutionProof, OperationIdentity, OperationState,
    Outcome, RequestId, RequestSnapshot, RequestStore, RequestTool, RequestWriteIntent,
    ResultEvidence, SettlementInput, SettlementKind, State, StateRevision, ThreadId,
    TransitionError, decide_reservation, decide_settlement, settle_in_store,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoreError {
    Injected,
    Invalid,
}

impl From<TransitionError> for StoreError {
    fn from(_: TransitionError) -> Self {
        Self::Invalid
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureWindow {
    BeforeReceipt,
    AfterReceipt,
}

#[derive(Default)]
struct MemoryStore {
    requests: HashMap<RequestId, State>,
    results: HashMap<RequestId, ResultEvidence>,
    operation_receipts: HashMap<RequestId, bool>,
    fail_next: Option<FailureWindow>,
    writes: usize,
    effects: usize,
}

impl RequestStore for MemoryStore {
    type Error = StoreError;

    fn load_snapshot(&mut self, request_id: &RequestId) -> Result<RequestSnapshot, Self::Error> {
        Ok(RequestSnapshot {
            existing: self.requests.get(request_id).cloned(),
        })
    }

    fn apply_intents(&mut self, intents: &[RequestWriteIntent]) -> Result<(), Self::Error> {
        if self.fail_next == Some(FailureWindow::BeforeReceipt) {
            self.fail_next = None;
            return Err(StoreError::Injected);
        }
        for intent in intents {
            if let RequestWriteIntent::SaveRequest { state } = intent {
                let Some(operation) = state.operation.as_ref() else {
                    return Err(StoreError::Invalid);
                };
                self.requests.insert(
                    operation.identity.request_id.clone(),
                    state.as_ref().clone(),
                );
                self.writes += 1;
            }
        }
        if self.fail_next == Some(FailureWindow::AfterReceipt) {
            self.fail_next = None;
            return Err(StoreError::Injected);
        }
        Ok(())
    }
}

fn reserved_state() -> State {
    let account = AccountId::from("account-a");
    let thread = ThreadId::from("thread-a");
    let request_id = RequestId::from("request-a");
    let request = studio_operations::ReservationInput {
        identity: OperationIdentity {
            scope: IdentityScope::ToolRequest {
                account: account.clone(),
                thread: thread.clone(),
            },
            request_id: request_id.clone(),
        },
        alias_target: request_id,
        content: ContentFingerprint([4; 32]),
        attempt: AttemptIdentity {
            agent: AgentId::from("agent-a"),
            account,
            thread,
            epoch: AccountThreadEpoch(1),
            generation: None,
        },
        call_id: "call-a".to_owned(),
        alias_prefix: "thread:".to_owned(),
        request_alias: None,
    };
    let studio_operations::ReservationDecision::Admitted { state, .. } =
        decide_reservation(&RequestSnapshot { existing: None }, &request)
    else {
        panic!("request should reserve");
    };
    state
}

fn dispatched_state() -> State {
    let state = reserved_state();
    let operation = state
        .operation
        .as_ref()
        .expect("test reservation has operation");
    let decision = studio_operations::transition(
        &state,
        &Event {
            id: EventId::from("claim"),
            expected_revision: state.revision,
            identity: operation.identity.clone(),
            attempt: operation.attempt.clone(),
            kind: EventKind::ClaimDispatch,
        },
    );
    assert_eq!(decision.kind, DecisionKind::Transitioned);
    decision.next_state
}

fn unknown_state() -> State {
    let state = dispatched_state();
    let operation = state
        .operation
        .as_ref()
        .expect("test reservation has operation");
    studio_operations::transition(
        &state,
        &Event {
            id: EventId::from("lost-response"),
            expected_revision: state.revision,
            identity: operation.identity.clone(),
            attempt: operation.attempt.clone(),
            kind: EventKind::Failure(studio_operations::FailureKind::ConnectionLost),
        },
    )
    .next_state
}

fn evidence(tool: RequestTool, success: Option<bool>, error: Option<&str>) -> ResultEvidence {
    ResultEvidence {
        tool,
        success,
        read_only: false,
        first_error: error.map(str::to_owned),
        server_outcome: None,
    }
}

fn input(evidence: ResultEvidence) -> SettlementInput {
    SettlementInput {
        request_id: RequestId::from("request-a"),
        event_id: EventId::from("settle"),
        evidence,
        explicit_outcome: None,
        proof: None,
        operation_receipt_present: false,
        result_changed: true,
    }
}

fn decision(state: &State, input: &SettlementInput) -> (Outcome, SettlementKind) {
    let result = decide_settlement(state, input).expect("valid settlement");
    let operation = result.state.operation.as_ref().expect("settled operation");
    let kind = result.kind;
    let outcome = match operation.state {
        OperationState::Applied => Outcome::Applied,
        OperationState::NotApplied | OperationState::CancelledBeforeDispatch => Outcome::NotApplied,
        OperationState::Unknown => Outcome::Unknown,
        _ => panic!("expected terminal operation state"),
    };
    (outcome, kind)
}

fn selected_event(state: &State, input: &SettlementInput) -> EventKind {
    decide_settlement(state, input)
        .expect("valid settlement")
        .event
}

#[test]
fn successful_response_is_applied_and_server_nested_outcomes_are_preserved() {
    let state = dispatched_state();
    assert_eq!(
        decision(
            &state,
            &input(evidence(RequestTool::Other, Some(true), None))
        ),
        (Outcome::Applied, SettlementKind::Persisted)
    );
    assert_eq!(
        selected_event(
            &state,
            &input(evidence(RequestTool::Other, Some(true), None))
        ),
        EventKind::AppliedResponse
    );
    for outcome in [Outcome::Applied, Outcome::NotApplied, Outcome::Unknown] {
        let mut result = evidence(RequestTool::OrchestrationServers, Some(true), None);
        result.server_outcome = Some(outcome);
        assert_eq!(decision(&state, &input(result)).0, outcome);
    }
    let mut malformed_server = evidence(RequestTool::OrchestrationServers, Some(true), None);
    malformed_server.server_outcome = None;
    assert_eq!(
        decision(&state, &input(malformed_server)).0,
        Outcome::Applied
    );
}

#[test]
fn exact_rejection_messages_are_tool_scoped_and_near_misses_stay_unknown() {
    let cases = [
        (
            RequestTool::OrchestrationUserTask,
            "User tasks were removed. Send a message with orchestration_message target=user.",
        ),
        (RequestTool::OrchestrationTask, "limit must be 1 to 50"),
        (
            RequestTool::OrchestrationTask,
            "List changed or cursor is invalid. Read the first page again.",
        ),
        (RequestTool::OrchestrationTask, "Unknown task in this team"),
        (RequestTool::OrchestrationTask, "Unknown work item"),
        (
            RequestTool::OrchestrationTask,
            "This work item changed. Reload before editing",
        ),
        (
            RequestTool::OrchestrationTask,
            "Only the lead can create work",
        ),
        (
            RequestTool::OrchestrationTask,
            "Only the lead can change work assignments",
        ),
        (
            RequestTool::OrchestrationTask,
            "Only the assigned worker can submit its result",
        ),
        (
            RequestTool::OrchestrationTask,
            "Only the lead can accept or reject a result",
        ),
        (
            RequestTool::OrchestrationTask,
            "This result is already accepted",
        ),
        (
            RequestTool::OrchestrationTask,
            "Submit a result before review",
        ),
        (
            RequestTool::OrchestrationTask,
            "This work item is not ready",
        ),
        (RequestTool::OrchestrationTask, "Supply up to 50 file paths"),
        (
            RequestTool::OrchestrationTask,
            "Another agent owns this work item",
        ),
        (
            RequestTool::OrchestrationTask,
            "Dependencies have not been accepted",
        ),
        (RequestTool::OrchestrationTask, "Unknown managed agent"),
        (
            RequestTool::OrchestrationTask,
            "The file is outside this agent workspace",
        ),
        (
            RequestTool::OrchestrationTask,
            "Supply a source revision with 1 to 200 characters",
        ),
        (
            RequestTool::OrchestrationTask,
            "Supply a result with 1 to 32000 characters",
        ),
        (
            RequestTool::OrchestrationTask,
            "Supply test evidence with 1 to 32000 characters",
        ),
        (
            RequestTool::OrchestrationTask,
            "Work under review cannot be reassigned; reject its result before editing",
        ),
        (
            RequestTool::OrchestrationTask,
            "Review or accepted work cannot be reassigned; reject a result before editing",
        ),
        (
            RequestTool::OrchestrationTask,
            "Supply a review decision with 1 to 32000 characters",
        ),
        (
            RequestTool::OrchestrationTask,
            "Supply result with 1 to 32000 characters for accept or reject",
        ),
        (
            RequestTool::OrchestrationMessage,
            "A no-issue result requires the exact active review event and target",
        ),
        (
            RequestTool::OrchestrationMessage,
            "Versioned progress requires a progress_key and nonnegative integer progress_version",
        ),
        (
            RequestTool::OrchestrationMessage,
            "Unknown message importance",
        ),
        (
            RequestTool::OrchestrationMessage,
            "Message must have 1 to 12000 characters",
        ),
        (RequestTool::OrchestrationMessage, "Select another agent"),
        (RequestTool::OrchestrationMessage, "Unknown managed agent"),
        (
            RequestTool::OrchestrationMessage,
            "Recipient conversation was deleted",
        ),
        (
            RequestTool::OrchestrationMessage,
            "Only the lead can contact the user. Send your message to your lead.",
        ),
        (
            RequestTool::OrchestrationMessage,
            "This record belongs to another team",
        ),
        (
            RequestTool::OrchestrationAgentManage,
            "You can manage only your own descendant workers",
        ),
        (
            RequestTool::OrchestrationAgentManage,
            "Unknown managed agent",
        ),
        (
            RequestTool::OrchestrationMonitor,
            "Command timeout must be 1 second to 24 hours",
        ),
        (
            RequestTool::OrchestrationMonitor,
            "Supply a command with 1 to 12000 characters",
        ),
        (
            RequestTool::OrchestrationMonitor,
            "success_exit_codes must list 1 to 16 exit codes from 0 to 255",
        ),
        (
            RequestTool::OrchestrationMonitorInput,
            "This interactive monitor is not active",
        ),
        (
            RequestTool::OrchestrationWatch,
            "The file is outside this agent workspace",
        ),
        (
            RequestTool::OrchestrationWatch,
            "Choose a date within the next year",
        ),
        (
            RequestTool::OrchestrationWatch,
            "Interval must be 10 seconds to one year",
        ),
        (
            RequestTool::OrchestrationContext,
            "Unknown monitor in this team",
        ),
        (RequestTool::OrchestrationContext, "Unknown context topic"),
        (
            RequestTool::OrchestrationTitle,
            "Only a lead can set a title of 1 to 80 characters",
        ),
    ];
    let state = dispatched_state();
    for (tool, error) in cases {
        let input = input(evidence(tool, Some(false), Some(error)));
        assert_eq!(
            decision(&state, &input).0,
            Outcome::NotApplied,
            "{tool:?}: {error}"
        );
        assert_eq!(
            selected_event(&state, &input),
            EventKind::DefinitiveRejection(NonExecutionProof::ExactPreWriteRejection),
            "{tool:?}: {error}"
        );
    }
    assert_eq!(
        decision(
            &state,
            &input(evidence(
                RequestTool::OrchestrationTask,
                Some(false),
                Some("Unknown work item after mutation"),
            )),
        )
        .0,
        Outcome::Unknown
    );
    assert_eq!(
        decision(
            &state,
            &input(evidence(
                RequestTool::Other,
                Some(false),
                Some("Unknown task in this team")
            )),
        )
        .0,
        Outcome::Unknown
    );
}

#[test]
fn read_only_rejections_and_indeterminate_results_keep_their_outcomes() {
    let state = dispatched_state();
    for tool in [RequestTool::OrchestrationRead, RequestTool::Other] {
        let mut rejected = evidence(tool, Some(false), Some("transport failed"));
        rejected.read_only = tool == RequestTool::Other;
        assert_eq!(decision(&state, &input(rejected)).0, Outcome::NotApplied);
    }
    assert_eq!(
        decision(&state, &input(evidence(RequestTool::Other, None, None))).0,
        Outcome::Unknown
    );
}

#[test]
fn nested_receipt_overrides_only_inferred_rejection_and_keeps_unknown() {
    let state = dispatched_state();
    let mut inferred = input(evidence(
        RequestTool::OrchestrationTask,
        Some(false),
        Some("Unknown work item"),
    ));
    inferred.operation_receipt_present = true;
    assert_eq!(decision(&state, &inferred).0, Outcome::Unknown);
    assert_eq!(
        selected_event(&state, &inferred),
        EventKind::CommittedOperationReceipt
    );
    inferred.explicit_outcome = Some(Outcome::NotApplied);
    assert_eq!(decision(&state, &inferred).0, Outcome::NotApplied);
    assert_eq!(
        selected_event(&state, &inferred),
        EventKind::DefinitiveRejection(NonExecutionProof::ExactPreWriteRejection)
    );
}

#[test]
fn proof_selection_preserves_review_absence_and_read_only_proofs() {
    let state = unknown_state();
    let mut review_absent = input(evidence(RequestTool::Other, Some(false), Some("error")));
    review_absent.explicit_outcome = Some(Outcome::Unknown);
    review_absent.proof = Some(NonExecutionProof::ReviewChildAbsent);
    assert_eq!(decision(&state, &review_absent).0, Outcome::NotApplied);
    assert_eq!(
        selected_event(&state, &review_absent),
        EventKind::DefinitiveRejection(NonExecutionProof::ReviewChildAbsent)
    );
    let mut read_only = input(evidence(RequestTool::OrchestrationRead, Some(false), None));
    assert_eq!(decision(&state, &read_only).0, Outcome::NotApplied);
    assert_eq!(
        selected_event(&state, &read_only),
        EventKind::DefinitiveRejection(NonExecutionProof::ExactPreWriteRejection)
    );
    read_only.evidence.read_only = true;
    assert_eq!(decision(&state, &read_only).0, Outcome::NotApplied);
    assert_eq!(
        selected_event(&state, &read_only),
        EventKind::DefinitiveRejection(NonExecutionProof::ReadOnlyRejection)
    );
    let mut explicit_applied = input(evidence(RequestTool::Other, Some(false), Some("failure")));
    explicit_applied.explicit_outcome = Some(Outcome::Applied);
    explicit_applied.proof = Some(NonExecutionProof::ReviewChildAbsent);
    assert_eq!(
        selected_event(&state, &explicit_applied),
        EventKind::AppliedResponse
    );
    let mut explicit_unknown = input(evidence(RequestTool::Other, Some(false), Some("failure")));
    explicit_unknown.explicit_outcome = Some(Outcome::Unknown);
    assert_eq!(
        selected_event(&state, &explicit_unknown),
        EventKind::Failure(studio_operations::FailureKind::InvalidPostExecutionResponse)
    );
}

#[test]
fn duplicate_settlement_is_no_change_but_changed_unknown_result_is_persisted() {
    let state = dispatched_state();
    let unknown = input(evidence(RequestTool::Other, None, None));
    let saved = decide_settlement(&state, &unknown)
        .expect("settlement")
        .state;
    let mut same_result = unknown.clone();
    same_result.result_changed = false;
    let unchanged = decide_settlement(&saved, &same_result).expect("duplicate settlement");
    assert_eq!(unchanged.kind, SettlementKind::NoChange);
    assert!(unchanged.intents.is_empty());
    let updated = decide_settlement(&saved, &unknown).expect("changed unknown response");
    assert_eq!(updated.kind, SettlementKind::Persisted);
    assert_eq!(
        updated.state.operation.as_ref().map(|op| op.state),
        Some(OperationState::Unknown)
    );

    let applied = decide_settlement(
        &dispatched_state(),
        &input(evidence(RequestTool::Other, Some(true), None)),
    )
    .expect("applied settlement")
    .state;
    let changed_terminal = decide_settlement(&applied, &unknown).expect("terminal replay");
    assert_eq!(changed_terminal.kind, SettlementKind::NoChange);
    assert!(changed_terminal.intents.is_empty());
}

#[test]
fn settlement_requires_operation_state() {
    let error = decide_settlement(
        &State::empty(StateRevision(0)),
        &input(evidence(RequestTool::Other, None, None)),
    );
    assert_eq!(error, Err(TransitionError::OperationMissing));
}

#[test]
fn absent_receipt_is_always_unknown() {
    assert_eq!(
        studio_operations::missing_receipt_outcome(),
        Outcome::Unknown
    );
}

#[test]
fn fault_after_nested_commit_preserves_uncertainty_and_never_dispatches_again() {
    let id = RequestId::from("request-a");
    let mut store = MemoryStore::default();
    store.requests.insert(id.clone(), dispatched_state());
    store.operation_receipts.insert(id.clone(), true);
    store.fail_next = Some(FailureWindow::BeforeReceipt);
    let mut settled = input(evidence(
        RequestTool::OrchestrationTask,
        Some(false),
        Some("Unknown work item"),
    ));
    settled.operation_receipt_present = store.operation_receipts.get(&id).copied().unwrap_or(false);
    assert_eq!(
        settle_in_store(&mut store, &settled),
        Err(StoreError::Injected)
    );
    assert_eq!(store.writes, 0);
    assert!(store.operation_receipts.get(&id).copied().unwrap_or(false));
    let result = settle_in_store(&mut store, &settled).expect("settlement retry");
    assert_eq!(
        result.state.operation.as_ref().map(|op| op.state),
        Some(OperationState::Unknown)
    );
    assert_eq!(result.event, EventKind::CommittedOperationReceipt);
    assert!(store.operation_receipts.get(&id).copied().unwrap_or(false));
    assert_eq!(store.effects, 0);
}

#[test]
fn result_commit_before_receipt_retries_from_saved_result() {
    let id = RequestId::from("request-a");
    let mut store = MemoryStore::default();
    store.requests.insert(id.clone(), dispatched_state());
    let saved = evidence(RequestTool::Other, Some(true), None);
    store.results.insert(id.clone(), saved.clone());
    store.fail_next = Some(FailureWindow::BeforeReceipt);
    let mut settled = input(saved);
    assert_eq!(
        settle_in_store(&mut store, &settled),
        Err(StoreError::Injected)
    );
    assert_eq!(store.results.get(&id), Some(&settled.evidence));
    assert_eq!(store.writes, 0);
    settled.evidence = store
        .results
        .get(&id)
        .cloned()
        .unwrap_or_else(|| evidence(RequestTool::Other, None, None));
    let retry = settle_in_store(&mut store, &settled).expect("saved result settles");
    assert_eq!(
        retry.state.operation.as_ref().map(|op| op.state),
        Some(OperationState::Applied)
    );
    assert_eq!(store.writes, 1);
}

#[test]
fn receipt_commit_before_reply_replays_without_second_write_or_effect() {
    let id = RequestId::from("request-a");
    let mut store = MemoryStore::default();
    store.requests.insert(id.clone(), dispatched_state());
    let result = evidence(RequestTool::Other, Some(true), None);
    store.results.insert(id.clone(), result.clone());
    store.fail_next = Some(FailureWindow::AfterReceipt);
    let mut settled = input(result.clone());
    assert_eq!(
        settle_in_store(&mut store, &settled),
        Err(StoreError::Injected)
    );
    assert_eq!(store.results.get(&id), Some(&result));
    assert_eq!(store.writes, 1);
    settled.result_changed = false;
    let retry = settle_in_store(&mut store, &settled).expect("receipt replay");
    assert_eq!(retry.kind, SettlementKind::NoChange);
    assert_eq!(store.writes, 1);
    assert_eq!(store.effects, 0);
}
