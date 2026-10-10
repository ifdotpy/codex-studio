//! Pure operation lifecycle transitions.
//!
//! The caller owns validation, persistence, authorization, and effects. It must
//! compare [`Decision::expected_revision`] in the same transaction that stores
//! [`Decision::next_state`] and its [`EffectIntent`] values.

use std::fmt;

macro_rules! string_id {
    ($name:ident) => {
        #[doc = concat!("A named ", stringify!($name), " value.")]
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

macro_rules! number_id {
    ($name:ident) => {
        #[doc = concat!("A named ", stringify!($name), " value.")]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        pub struct $name(pub u64);
    };
}

string_id!(AccountId);
string_id!(AgentId);
string_id!(ThreadId);
string_id!(RequestId);
string_id!(EventId);
number_id!(AccountThreadEpoch);
number_id!(ProcessGeneration);
number_id!(StateRevision);

/// A caller-computed canonical content digest. Hashing/canonicalization is an
/// adapter responsibility and is intentionally outside this crate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ContentFingerprint(pub [u8; 32]);

/// Existing Python call sites use different request-key scopes. Keep those key
/// shapes closed and explicit; attempt epoch and process generation are not
/// identity components.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum IdentityScope {
    /// `account + thread + adapter-normalized call/request ID`.
    ToolRequest {
        account: AccountId,
        thread: ThreadId,
    },
    /// A global native-action request ID; agent/action/context are content.
    NativeAction,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OperationIdentity {
    pub scope: IdentityScope,
    pub request_id: RequestId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AttemptIdentity {
    pub agent: AgentId,
    pub account: AccountId,
    pub thread: ThreadId,
    pub epoch: AccountThreadEpoch,
    pub generation: Option<ProcessGeneration>,
}

/// Lifecycle states. `CancelledBeforeDispatch` records a local cancellation;
/// operation outcome remains separately available through [`Outcome`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OperationState {
    Reserved,
    Dispatched,
    CancelRequested,
    CancelledBeforeDispatch,
    NotApplied,
    Applied,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Outcome {
    NotApplied,
    Applied,
    Unknown,
}

/// Outcomes that may be retained after compacting a settled operation record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SettledOutcome {
    NotApplied,
    Applied,
}

/// Minimum durable identity metadata required to prevent a compacted settled
/// operation from being admitted and dispatched a second time.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OperationTombstone {
    pub identity: OperationIdentity,
    pub content: ContentFingerprint,
    pub attempt: AttemptIdentity,
    pub outcome: SettledOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Operation {
    pub identity: OperationIdentity,
    pub content: ContentFingerprint,
    pub attempt: AttemptIdentity,
    pub state: OperationState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct State {
    /// Revision of this operation slot, including an empty/tombstoned slot.
    pub revision: StateRevision,
    pub operation: Option<Operation>,
    /// Retained lookup metadata after a settled operation is compacted.
    pub tombstone: Option<OperationTombstone>,
}

impl State {
    /// Construct a never-seen slot snapshot. Compacted identities must use
    /// [`State::compacted`] to retain their identity, content, and settled outcome.
    #[must_use]
    pub const fn empty(revision: StateRevision) -> Self {
        Self {
            revision,
            operation: None,
            tombstone: None,
        }
    }

    /// Construct a compacted settled slot. Callers must assign the revision
    /// produced by their atomic compaction write.
    #[must_use]
    pub fn compacted(revision: StateRevision, tombstone: OperationTombstone) -> Self {
        Self {
            revision,
            operation: None,
            tombstone: Some(tombstone),
        }
    }

    #[must_use]
    pub fn outcome(&self) -> Option<Outcome> {
        if let Some(operation) = &self.operation {
            return match operation.state {
                OperationState::CancelledBeforeDispatch | OperationState::NotApplied => {
                    Some(Outcome::NotApplied)
                }
                OperationState::Applied => Some(Outcome::Applied),
                OperationState::Unknown => Some(Outcome::Unknown),
                OperationState::Reserved
                | OperationState::Dispatched
                | OperationState::CancelRequested => None,
            };
        }
        self.tombstone
            .as_ref()
            .map(|tombstone| match tombstone.outcome {
                SettledOutcome::NotApplied => Outcome::NotApplied,
                SettledOutcome::Applied => Outcome::Applied,
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum NonExecutionProof {
    /// Cancellation won the local queued-versus-claim race.
    QueuedCancellation,
    /// A read-only request failed before any mutating operation could run.
    ReadOnlyRejection,
    /// The adapter established an exact pre-write rejection.
    ExactPreWriteRejection,
    /// Python review recovery found no deterministic child. This is weaker than
    /// direct pre-effect evidence, but is retained for call-site compatibility.
    ReviewChildAbsent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AppliedProof {
    CommittedOperationReceipt,
    LateValidResponse,
    ExactNativeAcceptance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FailureKind {
    Timeout,
    ConnectionLost,
    InvalidPostExecutionResponse,
    ProcessLost,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EventKind {
    Admit {
        content: ContentFingerprint,
    },
    ClaimDispatch,
    Cancel,
    AppliedResponse,
    DefinitiveRejection(NonExecutionProof),
    Failure(FailureKind),
    AppliedEvidence(AppliedProof),
    NonExecutionEvidence(NonExecutionProof),
    /// A lookup found no receipt. This does not prove that execution did not occur.
    ReceiptMissing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Event {
    pub id: EventId,
    pub expected_revision: StateRevision,
    pub identity: OperationIdentity,
    pub attempt: AttemptIdentity,
    pub kind: EventKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TransitionError {
    IdentityMismatch,
    StaleAttemptContext,
    DifferentContent,
    StaleRevision,
    StaleEpoch,
    StaleGeneration,
    GenerationUnavailable,
    OperationAlreadyOccupied,
    OperationMissing,
    InvalidTransition,
    UnsupportedProof,
    ConflictingEvidence,
    RevisionExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EffectIntent {
    Reserve {
        event_id: EventId,
        identity: OperationIdentity,
        content: ContentFingerprint,
        attempt: AttemptIdentity,
    },
    Dispatch {
        event_id: EventId,
        identity: OperationIdentity,
        attempt: AttemptIdentity,
    },
    RequestCancellation {
        event_id: EventId,
        identity: OperationIdentity,
    },
    PersistOutcome {
        event_id: EventId,
        identity: OperationIdentity,
        outcome: Outcome,
    },
    ReconcileEvidence {
        event_id: EventId,
        identity: OperationIdentity,
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DecisionKind {
    Transitioned,
    ReturnExisting,
    NoEffect,
    ReceiptMissingUnknown,
    Rejected(TransitionError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Decision {
    /// The revision the caller must compare before persisting this decision.
    pub expected_revision: StateRevision,
    pub next_state: State,
    pub kind: DecisionKind,
    pub intents: Vec<EffectIntent>,
}

fn decision(
    state: &State,
    kind: DecisionKind,
    intents: Vec<EffectIntent>,
    changed: bool,
) -> Decision {
    let expected_revision = state.revision;
    let next_state = if changed {
        let Some(next) = state.revision.0.checked_add(1) else {
            return Decision {
                expected_revision,
                next_state: state.clone(),
                kind: DecisionKind::Rejected(TransitionError::RevisionExhausted),
                intents: Vec::new(),
            };
        };
        let mut next_state = state.clone();
        next_state.revision = StateRevision(next);
        next_state
    } else {
        state.clone()
    };
    Decision {
        expected_revision,
        next_state,
        kind,
        intents,
    }
}

fn reject(state: &State, error: TransitionError) -> Decision {
    decision(state, DecisionKind::Rejected(error), Vec::new(), false)
}

fn no_effect(state: &State) -> Decision {
    decision(state, DecisionKind::NoEffect, Vec::new(), false)
}

fn matching_attempt(
    operation: &Operation,
    event: &Event,
    allow_generation_binding: bool,
) -> Result<(), TransitionError> {
    if operation.attempt.agent != event.attempt.agent
        || operation.attempt.account != event.attempt.account
        || operation.attempt.thread != event.attempt.thread
    {
        return Err(TransitionError::StaleAttemptContext);
    }
    if operation.attempt.epoch != event.attempt.epoch {
        return Err(TransitionError::StaleEpoch);
    }
    if let Some(generation) = event.attempt.generation {
        match operation.attempt.generation {
            Some(expected) if expected == generation => {}
            Some(_) => return Err(TransitionError::StaleGeneration),
            None if allow_generation_binding => {}
            None => return Err(TransitionError::GenerationUnavailable),
        }
    }
    Ok(())
}

fn same_settled_event(operation: &Operation, event: &Event) -> bool {
    matches!(
        (&operation.state, &event.kind),
        (OperationState::Applied, EventKind::AppliedResponse)
            | (
                OperationState::NotApplied | OperationState::CancelledBeforeDispatch,
                EventKind::DefinitiveRejection(_)
            )
            | (OperationState::Unknown, EventKind::Failure(_))
            | (OperationState::Applied, EventKind::AppliedEvidence(_))
            | (
                OperationState::NotApplied,
                EventKind::NonExecutionEvidence(_)
            )
    )
}

/// Compute one lifecycle decision without performing effects or accessing I/O.
///
/// Every state/event pair returns a decision. Malformed ordering, stale scope,
/// and conflicting evidence are represented by [`DecisionKind::Rejected`].
#[must_use]
pub fn transition(state: &State, event: &Event) -> Decision {
    let Some(operation) = state.operation.as_ref() else {
        if let Some(tombstone) = state.tombstone.as_ref() {
            return match &event.kind {
                EventKind::Admit { content } if event.identity == tombstone.identity => {
                    if *content == tombstone.content {
                        decision(state, DecisionKind::ReturnExisting, Vec::new(), false)
                    } else {
                        reject(state, TransitionError::DifferentContent)
                    }
                }
                EventKind::Admit { .. } => reject(state, TransitionError::IdentityMismatch),
                EventKind::ReceiptMissing => reject(state, TransitionError::ConflictingEvidence),
                _ => reject(state, TransitionError::InvalidTransition),
            };
        }
        return match &event.kind {
            EventKind::Admit { content } => {
                if state.revision != StateRevision(0) {
                    return reject(state, TransitionError::OperationAlreadyOccupied);
                }
                if event.expected_revision != state.revision {
                    return reject(state, TransitionError::StaleRevision);
                }
                let operation = Operation {
                    identity: event.identity.clone(),
                    content: *content,
                    attempt: event.attempt.clone(),
                    state: OperationState::Reserved,
                };
                let mut next_state = decision(
                    state,
                    DecisionKind::Transitioned,
                    vec![EffectIntent::Reserve {
                        event_id: event.id.clone(),
                        identity: event.identity.clone(),
                        content: *content,
                        attempt: event.attempt.clone(),
                    }],
                    true,
                );
                if matches!(&next_state.kind, DecisionKind::Transitioned) {
                    next_state.next_state.operation = Some(operation);
                }
                next_state
            }
            EventKind::ReceiptMissing => decision(
                state,
                DecisionKind::ReceiptMissingUnknown,
                Vec::new(),
                false,
            ),
            _ => reject(state, TransitionError::OperationMissing),
        };
    };

    if operation.identity != event.identity {
        return reject(state, TransitionError::IdentityMismatch);
    }

    if let EventKind::Admit { content } = &event.kind {
        if operation.content != *content {
            return reject(state, TransitionError::DifferentContent);
        }
        return decision(state, DecisionKind::ReturnExisting, Vec::new(), false);
    }

    if let Err(error) = matching_attempt(
        operation,
        event,
        matches!(&event.kind, EventKind::ClaimDispatch),
    ) {
        return reject(state, error);
    }

    if matches!(&event.kind, EventKind::ReceiptMissing) {
        if matches!(
            operation.state,
            OperationState::CancelledBeforeDispatch
                | OperationState::NotApplied
                | OperationState::Applied
        ) {
            return reject(state, TransitionError::ConflictingEvidence);
        }
        return decision(
            state,
            DecisionKind::ReceiptMissingUnknown,
            Vec::new(),
            false,
        );
    }

    // A replayed delivery is safe even if its original compare revision is now
    // behind the saved state. Phase semantics prevent a second effect.
    if same_settled_event(operation, event)
        || matches!(
            (&operation.state, &event.kind),
            (OperationState::Dispatched, EventKind::ClaimDispatch)
                | (OperationState::CancelRequested, EventKind::Cancel)
                | (OperationState::CancelledBeforeDispatch, EventKind::Cancel)
                | (OperationState::NotApplied, EventKind::Cancel)
                | (OperationState::Applied, EventKind::Cancel)
                | (OperationState::Unknown, EventKind::Cancel)
        )
    {
        return no_effect(state);
    }

    if event.expected_revision != state.revision {
        return reject(state, TransitionError::StaleRevision);
    }

    let mut next_operation = operation.clone();
    let mut intents = Vec::new();
    let kind = match (&operation.state, &event.kind) {
        (OperationState::Reserved, EventKind::ClaimDispatch) => {
            if let Some(generation) = event.attempt.generation {
                if operation
                    .attempt
                    .generation
                    .is_some_and(|saved| saved != generation)
                {
                    return reject(state, TransitionError::StaleGeneration);
                }
                next_operation.attempt.generation = Some(generation);
            }
            next_operation.state = OperationState::Dispatched;
            intents.push(EffectIntent::Dispatch {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
                attempt: next_operation.attempt.clone(),
            });
            DecisionKind::Transitioned
        }
        (OperationState::Reserved, EventKind::Cancel) => {
            next_operation.state = OperationState::CancelledBeforeDispatch;
            intents.push(EffectIntent::PersistOutcome {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
                outcome: Outcome::NotApplied,
            });
            DecisionKind::Transitioned
        }
        (OperationState::Dispatched, EventKind::Cancel) => {
            next_operation.state = OperationState::CancelRequested;
            intents.push(EffectIntent::RequestCancellation {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
            });
            DecisionKind::Transitioned
        }
        (
            OperationState::Dispatched | OperationState::CancelRequested,
            EventKind::AppliedResponse,
        ) => {
            next_operation.state = OperationState::Applied;
            intents.push(EffectIntent::PersistOutcome {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
                outcome: Outcome::Applied,
            });
            DecisionKind::Transitioned
        }
        (
            OperationState::Reserved | OperationState::Dispatched | OperationState::CancelRequested,
            EventKind::DefinitiveRejection(proof),
        ) => {
            if *proof == NonExecutionProof::ReviewChildAbsent
                || *proof == NonExecutionProof::QueuedCancellation
                    && operation.state != OperationState::Reserved
            {
                return reject(state, TransitionError::UnsupportedProof);
            }
            next_operation.state = OperationState::NotApplied;
            intents.push(EffectIntent::PersistOutcome {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
                outcome: Outcome::NotApplied,
            });
            DecisionKind::Transitioned
        }
        (OperationState::Dispatched | OperationState::CancelRequested, EventKind::Failure(_)) => {
            next_operation.state = OperationState::Unknown;
            intents.push(EffectIntent::PersistOutcome {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
                outcome: Outcome::Unknown,
            });
            DecisionKind::Transitioned
        }
        (OperationState::Unknown, EventKind::AppliedEvidence(_)) => {
            next_operation.state = OperationState::Applied;
            intents.push(EffectIntent::ReconcileEvidence {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
                outcome: Outcome::Applied,
            });
            DecisionKind::Transitioned
        }
        (OperationState::Unknown, EventKind::NonExecutionEvidence(proof)) => {
            if *proof == NonExecutionProof::QueuedCancellation {
                return reject(state, TransitionError::UnsupportedProof);
            }
            next_operation.state = OperationState::NotApplied;
            intents.push(EffectIntent::ReconcileEvidence {
                event_id: event.id.clone(),
                identity: operation.identity.clone(),
                outcome: Outcome::NotApplied,
            });
            DecisionKind::Transitioned
        }
        (OperationState::Applied, EventKind::NonExecutionEvidence(_))
        | (OperationState::NotApplied, EventKind::AppliedEvidence(_))
        | (OperationState::CancelledBeforeDispatch, EventKind::AppliedEvidence(_)) => {
            return reject(state, TransitionError::ConflictingEvidence);
        }
        _ => return reject(state, TransitionError::InvalidTransition),
    };

    let mut decision = decision(state, kind, intents, true);
    if matches!(&decision.kind, DecisionKind::Transitioned) {
        decision.next_state.operation = Some(next_operation);
    }
    decision
}
