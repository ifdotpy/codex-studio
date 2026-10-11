//! Pure admission decision and the storage seam for tool-request reservation.

use crate::{
    AttemptIdentity, ContentFingerprint, DecisionKind, Event, EventId, EventKind,
    OperationIdentity, RequestId, State, StateRevision, TransitionError, transition,
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
    pub alias_target: crate::RequestId,
    pub content: ContentFingerprint,
    pub attempt: AttemptIdentity,
    pub call_id: String,
    pub alias_prefix: String,
    pub request_alias: Option<String>,
}

impl ReservationInput {
    fn aliases(&self) -> Vec<RequestAlias> {
        let request = self.alias_target.clone();
        let agent = self.attempt.agent.clone();
        let mut aliases = vec![
            RequestAlias {
                agent: agent.clone(),
                alias: self.call_id.clone(),
                request: request.clone(),
            },
            RequestAlias {
                agent: agent.clone(),
                alias: format!("{}{}", self.alias_prefix, self.call_id),
                request: request.clone(),
            },
        ];
        if let Some(alias) = self
            .request_alias
            .as_ref()
            .filter(|alias| !alias.is_empty())
        {
            aliases.push(RequestAlias {
                agent,
                alias: alias.clone(),
                request,
            });
        }
        aliases
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RequestSnapshot {
    pub existing: Option<State>,
}

pub type ReservationSnapshot = RequestSnapshot;

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RequestWriteIntent {
    SaveRequest { state: Box<State> },
    InsertAlias(RequestAlias),
}

pub type ReservationWriteIntent = RequestWriteIntent;

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReservationDecision {
    Admitted {
        state: State,
        intents: Vec<RequestWriteIntent>,
    },
    ReturnExisting {
        state: State,
        intents: Vec<RequestWriteIntent>,
    },
    Rejected {
        state: State,
        error: TransitionError,
    },
}

/// The storage owner loads one command snapshot and atomically applies its
/// typed intents. Python currently implements this boundary over Runtime.db().
pub trait RequestStore {
    type Error;

    fn load_snapshot(&mut self, request_id: &RequestId) -> Result<RequestSnapshot, Self::Error>;

    fn apply_intents(&mut self, intents: &[RequestWriteIntent]) -> Result<(), Self::Error>;
}

pub use RequestStore as ReservationStore;

/// Decide admission without reading storage, observing time, or performing I/O.
#[must_use]
pub fn decide_reservation(
    snapshot: &RequestSnapshot,
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
                .aliases()
                .into_iter()
                .map(RequestWriteIntent::InsertAlias)
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
            let mut intents = vec![RequestWriteIntent::SaveRequest {
                state: Box::new(decision.next_state.clone()),
            }];
            intents.extend(
                input
                    .aliases()
                    .into_iter()
                    .map(RequestWriteIntent::InsertAlias),
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
pub fn reserve_in_store<S: RequestStore>(
    store: &mut S,
    input: &ReservationInput,
) -> Result<ReservationDecision, S::Error> {
    let snapshot = store.load_snapshot(&input.identity.request_id)?;
    let decision = decide_reservation(&snapshot, input);
    let intents = match &decision {
        ReservationDecision::Admitted { intents, .. }
        | ReservationDecision::ReturnExisting { intents, .. } => intents,
        ReservationDecision::Rejected { .. } => return Ok(decision),
    };
    store.apply_intents(intents)?;
    Ok(decision)
}
