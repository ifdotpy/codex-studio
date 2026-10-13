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
    /// Publish the canonical child-stopped event with this exact identity, in
    /// the supplied transaction; a failure rolls the entire hold back.
    fn start_held(
        &mut self,
        tx: &Transaction<'_>,
        agent: &Value,
        reason: &str,
        event_id: &str,
    ) -> Result<(), RecoveryError>;

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
    pub observation_current:
        &'a dyn Fn(&ConnectionScope) -> Option<crate::recovery_absence::RecoveryObservation>,
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
                RecoveryAction::StartAbsent {
                    observation,
                    complete_history,
                    idle_twice,
                } => {
                    use crate::recovery_absence::{StartAbsenceDecision, start_absence};
                    if !expected.eligibility().unconfirmed_start {
                        return Ok(RecoveryOutcome::Superseded);
                    }
                    let Some(current) = (self.observation_current)(scope) else {
                        return Ok(RecoveryOutcome::Superseded);
                    };
                    if !current.valid(scope)
                        || !observation.valid(scope)
                        || current.supervisor != observation.supervisor
                    {
                        return Ok(RecoveryOutcome::Superseded);
                    }
                    let decision =
                        start_absence(expected, &current, *complete_history, *idle_twice);
                    let attempt = &a["startAttempt"];
                    match decision {
                        StartAbsenceDecision::Restore => {
                            let mut rows = Vec::new();
                            for event in attempt["events"]
                                .as_array()
                                .ok_or(RecoveryError::CommitFailed)?
                            {
                                let Some(row) = QueueEventRepository::get(
                                    tx,
                                    &QueueEventId::from(
                                        event.as_str().ok_or(RecoveryError::CommitFailed)?,
                                    ),
                                )?
                                else {
                                    return Ok(RecoveryOutcome::Unconfirmed);
                                };
                                if row.agent.0 != id
                                    || Some(row.epoch) != a["epoch"].as_i64()
                                    || row.status != "uncertain"
                                    || row.turn_id.is_some()
                                {
                                    return Ok(RecoveryOutcome::Unconfirmed);
                                }
                                rows.push(row);
                            }
                            for mut row in rows {
                                row.status = "pending".into();
                                row.error = None;
                                QueueEventRepository::put(tx, &row)?;
                            }
                            let object =
                                updated.as_object_mut().ok_or(RecoveryError::CommitFailed)?;
                            object.remove("startAttempt");
                            object.remove("startOutcomeHold");
                            updated["status"] = json!("queued");
                            updated["inFlight"] = json!(false);
                            updated["error"] = Value::Null;
                            updated["activity"] = Value::Null;
                            updated["activeTools"] = json!([]);
                            updated["turnRecovery"] = json!({"at":current.at,"turnId":null,"outcome":"input_absent","source":"replaced_native_child","attemptId":attempt["id"]});
                            json!({"status":"input_restored"})
                        }
                        StartAbsenceDecision::Hold => {
                            let stamp = chrono::DateTime::from_timestamp(current.at as i64, 0)
                                .ok_or(RecoveryError::CommitFailed)?
                                .format("%Y-%m-%d %H:%M:%S UTC");
                            let reason = format!(
                                "Start outcome unknown. Studio checked native history at {stamp}. The original request can still finish. Recover this worker before new input."
                            );
                            updated["status"] = json!("interrupted");
                            updated["inFlight"] = json!(false);
                            updated["error"] = json!(reason);
                            updated["activity"] = Value::Null;
                            updated["activeTools"] = json!([]);
                            updated["startAttempt"]["executionOutcome"] = json!("unknown");
                            updated["startOutcomeHold"] = json!({"stage":"held","at":current.at,"attemptId":attempt["id"],"threadId":a["threadId"],"connectionId":scope.connection,"evidence":"complete_history_absent_idle_twice_journal_drained"});
                            self.completion.start_held(
                                tx,
                                &updated,
                                &reason,
                                &format!(
                                    "start-unknown:{}",
                                    attempt["id"].as_str().ok_or(RecoveryError::CommitFailed)?
                                ),
                            )?;
                            json!({"status":"held","at":current.at})
                        }
                        StartAbsenceDecision::Waiting => {
                            updated["startRecoveryCheck"] = json!({"at":current.at,"attemptId":attempt["id"],"state":"waiting","reason":"start-outcome-unconfirmed","completeHistory":complete_history,"idleTwice":idle_twice,"quiet":current.quiet,"journalDrained":current.journal_drained});
                            json!({"status":"unconfirmed"})
                        }
                    }
                }

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

/// Parent events, work release and continuation intents belong to the canonical
/// completion owner. Every method writes only through the supplied transaction;
/// external publication/ACK follows commit in the account actor.
pub trait RecoveryDelivery {
    fn start_held(
        &mut self,
        tx: &Transaction<'_>,
        agent: &Value,
        reason: &str,
        event_id: &str,
    ) -> Result<(), RecoveryError>;
    fn terminal(
        &mut self,
        tx: &Transaction<'_>,
        agent: &mut Value,
        turn: &Value,
        mode: crate::recovery::CompletionMode,
    ) -> Result<(), RecoveryError>;
}

/// Reuses scheduler acceptance/terminal receipts. Recovery never manufactures a
/// provider response ticket or writes a delivered receipt for unrelated input.
pub struct LifecycleCompletionWriter<'a> {
    pub at: f64,
    pub delivery: &'a mut dyn RecoveryDelivery,
}
impl CompletionWriter for LifecycleCompletionWriter<'_> {
    fn start_held(
        &mut self,
        tx: &Transaction<'_>,
        agent: &Value,
        reason: &str,
        event_id: &str,
    ) -> Result<(), RecoveryError> {
        self.delivery.start_held(tx, agent, reason, event_id)
    }
    fn complete(
        &mut self,
        tx: &Transaction<'_>,
        expected: &RecoverySnapshot,
        _scope: &ConnectionScope,
        action: &RecoveryAction,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        use crate::recovery::CompletionMode;
        use studio_runtime_store::{SchedulerRepository, TurnAttemptIdentity};
        if !self.at.is_finite() || self.at < 0.0 {
            return Err(RecoveryError::CommitFailed);
        }
        let a = expected.record();
        let id = text(a, "id")?;
        let epoch = a["epoch"].as_i64().ok_or(RecoveryError::CommitFailed)?;
        let attempt = &a["startAttempt"];
        match action {
            RecoveryAction::AcceptStart { turn } => {
                if !expected.eligibility().unconfirmed_start || !valid_turn(turn, a, None) {
                    return Ok(RecoveryOutcome::Superseded);
                }
                let first = attempt["events"].as_array().and_then(|e| e.first());
                if first.is_none()
                    || !turn["items"].as_array().is_some_and(|items| {
                        items
                            .iter()
                            .any(|i| i["type"] == "userMessage" && i.get("clientId") == first)
                    })
                {
                    return Ok(RecoveryOutcome::Unconfirmed);
                }
                let accepted = SchedulerRepository::settle_started(
                    tx,
                    id,
                    epoch,
                    text(attempt, "id")?,
                    text(turn, "id")?,
                )?;
                Ok(if accepted {
                    RecoveryOutcome::Applied(json!({"status":"accepted","turnId":turn["id"]}))
                } else {
                    RecoveryOutcome::Superseded
                })
            }
            RecoveryAction::Terminal {
                turn,
                native_state,
                mode,
            } => {
                if matches!(
                    mode,
                    CompletionMode::Restart
                        | CompletionMode::QueuedRestart
                        | CompletionMode::QueuedActiveWait
                ) {
                    return Err(RecoveryError::Dependency(
                        "turn-recovery/restart-completion",
                    ));
                }
                if (*mode == CompletionMode::ObservedTurn
                    && (a["inFlight"] != true || crate::event_write::truthy(a.get("deletedAt"))))
                    || (*mode != CompletionMode::ObservedTurn && !expected.eligibility().connection)
                {
                    return Ok(RecoveryOutcome::Superseded);
                }
                let terminal = matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                );
                if !terminal
                    || !valid_turn(turn, a, Some(&a["turnId"]))
                    || !matches!(native_state.as_str(), "idle" | "notLoaded" | "active")
                {
                    return Ok(RecoveryOutcome::Unconfirmed);
                }
                let normal = *mode == CompletionMode::ObservedTurn
                    || (*mode == CompletionMode::AutomaticConnection
                        && crate::recovery::completion_permission(
                            expected,
                            turn,
                            unresolved_turn_receipts(tx, a, text(turn, "id")?)?,
                        ));
                let turn_id = text(turn, "id")?;
                let mut updated = if normal {
                    let Some(attempt_id) = attempt["id"].as_str() else {
                        return Err(RecoveryError::Dependency(
                            "turn-recovery/legacy-terminal-lifecycle",
                        ));
                    };
                    if *mode == CompletionMode::AutomaticConnection {
                        let mut restored = a.clone();
                        restored["autoWake"] = json!(true);
                        restored["inFlight"] = json!(true);
                        put_agent(tx, restored)?;
                    }
                    let identity = TurnAttemptIdentity {
                        agent_id: id.into(),
                        epoch,
                        attempt_id: attempt_id.into(),
                    };
                    if !crate::scheduler::settle_terminal_in_transaction(
                        tx,
                        &identity,
                        &json!({"threadId":a["threadId"],"turn":{"id":turn["id"],"status":turn["status"],"error":turn.get("error").cloned().unwrap_or(Value::Null)}}),
                    )? {
                        // A failed recheck after restoring permission must roll back.
                        return Err(RecoveryError::CommitFailed);
                    }
                    let raw = AgentRepository::get_raw(tx, &AgentId::from(id))?
                        .ok_or(RecoveryError::CommitFailed)?;
                    serde_json::from_str(&raw).map_err(|_| RecoveryError::CommitFailed)?
                } else {
                    let mut updated = a.clone();
                    updated["status"] = turn["status"].clone();
                    updated["error"] = turn.get("error").cloned().unwrap_or(Value::Null);
                    if turn["status"] == "failed" {
                        updated["nativeFailureHold"] = json!(true);
                        if updated["error"].is_null() {
                            updated["error"] =
                                json!({"message":"Codex ended this turn with an error."});
                        }
                    }
                    updated["inFlight"] = json!(false);
                    updated["autoWake"] = json!(false);
                    updated["turnId"] = Value::Null;
                    updated["activity"] = Value::Null;
                    updated["activeTools"] = json!([]);
                    updated["lastCompletedTurn"] = turn["id"].clone();
                    updated["lastCompletedTurnStatus"] = turn["status"].clone();
                    tx.execute(
                        "INSERT OR IGNORE INTO runtime_completed_turns(id) VALUES (?1)",
                        [format!("{id}:{turn_id}")],
                    )?;
                    updated
                };
                restore_text(tx, &mut updated, turn, self.at)?;
                if !normal {
                    let notice_id = format!("{id}:connection-recovery:{turn_id}");
                    let notice_text = format!(
                        "Connection checked. Codex reports the previous turn as {}.",
                        text(turn, "status")?
                    );
                    let notice = json!({"id":notice_id,"role":"system","title":"Connection recovery","text":notice_text,"truncated":false,"at":self.at,"nativeNotice":"info","turnId":turn_id,"previousError":a["error"],"details":a["error"],"turnStatus":turn["status"]});
                    studio_runtime_store::NotificationDispatchRepository::put_transcript_item(
                        tx,
                        &notice_id,
                        id,
                        &notice,
                        self.at,
                        &notice_text,
                        true,
                    )?;
                }
                tx.execute("UPDATE runtime_items SET record=json_set(record,'$.turnStatus',?1) WHERE agent=?2 AND json_extract(record,'$.turnId')=?3",rusqlite::params![text(turn,"status")?,id,turn_id])?;
                if *mode == CompletionMode::ObservedTurn {
                    updated["turnRecovery"] = json!({"at":self.at,"turnId":turn_id,"outcome":turn["status"],"source":"native_thread_read"});
                } else {
                    updated["connectionRecovery"] = json!({"at":self.at,"turnId":turn_id,"outcome":turn["status"],"source":"native_thread_read","previousError":a["error"]});
                    if normal {
                        updated["connectionRecovery"]["automatic"] = json!(true);
                        updated["connectionRecovery"]["completionDelivered"] = json!(true);
                    }
                }
                // This hook includes unresolved-operation holds and parent/work
                // intents. A missing owner fails the whole transaction visibly.
                self.delivery.terminal(tx, &mut updated, turn, *mode)?;
                put_agent(tx, updated)?;
                Ok(RecoveryOutcome::Applied(
                    json!({"status":"reconciled","turnId":turn_id,"outcome":turn["status"]}),
                ))
            }
            _ => Err(RecoveryError::EffectsUnavailable),
        }
    }
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, RecoveryError> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(RecoveryError::CommitFailed)
}
fn valid_turn(turn: &Value, agent: &Value, exact: Option<&Value>) -> bool {
    turn["id"].as_str().is_some_and(|s| !s.is_empty())
        && exact.is_none_or(|id| &turn["id"] == id)
        && turn
            .get("threadId")
            .is_none_or(|id| id == &agent["threadId"])
        && turn["items"]
            .as_array()
            .is_some_and(|items| items.len() <= 4096)
}
fn put_agent(tx: &Transaction<'_>, value: Value) -> Result<(), RecoveryError> {
    RecoverySnapshot::from_record(value.clone())?;
    AgentRepository::put(
        tx,
        &serde_json::from_value::<AgentRecord>(value).map_err(|_| RecoveryError::CommitFailed)?,
    )?;
    Ok(())
}
fn restore_text(
    tx: &Transaction<'_>,
    agent: &mut Value,
    turn: &Value,
    at: f64,
) -> Result<(), RecoveryError> {
    use studio_runtime_store::NotificationDispatchRepository as N;
    let agent_id = text(agent, "id")?.to_owned();
    for item in turn["items"]
        .as_array()
        .ok_or(RecoveryError::CommitFailed)?
    {
        if item["type"] != "agentMessage" {
            continue;
        }
        let id = format!("{agent_id}:{}", text(item, "id")?);
        let body = item.get("text").and_then(Value::as_str).unwrap_or("");
        let previous = N::transcript_item(tx, &agent_id, &id)?;
        let created = previous.as_ref().map_or(at, |(_, created)| *created);
        let mut record = json!({"id":id,"role":"assistant","title":"Assistant","at":at});
        record["text"] = json!(body.chars().take(20_000).collect::<String>());
        record["truncated"] = json!(body.chars().count() > 20_000);
        record["turnId"] = turn["id"].clone();
        record["turnStatus"] = turn["status"].clone();
        record["streaming"] = json!(false);
        record["phase"] = item.get("phase").cloned().unwrap_or(Value::Null);
        N::put_transcript_item(tx, &id, &agent_id, &record, created, body, true)?;
        if item["phase"] != "commentary" {
            agent["lastAnswer"] = json!(
                body.chars()
                    .skip(body.chars().count().saturating_sub(16_000))
                    .collect::<String>()
            );
            agent["tail"] = json!(
                body.chars()
                    .skip(body.chars().count().saturating_sub(300))
                    .collect::<String>()
            );
        }
    }
    Ok(())
}

/// Includes legacy unscoped operation rows. A completed native model turn is not
/// evidence that a local mutation or command ended. All reads share the commit.
pub fn unresolved_turn_receipts(
    tx: &Transaction<'_>,
    agent: &Value,
    turn: &str,
) -> Result<bool, RecoveryError> {
    let id = text(agent, "id")?;
    let epoch = agent["epoch"].as_i64().ok_or(RecoveryError::CommitFailed)?;
    if uncertain(tx, id, epoch)? {
        return Ok(true);
    }
    for (table, predicate) in [
        (
            "runtime_tasks",
            "json_extract(record,'$.status') IN ('lost','running','starting','approval')",
        ),
        (
            "runtime_monitors",
            "json_extract(record,'$.status') IN ('lost','running','starting','approval')",
        ),
        (
            "runtime_tool_requests",
            "(json_extract(record,'$.outcome')='unknown' OR json_extract(record,'$.stage') IN ('queued','running'))",
        ),
    ] {
        if !studio_runtime_store::system_reads::exists(tx, table)? {
            continue;
        }
        let exists = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE json_extract(record,'$.agent')=?1 AND {predicate} AND (json_extract(record,'$.turnId') IS NULL OR json_extract(record,'$.turnId')=?2))"),rusqlite::params![id,turn],|r|r.get::<_,bool>(0))?;
        if exists {
            return Ok(true);
        }
    }
    Ok(false)
}
