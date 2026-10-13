//! Consumer boundary for the account worker's recovery reader and completion
//! adapter. Reads precede the barrier; the adapter rechecks scope in its write
//! transaction. A read failure never becomes completion or permission to resend.
use crate::recovery_history::{
    ConnectionScope, HistoryError, HistoryMethod, HistoryRead, READ_DEADLINE, RecoveryReader,
};
use crate::recovery_policy::{RecoverySnapshot, native_operations_settled};
use serde_json::{Value, json};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionMode {
    ObservedTurn,
    ManualConnection,
    AutomaticConnection,
    Restart,
    QueuedRestart,
    QueuedActiveWait,
}
#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryAction {
    StartAbsent {
        observation: crate::recovery_absence::RecoveryObservation,
        complete_history: bool,
        idle_twice: bool,
    },
    AcceptStart {
        turn: Value,
    },
    RestorePreparation {
        disconnected: bool,
    },
    OrphanIdle {
        latest: Option<Value>,
    },
    Terminal {
        turn: Value,
        native_state: String,
        mode: CompletionMode,
    },
    RecordCheck {
        native_state: Value,
        turn_status: Option<Value>,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryOutcome {
    Skipped,
    Superseded,
    Unconfirmed,
    Applied(Value),
}
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("recovery effect adapter unavailable")]
    EffectsUnavailable,
    #[error("recovery effect transaction failed")]
    CommitFailed,
}
/// Integrator supplies the existing lifecycle and transcript writers here.
/// `apply` must re-read expected agent scope, event statuses and operation
/// receipts in the same transaction as its writes. It must reject stale
/// connections and preserve unknown operations; it must never send native input.
/// Terminal effects and their durable replay receipt commit together before ACK.
pub trait RecoveryEffects {
    fn apply(
        &mut self,
        expected: &RecoverySnapshot,
        scope: &ConnectionScope,
        action: RecoveryAction,
    ) -> Result<RecoveryOutcome, RecoveryError>;
}

/// Reconcile a currently observed exact turn or orphan busy flag. Unknown start
/// operations remain with the account worker's durable receipt recovery.
pub fn recover_turn(
    snapshot: &RecoverySnapshot,
    scope: ConnectionScope,
    reader: &mut dyn RecoveryReader,
    effects: &mut dyn RecoveryEffects,
) -> Result<RecoveryOutcome, RecoveryError> {
    let a = snapshot.record();
    if a.get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        != scope.account
    {
        return Ok(RecoveryOutcome::Superseded);
    }
    let eligible = snapshot.eligibility();
    if eligible.unconfirmed_start {
        let attempt = &a["startAttempt"];
        let mut read = HistoryRead::new(reader, scope.clone(), Instant::now() + READ_DEADLINE);
        let observation = read.observation()?;
        if attempt["connectionId"] != scope.connection && !observation.replaced_child(snapshot) {
            return effects.apply(
                snapshot,
                &scope,
                RecoveryAction::StartAbsent {
                    observation,
                    complete_history: false,
                    idle_twice: false,
                },
            );
        }
        let thread = a["threadId"]
            .as_str()
            .ok_or(HistoryError::Invalid("threadId"))?;
        let message = attempt["events"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .ok_or(HistoryError::Invalid("events"))?;
        match read.exact_start(thread, message) {
            Ok(Some(turn)) => {
                read.barrier()?;
                return effects.apply(snapshot, &scope, RecoveryAction::AcceptStart { turn });
            }
            history => {
                let complete_history = matches!(history, Ok(None));
                let mut idle_twice = complete_history;
                if complete_history {
                    for _ in 0..2 {
                        match read.thread_state(thread) {
                            Ok(state)
                                if idle(&state)
                                    && !crate::event_write::truthy(state.get("activeFlags")) => {}
                            _ => {
                                idle_twice = false;
                                break;
                            }
                        }
                    }
                }
                if read.barrier().is_err() {
                    idle_twice = false;
                }
                let observation = read.observation()?;
                return effects.apply(
                    snapshot,
                    &scope,
                    RecoveryAction::StartAbsent {
                        observation,
                        complete_history,
                        idle_twice,
                    },
                );
            }
        }
    }

    if !a["inFlight"].as_bool().unwrap_or(false) || crate::event_write::truthy(a.get("deletedAt")) {
        return Ok(RecoveryOutcome::Skipped);
    }
    let Some(thread) = a["threadId"].as_str().filter(|s| !s.is_empty()) else {
        return Ok(RecoveryOutcome::Skipped);
    };
    let mut read = HistoryRead::new(reader, scope.clone(), Instant::now() + READ_DEADLINE);
    let state = read.thread_state(thread)?;
    if !idle(&state) {
        return Ok(RecoveryOutcome::Unconfirmed);
    }
    let action = if eligible.orphan_busy {
        let page = read.call(
            HistoryMethod::Turns,
            json!({"threadId":thread,"limit":1,"sortDirection":"desc"}),
        )?;
        let rows = page["data"]
            .as_array()
            .ok_or(HistoryError::Invalid("data"))?;
        if rows.len() > 1 {
            return Err(HistoryError::Invalid("latest turn").into());
        }
        let latest = rows.first().cloned();
        if latest.as_ref().is_some_and(|t| {
            !terminal(t)
                || !t["id"].as_str().is_some_and(|s| !s.is_empty())
                || t.get("threadId").is_some_and(|id| id != thread)
        }) {
            return Ok(RecoveryOutcome::Unconfirmed);
        }
        if !idle(&read.thread_state(thread)?) {
            return Ok(RecoveryOutcome::Unconfirmed);
        }
        RecoveryAction::OrphanIdle { latest }
    } else {
        let Some(turn_id) = a["turnId"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(RecoveryOutcome::Skipped);
        };
        let Some(turn) = read.exact_turn(thread, turn_id)? else {
            return Ok(RecoveryOutcome::Unconfirmed);
        };
        let after = read.thread_state(thread)?;
        if !idle(&after) || !terminal(&turn) {
            return Ok(RecoveryOutcome::Unconfirmed);
        }
        RecoveryAction::Terminal {
            turn,
            native_state: state_name(&after)?.into(),
            mode: CompletionMode::ObservedTurn,
        }
    };
    read.barrier()?;
    effects.apply(snapshot, &scope, action)
}

/// Manual connection recovery never restores automatic delivery permission.
/// Active transport adoption is deliberately left to the worker that owns the
/// supervisor identity. An active read alone is not a receipt for that child.
pub fn recover_connection(
    snapshot: &RecoverySnapshot,
    scope: ConnectionScope,
    reader: &mut dyn RecoveryReader,
    effects: &mut dyn RecoveryEffects,
    automatic: bool,
) -> Result<RecoveryOutcome, RecoveryError> {
    let e = snapshot.eligibility();
    let a = snapshot.record();
    if a.get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        != scope.account
    {
        return Ok(RecoveryOutcome::Superseded);
    }
    if e.queued_active_wait {
        return Err(RecoveryError::EffectsUnavailable);
    }
    if !e.connection {
        return Ok(RecoveryOutcome::Superseded);
    }
    let mut read = HistoryRead::new(reader, scope.clone(), Instant::now() + READ_DEADLINE);
    let action = if e.preparation || e.disconnected_preparation {
        RecoveryAction::RestorePreparation {
            disconnected: e.disconnected_preparation,
        }
    } else {
        let thread = a["threadId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(HistoryError::Invalid("threadId"))?;
        let before = read.thread_state(thread)?;
        if !e.restart_turn && !idle(&before) {
            RecoveryAction::RecordCheck {
                native_state: before["type"].clone(),
                turn_status: None,
            }
        } else {
            let target = if e.restart_turn || e.queued_restart {
                &a["restartRecovery"]["turnId"]
            } else {
                &a["turnId"]
            };
            let target = target
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(HistoryError::Invalid("turnId"))?;
            let turn = read.exact_turn(thread, target)?;
            let after = read.thread_state(thread)?;
            let turn_status = turn
                .as_ref()
                .map(|t| t["status"].clone())
                .unwrap_or(json!("missing"));
            if let Some(turn) = turn.filter(|t| terminal(t) && idle(&after)) {
                let mode = if e.queued_active_wait {
                    CompletionMode::QueuedActiveWait
                } else if e.queued_restart {
                    CompletionMode::QueuedRestart
                } else if e.restart_turn {
                    CompletionMode::Restart
                } else if automatic {
                    CompletionMode::AutomaticConnection
                } else {
                    CompletionMode::ManualConnection
                };
                RecoveryAction::Terminal {
                    turn,
                    native_state: state_name(&after)?.into(),
                    mode,
                }
            } else {
                // In-progress restart adoption needs a retained supervisor proof,
                // supplied by the account worker, not inferred from thread status.
                RecoveryAction::RecordCheck {
                    native_state: after["type"].clone(),
                    turn_status: Some(turn_status),
                }
            }
        }
    };
    read.barrier()?;
    effects.apply(snapshot, &scope, action)
}
fn idle(state: &Value) -> bool {
    matches!(state["type"].as_str(), Some("idle" | "notLoaded"))
}
fn state_name(state: &Value) -> Result<&str, HistoryError> {
    state["type"]
        .as_str()
        .ok_or(HistoryError::Invalid("status.type"))
}
fn terminal(turn: &Value) -> bool {
    matches!(
        turn["status"].as_str(),
        Some("completed" | "failed" | "interrupted")
    )
}

/// Pure completion permission proof; the adapter obtains `unresolved_receipts`
/// inside the transaction, including unscoped legacy operations and uncertain input.
pub fn completion_permission(
    snapshot: &RecoverySnapshot,
    turn: &Value,
    unresolved_receipts: bool,
) -> bool {
    let a = snapshot.record();
    let previous = &a["disconnectRecovery"];
    native_operations_settled(turn)
        && turn["status"] == "completed"
        && previous["autoWake"] == true
        && a["nativeFailureHold"] != true
        && ["epoch", "accountKey", "threadId", "turnId"]
            .iter()
            .all(|k| previous.get(*k).unwrap_or(&Value::Null) == a.get(*k).unwrap_or(&Value::Null))
        && a.get("turnEpoch").unwrap_or(&a["epoch"]) == &a["epoch"]
        && !unresolved_receipts
}
