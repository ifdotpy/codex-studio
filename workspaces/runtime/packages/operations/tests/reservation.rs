use std::collections::HashMap;

use studio_operations::{
    AccountId, AccountThreadEpoch, AgentId, AttemptIdentity, ContentFingerprint, IdentityScope,
    OperationIdentity, RequestAlias, RequestId, ReservationDecision, ReservationInput,
    ReservationSnapshot, ReservationStore, ReservationWriteIntent, State, decide_reservation,
    reserve_in_store,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InjectedFailure {
    BeforeApply,
    AfterCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoreFailure;

#[derive(Default)]
struct MemoryStore {
    requests: HashMap<RequestId, State>,
    aliases: Vec<RequestAlias>,
    next_failure: Option<InjectedFailure>,
    request_writes: usize,
    dispatches: usize,
}

impl ReservationStore for MemoryStore {
    type Error = StoreFailure;

    fn load_snapshot(
        &mut self,
        input: &ReservationInput,
    ) -> Result<ReservationSnapshot, Self::Error> {
        let request_id = &input.identity.request_id;
        Ok(ReservationSnapshot {
            existing: self.requests.get(request_id).cloned(),
        })
    }

    fn apply_intents(&mut self, intents: &[ReservationWriteIntent]) -> Result<(), Self::Error> {
        if self.next_failure == Some(InjectedFailure::BeforeApply) {
            self.next_failure = None;
            return Err(StoreFailure);
        }
        for intent in intents {
            match intent {
                ReservationWriteIntent::ReserveRequest { state } => {
                    let Some(operation) = state.operation.as_ref() else {
                        return Err(StoreFailure);
                    };
                    self.requests.insert(
                        operation.identity.request_id.clone(),
                        state.as_ref().clone(),
                    );
                    self.request_writes += 1;
                }
                ReservationWriteIntent::InsertAlias(alias) => {
                    if !self.aliases.contains(alias) {
                        self.aliases.push(alias.clone());
                    }
                }
            }
        }
        if self.next_failure == Some(InjectedFailure::AfterCommit) {
            self.next_failure = None;
            return Err(StoreFailure);
        }
        Ok(())
    }
}

fn input(
    id: &str,
    agent: &str,
    epoch: u64,
    fingerprint: u8,
    call_id: &str,
    alias_prefix: &str,
    request_alias: Option<&str>,
) -> ReservationInput {
    let account = AccountId::from("account-a");
    let thread = studio_operations::ThreadId::from("thread-a");
    let request_id = RequestId::from(id);
    ReservationInput {
        identity: OperationIdentity {
            scope: IdentityScope::ToolRequest {
                account: account.clone(),
                thread: thread.clone(),
            },
            request_id: request_id.clone(),
        },
        alias_target: RequestId::from(id),
        content: ContentFingerprint([fingerprint; 32]),
        attempt: AttemptIdentity {
            agent: AgentId::from(agent),
            account,
            thread,
            epoch: AccountThreadEpoch(epoch),
            generation: None,
        },
        call_id: call_id.to_owned(),
        alias_prefix: alias_prefix.to_owned(),
        request_alias: request_alias.map(str::to_owned),
    }
}

fn admitted_state(request: &ReservationInput) -> State {
    let decision = decide_reservation(&ReservationSnapshot { existing: None }, request);
    let ReservationDecision::Admitted { state, .. } = decision else {
        panic!("new request should be admitted");
    };
    state
}

#[test]
fn new_request_emits_one_row_and_all_alias_intents() {
    let request = input(
        "request-a",
        "agent-a",
        4,
        7,
        "call-a",
        "thread:",
        Some("request-a"),
    );
    let decision = decide_reservation(&ReservationSnapshot { existing: None }, &request);
    let ReservationDecision::Admitted { state, intents } = decision else {
        panic!("new request should be admitted");
    };
    assert_eq!(
        state.operation.as_ref().map(|op| op.attempt.epoch),
        Some(AccountThreadEpoch(4))
    );
    assert!(matches!(
        intents.first(),
        Some(ReservationWriteIntent::ReserveRequest { .. })
    ));
    assert_eq!(intents.len(), 4);
}

#[test]
fn empty_request_id_alias_is_omitted() {
    let request = input("request-a", "agent-a", 4, 7, "call-a", "thread:", Some(""));
    let ReservationDecision::Admitted { intents, .. } =
        decide_reservation(&ReservationSnapshot { existing: None }, &request)
    else {
        panic!("new request should be admitted");
    };
    assert_eq!(intents.len(), 3);
    assert!(intents.iter().all(|intent| !matches!(
        intent,
        ReservationWriteIntent::InsertAlias(alias) if alias.alias.is_empty()
    )));
}

#[test]
fn existing_snapshot_without_an_operation_is_rejected() {
    let request = input("request-a", "agent-a", 4, 7, "call-a", "thread:", None);
    let snapshot = ReservationSnapshot {
        existing: Some(State::empty(studio_operations::StateRevision(1))),
    };
    assert!(matches!(
        decide_reservation(&snapshot, &request),
        ReservationDecision::Rejected {
            error: studio_operations::TransitionError::OperationMissing,
            ..
        }
    ));
}

#[test]
fn same_id_and_content_returns_saved_request_without_a_second_row_write() {
    let mut store = MemoryStore::default();
    let first = input("request-a", "agent-a", 4, 7, "call-a", "thread:", None);
    assert!(matches!(
        reserve_in_store(&mut store, &first),
        Ok(ReservationDecision::Admitted { .. })
    ));
    let retry = input("request-a", "agent-a", 9, 7, "call-b", "thread:", None);
    let decision =
        reserve_in_store(&mut store, &retry).expect("retry should read existing request");
    let ReservationDecision::ReturnExisting { intents, state } = decision else {
        panic!("same identity and content should return the saved request");
    };
    assert_eq!(
        state.operation.as_ref().map(|op| op.attempt.epoch),
        Some(AccountThreadEpoch(4))
    );
    assert_eq!(intents.len(), 2);
    assert!(matches!(
        &intents[0],
        ReservationWriteIntent::InsertAlias(alias) if alias.alias == "call-b"
    ));
    assert!(matches!(
        &intents[1],
        ReservationWriteIntent::InsertAlias(alias) if alias.alias == "thread:call-b"
    ));
    assert_eq!(store.request_writes, 1);
    assert_eq!(store.dispatches, 0);
}

#[test]
fn same_id_with_different_content_is_rejected_without_intents() {
    let original = input("request-a", "agent-a", 4, 7, "call-a", "thread:", None);
    let changed = input("request-a", "agent-a", 4, 8, "call-a", "thread:", None);
    let snapshot = ReservationSnapshot {
        existing: Some(admitted_state(&original)),
    };
    assert!(matches!(
        decide_reservation(&snapshot, &changed),
        ReservationDecision::Rejected {
            error: studio_operations::TransitionError::DifferentContent,
            ..
        }
    ));
}

#[test]
fn changed_actor_fingerprint_is_rejected_for_the_same_request_id() {
    let original = input("request-a", "agent-a", 4, 7, "call-a", "thread:", None);
    let changed_actor = input("request-a", "agent-b", 4, 8, "call-a", "thread:", None);
    let snapshot = ReservationSnapshot {
        existing: Some(admitted_state(&original)),
    };
    assert!(matches!(
        decide_reservation(&snapshot, &changed_actor),
        ReservationDecision::Rejected {
            error: studio_operations::TransitionError::DifferentContent,
            ..
        }
    ));
}

#[test]
fn same_reservation_survives_stale_agent_epoch() {
    let original = input("request-a", "agent-a", 4, 7, "call-a", "thread:", None);
    let advanced_epoch = input("request-a", "agent-a", 5, 7, "call-a", "thread:", None);
    let snapshot = ReservationSnapshot {
        existing: Some(admitted_state(&original)),
    };
    assert!(matches!(
        decide_reservation(&snapshot, &advanced_epoch),
        ReservationDecision::ReturnExisting { .. }
    ));
}

#[test]
fn conflicting_short_aliases_remain_distinct_and_ambiguous() {
    let mut store = MemoryStore::default();
    let first = input("request-a", "agent-a", 4, 7, "collision", "thread:", None);
    let second = input("request-b", "agent-a", 4, 8, "collision", "thread:", None);
    reserve_in_store(&mut store, &first).expect("first request admitted");
    reserve_in_store(&mut store, &second).expect("second canonical request admitted");
    let matches = store
        .aliases
        .iter()
        .filter(|alias| alias.alias == "collision")
        .count();
    assert_eq!(matches, 2);
    assert_eq!(store.requests.len(), 2);
}

#[test]
fn failure_before_row_write_leaves_no_row_alias_or_effect_and_retry_admits() {
    let request = input("request-a", "agent-a", 4, 7, "call-a", "thread:", None);
    let mut store = MemoryStore {
        next_failure: Some(InjectedFailure::BeforeApply),
        ..MemoryStore::default()
    };
    assert_eq!(reserve_in_store(&mut store, &request), Err(StoreFailure));
    assert!(store.requests.is_empty());
    assert!(store.aliases.is_empty());
    assert_eq!(store.dispatches, 0);
    assert!(matches!(
        reserve_in_store(&mut store, &request),
        Ok(ReservationDecision::Admitted { .. })
    ));
    assert_eq!(store.requests.len(), 1);
    assert_eq!(store.aliases.len(), 2);
    assert_eq!(store.dispatches, 0);
}

#[test]
fn failure_after_commit_before_response_retries_existing_without_second_dispatch() {
    let request = input("request-a", "agent-a", 4, 7, "call-a", "thread:", None);
    let mut store = MemoryStore {
        next_failure: Some(InjectedFailure::AfterCommit),
        ..MemoryStore::default()
    };
    assert_eq!(reserve_in_store(&mut store, &request), Err(StoreFailure));
    assert_eq!(store.requests.len(), 1);
    assert_eq!(store.aliases.len(), 2);
    let retry =
        reserve_in_store(&mut store, &request).expect("retry returns committed reservation");
    assert!(matches!(retry, ReservationDecision::ReturnExisting { .. }));
    assert_eq!(store.request_writes, 1);
    assert_eq!(store.dispatches, 0);
}
