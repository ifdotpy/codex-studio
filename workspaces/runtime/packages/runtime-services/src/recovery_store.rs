//! Local recovery effects. Every precondition is read again while holding the
//! store writer transaction; no provider or file effect runs under that lock.
use crate::{
    recovery::{RecoveryAction, RecoveryEffects, RecoveryError, RecoveryOutcome},
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
    scheduler::lifecycle_transaction_with,
};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, QueueEventId, QueueEventRepository, RuntimeStore,
    StoreError, Transaction,
};

/// Terminal delivery remains the account worker's existing lifecycle writer.
/// Implementations must use this transaction for every effect and replay receipt.
pub trait CompletionWriter {
    fn complete(
        &mut self,
        tx: &Transaction<'_>,
        expected: &RecoverySnapshot,
        scope: &ConnectionScope,
        action: &RecoveryAction,
    ) -> Result<RecoveryOutcome, RecoveryError>;
}
pub struct RecoveryStore<'a> {
    pub store: &'a RuntimeStore,
    pub connection_current: &'a dyn Fn(&ConnectionScope) -> bool,
    pub completion: &'a mut dyn CompletionWriter,
    pub at: f64,
}
impl From<rusqlite::Error> for RecoveryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}
impl RecoveryEffects for RecoveryStore<'_> {
    fn apply(
        &mut self,
        expected: &RecoverySnapshot,
        scope: &ConnectionScope,
        action: RecoveryAction,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        if !self.at.is_finite() || self.at < 0.0 {
            return Err(RecoveryError::CommitFailed);
        }
        lifecycle_transaction_with(self.store, |tx| {
            let a = expected.record();
            let id = a["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
            if !(self.connection_current)(scope)
                || a.get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    != scope.account
            {
                return Ok(RecoveryOutcome::Superseded);
            }
            let Some(raw) = AgentRepository::get_raw(tx, &AgentId::from(id))? else {
                return Ok(RecoveryOutcome::Superseded);
            };
            let current: Value =
                serde_json::from_str(&raw).map_err(|_| RecoveryError::CommitFailed)?;
            if &current != a {
                return Ok(RecoveryOutcome::Superseded);
            }
            let mut updated = a.clone();
            let result = match &action {
                RecoveryAction::RestorePreparation { disconnected } => {
                    let eligibility = expected.eligibility();
                    if (*disconnected && !eligibility.disconnected_preparation)
                        || (!disconnected && !eligibility.preparation)
                    {
                        return Ok(RecoveryOutcome::Superseded);
                    }
                    let attempt = if *disconnected {
                        &a["disconnectRecovery"]["startAttempt"]
                    } else {
                        &a["startAttempt"]
                    };
                    let events = attempt["events"]
                        .as_array()
                        .ok_or(RecoveryError::CommitFailed)?;
                    if !*disconnected
                        && uncertain(
                            tx,
                            id,
                            a["epoch"].as_i64().ok_or(RecoveryError::CommitFailed)?,
                        )?
                    {
                        return Ok(RecoveryOutcome::Unconfirmed);
                    }
                    let mut rows = Vec::new();
                    for event_id in events {
                        let event_id = event_id.as_str().ok_or(RecoveryError::CommitFailed)?;
                        let Some(row) =
                            QueueEventRepository::get(tx, &QueueEventId::from(event_id))?
                        else {
                            return Ok(RecoveryOutcome::Unconfirmed);
                        };
                        if row.agent.0 != id
                            || Some(row.epoch) != a["epoch"].as_i64()
                            || !(row.status == "pending"
                                || (*disconnected && row.status == "reserved"))
                            || (*disconnected && row.turn_id.is_some())
                        {
                            return Ok(RecoveryOutcome::Unconfirmed);
                        }
                        rows.push(row);
                    }
                    if *disconnected {
                        for mut row in rows {
                            row.status = "pending".into();
                            row.error = None;
                            QueueEventRepository::put(tx, &row)?;
                        }
                    }
                    updated["connectionRecovery"] = json!({"source":if *disconnected {"disconnected_preparation"}else{"transport_attach"},"at":self.at,"attemptId":attempt["id"],"eventIds":events,"previousError":a["error"],"outcome":"input_restored"});
                    updated["status"] = json!("queued");
                    updated["error"] = Value::Null;
                    if *disconnected {
                        updated["autoWake"] = json!(true);
                        updated["inFlight"] = json!(false);
                    } else if let Some(object) = updated.as_object_mut() {
                        object.remove("startAttempt");
                    }
                    json!({"status":"input_restored","attemptId":attempt["id"]})
                }
                RecoveryAction::OrphanIdle { latest } => {
                    if !expected.eligibility().orphan_busy {
                        return Ok(RecoveryOutcome::Superseded);
                    }
                    if let Some(turn) = latest {
                        let turn_id = turn["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
                        if !tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM runtime_completed_turns WHERE id=?1)",
                            [format!("{id}:{turn_id}")],
                            |r| r.get::<_, bool>(0),
                        )? {
                            return Ok(RecoveryOutcome::Unconfirmed);
                        }
                    }
                    if uncertain(
                        tx,
                        id,
                        a["epoch"].as_i64().ok_or(RecoveryError::CommitFailed)?,
                    )? {
                        return Ok(RecoveryOutcome::Unconfirmed);
                    }
                    updated["inFlight"] = json!(false);
                    updated["activity"] = Value::Null;
                    updated["activeTools"] = json!([]);
                    updated["status"] = json!(if a["autoWake"] == true {
                        "queued"
                    } else {
                        "paused"
                    });
                    if let Some(object) = updated.as_object_mut() {
                        object.remove("steerRejectedTurnId");
                    }
                    updated["turnRecovery"] = json!({"at":self.at,"turnId":null,"latestTurnId":latest.as_ref().map(|t|&t["id"]),"outcome":"idle","source":"native_thread_read"});
                    json!({"status":"reconciled","turnId":null,"outcome":"idle"})
                }
                RecoveryAction::RecordCheck {
                    native_state,
                    turn_status,
                } => {
                    updated["connectionCheck"] = json!({"epoch":a["epoch"],"accountKey":a["accountKey"],"threadId":a["threadId"],"turnId":a["turnId"],"at":self.at,"previousError":a["error"],"nativeState":native_state});
                    if expected.eligibility().queued_restart || expected.eligibility().restart_turn
                    {
                        updated["connectionCheck"]["turnId"] =
                            a["restartRecovery"]["turnId"].clone();
                    }
                    if let Some(status) = turn_status {
                        updated["connectionCheck"]["restartTurnStatus"] = status.clone();
                    }
                    json!({"status":"unconfirmed","checked":true})
                }
                _ => return self.completion.complete(tx, expected, scope, &action),
            };
            RecoverySnapshot::from_record(updated.clone())
                .map_err(|_| RecoveryError::CommitFailed)?;
            let record: AgentRecord =
                serde_json::from_value(updated).map_err(|_| RecoveryError::CommitFailed)?;
            AgentRepository::put(tx, &record)?;
            Ok(RecoveryOutcome::Applied(result))
        })
    }
}
fn uncertain(tx: &Transaction<'_>, id: &str, epoch: i64) -> Result<bool, RecoveryError> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status IN ('reserved','dispatching','uncertain'))",rusqlite::params![id,epoch],|r|r.get(0))?)
}
