use studio_operations::{
    AccountId, AccountThreadEpoch, AgentId, ContentFingerprint, DecisionKind, Event, EventId,
    EventKind, FailureKind, IdentityScope, NonExecutionProof, OperationIdentity, OperationState,
    OperationTombstone, Outcome, ProcessGeneration, RequestId, SettledOutcome, State,
    StateRevision, ThreadId, TransitionError, transition,
};

fn identity(request_id: &str) -> OperationIdentity {
    OperationIdentity {
        scope: IdentityScope::ToolRequest {
            account: AccountId::from("account-a"),
            thread: ThreadId::from("thread-a"),
        },
        request_id: RequestId::from(request_id),
    }
}

fn native_identity(request_id: &str) -> OperationIdentity {
    OperationIdentity {
        scope: IdentityScope::NativeAction,
        request_id: RequestId::from(request_id),
    }
}

fn attempt(epoch: u64, generation: Option<u64>) -> studio_operations::AttemptIdentity {
    studio_operations::AttemptIdentity {
        agent: AgentId::from("agent-a"),
        account: AccountId::from("account-a"),
        thread: ThreadId::from("thread-a"),
        epoch: AccountThreadEpoch(epoch),
        generation: generation.map(ProcessGeneration),
    }
}

fn event(
    identity: OperationIdentity,
    attempt: studio_operations::AttemptIdentity,
    expected_revision: StateRevision,
    kind: EventKind,
) -> Event {
    Event {
        id: EventId::from("event-a"),
        expected_revision,
        identity,
        attempt,
        kind,
    }
}

fn content(value: u8) -> ContentFingerprint {
    let mut bytes = [0; 32];
    bytes[0] = value;
    ContentFingerprint(bytes)
}

fn apply(
    state: &State,
    identity: OperationIdentity,
    attempt: studio_operations::AttemptIdentity,
    kind: EventKind,
) -> studio_operations::Decision {
    transition(state, &event(identity, attempt, state.revision, kind))
}

fn admitted(attempt: studio_operations::AttemptIdentity) -> State {
    let initial = State::empty(StateRevision(0));
    let decision = apply(
        &initial,
        identity("request-a"),
        attempt,
        EventKind::Admit {
            content: content(1),
        },
    );
    assert_eq!(decision.kind, DecisionKind::Transitioned);
    assert_eq!(decision.expected_revision, StateRevision(0));
    assert_eq!(decision.next_state.revision, StateRevision(1));
    assert_eq!(decision.intents.len(), 1);
    decision.next_state
}

fn state_is(state: &State, expected: OperationState) {
    assert_eq!(state.operation.as_ref().map(|op| op.state), Some(expected));
}

/// R-14: same scoped ID and content return the saved operation, including its
/// unresolved state, without emitting another dispatch intent.
#[test]
fn r14_same_id_same_content_returns_saved_or_unresolved_state() {
    let initial = State::empty(StateRevision(0));
    let first = apply(
        &initial,
        identity("request-a"),
        attempt(4, None),
        EventKind::Admit {
            content: content(1),
        },
    );
    let dispatched = apply(
        &first.next_state,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    let replay = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::Admit {
            content: content(1),
        },
    );
    assert_eq!(replay.kind, DecisionKind::ReturnExisting);
    assert_eq!(replay.next_state, dispatched.next_state);
    assert!(replay.intents.is_empty());

    let timeout = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::Failure(FailureKind::Timeout),
    );
    let unresolved_retry = apply(
        &timeout.next_state,
        identity("request-a"),
        attempt(5, Some(8)),
        EventKind::Admit {
            content: content(1),
        },
    );
    assert_eq!(unresolved_retry.kind, DecisionKind::ReturnExisting);
    assert_eq!(
        unresolved_retry
            .next_state
            .operation
            .as_ref()
            .map(|operation| operation.state),
        Some(OperationState::Unknown)
    );
}

/// R-14: changed content under one scoped request identity fails before effects.
#[test]
fn r14_same_id_different_content_rejects_before_any_effect() {
    let initial = State::empty(StateRevision(0));
    let first = apply(
        &initial,
        identity("request-a"),
        attempt(4, None),
        EventKind::Admit {
            content: content(1),
        },
    );
    let conflict = apply(
        &first.next_state,
        identity("request-a"),
        attempt(4, None),
        EventKind::Admit {
            content: content(2),
        },
    );
    assert_eq!(
        conflict.kind,
        DecisionKind::Rejected(TransitionError::DifferentContent)
    );
    assert!(conflict.intents.is_empty());
    assert_eq!(conflict.next_state, first.next_state);
}

/// R-14: epoch is attempt scope, not request identity; retries cannot dispatch anew.
#[test]
fn r14_same_id_same_content_newer_epoch_returns_existing_without_dispatch() {
    let reserved = admitted(attempt(4, None));
    let dispatched = apply(
        &reserved,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    let retry = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(5, Some(9)),
        EventKind::Admit {
            content: content(1),
        },
    );
    assert_eq!(retry.kind, DecisionKind::ReturnExisting);
    assert!(retry.intents.is_empty());
    state_is(&retry.next_state, OperationState::Dispatched);
}

/// R-14: compaction retains enough typed identity to prevent a fresh dispatch.
#[test]
fn r14_compacted_identity_retry_returns_saved_outcome_without_dispatch() {
    let tombstone = OperationTombstone {
        identity: identity("request-a"),
        content: content(1),
        attempt: attempt(4, Some(8)),
        outcome: SettledOutcome::Applied,
    };
    let compacted = State::compacted(StateRevision(3), tombstone);
    let retry = apply(
        &compacted,
        identity("request-a"),
        attempt(5, Some(9)),
        EventKind::Admit {
            content: content(1),
        },
    );
    assert_eq!(retry.kind, DecisionKind::ReturnExisting);
    assert_eq!(retry.next_state, compacted);
    assert_eq!(
        retry.next_state.outcome(),
        Some(studio_operations::Outcome::Applied)
    );
    assert!(retry.intents.is_empty());

    let redispatch = apply(
        &compacted,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    assert_eq!(
        redispatch.kind,
        DecisionKind::Rejected(TransitionError::InvalidTransition)
    );
    assert!(redispatch.intents.is_empty());

    let changed = apply(
        &compacted,
        identity("request-a"),
        attempt(5, Some(9)),
        EventKind::Admit {
            content: content(2),
        },
    );
    assert_eq!(
        changed.kind,
        DecisionKind::Rejected(TransitionError::DifferentContent)
    );
    assert!(changed.intents.is_empty());
}

/// R-18: delivery of a dispatch claim twice emits at most one dispatch intent.
#[test]
fn r18_duplicate_event_delivery_is_idempotent() {
    let reserved = admitted(attempt(4, None));
    let dispatch_event = event(
        identity("request-a"),
        attempt(4, Some(8)),
        reserved.revision,
        EventKind::ClaimDispatch,
    );
    let dispatched = transition(&reserved, &dispatch_event);
    let replay = transition(&dispatched.next_state, &dispatch_event);
    assert_eq!(replay.kind, DecisionKind::NoEffect);
    assert!(replay.intents.is_empty());
    assert_eq!(replay.next_state, dispatched.next_state);
}

/// R-18: an absent slot rejects a response; a saved response can settle a
/// reserved legacy receipt before the current process dispatches it.
#[test]
fn r18_saved_result_settles_reserved_but_not_absent_operation() {
    let empty = State::empty(StateRevision(0));
    let response = apply(
        &empty,
        identity("request-a"),
        attempt(4, None),
        EventKind::AppliedResponse,
    );
    assert_eq!(
        response.kind,
        DecisionKind::Rejected(TransitionError::OperationMissing)
    );
    assert!(response.intents.is_empty());

    let reserved = admitted(attempt(4, None));
    let response = apply(
        &reserved,
        identity("request-a"),
        attempt(4, None),
        EventKind::AppliedResponse,
    );
    assert_eq!(response.kind, DecisionKind::Transitioned);
    assert_eq!(response.next_state.outcome(), Some(Outcome::Applied));
}

/// R-18: old process generations and attempt epochs cannot change current state.
#[test]
fn r18_stale_generation_and_epoch_events_are_rejected() {
    let reserved = admitted(attempt(4, None));
    let dispatched = apply(
        &reserved,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    let stale_generation = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(4, Some(7)),
        EventKind::AppliedResponse,
    );
    assert_eq!(
        stale_generation.kind,
        DecisionKind::Rejected(TransitionError::StaleGeneration)
    );
    let stale_epoch = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(5, Some(8)),
        EventKind::AppliedResponse,
    );
    assert_eq!(
        stale_epoch.kind,
        DecisionKind::Rejected(TransitionError::StaleEpoch)
    );
    assert_eq!(stale_epoch.next_state, dispatched.next_state);

    let mut changed_context = attempt(4, Some(8));
    changed_context.thread = ThreadId::from("replacement-thread");
    let stale_context = apply(
        &dispatched.next_state,
        identity("request-a"),
        changed_context,
        EventKind::AppliedResponse,
    );
    assert_eq!(
        stale_context.kind,
        DecisionKind::Rejected(TransitionError::StaleAttemptContext)
    );
}

/// R-18: cancellation before dispatch proves no effect; after dispatch it only
/// records intent and leaves the outcome unresolved until a response/evidence.
#[test]
fn r18_cancellation_before_and_after_dispatch_have_distinct_outcomes() {
    let reserved = admitted(attempt(4, None));
    let before = apply(
        &reserved,
        identity("request-a"),
        attempt(4, None),
        EventKind::Cancel,
    );
    state_is(&before.next_state, OperationState::CancelledBeforeDispatch);
    assert_eq!(
        before.next_state.outcome(),
        Some(studio_operations::Outcome::NotApplied)
    );
    assert!(
        !before
            .intents
            .iter()
            .any(|intent| matches!(intent, studio_operations::EffectIntent::Dispatch { .. }))
    );

    let dispatched = apply(
        &reserved,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    let after = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::Cancel,
    );
    state_is(&after.next_state, OperationState::CancelRequested);
    assert_eq!(after.next_state.outcome(), None);
    assert!(after.intents.iter().any(|intent| matches!(
        intent,
        studio_operations::EffectIntent::RequestCancellation { .. }
    )));
}

/// R-15: timeout, disconnect, process loss, and invalid response stay unknown.
#[test]
fn r15_timeout_lost_connection_and_invalid_response_yield_unknown() {
    for failure in [
        FailureKind::Timeout,
        FailureKind::ConnectionLost,
        FailureKind::InvalidPostExecutionResponse,
        FailureKind::ProcessLost,
    ] {
        let reserved = admitted(attempt(4, None));
        let dispatched = apply(
            &reserved,
            identity("request-a"),
            attempt(4, Some(8)),
            EventKind::ClaimDispatch,
        );
        let failed = apply(
            &dispatched.next_state,
            identity("request-a"),
            attempt(4, Some(8)),
            EventKind::Failure(failure),
        );
        assert_eq!(
            failed.next_state.outcome(),
            Some(studio_operations::Outcome::Unknown)
        );
        assert_eq!(
            failed
                .next_state
                .operation
                .as_ref()
                .map(|operation| operation.state),
            Some(OperationState::Unknown)
        );
    }
}

#[test]
fn r15_cached_failure_before_local_dispatch_remains_unknown() {
    let reserved = admitted(attempt(4, None));
    let failed = apply(
        &reserved,
        identity("request-a"),
        attempt(4, None),
        EventKind::Failure(FailureKind::InvalidPostExecutionResponse),
    );
    assert_eq!(failed.next_state.outcome(), Some(Outcome::Unknown));
    assert_eq!(
        failed
            .next_state
            .operation
            .as_ref()
            .map(|operation| operation.state),
        Some(OperationState::Unknown)
    );
}

/// R-14/R-15: absence of a receipt says unknown, never non-execution.
#[test]
fn r14_missing_receipt_alone_does_not_prove_nonexecution() {
    let empty = State::empty(StateRevision(0));
    let missing = apply(
        &empty,
        identity("missing"),
        attempt(4, None),
        EventKind::ReceiptMissing,
    );
    assert_eq!(missing.kind, DecisionKind::ReceiptMissingUnknown);
    assert!(missing.next_state.operation.is_none());
    assert!(missing.intents.is_empty());

    let unknown = unknown_state();
    let missing = apply(
        &unknown,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ReceiptMissing,
    );
    assert_eq!(missing.kind, DecisionKind::ReceiptMissingUnknown);
    assert_eq!(
        missing.next_state.outcome(),
        Some(studio_operations::Outcome::Unknown)
    );
}

/// R-15: a missing lookup cannot contradict an already saved definitive outcome.
#[test]
fn r15_missing_receipt_cannot_override_saved_definitive_outcome() {
    let reserved = admitted(attempt(4, None));
    let applied = apply(
        &apply(
            &reserved,
            identity("request-a"),
            attempt(4, Some(8)),
            EventKind::ClaimDispatch,
        )
        .next_state,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::AppliedResponse,
    )
    .next_state;
    let missing_after_applied = apply(
        &applied,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ReceiptMissing,
    );
    assert_eq!(
        missing_after_applied.kind,
        DecisionKind::Rejected(TransitionError::ConflictingEvidence)
    );
    assert_eq!(missing_after_applied.next_state, applied);
    assert!(missing_after_applied.intents.is_empty());

    let not_applied = apply(
        &reserved,
        identity("request-a"),
        attempt(4, None),
        EventKind::Cancel,
    )
    .next_state;
    let missing_after_not_applied = apply(
        &not_applied,
        identity("request-a"),
        attempt(4, None),
        EventKind::ReceiptMissing,
    );
    assert_eq!(
        missing_after_not_applied.kind,
        DecisionKind::Rejected(TransitionError::ConflictingEvidence)
    );
    assert_eq!(missing_after_not_applied.next_state, not_applied);
    assert!(missing_after_not_applied.intents.is_empty());
}

/// A committed operation receipt next to a failed tool result does not settle
/// whether the tool request itself applied.
#[test]
fn committed_receipt_with_inferred_failure_stays_unknown() {
    let dispatched = apply(
        &admitted(attempt(4, None)),
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    )
    .next_state;
    let receipt_with_failure = apply(
        &dispatched,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::CommittedOperationReceipt,
    );
    assert_eq!(
        receipt_with_failure.next_state.outcome(),
        Some(Outcome::Unknown)
    );

    let unknown = unknown_state();
    let recovered = apply(
        &unknown,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::CommittedOperationReceipt,
    );
    assert_eq!(
        recovered
            .next_state
            .operation
            .as_ref()
            .map(|operation| operation.identity.request_id.clone()),
        Some(RequestId::from("request-a"))
    );
    assert_eq!(recovered.next_state.outcome(), Some(Outcome::Unknown));
    assert!(recovered.intents.is_empty());
}

#[test]
fn late_valid_result_is_distinct_from_committed_operation_receipt() {
    let recovered = apply(
        &unknown_state(),
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::AppliedResponse,
    );
    assert_eq!(recovered.next_state.outcome(), Some(Outcome::Applied));
}

#[test]
fn exact_prewrite_rejection_resolves_unknown_in_core() {
    let recovered = apply(
        &unknown_state(),
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::DefinitiveRejection(NonExecutionProof::ExactPreWriteRejection),
    );
    assert_eq!(recovered.next_state.outcome(), Some(Outcome::NotApplied));
}

/// R-18: completion generation may be absent as in Python's general finish path;
/// a supplied generation is checked against the dispatch attempt.
#[test]
fn r18_optional_completion_generation_follows_python_rule() {
    let reserved = admitted(attempt(4, None));
    let dispatched = apply(
        &reserved,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    let absent = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(4, None),
        EventKind::AppliedResponse,
    );
    assert_eq!(
        absent.next_state.outcome(),
        Some(studio_operations::Outcome::Applied)
    );

    let dispatched = apply(
        &reserved,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    let matching = apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::Failure(FailureKind::ConnectionLost),
    );
    assert_eq!(
        matching.next_state.outcome(),
        Some(studio_operations::Outcome::Unknown)
    );
}

/// R-17: decisions carry the exact prior revision, and empty slots are versioned.
#[test]
fn r17_empty_slot_revision_is_checked_before_reservation() {
    let empty = State::empty(StateRevision(0));
    let stale = transition(
        &empty,
        &event(
            identity("request-a"),
            attempt(4, None),
            StateRevision(1),
            EventKind::Admit {
                content: content(1),
            },
        ),
    );
    assert_eq!(
        stale.kind,
        DecisionKind::Rejected(TransitionError::StaleRevision)
    );
    assert!(stale.intents.is_empty());
    let valid = transition(
        &empty,
        &event(
            identity("request-a"),
            attempt(4, None),
            StateRevision(0),
            EventKind::Admit {
                content: content(1),
            },
        ),
    );
    assert_eq!(valid.expected_revision, StateRevision(0));
    assert_eq!(valid.next_state.revision, StateRevision(1));

    let compacted_without_identity = State::empty(StateRevision(3));
    let rejected = transition(
        &compacted_without_identity,
        &event(
            identity("request-a"),
            attempt(4, None),
            StateRevision(3),
            EventKind::Admit {
                content: content(1),
            },
        ),
    );
    assert_eq!(
        rejected.kind,
        DecisionKind::Rejected(TransitionError::OperationAlreadyOccupied)
    );
}

/// R-17: revision overflow is rejected without panic or effect intent.
#[test]
fn r17_revision_exhaustion_is_an_explicit_rejection() {
    let mut full = admitted(attempt(4, None));
    full.revision = StateRevision(u64::MAX);
    let result = transition(
        &full,
        &event(
            identity("request-a"),
            attempt(4, Some(8)),
            StateRevision(u64::MAX),
            EventKind::ClaimDispatch,
        ),
    );
    assert_eq!(
        result.kind,
        DecisionKind::Rejected(TransitionError::RevisionExhausted)
    );
    assert!(result.intents.is_empty());
}

/// R-34: global native-action IDs retain their call-site-specific identity type.
#[test]
fn r34_native_action_scope_is_distinct_and_closed() {
    let initial = State::empty(StateRevision(0));
    let admitted = apply(
        &initial,
        native_identity("native-id"),
        attempt(4, None),
        EventKind::Admit {
            content: content(1),
        },
    );
    let other_scope = apply(
        &admitted.next_state,
        identity("native-id"),
        attempt(4, None),
        EventKind::Admit {
            content: content(1),
        },
    );
    assert_eq!(
        other_scope.kind,
        DecisionKind::Rejected(TransitionError::IdentityMismatch)
    );
}

fn unknown_state() -> State {
    let reserved = admitted(attempt(4, None));
    let dispatched = apply(
        &reserved,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::ClaimDispatch,
    );
    apply(
        &dispatched.next_state,
        identity("request-a"),
        attempt(4, Some(8)),
        EventKind::Failure(FailureKind::Timeout),
    )
    .next_state
}

/// R-14/R-15/R-18: generated traces preserve settled outcomes, emit at most one
/// dispatch intent, and settle `Unknown` only with proof that establishes the
/// tool request's outcome (a committed operation receipt alone does not).
#[test]
fn property_r14_r15_r18_generated_event_sequences_preserve_invariants() {
    // Exhaustively enumerate 9^5 short traces; this is deterministic and needs
    // no property-test runtime or external process.
    const ALPHABET: u64 = 9;
    const TRACE_LENGTH: u32 = 5;
    let total = ALPHABET.pow(TRACE_LENGTH);
    for mut encoded in 0..total {
        let mut state = admitted(attempt(4, None));
        let mut dispatch_count = 0;
        for _ in 0..TRACE_LENGTH {
            let old = state.clone();
            let token = encoded % ALPHABET;
            encoded /= ALPHABET;
            let kind = match token {
                0 => EventKind::ClaimDispatch,
                1 => EventKind::Cancel,
                2 => EventKind::AppliedResponse,
                3 => EventKind::DefinitiveRejection(NonExecutionProof::ExactPreWriteRejection),
                4 => EventKind::Failure(FailureKind::Timeout),
                5 => EventKind::CommittedOperationReceipt,
                6 => EventKind::NonExecutionEvidence(NonExecutionProof::ReviewChildAbsent),
                7 => EventKind::ReceiptMissing,
                _ => EventKind::Admit {
                    content: content(1),
                },
            };
            let decision = apply(
                &state,
                identity("request-a"),
                attempt(4, Some(8)),
                kind.clone(),
            );
            let emitted_dispatch = decision
                .intents
                .iter()
                .filter(|intent| matches!(intent, studio_operations::EffectIntent::Dispatch { .. }))
                .count();
            dispatch_count += emitted_dispatch;
            assert!(dispatch_count <= 1, "trace emitted multiple dispatches");

            if matches!(
                old.outcome(),
                Some(studio_operations::Outcome::Applied | studio_operations::Outcome::NotApplied)
            ) {
                assert_eq!(
                    decision.next_state.outcome(),
                    old.outcome(),
                    "settled outcome regressed"
                );
            }
            if old
                .operation
                .as_ref()
                .is_some_and(|op| op.state == OperationState::Unknown)
                && decision.next_state.outcome() != Some(studio_operations::Outcome::Unknown)
            {
                assert!(matches!(
                    kind,
                    EventKind::AppliedResponse
                        | EventKind::AppliedEvidence(_)
                        | EventKind::DefinitiveRejection(_)
                        | EventKind::NonExecutionEvidence(_)
                ));
            }
            state = decision.next_state;
        }
    }
}

/// AC-05: replay Python's recorded scenarios from a language-neutral JSON file.
#[test]
fn ac05_python_contract_cases_are_data_driven() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/python-contract-cases.json"))
            .expect("fixture JSON is valid");
    let cases = fixtures.as_array().expect("fixture root is an array");
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().expect("case has a name");
        let source_test = case["source_test"]
            .as_str()
            .expect("case cites Python test");
        assert!(!source_test.is_empty(), "{name} has source mapping");
        let mut state = State::empty(StateRevision(0));
        let operation_identity = OperationIdentity {
            scope: IdentityScope::ToolRequest {
                account: AccountId::from("account-a"),
                thread: ThreadId::from("thread-a"),
            },
            request_id: RequestId::from(name),
        };
        for step in case["steps"].as_array().expect("steps are an array") {
            let epoch = step["epoch"].as_u64().unwrap_or(4);
            let generation = step["generation"].as_u64();
            let kind = match step["kind"].as_str().expect("step has a kind") {
                "admit" => EventKind::Admit {
                    content: if step["content"] == "spawn-other" {
                        content(2)
                    } else {
                        content(1)
                    },
                },
                "dispatch" => EventKind::ClaimDispatch,
                "cancel" => EventKind::Cancel,
                "server_restart" => EventKind::ServerRestarted,
                "applied_response" => EventKind::AppliedResponse,
                "timeout" => EventKind::Failure(FailureKind::Timeout),
                "connection_lost" => EventKind::Failure(FailureKind::ConnectionLost),
                "invalid_response" => EventKind::Failure(FailureKind::InvalidPostExecutionResponse),
                "process_lost" => EventKind::Failure(FailureKind::ProcessLost),
                "committed_receipt" => EventKind::CommittedOperationReceipt,
                "receipt_missing" => EventKind::ReceiptMissing,
                other => panic!("unmatched fixture step {other} in {name}"),
            };
            let decision = apply(
                &state,
                operation_identity.clone(),
                attempt(epoch, generation),
                kind,
            );
            assert!(
                !matches!(
                    decision.kind,
                    DecisionKind::Rejected(TransitionError::InvalidTransition)
                ),
                "unmatched fixture step in {name}"
            );
            state = decision.next_state;
        }
        let expected = case["rust_state"].as_str().expect("case has Rust state");
        let actual = state
            .operation
            .as_ref()
            .map_or("Absent", |operation| match operation.state {
                OperationState::Reserved => "Reserved",
                OperationState::Dispatched => "Dispatched",
                OperationState::CancelRequested => "CancelRequested",
                OperationState::CancelledBeforeDispatch => "CancelledBeforeDispatch",
                OperationState::NotApplied => "NotApplied",
                OperationState::Applied => "Applied",
                OperationState::Unknown => "Unknown",
            });
        assert_eq!(actual, expected, "{name} from {source_test}");
        assert!(
            case["python_stage"].is_string(),
            "{name} records Python stage"
        );
        assert!(
            case["python_outcome"].is_string(),
            "{name} records Python outcome"
        );
    }
}

#[test]
fn server_restart_settles_reserved_and_preserves_dispatched_uncertainty() {
    let identity = OperationIdentity {
        scope: IdentityScope::ToolRequest {
            account: AccountId::from("account-a"),
            thread: ThreadId::from("thread-a"),
        },
        request_id: RequestId::from("restart"),
    };
    let reserved = apply(
        &State::empty(StateRevision(0)),
        identity.clone(),
        attempt(4, None),
        EventKind::Admit {
            content: content(1),
        },
    )
    .next_state;
    assert_eq!(
        apply(
            &reserved,
            identity.clone(),
            attempt(4, None),
            EventKind::ServerRestarted,
        )
        .next_state
        .outcome(),
        Some(Outcome::NotApplied)
    );
    let dispatched = apply(
        &reserved,
        identity.clone(),
        attempt(4, Some(7)),
        EventKind::ClaimDispatch,
    )
    .next_state;
    assert_eq!(
        apply(
            &dispatched,
            identity,
            attempt(4, Some(7)),
            EventKind::ServerRestarted,
        )
        .next_state
        .outcome(),
        Some(Outcome::Unknown)
    );
}
