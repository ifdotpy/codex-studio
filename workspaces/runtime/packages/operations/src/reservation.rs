//! Pure admission decision and the storage seam for tool-request reservation.

use crate::{
    AttemptIdentity, ContentFingerprint, DecisionKind, Event, EventId, EventKind,
    OperationIdentity, State, StateRevision, TransitionError, transition,
};

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RequestAlias {
    pub agent: crate::AgentId,
    pub alias: String,
    pub request: crate::RequestId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ReservationInput {
    pub identity: OperationIdentity,
    pub content: ContentFingerprint,
    pub attempt: AttemptIdentity,
    pub aliases: Vec<RequestAlias>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ReservationSnapshot {
    pub existing: Option<State>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReservationWriteIntent {
    ReserveRequest { state: Box<State> },
    InsertAlias(RequestAlias),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReservationDecision {
    Admitted {
        state: State,
        intents: Vec<ReservationWriteIntent>,
    },
    ReturnExisting {
        state: State,
        intents: Vec<ReservationWriteIntent>,
    },
    Rejected {
        state: State,
        error: TransitionError,
    },
}

/// The storage owner loads one command snapshot and atomically applies its
/// typed intents. Python currently implements this boundary over Runtime.db().
pub trait ReservationStore {
    type Error;

    fn load_snapshot(
        &mut self,
        input: &ReservationInput,
    ) -> Result<ReservationSnapshot, Self::Error>;

    fn apply_intents(&mut self, intents: &[ReservationWriteIntent]) -> Result<(), Self::Error>;
}

/// Decide admission without reading storage, observing time, or performing I/O.
#[must_use]
pub fn decide_reservation(
    snapshot: &ReservationSnapshot,
    input: &ReservationInput,
) -> ReservationDecision {
    let Some(state) = snapshot.existing.as_ref() else {
        return decide_new_reservation(&State::empty(StateRevision(0)), input);
    };
    let Some(operation) = state.operation.as_ref() else {
        return ReservationDecision::Rejected {
            state: state.clone(),
            error: TransitionError::OperationMissing,
        };
    };
    let event = Event {
        id: EventId::from("tool-request-reservation"),
        expected_revision: state.revision,
        identity: operation.identity.clone(),
        attempt: operation.attempt.clone(),
        kind: EventKind::Admit {
            content: input.content,
        },
    };
    match transition(state, &event).kind {
        DecisionKind::ReturnExisting => ReservationDecision::ReturnExisting {
            state: state.clone(),
            intents: input
                .aliases
                .iter()
                .cloned()
                .map(ReservationWriteIntent::InsertAlias)
                .collect(),
        },
        DecisionKind::Rejected(error) => ReservationDecision::Rejected {
            state: state.clone(),
            error,
        },
        DecisionKind::Transitioned
        | DecisionKind::NoEffect
        | DecisionKind::ReceiptMissingUnknown => ReservationDecision::Rejected {
            state: state.clone(),
            error: TransitionError::InvalidTransition,
        },
    }
}

fn decide_new_reservation(state: &State, input: &ReservationInput) -> ReservationDecision {
    let event = Event {
        id: EventId::from("tool-request-reservation"),
        expected_revision: state.revision,
        identity: input.identity.clone(),
        attempt: input.attempt.clone(),
        kind: EventKind::Admit {
            content: input.content,
        },
    };
    let decision = transition(state, &event);
    match decision.kind {
        DecisionKind::Transitioned => {
            let mut intents = vec![ReservationWriteIntent::ReserveRequest {
                state: Box::new(decision.next_state.clone()),
            }];
            intents.extend(
                input
                    .aliases
                    .iter()
                    .cloned()
                    .map(ReservationWriteIntent::InsertAlias),
            );
            ReservationDecision::Admitted {
                state: decision.next_state,
                intents,
            }
        }
        DecisionKind::Rejected(error) => ReservationDecision::Rejected {
            state: decision.next_state,
            error,
        },
        DecisionKind::ReturnExisting
        | DecisionKind::NoEffect
        | DecisionKind::ReceiptMissingUnknown => ReservationDecision::Rejected {
            state: decision.next_state,
            error: TransitionError::InvalidTransition,
        },
    }
}

/// Exercise the same load/decide/apply boundary with any store implementation.
pub fn reserve_in_store<S: ReservationStore>(
    store: &mut S,
    input: &ReservationInput,
) -> Result<ReservationDecision, S::Error> {
    let snapshot = store.load_snapshot(input)?;
    let decision = decide_reservation(&snapshot, input);
    let intents = match &decision {
        ReservationDecision::Admitted { intents, .. }
        | ReservationDecision::ReturnExisting { intents, .. } => intents,
        ReservationDecision::Rejected { .. } => return Ok(decision),
    };
    store.apply_intents(intents)?;
    Ok(decision)
}
