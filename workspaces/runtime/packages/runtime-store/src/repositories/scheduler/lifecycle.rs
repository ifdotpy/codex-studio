use super::*;
use crate::CachedSql;
pub(super) use crate::repositories::transcript::stored_truthy as truthy;
use studio_runtime_schema::provider_errors::{KnownCodexErrorKind, known_error_kind};
use studio_runtime_schema::shared_values::{
    PROVIDER_START_NOT_SENT_ERROR as NOT_SENT_ERROR,
    PROVIDER_START_UNKNOWN_ERROR as UNKNOWN_START_ERROR,
};

impl SchedulerRepository {
    pub fn settle_unclean_start(
        transaction: &Transaction<'_>,
        identity: &TurnAttemptIdentity,
        thread: &str,
        first_event: &str,
        turn: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = load_agent(transaction, &identity.agent_id)? else {
            return Ok(false);
        };
        let Some(attempt) = agent.start_attempt.as_ref() else {
            return Ok(false);
        };
        let Some(events) = attempt.events.as_ref() else {
            return Ok(false);
        };
        let held = matches!(&agent.start_outcome_hold,
            Some(crate::StoredStartOutcomeHold::Known(crate::StartOutcomeHold::Held { reason: Some(reason), attempt_id: Some(id), .. }))
                if reason == Self::UNCLEAN_START_HOLD && id == &identity.attempt_id);
        if !held
            || agent.status != AgentStatus::Interrupted
            || agent.epoch != identity.epoch
            || attempt.id.as_deref() != Some(&identity.attempt_id)
            || attempt.epoch != Some(identity.epoch)
            || attempt.submitted != Some(false)
            || attempt.thread_id.as_deref().is_some_and(|id| id != thread)
            || attempt.turn_id.is_some()
            || attempt.observed_turn_id.is_some()
            || attempt.action.is_some()
            || attempt.active_at_reservation == Some(true)
            || events.first().map(String::as_str) != Some(first_event)
            || turn.is_empty()
            || agent.thread_id.as_deref() != Some(thread)
            || agent.auto_wake != Some(false)
            || agent.in_flight != Some(false)
        {
            return Ok(false);
        }
        let event_ids = (serde_json::to_string(events)?, events.len());
        let count: i64 = transaction.query_row_cached(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND epoch=?2 AND id IN (SELECT value FROM json_each(?3)) AND status='uncertain' AND turn_id IS NULL AND error=?4",
            params![identity.agent_id, identity.epoch, event_ids.0, Self::UNCLEAN_START_HOLD], |row| row.get(0))?;
        if usize::try_from(count).ok() != Some(events.len()) {
            return Ok(false);
        }
        deliver_attempt_events(
            transaction,
            &identity.agent_id,
            identity.epoch,
            &event_ids,
            turn,
        )?;
        agent
            .set_start_attempt_turn_id(Some(turn))
            .map_err(|_| invalid_agent(&identity.agent_id))?;
        agent.start_outcome_hold = None;
        agent.extra.insert(
            "error".into(),
            crate::projection_value(&Option::<String>::None),
        );
        publish_agent(transaction, &agent)?;
        Ok(true)
    }
    pub const UNCLEAN_START_HOLD: &'static str =
        "The server stopped before input delivery could be confirmed.";

    pub fn hold_unclean_starts(transaction: &Transaction<'_>) -> StoreResult<usize> {
        let mut query = transaction.prepare_cached(
            "SELECT id FROM runtime_agents WHERE json_extract(record,'$.startAttempt.submitted')=0 AND EXISTS(SELECT 1 FROM runtime_events WHERE agent=runtime_agents.id AND status='reserved')",
        )?;
        let ids = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(query);
        let mut held = 0;
        for id in ids {
            let mut agent = load_agent(transaction, &id)?.ok_or_else(|| invalid_agent(&id))?;
            let attempt = agent
                .start_attempt
                .as_ref()
                .ok_or_else(|| invalid_agent(&id))?;
            let attempt_id = attempt.id.clone().ok_or_else(|| invalid_agent(&id))?;
            let epoch = attempt.epoch.ok_or_else(|| invalid_agent(&id))?;
            let events = attempt
                .events
                .clone()
                .filter(|events| !events.is_empty())
                .ok_or_else(|| invalid_agent(&id))?;
            if attempt.submitted != Some(false) || agent.start_outcome_hold.is_some() {
                return Err(invalid_agent(&id));
            }
            let events_json = serde_json::to_string(&events)?;
            let changed = transaction.execute_cached(
                "UPDATE runtime_events SET status='uncertain',error=?1 WHERE agent=?2 AND epoch=?3 AND status='reserved' AND id IN (SELECT value FROM json_each(?4))",
                params![Self::UNCLEAN_START_HOLD, id, epoch, events_json],
            )?;
            if changed != events.len() {
                return Err(invalid_agent(&id));
            }
            agent.start_outcome_hold = Some(crate::StoredStartOutcomeHold::Known(
                crate::StartOutcomeHold::Held {
                    operation_id: None,
                    connection_generation: None,
                    reason: Some(Self::UNCLEAN_START_HOLD.into()),
                    attempt_id: Some(attempt_id),
                    extra: Default::default(),
                },
            ));
            agent.status = AgentStatus::Interrupted;
            agent.in_flight = Some(false);
            agent.auto_wake = Some(false);
            agent.extra.insert(
                "error".into(),
                crate::projection_value(&Self::UNCLEAN_START_HOLD),
            );
            publish_agent(transaction, &agent)?;
            held += 1;
        }
        Ok(held)
    }
    /// Hold further input after manual recovery without stopping the exact
    /// native turn that recovery already observed. This owns the hold record.
    pub fn hold_recovered_delivery(agent: &mut AgentRecord) -> StoreResult<()> {
        let active = agent.in_flight == Some(true);
        if active {
            let attempt = stored_attempt(agent).ok_or_else(|| invalid_agent(&agent.id.0))?;
            let turn = bound_turn(agent, attempt).ok_or_else(|| invalid_agent(&agent.id.0))?;
            let hold = json!({"epoch":agent.epoch,"attemptId":attempt.id,"accountKey":agent.account_key,"threadId":agent.thread_id.as_deref(),"turnId":turn,"generation":attempt.connection_generation});
            agent
                .extra
                .insert("manualRecoveryDeliveryHold".into(), hold);
        }
        agent.auto_wake = Some(false);
        if agent.status == AgentStatus::Queued || active {
            agent.status = AgentStatus::Paused;
        }
        Ok(())
    }

    /// Read the existing hold proof without changing delivery permission. A
    /// real Stop or a stale native identity cannot match the current turn.
    pub fn manual_delivery_hold_tracks(agent: &AgentRecord) -> bool {
        if agent.status != AgentStatus::Paused || agent.in_flight != Some(true) {
            return false;
        }
        let Some(turn) = agent.turn_id.as_deref() else {
            return false;
        };
        let Some(attempt_id) = stored_attempt(agent).and_then(|attempt| attempt.id.as_deref())
        else {
            return false;
        };
        manual_delivery_hold_tracks(
            agent,
            &TurnAttemptIdentity {
                agent_id: agent.id.0.clone(),
                epoch: agent.epoch,
                attempt_id: attempt_id.to_owned(),
            },
            turn,
        )
    }

    /// Unknown delivery is never replayed; a proven unsent request restores only its own batch.
    pub fn settle_start_outcome(
        transaction: &Transaction<'_>,
        identity: &TurnAttemptIdentity,
        outcome: &str,
        reason: Option<&str>,
    ) -> StoreResult<()> {
        let Some(agent) = load_agent(transaction, &identity.agent_id)? else {
            return Ok(());
        };
        let message = reason.unwrap_or(if outcome == "uncertain" {
            UNKNOWN_START_ERROR
        } else {
            NOT_SENT_ERROR
        });
        let reach = match outcome {
            "uncertain" => StartReach::Provider,
            // The provider refused the start for good (its saved session is gone).
            // The input was never sent: it settles as failed, and its owner may send it again.
            "failed" => StartReach::Provider,
            // Callers settle `pending` only for a request proven never written.
            "pending" => StartReach::NotWritten,
            _ => StartReach::Preparation,
        };
        apply_start_error(
            transaction,
            agent,
            identity,
            outcome == "uncertain",
            reach,
            message,
            json!({"outcome":outcome,"reason":reason}),
        )?;
        Ok(())
    }

    /// An activity notice is not a receipt for the input batch. It may still require interruption.
    pub fn observe_started(
        transaction: &Transaction<'_>,
        identity: &TurnAttemptIdentity,
        turn_id: &str,
    ) -> StoreResult<Option<bool>> {
        let Some(ValidatedAttempt {
            mut agent,
            authority,
            ..
        }) = load_attempt(
            transaction,
            &identity.agent_id,
            identity.epoch,
            &identity.attempt_id,
            None,
        )?
        else {
            return Ok(None);
        };
        if completed_turn(transaction, &identity.agent_id, turn_id)?
            || authority == AttemptAuthority::Deleted
        {
            return Ok(None);
        }
        let attempt = agent
            .start_attempt
            .as_mut()
            .ok_or_else(|| invalid_agent(&identity.agent_id))?;
        if attempt
            .turn_id
            .as_deref()
            .is_some_and(|known| known != turn_id)
        {
            return Ok(None);
        }
        // Do not use observedTurnId to constrain a later correlated acceptance.
        attempt.observed_turn_id = Some(turn_id.to_owned());
        record_notification(transaction, &mut agent)?;
        agent.activity = Some(crate::records::AgentActivity {
            phase: Some("thinking".into()),
            at: Some(timestamp()),
            ..Default::default()
        });
        agent.active_tools = Some(Vec::new());
        agent.last_answer = Some(String::new());
        agent.turn_id = Some(turn_id.to_string());
        agent.native_response_turn = None;
        agent.native_safety_buffering = None;
        agent.native_status = None;
        agent.extra.remove("nativeTurnError");
        if current_attempt(&agent, identity.epoch) {
            agent.status = AgentStatus::Running;
            agent.in_flight = Some(true);
            let response_error = agent
                .start_attempt
                .as_ref()
                .and_then(|a| a.response_error.as_deref());
            if response_error.map_or_else(
                || !agent.extra.contains_key("error"),
                |response| agent.extra.get("error").and_then(Value::as_str) == Some(response),
            ) || agent.extra.get("error") == Some(&json!(UNKNOWN_START_ERROR))
            {
                agent.extra.insert("error".into(), Value::Null);
            }
        }
        if agent
            .start_outcome_hold
            .as_ref()
            .and_then(crate::StoredStartOutcomeHold::attempt_id)
            == Some(identity.attempt_id.as_str())
        {
            agent.start_outcome_hold = None;
        }
        let interrupt = (agent.status == AgentStatus::Paused
            && !manual_delivery_hold_tracks(&agent, identity, turn_id))
            || agent.epoch != identity.epoch;
        publish_agent(transaction, &agent)?;
        Ok(Some(interrupt))
    }

    /// Preserve native identity and continuation authority without fabricating a completion.
    /// Returns the agents this disconnect left interrupted.
    pub fn disconnect_account(
        transaction: &Transaction<'_>,
        account: &str,
        generation: u64,
        at: f64,
        restart: bool,
    ) -> StoreResult<Vec<String>> {
        // Decide busy-turn input before its `dispatching` rows become `uncertain`.
        Self::abandon_steers(transaction, account, generation.saturating_add(1))?;
        let mut interrupted = Vec::new();
        for mut agent in account_agents(transaction, account, &unresolved_sql())? {
            if stored_attempt(&agent)
                .and_then(|attempt| attempt.connection_generation)
                .is_none_or(|g| g == generation)
                && disconnect_agent(transaction, &mut agent, at, restart, true)?
                && agent.status == AgentStatus::Interrupted
            {
                interrupted.push(agent.id.0);
            }
        }
        Ok(interrupted)
    }

    /// Disconnect the VM actors of an ended account worker whose host provider
    /// stays retained. Returns the agents this disconnect left interrupted.
    pub fn disconnect_vm_actors(
        transaction: &Transaction<'_>,
        account: &str,
        generation: u64,
        at: f64,
    ) -> StoreResult<Vec<String>> {
        let mut interrupted = Vec::new();
        for mut agent in account_agents(transaction, account, &unresolved_sql())? {
            if agent.execution_mode == Some(crate::AgentExecutionMode::Vm)
                && stored_attempt(&agent)
                    .and_then(|attempt| attempt.connection_generation)
                    .is_none_or(|g| g == generation)
                && disconnect_agent(transaction, &mut agent, at, false, false)?
                && agent.status == AgentStatus::Interrupted
            {
                interrupted.push(agent.id.0);
            }
        }
        Ok(interrupted)
    }

    /// Retire one VM actor after its provider session closes, preserving that
    /// actor's recovery receipt without disconnecting sibling actors.
    pub fn disconnect_vm_actor(
        transaction: &Transaction<'_>,
        id: &str,
        epoch: i64,
        account: &str,
        generation: u64,
        at: f64,
    ) -> StoreResult<bool> {
        for mut agent in account_agents(transaction, account, &unresolved_sql())? {
            if agent.id.0 != id
                || agent.epoch != epoch
                || agent.execution_mode != Some(crate::AgentExecutionMode::Vm)
                || stored_attempt(&agent)
                    .and_then(|attempt| attempt.connection_generation)
                    .is_some_and(|current| current != generation)
            {
                continue;
            }
            disconnect_agent(transaction, &mut agent, at, false, false)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// A new supervisor generation cannot prove an old turn's outcome. History reconciliation
    /// is a named dependency; preserve the Python recovery receipt and hold the old input.
    /// `restart` says whether the backend restarted; otherwise only the provider
    /// process was replaced and an older generation is a provider disconnect.
    pub fn reconcile_generation(
        transaction: &Transaction<'_>,
        account: &str,
        generation: u64,
        at: f64,
        restart: bool,
    ) -> StoreResult<Vec<String>> {
        // Decide busy-turn input before its `dispatching` rows become `uncertain`.
        Self::abandon_steers(transaction, account, generation)?;
        let mut interrupted = Vec::new();
        for mut agent in account_agents(transaction, account, &unresolved_sql())? {
            let known = stored_attempt(&agent).and_then(|a| a.connection_generation);
            if known != Some(generation) {
                if disconnect_agent(transaction, &mut agent, at, restart, false)?
                    && agent.status == AgentStatus::Interrupted
                {
                    interrupted.push(agent.id.0);
                }
            } else if restart_continuation_allowed(&agent) {
                let previous_error = agent.extra.get("error").cloned().unwrap_or(Value::Null);
                agent.status = AgentStatus::Running;
                agent.auto_wake = Some(true);
                agent.in_flight = Some(true);
                agent.extra.insert("error".into(), Value::Null);
                agent.extra.insert("connectionRecovery".into(),json!({"source":"native_thread_read","at":at,"turnId":agent.turn_id.as_deref(),"outcome":"active","automatic":true,"previousError":previous_error}));
                if let Some(marker) = agent.restart_recovery.as_deref_mut() {
                    marker.stage = Some("continued".into());
                    marker.extra.insert("observedAt".into(), json!(at));
                    marker.outcome = Some("active".into());
                }
                publish_agent(transaction, &agent)?;
            } else if transport_continuation_allowed(&agent) {
                let previous_error = agent.extra.get("error").cloned().unwrap_or(Value::Null);
                agent.auto_wake = Some(true);
                agent.in_flight = Some(true);
                agent.extra.insert("error".into(), Value::Null);
                agent.status = AgentStatus::Running;
                agent.extra.insert(
                    "connectionRecovery".into(),
                    json!({
                        "source":"native_thread_read", "at":at, "turnId":agent.turn_id.as_deref(),
                        "outcome":"active", "automatic":true, "previousError":previous_error,
                        "supervisor":null
                    }),
                );
                publish_agent(transaction, &agent)?;
            }
        }
        Ok(interrupted)
    }

    /// The attempt that a new connection must restore from a recovery receipt
    /// of `agent`, which has no `startAttempt`, so that journal delivery and
    /// history reconciliation can settle it. A disconnect receipt names an
    /// unresolved attempt only while the agent is still interrupted (see
    /// `OPEN_DISCONNECT_RECEIPT_SQL`). A restart marker names one while it is
    /// open or `held` for history reconciliation. An attempt whose turn
    /// already ended owes nothing: restored, it looked unresolved, so the next
    /// provider generation interrupted the finished agent again.
    pub fn open_recovery_attempt(
        connection: &Connection,
        agent: &AgentRecord,
    ) -> StoreResult<Option<crate::StartAttemptView>> {
        if agent.start_attempt.is_some() {
            return Ok(None);
        }
        let disconnect = agent
            .disconnect_recovery
            .as_deref()
            .filter(|_| agent.status == AgentStatus::Interrupted);
        let restart = agent.restart_recovery.as_deref().filter(|marker| {
            matches!(
                marker.stage.as_deref(),
                Some("pending" | "superseded" | "held")
            )
        });
        let Some(attempt) = disconnect
            .or(restart)
            .and_then(|receipt| receipt.start_attempt.as_deref())
        else {
            return Ok(None);
        };
        Ok((!settled_attempt(connection, agent, attempt)?).then(|| attempt.clone()))
    }

    /// Return each idle agent of `account` that a stale recovery receipt left
    /// interrupted to the outcome of its last completed turn. An earlier build
    /// restored the receipt's settled attempt as live, and a later provider
    /// generation interrupted the agent for it. Nothing is sent to the
    /// provider and no parent is notified: the turn already reported its end.
    /// A turn that the disconnect really cut off has no terminal receipt and
    /// keeps its recovery. Returns the repaired agent ids.
    pub fn settle_stale_recovery(
        transaction: &Transaction<'_>,
        account: &str,
        at: f64,
    ) -> StoreResult<Vec<String>> {
        let mut repaired = Vec::new();
        for mut agent in account_agents(
            transaction,
            account,
            "(json_type(record,'$.disconnectRecovery.startAttempt')='object' \
              OR json_type(record,'$.restartRecovery.startAttempt')='object') \
             AND COALESCE(json_extract(record,'$.inFlight'),0)=0",
        )? {
            let Some(attempt) = stored_attempt(&agent).cloned() else {
                continue;
            };
            let Some(id) = attempt.id.as_deref() else {
                continue;
            };
            if !recovery_attempt_ids(&agent).any(|saved| saved == id)
                || !settled_attempt(transaction, &agent, &attempt)?
            {
                continue;
            }
            // A later turn that is still open is not proven ended.
            if let Some(turn) = agent.turn_id.as_deref().filter(|turn| !turn.is_empty())
                && !completed_turn(transaction, &agent.id.0, turn)?
            {
                continue;
            }
            let last = agent
                .extra
                .get("lastCompletedTurn")
                .and_then(Value::as_str)
                .filter(|turn| !turn.is_empty())
                .map(str::to_owned);
            let mut changed = false;
            if agent.start_attempt.as_ref().and_then(|a| a.id.as_deref()) == Some(id)
                && (last.is_none() || bound_turn(&agent, &attempt) != last.as_deref())
            {
                agent.start_attempt = None;
                changed = true;
            }
            if let Some(marker) = agent.restart_recovery.as_deref_mut()
                && matches!(marker.stage.as_deref(), Some("pending" | "superseded"))
                && marker
                    .start_attempt
                    .as_deref()
                    .and_then(|a| a.id.as_deref())
                    == Some(id)
            {
                marker.stage = Some("finished".into());
                marker.extra.insert("reconciledAt".into(), json!(at));
                marker.outcome = Some("settled".into());
                changed = true;
            }
            let outcome = match agent
                .extra
                .get("lastCompletedTurnStatus")
                .and_then(Value::as_str)
            {
                Some("completed") => Some(AgentStatus::Completed),
                Some("failed") => Some(AgentStatus::Failed),
                _ => None,
            };
            if let Some(status) = outcome.filter(|_| {
                agent.status == AgentStatus::Interrupted
                    && last.is_some()
                    && matches!(
                        agent.extra.get("error").and_then(Value::as_str),
                        Some(
                            PROVIDER_DISCONNECTED_TURN_ERROR
                                | VM_PROVIDER_DISCONNECTED_TURN_ERROR
                                | LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR
                                | BACKEND_RESTARTED_TURN_ERROR
                        )
                    )
            }) {
                let error = if status == AgentStatus::Failed {
                    agent.extra.insert("nativeFailureHold".into(), json!(true));
                    agent
                        .extra
                        .get("lastCompletedTurnError")
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                agent.extra.insert("error".into(), error);
                agent.status = status;
                changed = true;
            }
            if changed {
                publish_agent(transaction, &agent)?;
                repaired.push(agent.id.0);
            }
        }
        Ok(repaired)
    }

    /// Repair the caller's exact, classified authentication failure after verified sign-in.
    /// Other lifecycle owners retain their holds. No historical input is changed.
    pub fn repair_authenticated_failure(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        account: &str,
        expected_error: &Value,
    ) -> StoreResult<bool> {
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(false);
        };
        if agent.epoch != epoch
            || agent.status != AgentStatus::Failed
            || agent.account_key != account
            || agent.extra.get("error") != Some(expected_error)
            || agent.auto_wake != Some(true)
            || agent.in_flight == Some(true)
            || agent
                .turn_id
                .as_deref()
                .is_some_and(|turn| !turn.is_empty())
            || agent
                .active_tools
                .as_ref()
                .is_some_and(|tools| !tools.is_empty())
            || agent.native_safety_buffering.is_some()
            || [
                "manualRecoveryDeliveryHold",
                "authWait",
                "capacityRetry",
                "usageResume",
            ]
            .iter()
            .any(|key| agent.extra.get(*key).is_some_and(truthy))
            || agent.start_outcome_hold.is_some()
            || agent
                .native_release
                .as_ref()
                .and_then(|release| release.extra.get("resetPending"))
                .is_some_and(truthy)
            || agent.restart_recovery.as_deref().is_some_and(|marker| {
                restart_marker_open(marker) || marker.stage.as_deref() == Some("held")
            })
        {
            return Ok(false);
        }
        if let Some(attempt) = agent.start_attempt.as_ref()
            && !settled_attempt(transaction, &agent, attempt)?
        {
            return Ok(false);
        }
        agent.extra.remove("nativeFailureHold");
        if !continuation_allowed(&agent) {
            return Ok(false);
        }
        agent.extra.insert("error".into(), Value::Null);
        agent.native_status = None;
        agent.status = if Self::has_claimable_input(transaction, agent_id, epoch)? {
            AgentStatus::Queued
        } else {
            AgentStatus::Idle
        };
        publish_agent(transaction, &agent)?;
        Ok(true)
    }

    /// Write uncertainty before transport admission: process restart cannot authorize a resend.
    pub fn prepare_interrupt(
        tx: &Transaction<'_>,
        agent_id: &str,
        thread: &str,
        turn: &str,
        generation: u64,
        ticket: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = load_agent(tx, agent_id)? else {
            return Ok(false);
        };
        let Some(attempt) = agent.start_attempt.as_ref() else {
            return Ok(false);
        };
        let active_turn = !turn.is_empty()
            && [
                attempt.turn_id.as_deref(),
                attempt.observed_turn_id.as_deref(),
            ]
            .contains(&Some(turn));
        let active_review = turn.is_empty()
            && agent.extra.get("nativeReview").is_some_and(truthy)
            && agent.in_flight == Some(true)
            && attempt.action == Some(crate::StartAttemptAction::Review)
            && attempt.submitted == Some(true)
            && attempt.thread_id.as_deref() == Some(thread);
        if agent.thread_id.as_deref() != Some(thread)
            || !(active_turn || active_review)
            || interrupt_uncertain(attempt, thread, turn)
        {
            return Ok(false);
        }
        agent
            .start_attempt
            .as_mut()
            .expect("attempt checked")
            .interrupt = Some(Box::new(crate::StartInterruptView {
            request_ticket: Some(ticket.into()),
            generation: Some(generation),
            thread_id: Some(thread.into()),
            turn_id: Some(turn.into()),
            outcome: Some(crate::InterruptOutcome::Unknown),
            response_error: None,
            ..Default::default()
        }));
        publish_agent(tx, &agent)?;
        Ok(true)
    }
    pub fn settle_interrupt(
        tx: &Transaction<'_>,
        ticket: &str,
        generation: u64,
        outcome: &str,
    ) -> StoreResult<()> {
        Self::settle_interrupt_result(tx, ticket, generation, outcome, None)
    }
    pub fn settle_interrupt_result(
        tx: &Transaction<'_>,
        ticket: &str,
        generation: u64,
        outcome: &str,
        error: Option<&str>,
    ) -> StoreResult<()> {
        let outcome = <crate::InterruptOutcome as serde::Deserialize>::deserialize(
            serde::de::value::StrDeserializer::<serde::de::value::Error>::new(outcome),
        )
        .map_err(|_| StoreError::RecordContract("invalid interrupt outcome"))?;
        let generation = i64::try_from(generation)
            .map_err(|_| StoreError::SchemaMismatch("invalid provider generation".into()))?;
        let records = select_agents(
            tx,
            "SELECT id,record FROM runtime_agents WHERE json_extract(record,'$.startAttempt.interrupt.requestTicket')=?1 AND json_extract(record,'$.startAttempt.interrupt.generation')=?2",
            params![ticket, generation],
        )?;
        for mut agent in records {
            let receipt = agent
                .start_attempt
                .as_mut()
                .and_then(|a| a.interrupt.as_mut())
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            let previous = receipt.outcome.ok_or_else(|| invalid_agent(&agent.id.0))?;
            if previous == outcome && receipt.response_error.as_deref() == error {
                continue;
            }
            // An unsent ticket was proven not written. A worker that reattaches
            // to the same provider generation may send it once under the same
            // ticket, and that send settles it once more.
            let sent_later = previous == crate::InterruptOutcome::Unsent
                && outcome != crate::InterruptOutcome::Unsent;
            if (previous != crate::InterruptOutcome::Unknown && !sent_later)
                || (previous == outcome && receipt.response_error.is_some())
            {
                return Err(invalid_agent(&agent.id.0));
            }
            receipt.outcome = Some(outcome);
            if let Some(error) = error {
                receipt.response_error = Some(error.to_owned());
            }
            if let Some(error) = error {
                agent.extra.insert(
                    "error".into(),
                    json!(format!(
                        "Stop requested; interrupt acknowledgement unavailable: {error}"
                    )),
                );
            }
            publish_agent(tx, &agent)?;
        }
        Ok(())
    }

    /// Apply the local stop decision; the caller owns cancellation of external work.
    pub fn stop_agent(tx: &Transaction<'_>, id: &str, reason: &str) -> StoreResult<bool> {
        let Some(mut agent) = load_agent(tx, id)? else {
            return Ok(false);
        };
        agent.epoch = agent
            .epoch
            .checked_add(1)
            .ok_or_else(|| invalid_agent(id))?;
        agent.status = AgentStatus::Paused;
        agent.auto_wake = Some(false);
        agent.extra.insert("error".into(), json!(reason));
        if let Some(mut park) = agent.extra.remove("parkReceipt") {
            park["cancelledAtEpoch"] = json!(agent.epoch);
            agent.extra.insert("cancelledPark".into(), park);
        }
        agent.parked_event = None;
        agent.extra.remove("parkAfterTurn");
        tx.execute_cached(
            "UPDATE runtime_events SET status='cancelled' WHERE agent=?1 AND status='pending'",
            [id],
        )?;
        publish_agent(tx, &agent)?;
        Ok(true)
    }

    /// Resolve an account-scoped provider thread to its current submitted attempt.
    /// The boolean distinguishes an unknown thread from a known thread with no
    /// matching live attempt, so callers can count only truly unknown notices.
    pub fn find_attempt_for_thread(
        connection: &Connection,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        turn_id: Option<&str>,
    ) -> StoreResult<(bool, Option<(TurnAttemptIdentity, bool)>)> {
        Self::find_scoped_attempt(
            connection,
            None,
            account_key,
            generation,
            thread_id,
            turn_id,
        )
    }

    /// Resolve a provider stream whose exact actor is already known.
    pub fn find_attempt_for_actor(
        connection: &Connection,
        actor: &str,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        turn_id: Option<&str>,
    ) -> StoreResult<Option<TurnAttemptIdentity>> {
        Ok(Self::find_scoped_attempt(
            connection,
            Some(actor),
            account_key,
            generation,
            thread_id,
            turn_id,
        )?
        .1
        .map(|(identity, _)| identity))
    }

    fn find_scoped_attempt(
        connection: &Connection,
        actor: Option<&str>,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        turn_id: Option<&str>,
    ) -> StoreResult<(bool, Option<(TurnAttemptIdentity, bool)>)> {
        let mut statement = connection.prepare_cached(
            "SELECT id,record FROM runtime_agents \
             WHERE id IN (SELECT chat FROM sessions WHERE thread_id=?1 AND state<>'ended') AND account_key=?2 AND (?3 IS NULL OR id=?3) ORDER BY rowid LIMIT 2",
        )?;
        let rows = statement
            .query_map(params![thread_id, account_key, actor], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let [(id, record)] = rows.as_slice() else {
            return Ok((!rows.is_empty(), None));
        };
        let agent = decode_agent(connection, id, record)?;
        let Some(attempt) = stored_attempt(&agent) else {
            return Ok((true, None));
        };
        let Some(attempt_id) = attempt.id.as_deref() else {
            return Err(invalid_agent(&agent.id.0));
        };
        let Some(epoch) = attempt.epoch else {
            return Err(invalid_agent(&agent.id.0));
        };
        let completed_turn = match turn_id {
            Some(turn) => completed_turn(connection, &agent.id.0, turn)?,
            None => false,
        };
        let current_attempt = agent.epoch == epoch
            && agent.turn_epoch == Some(epoch)
            && agent.auto_wake == Some(true)
            && agent.in_flight == Some(true);
        let stopped_after_submit = agent.status == AgentStatus::Paused
            && agent.auto_wake == Some(false)
            && agent.turn_epoch == Some(epoch);
        if (!current_attempt && !stopped_after_submit && !recovering(&agent, epoch))
            || attempt.submitted != Some(true)
            || attempt.account_key.as_deref() != Some(account_key)
            || attempt.thread_id.as_deref() != Some(thread_id)
            || attempt.connection_generation != Some(generation)
            || attempt.request_ticket.is_none()
            || completed_turn
            || attempt
                .execution_outcome
                .is_some_and(|v| v != studio_runtime_schema::persistence::ExecutionStatus::Unknown)
            || turn_id.is_some_and(|turn_id| {
                stored_attempt(&agent)
                    .and_then(|attempt| bound_turn(&agent, attempt))
                    .is_some_and(|known| known != turn_id)
            })
        {
            return Ok((true, None));
        }
        let bound = stored_attempt(&agent)
            .and_then(|attempt| bound_turn(&agent, attempt))
            .is_some();
        Ok((
            true,
            Some((
                TurnAttemptIdentity {
                    agent_id: agent.id.0.clone(),
                    epoch,
                    attempt_id: attempt_id.to_owned(),
                },
                bound,
            )),
        ))
    }

    /// Bind a turn that the provider started on its own to the idle agent of
    /// `thread_id`, as Python `Runtime.notification` does. Claude starts such
    /// a turn when a background task of the session finishes after its last
    /// turn ended. Without this binding the agent stays idle in the store
    /// while the turn runs, so every managed tool call of the turn has no
    /// active turn and fails.
    ///
    /// The agent must be idle on its current epoch (`completed`, `waiting` or
    /// `queued`, not in flight, not held), wake automatically, and its last
    /// attempt must be on this provider generation of the same account and
    /// thread; that attempt supplies the connection. The new attempt carries
    /// no input (`providerTurn`), so its turn settles like any other turn and
    /// queued input reaches it as busy-turn input. Returns `None` and writes
    /// nothing when any condition fails.
    pub fn adopt_provider_turn(
        transaction: &Transaction<'_>,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        turn_id: &str,
        expected_agent: Option<&str>,
    ) -> StoreResult<Option<TurnAttemptIdentity>> {
        let ids = {
            let mut statement = transaction.prepare_cached(
                "SELECT id FROM runtime_agents \
                 WHERE id IN (SELECT chat FROM sessions WHERE thread_id=?1 AND state<>'ended') AND account_key=?2 ORDER BY rowid LIMIT 2",
            )?;
            statement
                .query_map(params![thread_id, account_key], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let [id] = ids.as_slice() else {
            return Ok(None);
        };
        if expected_agent.is_some_and(|expected| expected != id) {
            return Ok(None);
        }
        let Some(mut agent) = load_agent(transaction, id)? else {
            return Ok(None);
        };
        let flag = |key: &str| agent.extra.get(key).is_some_and(truthy);
        let Some(previous) = agent.start_attempt.as_ref() else {
            return Ok(None);
        };
        let Some(connection) = previous
            .connection_id
            .as_deref()
            .filter(|connection| !connection.is_empty())
        else {
            return Ok(None);
        };
        let idle = matches!(
            agent.status,
            AgentStatus::Completed
                | AgentStatus::Waiting
                | AgentStatus::WaitingOnMonitor
                | AgentStatus::Queued
        );
        if !idle
            || agent.is_deleted()
            || agent.in_flight == Some(true)
            || flag("nativeFailureHold")
            || flag("remoteWorker")
            || flag("remoteAnchor")
            || agent.auto_wake != Some(true)
            || agent.thread_id.as_deref() != Some(thread_id)
            || agent.account_key != account_key
            || previous.epoch != Some(agent.epoch)
            || previous.account_key.as_deref() != Some(account_key)
            || previous.thread_id.as_deref() != Some(thread_id)
            || previous.connection_generation != Some(generation)
            || completed_turn(transaction, id, turn_id)?
        {
            return Ok(None);
        }
        let attempt_id = format!("{PROVIDER_TURN_ATTEMPT_PREFIX}{id}:{turn_id}");
        let attempt = crate::StartAttemptView {
            id: Some(attempt_id.clone()),
            epoch: Some(agent.epoch),
            active_at_reservation: Some(false),
            account_key: Some(account_key.into()),
            events: Some(Vec::new()),
            submitted: Some(true),
            created: Some(timestamp()),
            provider_turn: Some(true),
            request_ticket: Some(attempt_id.clone()),
            connection_id: Some(connection.to_owned()),
            connection_generation: Some(generation),
            thread_id: Some(thread_id.into()),
            turn_id: Some(turn_id.into()),
            ..Default::default()
        };
        let epoch = agent.epoch;
        agent.start_attempt = Some(Box::new(attempt));
        agent.turn_epoch = Some(epoch);
        publish_agent(transaction, &agent)?;
        Ok(Some(TurnAttemptIdentity {
            agent_id: id.clone(),
            epoch,
            attempt_id,
        }))
    }

    /// Record durable provider correlation before `turn/start` is written.
    #[allow(clippy::too_many_arguments)]
    pub fn mark_submitted_identity(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        request_id: &str,
        account_key: &str,
        generation: u64,
        connection_id: &str,
        thread_id: &str,
    ) -> StoreResult<bool> {
        let receipt_input = json!({"agent":agent_id,"epoch":epoch,"ticket":request_id,"account":account_key,"generation":generation,"connection":connection_id,"thread":thread_id});
        if let Some(result) = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::Submission,
            &receipt_input,
        )? {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
        // SCHEDULER-POLICY INTEGRATION: call reconcile_obsolete_before_submit(
        // transaction, agent_id, epoch, attempt_id) here; false stops submission.
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(false);
        };
        let Some(attempt) = agent.start_attempt.as_ref() else {
            return Ok(false);
        };
        if agent.epoch != epoch
            || agent.account_key.as_str() != account_key
            || agent.thread_id.as_deref() != Some(thread_id)
            || agent.status != AgentStatus::Starting
            || agent.auto_wake != Some(true)
            || agent.in_flight != Some(true)
            || agent.extra.get("nativeFailureHold").is_some_and(truthy)
            || attempt.id.as_deref() != Some(attempt_id)
            || attempt.epoch != Some(epoch)
            || attempt.submitted != Some(false)
        {
            return Ok(false);
        }
        if let Some(saved) = crate::ExecutionHistoryRepository::attempt(transaction, attempt_id)?
            && saved.claim_result.is_some()
        {
            replay_claim(transaction, agent.clone(), epoch, &saved)?;
        }
        let events = validated_events(transaction, &agent, attempt)?;
        let changed = transaction.execute_cached(
            "UPDATE runtime_events SET status='dispatching' WHERE agent=?1 AND epoch=?2 AND status='reserved' AND id IN (SELECT value FROM json_each(?3))",
            params![agent_id, epoch, events])?;
        require_membership(
            changed,
            attempt.events.as_ref().map_or(0, Vec::len),
            agent_id,
        )?;
        let attempt = agent
            .start_attempt
            .as_mut()
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.submitted = Some(true);
        attempt.request_ticket = Some(request_id.into());
        attempt.native_operation_id = Some(format!(
            "turn:{agent_id}:{}:attempt:{attempt_id}",
            attempt
                .events
                .as_ref()
                .and_then(|events| events.first())
                .map_or(attempt_id, String::as_str)
        ));
        attempt.connection_id = Some(connection_id.into());
        attempt.supervisor_identity = None;
        attempt.settings_fixed = Some(true);
        attempt.account_key = Some(account_key.into());
        attempt.connection_generation = Some(generation);
        attempt.thread_id = Some(thread_id.into());
        agent
            .extra
            .entry("complaintsPresented".into())
            .or_insert(json!([]));
        publish_agent(transaction, &agent)?;
        ExecutionHistoryRepository::save_receipt(
            transaction,
            attempt_id,
            ExecutionReceiptKind::Submission,
            receipt_input,
            json!(true),
        )?;
        Ok(true)
    }

    /// Return only event reservations whose persisted attempt proves no turn was submitted.
    pub fn recover_unsubmitted(transaction: &Transaction<'_>) -> StoreResult<usize> {
        // Only accounts with a reserved event can match the per-account
        // predicate below; the runtime_event_queue index finds them directly.
        let mut statement = transaction.prepare_cached("SELECT DISTINCT a.account_key FROM runtime_agents a WHERE a.id IN (SELECT agent FROM runtime_events WHERE status='reserved')")?;
        let accounts = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        accounts.iter().try_fold(0, |count, account| {
            Ok(count + Self::recover_unsubmitted_for_account(transaction, account)?)
        })
    }

    pub fn recover_unsubmitted_for_account(
        transaction: &Transaction<'_>,
        account: &str,
    ) -> StoreResult<usize> {
        let records = account_agents(
            transaction,
            account,
            "EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent=runtime_agents.id AND e.status='reserved')",
        )?;
        let mut recovered = 0;
        for agent in records {
            recovered += requeue_unsubmitted(transaction, agent)?.unwrap_or(0);
        }
        Ok(recovered)
    }

    /// Return the exact input batch of an idle start to the queue before the
    /// provider received it, and queue the agent to claim it again. The caller
    /// proves that the input never reached the provider: the session proved
    /// its request not written, or the durable submitted commit refused it
    /// before any write. So the retry cannot repeat a message. `reason` names
    /// the request and the cause; the attempt keeps it as its error.
    ///
    /// A start reserved during a busy turn takes the busy rejection path of
    /// `return_unapplied_input`. Returns false and changes nothing when the
    /// start outcome is already settled, when the attempt is no longer the
    /// agent's current start, or when its run already returned
    /// [`START_RETURN_LIMIT`] batches. The caller then settles the start with
    /// the same reason.
    pub fn return_unstarted_input(
        transaction: &Transaction<'_>,
        identity: &TurnAttemptIdentity,
        reason: &str,
    ) -> StoreResult<bool> {
        let Some(agent) = load_agent(transaction, &identity.agent_id)? else {
            return Ok(false);
        };
        if ExecutionHistoryRepository::receipt::<serde_json::Value>(
            transaction,
            &identity.attempt_id,
            ExecutionReceiptKind::StartResult,
        )?
        .is_some()
            || ExecutionHistoryRepository::start_returns(transaction, &identity.attempt_id)?
                >= START_RETURN_LIMIT
        {
            return Ok(false);
        }
        if !return_unapplied_input(
            transaction,
            agent,
            identity,
            reason,
            json!({"outcome":"returned","reason":reason}),
        )? {
            return Ok(false);
        }
        ExecutionHistoryRepository::mark_start_return(transaction, &identity.attempt_id)?;
        Ok(true)
    }

    /// Return one unsubmitted start attempt's reserved input to the queue. Its
    /// input never reached `turn/start`, so the retry cannot repeat a message.
    pub fn requeue_unsubmitted_attempt(
        transaction: &Transaction<'_>,
        identity: &TurnAttemptIdentity,
    ) -> StoreResult<bool> {
        let Some(agent) = load_agent(transaction, &identity.agent_id)? else {
            return Ok(false);
        };
        if agent.start_attempt.as_ref().is_none_or(|attempt| {
            attempt.id.as_deref() != Some(identity.attempt_id.as_str())
                || attempt.epoch != Some(identity.epoch)
                || attempt.submitted != Some(false)
                // An earlier outcome of the same request (an uncertain write,
                // then the resolution when its provider session closes)
                // already settled this attempt and released its input.
                || attempt.execution_outcome.is_some_and(|outcome| outcome != studio_runtime_schema::persistence::ExecutionStatus::Unknown)
        }) {
            return Ok(false);
        }
        Ok(requeue_unsubmitted(transaction, agent)?.is_some())
    }

    /// Restore Python-shaped scoped receipts; the account worker will reattach without replay.
    pub fn recover_submitted(transaction: &Transaction<'_>) -> StoreResult<usize> {
        let query = format!(
            "SELECT id,record FROM runtime_agents WHERE COALESCE(json_extract(record,'$.remoteWorker'),0)=0 AND COALESCE(json_extract(record,'$.remoteAnchor'),0)=0 AND ((json_extract(record,'$.startAttempt.submitted')=1 AND COALESCE(json_extract(record,'$.startAttempt.executionOutcome'),'unknown')='unknown') OR (json_extract(record,'$.startAttempt') IS NULL AND (json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval')))) AND NOT ({COMPLETED_START_SQL}) AND NOT ({SETTLED_IDLE_TURN_SQL})"
        );
        let mut statement = transaction.prepare_cached(&query)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut recovered = 0;
        for (id, raw) in rows {
            let mut agent = match decode_agent(transaction, &id, &raw) {
                Ok(agent) => agent,
                Err(StoreError::InvalidSyncObject { scope, .. }) if scope == "runtime_agents" => {
                    eprintln!("scheduler startup skipped malformed submitted row id={id}");
                    continue;
                }
                Err(StoreError::InvalidSyncObjectField { scope, .. })
                    if scope == "runtime_agents" =>
                {
                    eprintln!("scheduler startup skipped invalid-field submitted row id={id}");
                    continue;
                }
                Err(error) => return Err(error),
            };
            clear_hold(&mut agent);
            if disconnect_agent(transaction, &mut agent, timestamp(), true, true)? {
                recovered += 1;
            }
        }
        Ok(recovered)
    }

    /// Bind a provider turn to the exact submitted scheduler attempt.
    #[allow(clippy::too_many_arguments)]
    pub fn settle_started_provider(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        request_id: &str,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        turn_id: &str,
    ) -> StoreResult<Option<bool>> {
        if let Some(result) = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            &acceptance_input(agent_id, epoch, turn_id),
        )? {
            let saved = ExecutionHistoryRepository::attempt(transaction, attempt_id)?
                .ok_or_else(|| invalid_agent(agent_id))?;
            if saved.start.request_ticket.as_deref() != Some(request_id)
                || saved.start.account_key.as_deref() != Some(account_key)
                || saved.start.connection_generation != Some(generation)
                || saved.start.thread_id.as_deref() != Some(thread_id)
            {
                return Err(ExecutionHistoryRepository::invalid(
                    "runtime_execution_attempts",
                    attempt_id,
                ));
            }
            return Ok(result["interrupt"].as_bool());
        }
        let Some(agent) = load_attempt(
            transaction,
            agent_id,
            epoch,
            attempt_id,
            Some((request_id, account_key, generation, thread_id)),
        )?
        else {
            return Ok(None);
        };
        if acceptance_decision(&agent.agent, epoch, turn_id, agent.authority)
            == AcceptanceDecision::InterruptOnly
        {
            return Ok(Some(true));
        }
        if !Self::apply_acceptance(transaction, agent, epoch, attempt_id, turn_id)? {
            return Ok(None);
        }
        let saved = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            &acceptance_input(agent_id, epoch, turn_id),
        )?
        .ok_or_else(|| invalid_agent(agent_id))?;
        Ok(saved["interrupt"].as_bool())
    }

    /// Bind a safety retry start that has no queued Studio events. This mirrors
    /// Python's safety `start_accepted` path while preserving the same provider
    /// ticket, account, generation, thread, turn and stop-race checks.
    #[allow(clippy::too_many_arguments)]
    pub fn settle_safety_started_provider(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        request_id: &str,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        turn_id: &str,
    ) -> StoreResult<Option<bool>> {
        let receipt_input = acceptance_input(agent_id, epoch, turn_id);
        if let Some(result) = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            &receipt_input,
        )? {
            let saved = ExecutionHistoryRepository::attempt(transaction, attempt_id)?
                .ok_or_else(|| invalid_agent(agent_id))?;
            if saved.start.request_ticket.as_deref() != Some(request_id)
                || saved.start.account_key.as_deref() != Some(account_key)
                || saved.start.connection_generation != Some(generation)
                || saved.start.thread_id.as_deref() != Some(thread_id)
            {
                return Err(ExecutionHistoryRepository::invalid(
                    "runtime_execution_attempts",
                    attempt_id,
                ));
            }
            return Ok(result["interrupt"].as_bool());
        }
        let Some(agent) = load_agent(transaction, agent_id)? else {
            return Ok(None);
        };
        let Some(attempt) = agent.start_attempt.as_ref() else {
            return Ok(None);
        };
        if attempt.id.as_deref() != Some(attempt_id)
            || attempt.epoch != Some(epoch)
            || attempt.submitted != Some(true)
            || attempt.account_key.as_deref() != Some(account_key)
            || attempt.thread_id.as_deref() != Some(thread_id)
            || agent.account_key.as_str() != account_key
            || agent.thread_id.as_deref() != Some(thread_id)
            || attempt.request_ticket.as_deref() != Some(request_id)
            || attempt.connection_generation != Some(generation)
            || attempt
                .events
                .as_ref()
                .is_none_or(|events| !events.is_empty())
        {
            return Ok(None);
        }
        let authority = attempt_authority(&agent, epoch);
        let interrupt = match acceptance_decision(&agent, epoch, turn_id, authority) {
            AcceptanceDecision::Bind { interrupt, .. } => interrupt,
            AcceptanceDecision::InterruptOnly => return Ok(Some(true)),
            AcceptanceDecision::Ignore => return Ok(None),
        };
        ExecutionHistoryRepository::record(transaction, &agent)?;
        let accepted = Self::apply_acceptance(
            transaction,
            ValidatedAttempt {
                agent,
                event_ids: ("[]".to_owned(), 0),
                authority,
            },
            epoch,
            attempt_id,
            turn_id,
        )?;
        Ok(accepted.then_some(interrupt))
    }

    /// Reject a safety retry start that owns no queued Studio events. Keep this
    /// path narrow so ordinary starts still require validated event batches.
    #[allow(clippy::too_many_arguments)]
    pub fn settle_safety_start_rejected_provider(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        request_id: &str,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        message: &str,
    ) -> StoreResult<bool> {
        let input = json!({"error":{"message":message},"ticket":request_id,"account":account_key,"generation":generation,"thread":thread_id});
        let receipt_input = json!({"agent":agent_id,"epoch":epoch,"input":input});
        if let Some(result) = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            &receipt_input,
        )? {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(false);
        };
        let Some(attempt) = agent.start_attempt.as_ref() else {
            return Ok(false);
        };
        if attempt.id.as_deref() != Some(attempt_id)
            || attempt.epoch != Some(epoch)
            || attempt.submitted != Some(true)
            || attempt.account_key.as_deref() != Some(account_key)
            || attempt.thread_id.as_deref() != Some(thread_id)
            || attempt.request_ticket.as_deref() != Some(request_id)
            || attempt.connection_generation != Some(generation)
            || attempt
                .events
                .as_ref()
                .is_none_or(|events| !events.is_empty())
        {
            return Ok(false);
        }
        let decision = start_error_decision(&agent, epoch, false, StartReach::Provider)?;
        if decision.return_busy_input {
            return Err(ExecutionHistoryRepository::invalid(
                "runtime_execution_attempts",
                attempt_id,
            ));
        }
        if let Some(status) = decision.visible {
            agent.status = status;
            agent.extra.insert("error".into(), json!(message));
        }
        agent.in_flight = Some(false);
        agent.start_outcome_hold = None;
        agent
            .start_attempt
            .as_mut()
            .ok_or_else(|| invalid_agent(agent_id))?
            .execution_outcome =
            Some(studio_runtime_schema::persistence::ExecutionStatus::Rejected);
        publish_agent(transaction, &agent)?;
        ExecutionHistoryRepository::save_receipt(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            receipt_input,
            json!(true),
        )?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn settle_start_rejected_provider(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        request_id: &str,
        account_key: &str,
        generation: u64,
        thread_id: &str,
        error: Value,
    ) -> StoreResult<bool> {
        let input = json!({"error":error,"ticket":request_id,"account":account_key,"generation":generation,"thread":thread_id});
        if let Some(result) = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            &json!({"agent":agent_id,"epoch":epoch,"input":input}),
        )? {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
        let Some(agent) = load_attempt(
            transaction,
            agent_id,
            epoch,
            attempt_id,
            Some((request_id, account_key, generation, thread_id)),
        )?
        else {
            return Ok(false);
        };
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider rejected turn/start")
            .to_owned();
        if proven_unapplied_disk_full(&error) {
            return return_unapplied_input(
                transaction,
                agent.agent,
                &TurnAttemptIdentity {
                    agent_id: agent_id.into(),
                    epoch,
                    attempt_id: attempt_id.into(),
                },
                &message,
                input,
            );
        }
        apply_start_error(
            transaction,
            agent.agent,
            &TurnAttemptIdentity {
                agent_id: agent_id.into(),
                epoch,
                attempt_id: attempt_id.into(),
            },
            false,
            StartReach::Provider,
            &message,
            input,
        )
    }

    pub fn settle_started(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        turn_id: &str,
    ) -> StoreResult<bool> {
        let receipt_input = acceptance_input(agent_id, epoch, turn_id);
        if let Some(result) = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            &receipt_input,
        )? {
            return result["bound"]
                .as_bool()
                .ok_or_else(|| invalid_agent(agent_id));
        }
        let Some(validated) = load_attempt(transaction, agent_id, epoch, attempt_id, None)? else {
            return Ok(false);
        };
        Self::apply_acceptance(transaction, validated, epoch, attempt_id, turn_id)
    }

    /// Accept a submitted attempt retained only in the durable restart marker.
    /// Recheck all typed fields of the held snapshot and uncertain event rows
    /// (excluding opaque provider payload bytes) before the
    /// ordinary acceptance path writes any agent or event state.
    pub fn settle_restart_started(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        expected_attempt: &crate::StartAttemptView,
        expected_wait: &Value,
        turn_id: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(false);
        };
        let Some(marker) = agent.restart_recovery.as_deref() else {
            return Ok(false);
        };
        let attempt_id = expected_attempt
            .id
            .as_deref()
            .ok_or_else(|| invalid_agent(agent_id))?;
        if agent.epoch != epoch
            || agent.extra.get("contextRepairWait") != Some(expected_wait)
            || marker.stage.as_deref() != Some("held")
            || !marker.start_attempt.as_deref().is_some_and(|attempt| same_typed_attempt(attempt, expected_attempt))
            || agent.start_attempt.as_ref()
                .is_some_and(|attempt| !same_typed_attempt(attempt, expected_attempt))
            || expected_attempt.epoch != Some(epoch)
            || expected_attempt.submitted != Some(true)
            // A process stop can end a submitted start before its response
            // records an outcome; the held marker's attempt then has none,
            // which is unknown too.
            || expected_attempt.execution_outcome.is_some_and(|outcome| outcome != studio_runtime_schema::persistence::ExecutionStatus::Unknown)
            || expected_attempt.account_key.as_deref() != Some(agent.account_key.as_str())
            || expected_attempt.thread_id != agent.thread_id
        {
            return Ok(false);
        }
        let event_ids = validated_events(transaction, &agent, expected_attempt)?;
        let expected_count = expected_attempt.events.as_ref().map_or(0, Vec::len);
        let uncertain_count: i64 = transaction.query_row_cached(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND epoch=?2 AND id IN (SELECT value FROM json_each(?3)) AND status='uncertain' AND turn_id IS NULL",
            params![agent_id, epoch, event_ids],
            |row| row.get(0),
        )?;
        if usize::try_from(uncertain_count).ok() != Some(expected_count) {
            return Ok(false);
        }
        agent.start_attempt = Some(Box::new(expected_attempt.clone()));
        let validated = ValidatedAttempt {
            authority: attempt_authority(&agent, epoch),
            agent,
            event_ids: (event_ids, expected_count),
        };
        Self::apply_acceptance(transaction, validated, epoch, attempt_id, turn_id)
    }

    fn apply_acceptance(
        transaction: &Transaction<'_>,
        validated: ValidatedAttempt,
        epoch: i64,
        attempt_id: &str,
        turn_id: &str,
    ) -> StoreResult<bool> {
        let ValidatedAttempt {
            mut agent,
            event_ids,
            authority,
        } = validated;
        let agent_id = agent.id.0.clone();
        let agent_id = agent_id.as_str();
        let receipt_input = acceptance_input(agent_id, epoch, turn_id);
        if completed_turn(transaction, agent_id, turn_id)? {
            return Ok(false);
        }
        let current_attempt = current_attempt(&agent, epoch);
        if matches!(
            authority,
            AttemptAuthority::Deleted | AttemptAuthority::Inactive
        ) {
            return Ok(false);
        }
        let AcceptanceDecision::Bind { interrupt, .. } =
            acceptance_decision(&agent, epoch, turn_id, authority)
        else {
            return Ok(false);
        };
        deliver_attempt_events(transaction, agent_id, epoch, &event_ids, turn_id)?;
        mark_timing(transaction, &event_ids.0, "acceptedAt")?;

        let attempt = agent
            .start_attempt
            .as_mut()
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.turn_id = Some(turn_id.into());
        if agent.epoch == epoch {
            attempt.execution_outcome = None;
        }
        agent.turn_id = Some(turn_id.to_string());
        agent.in_flight = Some(true);
        if current_attempt || recovering(&agent, epoch) {
            if current_attempt {
                agent.status = AgentStatus::Running;
                agent.extra.insert("error".into(), Value::Null);
            }
            if agent
                .start_outcome_hold
                .as_ref()
                .and_then(crate::StoredStartOutcomeHold::attempt_id)
                == Some(attempt_id)
            {
                agent.start_outcome_hold = None;
                if agent
                    .extra
                    .get("nativeFailureHold")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    agent.extra.insert("nativeFailureHold".into(), json!(false));
                }
                agent.extra.insert("error".into(), Value::Null);
            }
        }
        publish_agent(transaction, &agent)?;
        ExecutionHistoryRepository::save_receipt(
            transaction,
            attempt_id,
            ExecutionReceiptKind::StartResult,
            receipt_input,
            json!({"bound":true,"interrupt":interrupt}),
        )?;
        Ok(true)
    }

    /// A settled owner with unfinished commands remains visibly waiting. This
    /// never changes explicit stops or failure holds and never claims input.
    pub fn wait_for_owned_monitors(
        transaction: &Transaction<'_>,
        agent: &mut AgentRecord,
    ) -> StoreResult<()> {
        if agent.status != AgentStatus::Completed
            || agent.auto_wake != Some(true)
            || !continuation_allowed(agent)
        {
            return Ok(());
        }
        let statuses = crate::startup_recovery::ACTIVE_MONITOR_STATUSES
            .iter()
            .map(|status| format!("'{status}'"))
            .collect::<Vec<_>>()
            .join(",");
        let running = transaction.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM runtime_monitors WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.epoch')=?2 AND json_extract(record,'$.status') IN ({statuses}))"),
            (&agent.id.0, agent.epoch), |row| row.get::<_, bool>(0),
        )?;
        if running {
            agent.status = AgentStatus::WaitingOnMonitor;
        }
        Ok(())
    }

    /// Apply one terminal provider outcome, ignoring stale attempt or epoch callbacks.
    /// Busy-turn input still open for the turn is decided first, even for a stale
    /// callback: `unapplied_steers` (first input ids the provider proves it never
    /// read) return to the queue, the rest becomes unconfirmed.
    #[allow(clippy::too_many_arguments)]
    pub fn settle_terminal(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        turn_id: &str,
        status: AgentStatus,
        error: Option<Value>,
        native_status: &str,
        native_error: Option<&Value>,
        unapplied_steers: &[&str],
    ) -> StoreResult<bool> {
        let mut error = error;
        if let Some(error) = error.as_mut() {
            studio_runtime_schema::claude_failure::sanitize_error(error);
        }
        let mut native_error = native_error.cloned();
        if let Some(error) = native_error.as_mut() {
            studio_runtime_schema::claude_failure::sanitize_error(error);
        }
        let native_error = native_error.as_ref();
        let receipt_input = json!({"agent":agent_id,"epoch":epoch,"turn":turn_id,"status":status,"error":error,"nativeStatus":native_status,"nativeError":native_error});
        if let Some(result) = ExecutionHistoryRepository::replay(
            transaction,
            attempt_id,
            ExecutionReceiptKind::Terminal,
            &receipt_input,
        )? {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
        Self::resolve_steers_at_terminal(transaction, agent_id, turn_id, unapplied_steers)?;
        let outcome = match status {
            AgentStatus::Completed => "completed",
            AgentStatus::Failed => "failed",
            AgentStatus::Interrupted => "interrupted",
            AgentStatus::Unknown
            | AgentStatus::Idle
            | AgentStatus::Queued
            | AgentStatus::Starting
            | AgentStatus::Running
            | AgentStatus::Waiting
            | AgentStatus::WaitingOnMonitor
            | AgentStatus::Approval
            | AgentStatus::Blocked
            | AgentStatus::Paused
            | AgentStatus::Parked
            | AgentStatus::Abandoned
            | AgentStatus::CapacityRetry
            | AgentStatus::Other(_) => {
                return Err(StoreError::SchemaMismatch(
                    "invalid terminal scheduler outcome".to_owned(),
                ));
            }
        };
        let Some(ValidatedAttempt {
            mut agent,
            event_ids,
            authority,
        }) = load_attempt(transaction, agent_id, epoch, attempt_id, None)?
        else {
            return Ok(false);
        };
        if completed_turn(transaction, agent_id, turn_id)? {
            return Ok(false);
        }
        if agent
            .start_attempt
            .as_ref()
            .and_then(|a| bound_turn(&agent, a))
            != Some(turn_id)
        {
            return Ok(false);
        }
        let current_attempt = current_attempt(&agent, epoch);
        if matches!(
            authority,
            AttemptAuthority::Deleted | AttemptAuthority::Inactive
        ) {
            return Ok(false);
        }
        deliver_attempt_events(transaction, agent_id, epoch, &event_ids, turn_id)?;

        {
            let attempt = agent
                .start_attempt
                .as_mut()
                .ok_or_else(|| invalid_agent(agent_id))?;
            attempt.turn_id = Some(turn_id.into());
            if native_status == "completed" && outcome == "failed" {
                attempt.execution_outcome =
                    Some(studio_runtime_schema::persistence::ExecutionStatus::Failed);
            }
        }
        record_notification(transaction, &mut agent)?;
        if let Some(marker) = agent.restart_recovery.as_deref_mut()
            && marker.turn_id.as_deref() == Some(turn_id)
            && matches!(marker.stage.as_deref(), Some("reattached" | "continued"))
        {
            marker.stage = Some("finished".into());
            marker
                .extra
                .insert("reconciledAt".into(), json!(timestamp()));
            marker.outcome = Some(native_status.into());
        }
        // A later turn of the same thread finished, so the restart turn can no
        // longer continue. An open marker would let the next restart interrupt
        // this idle agent again (rehearsal 9, agent 3a35babb).
        let thread = agent.thread_id.clone();
        if let Some(marker) = agent.restart_recovery.as_deref_mut()
            && restart_marker_open(marker)
            && marker.turn_id.as_deref().is_some_and(|old| old != turn_id)
            && marker.epoch.as_ref().and_then(|v| v.number().as_i64()) == Some(epoch)
            && thread.is_some_and(|thread| marker.thread_id.as_deref() == Some(thread.as_str()))
        {
            marker.stage = Some("finished".into());
            marker
                .extra
                .insert("reconciledAt".into(), json!(timestamp()));
            marker.outcome = Some("replaced".into());
            marker.extra.insert("replacedByTurn".into(), json!(turn_id));
        }
        let clears_unknown = agent
            .start_outcome_hold
            .as_ref()
            .and_then(crate::StoredStartOutcomeHold::attempt_id)
            == Some(attempt_id);
        if clears_unknown {
            agent.start_outcome_hold = None;
        }
        crate::domain::complete_turn(transaction, agent_id, turn_id)?;
        agent
            .extra
            .insert("lastCompletedTurn".into(), json!(turn_id));
        agent
            .extra
            .insert("lastCompletedTurnStatus".into(), json!(outcome));
        agent.extra.insert(
            "lastCompletedTurnError".into(),
            error.clone().unwrap_or(Value::Null),
        );
        agent.turn_id = None;
        agent.activity = None;
        agent.active_tools = Some(Vec::new());
        agent.in_flight = Some(false);
        if status == AgentStatus::Failed {
            agent.extra.insert("nativeFailureHold".into(), json!(true));
        }
        agent.native_response_turn = None;
        agent.native_safety_buffering = None;
        agent.native_status = None;
        agent.extra.remove("nativeTurnError");
        if let Some(error) = error.as_ref()
            && known_error_kind(error) == Some(KnownCodexErrorKind::MisalignmentPolicyViolation)
        {
            agent.extra.insert(
                "nativeThreadBlock".into(),
                json!({"threadId":agent.thread_id.as_deref(),"error":error}),
            );
        }
        if current_attempt || recovering(&agent, epoch) {
            if let Some(error) = error {
                agent.extra.insert("error".into(), error);
            } else {
                agent.extra.insert("error".into(), Value::Null);
            }
            if status == AgentStatus::Failed {
                agent.extra.insert("nativeFailureHold".into(), json!(true));
            } else if clears_unknown {
                agent.extra.insert("nativeFailureHold".into(), json!(false));
            }
            agent.status = status.clone();
        }
        if current_attempt
            && continuation_allowed(&agent)
            && !agent.extra.get("nativeFailureHold").is_some_and(truthy)
            && Self::has_pending_continuation_event(transaction, agent_id, epoch)?
        {
            // Python queues the worker here, and its next scheduler pass sets an
            // empty `pending_batch` to `waiting`. Rust wakes a queued worker
            // only for pending input, so a worker that has only unclaimable
            // (`uncertain`) input goes to `waiting` directly instead of staying
            // `queued` for good (agent 841ebc0e in rehearsal 8).
            agent.status = if Self::has_claimable_input(transaction, agent_id, epoch)? {
                AgentStatus::Queued
            } else {
                AgentStatus::Waiting
            };
        }
        if current_attempt {
            Self::wait_for_owned_monitors(transaction, &mut agent)?;
        }
        if current_attempt {
            parked_after_turn(&mut agent);
        }
        if native_status == "completed" && current_attempt {
            agent.extra.insert("complaintMisses".into(), json!(0));
        }
        publish_agent(transaction, &agent)?;
        crate::ExecutionHistoryRepository::terminal(
            transaction,
            agent_id,
            turn_id,
            <studio_runtime_schema::persistence::ExecutionStatus as serde::Deserialize>::deserialize(
                serde::de::value::StrDeserializer::<serde_json::Error>::new(native_status),
            )?,
            native_error,
        )?;
        ExecutionHistoryRepository::save_receipt(
            transaction,
            attempt_id,
            ExecutionReceiptKind::Terminal,
            receipt_input,
            json!(true),
        )?;
        Ok(true)
    }

    /// Match Python Runtime.notification's continuation ownership rule at turn completion.
    /// An unresolved initial receipt is excluded; other dispatching/uncertain inputs still own
    /// the child's continuation until exact delivery resolution.
    pub fn has_pending_continuation_event(
        connection: &Connection,
        agent_id: &str,
        epoch: i64,
    ) -> StoreResult<bool> {
        Ok(connection.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND (status IN ('pending','reserved') OR (id<>?3 AND status IN ('dispatching','uncertain'))))",
            params![agent_id, epoch, format!("{agent_id}:initial")],
            |row| row.get::<_, bool>(0),
        )?)
    }

    /// Input that a claim owns or can still take. Python `pending_batch` reads
    /// `pending` rows of the current epoch; `reserved` rows belong to a claim.
    pub fn has_claimable_input(
        connection: &Connection,
        agent_id: &str,
        epoch: i64,
    ) -> StoreResult<bool> {
        Ok(connection.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status IN ('pending','reserved'))",
            params![agent_id, epoch],
            |row| row.get::<_, bool>(0),
        )?)
    }
}

pub(super) fn acceptance_input(agent: &str, epoch: i64, turn: &str) -> Value {
    json!({"agent":agent,"epoch":epoch,"accepted":turn})
}
/// Input batches of one execution run that
/// [`SchedulerRepository::return_unstarted_input`] returns to the queue before
/// the provider received them. Unit: returned attempts. The next such start of
/// that run settles with its named cause instead, so a cause that never clears
/// cannot restart the agent forever.
pub const START_RETURN_LIMIT: usize = 3;
const WAITING_START: &str =
    "The native start result is unknown. Studio is checking the original input in native history.";

/// What a settled start outcome proves about how far its input went.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StartReach {
    /// The provider answered the request, or its outcome is unknown.
    Provider,
    /// The start stopped before its request; a submitted start may still have
    /// been written, so only an unsubmitted one returns its input.
    Preparation,
    /// The session proved that it never wrote the request, also after the
    /// durable submitted mark, so the input never reached the provider.
    NotWritten,
}

pub(super) struct StartErrorDecision {
    event_status: studio_runtime_schema::persistence::EventStatus,
    evidence: studio_runtime_schema::persistence::AttemptSubmission,
    visible: Option<AgentStatus>,
    in_flight: bool,
    return_busy_input: bool,
}
pub(super) fn start_error_decision(
    agent: &AgentRecord,
    epoch: i64,
    unknown: bool,
    reach: StartReach,
) -> StoreResult<StartErrorDecision> {
    let attempt = agent.start_attempt.as_ref();
    let submitted = ValidatedAttempt::submission(agent)? == StartSubmission::Submitted;
    let busy = attempt.and_then(|a| a.active_at_reservation) == Some(true);
    let current = agent.epoch == epoch;
    let visible = (current && agent.auto_wake == Some(true)).then_some(if busy {
        AgentStatus::Running
    } else if unknown {
        AgentStatus::Waiting
    } else {
        AgentStatus::Failed
    });
    Ok(StartErrorDecision {
        event_status: if unknown {
            if submitted {
                studio_runtime_schema::persistence::EventStatus::Uncertain
            } else {
                studio_runtime_schema::persistence::EventStatus::Reserved
            }
        } else if !current {
            studio_runtime_schema::persistence::EventStatus::Cancelled
        } else if reach == StartReach::NotWritten
            || (reach == StartReach::Preparation && !submitted)
        {
            studio_runtime_schema::persistence::EventStatus::Pending
        } else {
            studio_runtime_schema::persistence::EventStatus::Failed
        },
        evidence: if unknown {
            studio_runtime_schema::persistence::AttemptSubmission::Unknown
        } else if submitted {
            studio_runtime_schema::persistence::AttemptSubmission::Rejected
        } else {
            studio_runtime_schema::persistence::AttemptSubmission::Unsent
        },
        visible,
        in_flight: unknown || busy,
        return_busy_input: !unknown && busy && current && agent.auto_wake == Some(true),
    })
}
/// The provider states that the disk was full and it never handed this input to
/// the model (the Claude bridge could not write its pre-input receipt).
fn proven_unapplied_disk_full(error: &Value) -> bool {
    let data = &error["data"];
    data["turnStartOutcome"] == "not_applied"
        && data["diskFull"] == true
        && data["inputSubmitted"] != true
}

/// Return an idle start's exact input to the queue after a proven unapplied
/// disk-full rejection. The worker starts it again when free space returns; the
/// input is not failed and no submitted input is replayed. A busy (steer) start
/// keeps the ordinary busy-rejection path.
fn return_unapplied_input(
    tx: &Transaction<'_>,
    mut agent: AgentRecord,
    identity: &TurnAttemptIdentity,
    message: &str,
    input: Value,
) -> StoreResult<bool> {
    let receipt_input = json!({"agent":identity.agent_id,"epoch":identity.epoch,"input":input});
    if let Some(result) = ExecutionHistoryRepository::replay_settled(
        tx,
        &identity.attempt_id,
        ExecutionReceiptKind::StartResult,
        &receipt_input,
    )? {
        return result
            .as_bool()
            .ok_or_else(|| invalid_agent(&identity.agent_id));
    }
    let Some(attempt) = agent.start_attempt.as_ref() else {
        return Ok(false);
    };
    if attempt.active_at_reservation == Some(true) {
        return apply_start_error(
            tx,
            agent,
            identity,
            false,
            StartReach::Provider,
            message,
            input,
        );
    }
    if attempt.id.as_deref() != Some(identity.attempt_id.as_str())
        || attempt.epoch != Some(identity.epoch)
        || attempt.turn_id.is_some()
        || attempt.observed_turn_id.is_some()
        || agent.epoch != identity.epoch
        || agent.auto_wake != Some(true)
    {
        return Ok(false);
    }
    let events = validated_events(tx, &agent, attempt)?;
    ExecutionHistoryRepository::reject_attempt(tx, &identity.attempt_id, message)?;
    tx.execute_cached(
        "UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL WHERE agent=?1 AND epoch=?2 AND id IN(SELECT value FROM json_each(?3)) AND status IN ('reserved','dispatching','uncertain')",
        params![identity.agent_id, identity.epoch, events],
    )?;
    agent.status = AgentStatus::Queued;
    agent.extra.insert("error".into(), Value::Null);
    agent.in_flight = Some(false);
    agent.start_attempt = None;
    agent.start_outcome_hold = None;
    publish_agent(tx, &agent)?;
    ExecutionHistoryRepository::save_receipt(
        tx,
        &identity.attempt_id,
        ExecutionReceiptKind::StartResult,
        receipt_input,
        json!(true),
    )?;
    Ok(true)
}

pub(super) fn apply_start_error(
    tx: &Transaction<'_>,
    mut agent: AgentRecord,
    identity: &TurnAttemptIdentity,
    unknown: bool,
    reach: StartReach,
    message: &str,
    input: Value,
) -> StoreResult<bool> {
    let kind = if unknown {
        ExecutionReceiptKind::Unknown
    } else {
        ExecutionReceiptKind::StartResult
    };
    let receipt_input = json!({"agent":identity.agent_id,"epoch":identity.epoch,"input":input});
    if let Some(result) =
        ExecutionHistoryRepository::replay_settled(tx, &identity.attempt_id, kind, &receipt_input)?
    {
        return result
            .as_bool()
            .ok_or_else(|| invalid_agent(&identity.agent_id));
    }
    let Some(attempt) = agent.start_attempt.as_ref() else {
        return Ok(false);
    };
    if attempt.id.as_deref() != Some(identity.attempt_id.as_str())
        || attempt.epoch != Some(identity.epoch)
        || attempt.turn_id.is_some()
        || attempt.observed_turn_id.is_some()
        || attempt
            .account_key
            .as_deref()
            .is_some_and(|v| v != agent.account_key)
    {
        return Ok(false);
    }
    let events = validated_events(tx, &agent, attempt)?;
    let count = attempt.events.as_ref().map_or(0, Vec::len);
    let decision = start_error_decision(&agent, identity.epoch, unknown, reach)?;
    if decision.return_busy_input {
        ExecutionHistoryRepository::reject_attempt(tx, &identity.attempt_id, message)?;
        tx.execute_cached("UPDATE runtime_events SET status='pending',turn_id=NULL,error=CASE WHEN EXISTS(SELECT 1 FROM runtime_event_meta m WHERE m.id=runtime_events.id AND json_extract(m.record,'$.sendNow')=1) THEN ?1 ELSE NULL END WHERE agent=?2 AND epoch=?3 AND id IN(SELECT value FROM json_each(?4)) AND status IN ('reserved','dispatching','uncertain')",
            params![message, identity.agent_id, identity.epoch, events])?;
        agent.extra.insert(
            "steerRejectedTurnId".into(),
            agent
                .turn_id
                .clone()
                .map(Value::String)
                .filter(|v| v.is_string())
                .unwrap_or(json!("")),
        );
        agent.status = if agent.in_flight == Some(true) {
            AgentStatus::Running
        } else {
            AgentStatus::Queued
        };
        agent.extra.insert("error".into(), Value::Null);
        agent.start_attempt = None;
        publish_agent(tx, &agent)?;
        ExecutionHistoryRepository::save_receipt(
            tx,
            &identity.attempt_id,
            kind,
            receipt_input,
            json!(true),
        )?;
        return Ok(true);
    }
    let changed = tx.execute_cached("UPDATE runtime_events SET status=?1,error=?2 WHERE agent=?3 AND epoch=?4 AND id IN(SELECT value FROM json_each(?5)) AND status IN ('pending','reserved','dispatching','uncertain')",
        params![decision.event_status,message,identity.agent_id,identity.epoch,events])?;
    require_membership(changed, count, &identity.agent_id)?;
    if let Some(status) = decision.visible {
        agent.status = status;
        agent.extra.insert(
            "error".into(),
            json!(if unknown { WAITING_START } else { message }),
        );
        if unknown {
            agent
                .start_attempt
                .as_mut()
                .ok_or_else(|| invalid_agent(&identity.agent_id))?
                .response_error = Some(WAITING_START.into());
        }
    }
    agent.in_flight = Some(decision.in_flight);
    if !unknown {
        agent.start_outcome_hold = None;
    }
    agent
        .start_attempt
        .as_mut()
        .ok_or_else(|| invalid_agent(&identity.agent_id))?
        .execution_outcome = Some(
        decision
            .evidence
            .as_str()
            .parse()
            .map_err(|_| invalid_agent(&identity.agent_id))?,
    );
    publish_agent(tx, &agent)?;
    ExecutionHistoryRepository::save_receipt(
        tx,
        &identity.attempt_id,
        kind,
        receipt_input,
        json!(true),
    )?;
    Ok(true)
}

// A recovery receipt names an unresolved attempt only while it is open, as
// Python `codex_restart_recovery.restore` reads it: a restart marker while it is
// `pending` (or `superseded`, which keeps its continuation check), and a
// disconnect receipt while the agent is still `interrupted`. A `held`,
// `finished`, `continued`, `reattached` or `input_restored` marker, or the
// receipt of an agent that later returned to work, is history. A later provider
// generation must not interrupt or hold that agent again. `stored_attempt` is
// the Rust form of the same rule.
const OPEN_DISCONNECT_RECEIPT_SQL: &str =
    "COALESCE(json_extract(record,'$.status'),'')='interrupted'";
const OPEN_RESTART_MARKER_SQL: &str =
    "COALESCE(json_extract(record,'$.restartRecovery.stage'),'') IN ('pending','superseded')";
const OPEN_RESTART_MARKER_STAGES: [&str; 2] = ["pending", "superseded"];
fn unresolved_base_sql() -> String {
    let attempt = |field: &str| {
        format!(
            "json_extract(record,'$.startAttempt.{field}'),\
             CASE WHEN {OPEN_DISCONNECT_RECEIPT_SQL} \
               THEN json_extract(record,'$.disconnectRecovery.startAttempt.{field}') END,\
             CASE WHEN {OPEN_RESTART_MARKER_SQL} \
               THEN json_extract(record,'$.restartRecovery.startAttempt.{field}') END"
        )
    };
    format!(
        "COALESCE({})=1 AND COALESCE({},'unknown')='unknown'",
        attempt("submitted"),
        attempt("executionOutcome")
    )
}
// A queued agent whose saved attempt completed has new input but no new claim:
// its earlier turn is settled, so a new provider generation must not interrupt it.
const COMPLETED_START_SQL: &str = "COALESCE(json_extract(record,'$.status'),'') IN ('completed','waiting','waiting_on_monitor','paused','parked','queued') AND COALESCE(json_extract(record,'$.inFlight'),0)=0 AND COALESCE(json_extract(record,'$.startAttempt.submitted'),0)=1 AND json_extract(record,'$.startAttempt.action') IS NULL AND COALESCE(json_extract(record,'$.startAttempt.activeAtReservation'),0)=0 AND json_extract(record,'$.startOutcomeHold') IS NULL AND COALESCE(json_extract(record,'$.startAttempt.responseError'),'')='' AND COALESCE(json_extract(record,'$.startAttempt.executionOutcome'),'')!='unknown' AND COALESCE(json_type(record,'$.startAttempt.turnId'),'')='text' AND COALESCE(json_extract(record,'$.startAttempt.turnId'),'')!='' AND json_extract(record,'$.startAttempt.turnId')=COALESCE(json_extract(record,'$.lastCompletedTurn'),'') AND COALESCE(json_extract(record,'$.lastCompletedTurnStatus'),'')='completed'";
// An idle agent whose attempt turn is its last finished turn has nothing to
// recover, also when the attempt was reserved during an active turn
// (`activeAtReservation`). Python restart recovery never touches an idle agent.
// A turn that ended `failed` or `interrupted` is settled too: a later backend
// stop or provider generation must not interrupt it again or replace its error
// (rehearsal 11, agent 841ebc0e). The startup scan and every disconnect path
// select through this rule.
const SETTLED_IDLE_TURN_SQL: &str = "COALESCE(json_extract(record,'$.status'),'') IN ('completed','waiting','waiting_on_monitor','paused','parked','queued','failed','interrupted') AND COALESCE(json_extract(record,'$.inFlight'),0)=0 AND COALESCE(json_extract(record,'$.startAttempt.turnId'),json_extract(record,'$.startAttempt.observedTurnId'),'')!='' AND COALESCE(json_extract(record,'$.startAttempt.turnId'),json_extract(record,'$.startAttempt.observedTurnId'))=json_extract(record,'$.lastCompletedTurn') AND COALESCE(json_extract(record,'$.lastCompletedTurnStatus'),'') IN ('completed','failed','interrupted')";
pub(super) fn unresolved_sql() -> String {
    format!(
        "{} AND NOT ({COMPLETED_START_SQL}) AND NOT ({SETTLED_IDLE_TURN_SQL})",
        unresolved_base_sql()
    )
}
/// The live agents of account `?1` that match `predicate`; it searches the
/// account index instead of scanning agents.
fn account_agents_sql(predicate: &str) -> String {
    format!(
        "SELECT id,record FROM runtime_agents WHERE account_key=?1 AND deleted_at IS NULL AND ({predicate})"
    )
}
pub(super) fn account_agents(
    connection: &Connection,
    account: &str,
    predicate: &str,
) -> StoreResult<Vec<AgentRecord>> {
    let sql = account_agents_sql(predicate);
    let mut statement = connection.prepare_cached(&sql)?;
    let rows = statement
        .query_map([account], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut agents = Vec::with_capacity(rows.len());
    for (id, raw) in rows {
        match decode_agent(connection, &id, &raw) {
            Ok(agent) => agents.push(agent),
            Err(error) => {
                let held = connection.query_row(
                    "SELECT hold_status<>?1 FROM runtime_agents WHERE id=?2",
                    params![studio_runtime_schema::persistence::HoldStatus::Clear, id],
                    |row| row.get::<_, bool>(0),
                )?;
                if !held {
                    return Err(error);
                }
            }
        }
    }
    Ok(agents)
}
pub(super) fn select_agents(
    db: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> StoreResult<Vec<AgentRecord>> {
    let mut statement = db.prepare_cached(sql)?;
    let rows = statement.query_map(args, |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, raw) = row?;
        decode_agent(db, &id, &raw)
    })
    .collect()
}

pub(super) fn record_notification(
    transaction: &Transaction<'_>,
    agent: &mut AgentRecord,
) -> StoreResult<()> {
    agent.events = Some(agent.events.unwrap_or(0) + 1);
    let at: String =
        transaction.query_row_cached("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| {
            r.get(0)
        })?;
    agent.last_event = Some(at);
    Ok(())
}

pub(super) fn timestamp() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
pub(super) fn clear_hold(agent: &mut AgentRecord) {
    if !agent
        .start_outcome_hold
        .as_ref()
        .is_some_and(crate::StoredStartOutcomeHold::is_account_settlement)
    {
        return;
    }
    let Some(crate::StoredStartOutcomeHold::Known(crate::StartOutcomeHold::AccountSettlement {
        previous_native_failure_hold,
        previous_error,
        reason,
        ..
    })) = agent.start_outcome_hold.take()
    else {
        return;
    };
    if previous_native_failure_hold.present {
        agent.extra.insert(
            "nativeFailureHold".into(),
            previous_native_failure_hold.value,
        );
    } else {
        agent.extra.remove("nativeFailureHold");
    }
    if agent.extra.get("error").and_then(Value::as_str) == Some(reason.as_str()) {
        agent.extra.insert("error".into(), previous_error);
    }
}
/// Requeue an exact unsubmitted current attempt, or cancel a stale epoch's
/// reservation. Returns the requeued event count when the agent was requeued.
fn requeue_unsubmitted(
    transaction: &Transaction<'_>,
    mut agent: AgentRecord,
) -> StoreResult<Option<usize>> {
    let attempt = agent
        .start_attempt
        .as_ref()
        .ok_or_else(|| invalid_agent(&agent.id.0))?;
    if attempt.submitted == Some(true) {
        return Ok(None);
    }
    if attempt.submitted != Some(false) {
        return Err(invalid_agent(&agent.id.0));
    }
    let events = validated_events(transaction, &agent, attempt)?;
    let epoch = attempt.epoch.ok_or_else(|| invalid_agent(&agent.id.0))?;
    let exact: bool = transaction.query_row_cached(
        "SELECT NOT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='reserved' AND id NOT IN(SELECT value FROM json_each(?3))) AND (SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='reserved' AND id IN(SELECT value FROM json_each(?3)))=json_array_length(?3)",
        params![agent.id.0,epoch,events], |r|r.get(0))?;
    if !exact {
        if agent.epoch == epoch {
            return Err(invalid_agent(&agent.id.0));
        }
        return Ok(None);
    }
    let current = agent.epoch == epoch;
    if current
        && (agent.status != AgentStatus::Starting
            || agent.auto_wake != Some(true)
            || agent.in_flight != Some(true))
    {
        return Ok(None);
    }
    let count = transaction.execute_cached(
        "UPDATE runtime_events SET status=?1,turn_id=NULL,error=NULL WHERE agent=?2 AND epoch=?3 AND status='reserved' AND id IN(SELECT value FROM json_each(?4))",
        params![if current {"pending"} else {"cancelled"},agent.id.0,epoch,events])?;
    if !current {
        return Ok(None);
    }
    agent.start_attempt = None;
    agent.in_flight = Some(false);
    agent.status = AgentStatus::Queued;
    publish_agent(transaction, &agent)?;
    Ok(Some(count))
}

pub(super) fn disconnect_agent(
    transaction: &Transaction<'_>,
    agent: &mut AgentRecord,
    at: f64,
    restart: bool,
    auto_reconcile_unknown: bool,
) -> StoreResult<bool> {
    let attempt = stored_attempt(agent).cloned();
    if attempt
        .as_ref()
        .is_some_and(|a| attempt_already_decided(agent, a))
    {
        return Ok(false);
    }
    let epoch = match attempt.as_ref() {
        None => agent.epoch,
        Some(attempt) => attempt.epoch.ok_or_else(|| invalid_agent(&agent.id.0))?,
    };
    let paused = agent.status == AgentStatus::Paused;
    // Only the accepted attempt identifies the submitted turn.
    let turn = attempt
        .as_ref()
        .and_then(|a| a.turn_id.as_deref())
        .filter(|turn| !turn.is_empty())
        .map(str::to_owned);
    let unknown = turn.is_none();
    // A submitted start without a turn waits for one native history check of
    // its input. A backend restart holds every such start. A provider
    // disconnect while the backend runs holds a start whose reply the worker
    // already settled as unknown. Interrupted instead, its attempt stayed in
    // the disconnect receipt, which no recovery pass checks, and its input
    // stayed `uncertain` although the provider had applied it.
    let restart_input_wait = auto_reconcile_unknown
        && (restart
            || attempt.as_ref().and_then(|a| a.execution_outcome)
                == Some(studio_runtime_schema::persistence::ExecutionStatus::Unknown))
        && !paused
        && agent.auto_wake == Some(true)
        && attempt.as_ref().and_then(|a| a.submitted) == Some(true)
        && unknown
        && agent.thread_id.as_deref().is_some_and(|v| !v.is_empty())
        && restart_input_events_valid(
            transaction,
            &agent.id.0,
            epoch,
            attempt.as_ref().and_then(|a| a.events.as_deref()),
        )?;
    // `supervisor` is the attempt's supervisor identity, or null when it has
    // none. The connection generation stays in `startAttempt`, which recovery
    // scope matching reads; a generation alone identifies no supervisor.
    let connection_id = match agent.extra.get("connectionId") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => return Err(invalid_agent(&agent.id.0)),
    };
    let mut receipt = studio_runtime_schema::recovery_record::RecoveryFields {
        epoch: Some(
            studio_runtime_schema::recovery_record::SafeInteger::from_i64(epoch)
                .ok_or_else(|| invalid_agent(&agent.id.0))?,
        ),
        account_key: Some(agent.account_key.clone()),
        thread_id: agent.thread_id.clone(),
        turn_id: turn,
        start_attempt: attempt.clone().map(Box::new),
        connection_id,
        auto_wake: agent.auto_wake,
        ..Default::default()
    };
    receipt.extra.insert("at".into(), json!(at));
    receipt.extra.insert(
        "supervisor".into(),
        crate::projection_value(
            &attempt
                .as_ref()
                .and_then(|a| a.supervisor_identity.as_ref()),
        ),
    );
    if !agent
        .disconnect_recovery
        .as_deref()
        .is_some_and(|old| same_recovery_scope(old, &receipt))
    {
        agent.disconnect_recovery = Some(Box::new(receipt.clone()));
    } else if let Some(previous) = agent.disconnect_recovery.as_deref_mut() {
        // Binding a formerly unknown turn enriches the same receipt without revoking its saved authority.
        previous.turn_id = receipt.turn_id.clone();
        previous.start_attempt = receipt.start_attempt.clone();
    }
    if restart || restart_input_wait {
        if restart_input_wait {
            let mut marker = receipt.clone();
            marker.connection_id = None;
            marker.extra.remove("supervisor");
            marker.stage = Some("held".into());
            marker.extra.insert(
                "reason".into(),
                json!("Native input submission has no confirmed turn identity."),
            );
            agent.restart_recovery = Some(Box::new(marker));
            agent.disconnect_recovery = None;
            let event_ids = attempt
                .as_ref()
                .and_then(|a| a.events.as_ref())
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            let first_event = event_ids
                .first()
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            let error =
                format!("Context repair waits for a confirmed input receipt: {first_event}");
            agent.extra.insert("contextRepairWait".into(), json!({
                "error":error,
                "scope":"native",
                "source":{"id":agent.id.0,"accountKey":agent.account_key.as_str(),"epoch":agent.epoch,"threadId":agent.thread_id.as_deref(),"attemptId":null},
                "events":event_ids,
                "nextCheckAt":0
            }));
        } else {
            let mut marker = agent
                .disconnect_recovery
                .as_deref()
                .cloned()
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            marker.stage = Some(if unknown { "held" } else { "pending" }.into());
            // The same recovery keeps the record of its one automatic history check.
            if let Some(check) = agent
                .restart_recovery
                .as_deref()
                .filter(|previous| same_recovery_scope(previous, &marker))
                .and_then(|previous| previous.extra.get("automaticCheck"))
                .cloned()
            {
                marker.extra.insert("automaticCheck".into(), check);
            }
            if unknown {
                marker.extra.insert(
                    "reason".into(),
                    json!("Native input submission has no confirmed turn identity."),
                );
            }
            agent.restart_recovery = Some(Box::new(marker));
        }
        if let Some(receipt) = agent.disconnect_recovery.as_deref_mut() {
            receipt.source = Some(
                studio_runtime_schema::recovery_record::RecoverySource::Text("restart".into()),
            );
        }
    }
    if restart_input_wait {
        agent.status = AgentStatus::Queued;
    } else if !paused {
        agent.status = AgentStatus::Interrupted;
    }
    agent.auto_wake = Some(restart_input_wait);
    if restart_input_wait {
        agent.extra.remove("nativeFailureHold");
        agent.start_outcome_hold = None;
        let wait_error = agent.extra["contextRepairWait"]["error"].clone();
        agent.extra.insert("error".into(), wait_error);
        agent.start_attempt = None;
    } else if !paused
        && agent
            .start_outcome_hold
            .as_ref()
            .is_none_or(|h| !h.is_account_settlement())
    {
        agent.extra.insert(
            "error".into(),
            json!(if restart {
                BACKEND_RESTARTED_TURN_ERROR
            } else {
                PROVIDER_DISCONNECTED_TURN_ERROR
            }),
        );
    }
    agent.in_flight = Some(false);
    crate::domain::mark_sessions_lost(transaction, &agent.id.0)?;
    transaction.execute_cached("UPDATE runtime_events SET status='uncertain',error=?3 WHERE agent=?1 AND epoch=?2 AND status='dispatching'", params![agent.id.0,epoch,if restart {"Server restarted before delivery acknowledgement"} else {"Codex disconnected"}])?;
    if restart_input_wait {
        transaction.execute_cached("UPDATE runtime_events SET status='uncertain' WHERE agent=?1 AND epoch=?2 AND id IN (SELECT value FROM json_each(?3)) AND status IN ('reserved','dispatching','uncertain')", params![agent.id.0,epoch,serde_json::to_string(&attempt.as_ref().and_then(|a| a.events.as_ref()))?])?;
    }
    if !restart {
        agent.start_attempt = None;
    }
    publish_agent(transaction, agent)?;
    Ok(true)
}

fn restart_input_events_valid(
    transaction: &Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    events: Option<&[String]>,
) -> StoreResult<bool> {
    let Some(events) = events.filter(|events| !events.is_empty()) else {
        return Ok(false);
    };
    let mut seen = std::collections::BTreeSet::new();
    for event in events {
        let Some(id) = (!event.is_empty()).then_some(event.as_str()) else {
            return Ok(false);
        };
        if !seen.insert(id) {
            return Ok(false);
        }
        let exists = transaction.query_row_cached(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE id=?1 AND agent=?2 AND epoch=?3 AND status IN ('reserved','dispatching','uncertain'))",
            params![id, agent_id, epoch],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn interrupt_uncertain(
    attempt: &crate::StartAttemptView,
    thread: &str,
    turn: &str,
) -> bool {
    attempt.interrupt.as_ref().is_some_and(|receipt| {
        receipt.thread_id.as_deref() == Some(thread)
            && receipt.turn_id.as_deref() == Some(turn)
            && matches!(
                receipt.outcome,
                Some(crate::InterruptOutcome::Unknown | crate::InterruptOutcome::Accepted)
            )
    })
}
// Provider payload bytes may have been reserialized in the recovery snapshot.
// Keep every typed field in the optimistic concurrency check.
pub(super) fn same_typed_attempt(a: &crate::StartAttemptView, b: &crate::StartAttemptView) -> bool {
    fn without_payloads(mut attempt: crate::StartAttemptView) -> crate::StartAttemptView {
        fn blank(value: &mut studio_runtime_schema::raw_json::RawJson) {
            *value = studio_runtime_schema::raw_json::RawJson(
                serde_json::value::RawValue::from_string("null".into()).expect("valid null"),
            );
        }
        if let Some(request) = &mut attempt.native_request {
            blank(&mut request.params);
        }
        if let Some(settings) = &mut attempt.model_settings {
            if let Some(params) = &mut settings.params {
                blank(params);
            }
            if let Some(settings) = &mut settings.settings {
                blank(settings);
            }
        }
        if let Some(request) = &mut attempt.claude_input_request {
            blank(&mut request.configuration);
            for input in &mut request.input {
                blank(input);
            }
            for event in &mut request.events {
                for asset in &mut event.assets {
                    blank(asset);
                }
            }
        }
        if let Some(workspace) = &mut attempt.vm_workspace_preparation {
            blank(&mut workspace.params);
        }
        attempt
    }
    without_payloads(a.clone()) == without_payloads(b.clone())
}

pub(super) fn same_recovery_scope(
    a: &studio_runtime_schema::recovery_record::RecoveryFields,
    b: &studio_runtime_schema::recovery_record::RecoveryFields,
) -> bool {
    a.epoch == b.epoch
        && a.account_key == b.account_key
        && a.thread_id == b.thread_id
        && (a.turn_id.is_none() || b.turn_id.is_none() || a.turn_id == b.turn_id)
        && a.start_attempt.as_deref().and_then(|v| v.id.as_deref())
            == b.start_attempt.as_deref().and_then(|v| v.id.as_deref())
        && a.start_attempt
            .as_deref()
            .and_then(|v| v.connection_generation)
            == b.start_attempt
                .as_deref()
                .and_then(|v| v.connection_generation)
}

/// The attempt that a disconnect or restart may still have to resolve. Recovery
/// receipts count only while open; see `OPEN_RESTART_MARKER_SQL`.
pub(super) fn stored_attempt(agent: &AgentRecord) -> Option<&crate::StartAttemptView> {
    agent
        .start_attempt
        .as_deref()
        .or_else(|| {
            (agent.status == AgentStatus::Interrupted)
                .then_some(agent.disconnect_recovery.as_deref())
                .flatten()
                .and_then(|r| r.start_attempt.as_deref())
        })
        .or_else(|| {
            agent
                .restart_recovery
                .as_deref()
                .filter(|marker| matches!(marker.stage.as_deref(), Some("pending" | "superseded")))
                .and_then(|r| r.start_attempt.as_deref())
        })
}
/// Whether the provider turn of `attempt` already ended. The settled attempt
/// holds the `terminal` receipt that turn settlement saves on its execution
/// attempt, or its bound turn is a completed turn of the agent. A native
/// action attempt saved without `turnId` (live reviewers 8388eb70, 6f209a16
/// and d0e0015f) is proven only by that receipt.
pub(super) fn settled_attempt(
    connection: &Connection,
    agent: &AgentRecord,
    attempt: &crate::StartAttemptView,
) -> StoreResult<bool> {
    if let Some(turn) = bound_turn(agent, attempt).filter(|turn| !turn.is_empty())
        && (agent.extra.get("lastCompletedTurn").and_then(Value::as_str) == Some(turn)
            || completed_turn(connection, &agent.id.0, turn)?)
    {
        return Ok(true);
    }
    let Some(id) = attempt.id.as_deref() else {
        return Ok(false);
    };
    let status=connection.query_row_cached(
        "SELECT json_extract(record,'$.turnStatus') FROM runtime_execution_attempts WHERE id=?1",
        [id], |row|row.get::<_,Option<studio_runtime_schema::persistence::ExecutionStatus>>(0),
    ).optional()?.flatten();
    Ok(matches!(
        status,
        Some(
            studio_runtime_schema::persistence::ExecutionStatus::Completed
                | studio_runtime_schema::persistence::ExecutionStatus::Failed
                | studio_runtime_schema::persistence::ExecutionStatus::Interrupted
                | studio_runtime_schema::persistence::ExecutionStatus::Cancelled
        )
    ))
}
/// The id of the attempt that a recovery receipt of `agent` names.
fn recovery_attempt_ids(agent: &AgentRecord) -> impl Iterator<Item = &str> {
    [
        agent.disconnect_recovery.as_deref(),
        agent.restart_recovery.as_deref(),
    ]
    .into_iter()
    .filter_map(|receipt| receipt?.start_attempt.as_deref()?.id.as_deref())
}
fn restart_marker_open(marker: &studio_runtime_schema::recovery_record::RecoveryFields) -> bool {
    marker
        .stage
        .as_deref()
        .is_some_and(|stage| OPEN_RESTART_MARKER_STAGES.contains(&stage))
}
/// A recovery decision already recorded for this exact attempt is final. A later
/// provider generation adds no evidence about it, and applying it again would
/// undo the user's next message. Python `restore` never revisits such a marker.
fn attempt_already_decided(agent: &AgentRecord, attempt: &crate::StartAttemptView) -> bool {
    agent.restart_recovery.as_deref().is_some_and(|marker| {
        !restart_marker_open(marker)
            && marker
                .start_attempt
                .as_deref()
                .and_then(|value| value.id.as_ref())
                .is_some()
            && marker
                .start_attempt
                .as_deref()
                .and_then(|value| value.id.as_ref())
                == attempt.id.as_ref()
            && marker
                .epoch
                .as_ref()
                .and_then(|value| value.number().as_i64())
                == attempt.epoch
    })
}
pub(super) fn restart_continuation_allowed(agent: &AgentRecord) -> bool {
    let Some(marker) = agent.restart_recovery.as_deref() else {
        return false;
    };
    continuation_allowed(agent)
        && recovering(agent, agent.epoch)
        && restart_marker_open(marker)
        && agent
            .disconnect_recovery
            .as_deref()
            .and_then(|r| r.auto_wake)
            == Some(true)
        && marker.turn_id.is_some()
        && marker.turn_id.as_deref() == agent.turn_id.as_deref()
        && marker
            .epoch
            .as_ref()
            .and_then(|value| value.number().as_i64())
            == Some(agent.epoch)
        && marker.account_key.as_deref().unwrap_or("default") == agent.account_key
        && marker.thread_id.as_deref() == agent.thread_id.as_deref()
}
pub(super) fn transport_continuation_allowed(agent: &AgentRecord) -> bool {
    let Some(previous) = agent.disconnect_recovery.as_deref() else {
        return false;
    };
    let attempt = agent.start_attempt.as_deref();
    continuation_allowed(agent)
        && agent.status == AgentStatus::Interrupted
        && agent.in_flight != Some(true)
        && agent.auto_wake != Some(true)
        && agent.turn_id.is_some()
        && matches!(
            agent.extra.get("error").and_then(Value::as_str),
            Some(
                PROVIDER_DISCONNECTED_TURN_ERROR
                    | VM_PROVIDER_DISCONNECTED_TURN_ERROR
                    | LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR
            )
        )
        && !matches!(
            agent
                .restart_recovery
                .as_deref()
                .and_then(|v| v.stage.as_deref()),
            Some("pending" | "superseded")
        )
        && !matches!(previous.source.as_ref(), Some(studio_runtime_schema::recovery_record::RecoverySource::Text(source)) if source == "restart")
        && previous.auto_wake == Some(true)
        && previous.account_key.as_deref().unwrap_or("default") == agent.account_key
        && previous.thread_id.as_deref() == agent.thread_id.as_deref()
        && previous.turn_id.as_deref() == agent.turn_id.as_deref()
        && previous
            .epoch
            .as_ref()
            .and_then(|value| value.number().as_i64())
            == Some(agent.epoch)
        && agent.turn_epoch.is_none_or(|v| v == agent.epoch)
        && attempt.is_none_or(|a| {
            Some(a) == previous.start_attempt.as_deref()
                && a.submitted == Some(true)
                && a.epoch == Some(agent.epoch)
                && a.action.is_none()
        })
}

pub(super) fn continuation_allowed(agent: &AgentRecord) -> bool {
    !agent.is_deleted()
        && ![
            "nativeFailureHold",
            "accountTransferId",
            "workspaceOperation",
            "contextRepairWait",
        ]
        .iter()
        .any(|key| agent.extra.get(*key).is_some_and(truthy))
        && !matches!(
            agent
                .extra
                .get("contextRepair")
                .and_then(|v| v["phase"].as_str()),
            Some("preparing" | "submitted" | "unknown" | "ready")
        )
        && !agent.extra.get("nativeThreadBlock").is_some_and(|block| {
            block["threadId"].is_string()
                && block.get("threadId").and_then(Value::as_str) == agent.thread_id.as_deref()
        })
        && !agent.extra.get("error").is_some_and(|error| {
            known_error_kind(error) == Some(KnownCodexErrorKind::MisalignmentPolicyViolation)
        })
}
pub(super) fn load_agent(db: &Connection, id: &str) -> StoreResult<Option<AgentRecord>> {
    db.query_row_cached("SELECT record FROM runtime_agents WHERE id=?1", [id], |r| {
        r.get::<_, String>(0)
    })
    .optional()?
    .map(|raw| decode_agent(db, id, &raw))
    .transpose()
}
/// The checked agent row `id` with its volatile fields, typed.
pub(super) fn decode_agent(
    connection: &Connection,
    id: &str,
    raw: &str,
) -> StoreResult<AgentRecord> {
    crate::records::decode_agent_record(connection, id, raw)
}
pub(super) fn current_attempt(agent: &AgentRecord, epoch: i64) -> bool {
    agent.epoch == epoch && agent.turn_epoch == Some(epoch) && agent.auto_wake == Some(true)
}
fn manual_delivery_hold_tracks(
    agent: &AgentRecord,
    identity: &TurnAttemptIdentity,
    turn: &str,
) -> bool {
    let Some(attempt) = stored_attempt(agent) else {
        return false;
    };
    agent.epoch == identity.epoch
        && agent.auto_wake == Some(false)
        && agent.turn_epoch == Some(identity.epoch)
        && agent.turn_id.as_deref() == Some(turn)
        && attempt.id.as_deref() == Some(identity.attempt_id.as_str())
        && attempt.epoch == Some(identity.epoch)
        && bound_turn(agent, attempt) == Some(turn)
        && attempt.submitted == Some(true)
        && attempt
            .request_ticket
            .as_deref()
            .is_some_and(|ticket| !ticket.is_empty())
        && attempt
            .connection_generation
            .is_some_and(|generation| generation > 0)
        && agent.extra.get("manualRecoveryDeliveryHold")
            == Some(
                &json!({"epoch":identity.epoch,"attemptId":identity.attempt_id,"accountKey":agent.account_key,"threadId":agent.thread_id.as_deref(),"turnId":turn,"generation":attempt.connection_generation}),
            )
}
pub(super) fn stopped_attempt(agent: &AgentRecord, epoch: i64) -> bool {
    agent.status == AgentStatus::Paused
        && agent.auto_wake == Some(false)
        && agent.turn_epoch == Some(epoch)
}

pub(super) fn recovering(agent: &AgentRecord, epoch: i64) -> bool {
    agent.disconnect_recovery.as_deref().is_some_and(|r| {
        r.epoch.as_ref().and_then(|value| value.number().as_i64()) == Some(epoch)
            && r.account_key.as_deref() == Some(agent.account_key.as_str())
            && r.thread_id == agent.thread_id
    }) && agent.status == AgentStatus::Interrupted
        && agent.epoch == epoch
}

pub(super) fn mark_timing(tx: &Transaction<'_>, ids: &str, field: &str) -> StoreResult<()> {
    let exists: bool = tx.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='runtime_event_meta')",
        [],
        |r| r.get(0),
    )?;
    if exists {
        let path = format!("$.timing.{field}");
        tx.execute_cached("INSERT INTO runtime_event_meta(id,record) SELECT value,json_set('{}',?1,?2) FROM json_each(?3) WHERE 1 ON CONFLICT(id) DO UPDATE SET record=json_set(runtime_event_meta.record,?1,?2)",params![path,timestamp(),ids])?;
    }
    Ok(())
}
pub(super) fn publish_agent(transaction: &Transaction<'_>, agent: &AgentRecord) -> StoreResult<()> {
    let mut agent = agent.clone();
    crate::ExecutionHistoryRepository::record_current(transaction, &mut agent)?;
    AgentRepository::put(transaction, &agent)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AttemptAuthority {
    Current,
    Stopped,
    Recovery,
    Deleted,
    Inactive,
}
pub(super) fn attempt_authority(agent: &AgentRecord, epoch: i64) -> AttemptAuthority {
    if agent.is_deleted() {
        AttemptAuthority::Deleted
    } else if current_attempt(agent, epoch) {
        AttemptAuthority::Current
    } else if stopped_attempt(agent, epoch) {
        AttemptAuthority::Stopped
    } else if recovering(agent, epoch) {
        AttemptAuthority::Recovery
    } else {
        AttemptAuthority::Inactive
    }
}
pub(crate) struct ValidatedAttempt {
    agent: AgentRecord,
    event_ids: (String, usize),
    authority: AttemptAuthority,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartSubmission {
    Submitted,
    Unsubmitted,
}
impl ValidatedAttempt {
    /// The persisted boolean is proof; missing or malformed evidence is never unsent.
    pub(crate) fn submission(agent: &AgentRecord) -> StoreResult<StartSubmission> {
        match agent
            .start_attempt
            .as_ref()
            .and_then(|attempt| attempt.submitted)
        {
            Some(true) => Ok(StartSubmission::Submitted),
            Some(false) => Ok(StartSubmission::Unsubmitted),
            None => Err(invalid_agent(&agent.id.0)),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AcceptanceDecision {
    Ignore,
    InterruptOnly,
    Bind { visible: bool, interrupt: bool },
}
pub(super) fn acceptance_decision(
    agent: &AgentRecord,
    epoch: i64,
    turn: &str,
    authority: AttemptAuthority,
) -> AcceptanceDecision {
    if authority == AttemptAuthority::Deleted {
        return AcceptanceDecision::InterruptOnly;
    }
    if authority == AttemptAuthority::Inactive
        || agent
            .start_attempt
            .as_ref()
            .and_then(|attempt| attempt.turn_id.as_deref())
            .is_some_and(|old| old != turn)
    {
        return AcceptanceDecision::Ignore;
    }
    AcceptanceDecision::Bind {
        visible: authority == AttemptAuthority::Current,
        interrupt: agent.status == AgentStatus::Paused
            || agent.epoch != epoch
            || agent.auto_wake != Some(true),
    }
}
type ProviderScope<'a> = (&'a str, &'a str, u64, &'a str);
pub(super) fn load_attempt(
    tx: &Transaction<'_>,
    id: &str,
    epoch: i64,
    attempt_id: &str,
    scope: Option<ProviderScope<'_>>,
) -> StoreResult<Option<ValidatedAttempt>> {
    let Some(mut agent) = load_agent(tx, id)? else {
        return Ok(None);
    };
    if agent.start_attempt.is_none()
        && scope.is_some()
        && recovering(&agent, epoch)
        && let Some(attempt) = agent
            .disconnect_recovery
            .as_deref()
            .and_then(|r| r.start_attempt.as_deref())
            .cloned()
    {
        agent.start_attempt = Some(Box::new(attempt));
    }
    let Some(attempt) = agent.start_attempt.as_ref() else {
        return Ok(None);
    };
    if attempt.id.as_deref() != Some(attempt_id)
        || attempt.epoch != Some(epoch)
        || ValidatedAttempt::submission(&agent)? != StartSubmission::Submitted
        || attempt.account_key.as_deref() != Some(agent.account_key.as_str())
        || attempt.thread_id != agent.thread_id
    {
        return Ok(None);
    }
    if let Some((ticket, account, generation, thread)) = scope
        && (attempt.request_ticket.as_deref() != Some(ticket)
            || attempt.account_key.as_deref() != Some(account)
            || attempt.connection_generation != Some(generation)
            || attempt.thread_id.as_deref() != Some(thread))
    {
        return Ok(None);
    }
    let event_ids = (
        validated_events(tx, &agent, attempt)?,
        attempt.events.as_ref().map_or(0, Vec::len),
    );
    let authority = attempt_authority(&agent, epoch);
    Ok(Some(ValidatedAttempt {
        agent,
        event_ids,
        authority,
    }))
}

pub(super) fn deliver_attempt_events(
    transaction: &Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    event_ids: &(String, usize),
    turn_id: &str,
) -> StoreResult<()> {
    // A terminal frame proves the turn ended, not that input cancelled by an
    // atomic interrupt reached the model. Preserve its exact queue receipt.
    let held: i64 = transaction.query_row_cached(
        "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND epoch=?2 \
         AND id IN (SELECT value FROM json_each(?3)) \
         AND ((status='cancelled' AND error=?4) OR (status='uncertain' AND error=?5))",
        params![
            agent_id,
            epoch,
            event_ids.0,
            super::steer::STOP_CANCELLED_ERROR,
            super::steer::STOP_QUEUED_ERROR
        ],
        |row| row.get(0),
    )?;
    let changed = transaction.execute_cached(
        "UPDATE runtime_events SET status='delivered',turn_id=?1,error=NULL WHERE agent=?2 AND epoch=?3 AND id IN (SELECT value FROM json_each(?4)) AND (status IN ('reserved','dispatching','uncertain') OR (status='delivered' AND turn_id=?1)) AND NOT (status='uncertain' AND error IS ?5)",
        params![turn_id, agent_id, epoch, event_ids.0, super::steer::STOP_QUEUED_ERROR])?;
    let held = usize::try_from(held).map_err(|_| invalid_agent(agent_id))?;
    require_membership(changed + held, event_ids.1, agent_id)?;
    let event_ids: Vec<String> = serde_json::from_str(&event_ids.0)?;
    SchedulerRepository::project_delivered_chat_events(
        transaction,
        agent_id,
        epoch,
        turn_id,
        &event_ids,
    )
}

/// Keep the durable chat projection in the same transaction as scheduler
/// acceptance, whether acceptance came from a turn/start response or a native
/// userMessage receipt. Only an exact submitted event may advance its recipient.
impl SchedulerRepository {
    /// Idempotently project chat deliveries for events proven delivered to a turn.
    pub fn project_delivered_chat_events(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        turn_id: &str,
        event_ids: &[String],
    ) -> StoreResult<()> {
        Self::project_chat_event_status(
            transaction,
            agent_id,
            epoch,
            turn_id,
            event_ids,
            "delivered",
        )
    }

    pub(super) fn project_chat_event_status(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        turn_id: &str,
        event_ids: &[String],
        status: &str,
    ) -> StoreResult<()> {
        let messages_table_exists: bool = transaction.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_chat_messages')",
        [],
        |row| row.get(0),
    )?;
        if !messages_table_exists {
            return Ok(());
        }
        for event_id in event_ids {
            let Some(event) = crate::QueueEventRepository::get(
                transaction,
                &crate::QueueEventId::from(event_id.as_str()),
            )?
            else {
                continue;
            };
            if event.agent.0 != agent_id
                || event.epoch != epoch
                || event.status != status
                || (status == "delivered" && event.turn_id.as_deref() != Some(turn_id))
                || event.kind != "agent_message"
            {
                continue;
            }
            let Some(receipt) = studio_runtime_schema::notification_text::chat_receipt(&event.text)
            else {
                continue;
            };
            if event.id.0 != format!("chat:{}:{agent_id}", receipt.message_id) {
                continue;
            }
            let Some(mut message) = crate::MessageRepository::get(
                transaction,
                &crate::ChatMessageId::from(receipt.message_id.as_str()),
            )?
            else {
                continue;
            };
            if message.room != receipt.room
                || message.sender != receipt.sender
                || message.text != receipt.text
            {
                continue;
            }
            if let Some(delivery) = message
                .deliveries
                .as_object_mut()
                .and_then(|deliveries| deliveries.get_mut(agent_id))
                && (matches!(delivery.as_str(), Some("queued" | "pending"))
                    || (status != "delivered" && delivery.as_str() == Some("delivered")))
            {
                *delivery = json!(if status == "uncertain" {
                    "pending"
                } else {
                    status
                });
                crate::MessageRepository::put(transaction, &message)?;
            }
        }
        Ok(())
    }
}

pub(super) fn require_membership(changed: usize, expected: usize, agent: &str) -> StoreResult<()> {
    if changed != expected {
        return Err(invalid_agent(agent));
    }
    Ok(())
}

pub(super) fn validated_events(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    attempt: &crate::StartAttemptView,
) -> StoreResult<String> {
    let events = attempt
        .events
        .as_ref()
        .ok_or_else(|| invalid_agent(&agent.id.0))?;
    let events = serde_json::to_string(events)?;
    // A native action and a provider-started turn submit no queued input;
    // every other attempt submits at least one event.
    let empty_allowed = native_action_attempt(attempt) || provider_turn_attempt(attempt);
    let valid: bool = transaction.query_row_cached(
        "SELECT (COUNT(*) > 0 OR ?2) AND COUNT(*)=COUNT(DISTINCT value) FROM json_each(?1)",
        params![events, empty_allowed],
        |r| r.get(0),
    )?;
    if !valid {
        return Err(invalid_agent(&agent.id.0));
    }
    Ok(events)
}

/// True for a `compact` or `review` attempt ([`NATIVE_ACTION_ATTEMPT_KINDS`]).
pub(super) fn native_action_attempt(attempt: &crate::StartAttemptView) -> bool {
    matches!(
        attempt.action,
        Some(crate::StartAttemptAction::Compact | crate::StartAttemptAction::Review)
    )
}

/// Attempt id prefix of a turn the provider started on its own
/// ([`SchedulerRepository::adopt_provider_turn`]): `provider-turn:{agent}:{turn}`.
const PROVIDER_TURN_ATTEMPT_PREFIX: &str = "provider-turn:";

/// True for the attempt of a provider-started turn, which carries no input.
pub(super) fn provider_turn_attempt(attempt: &crate::StartAttemptView) -> bool {
    attempt.provider_turn == Some(true)
        && attempt
            .id
            .as_deref()
            .is_some_and(|id| id.starts_with(PROVIDER_TURN_ATTEMPT_PREFIX))
}

/// The provider turn an attempt is bound to. An ordinary attempt is bound by
/// its own `turnId`. A native action attempt saved before its acceptance bound
/// `startAttempt.turnId` (live reviewers 8388eb70, 6f209a16 and d0e0015f,
/// 20 October 2026) is bound by the agent turn on the same thread, which its
/// review/start reply or turn/started notice set. Without this, the terminal
/// notice of that turn stays deferred and holds the account journal.
pub(super) fn bound_turn<'a>(
    agent: &'a AgentRecord,
    attempt: &'a crate::StartAttemptView,
) -> Option<&'a str> {
    attempt.turn_id.as_deref().or_else(|| {
        (native_action_attempt(attempt)
            && attempt.thread_id.is_some()
            && attempt.thread_id.as_deref() == agent.thread_id.as_deref())
        .then_some(agent.turn_id.as_deref())
        .flatten()
    })
}

pub(super) fn invalid_agent(agent_id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: "runtime_agents".to_owned(),
        id: agent_id.to_owned(),
    }
}

pub(super) fn completed_turn(
    connection: &Connection,
    agent: &str,
    turn: &str,
) -> StoreResult<bool> {
    crate::domain::turn_completed(connection, agent, turn)
}

/// Python `codex_agent_management.parked_after_turn`: a park requested during
/// a turn takes effect when that turn ends; a stopped event waiter is parked.
fn parked_after_turn(agent: &mut AgentRecord) {
    if agent.parked_event.as_deref().is_none_or(str::is_empty) {
        return;
    }
    if agent.extra.get("parkAfterTurn").is_some_and(truthy) {
        agent.extra.remove("parkAfterTurn");
        agent.auto_wake = Some(false);
    }
    if agent.auto_wake != Some(true) {
        agent.status = AgentStatus::Parked;
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod account_scope_tests {
    #[test]
    fn clearing_account_hold_restores_prior_error_and_hold() {
        let mut agent: crate::AgentRecord = serde_json::from_str(
            r#"{"id":"a","rootId":"a","epoch":1,"status":"waiting","accountKey":"default","compactions":0,"error":"paused","nativeFailureHold":true,"startOutcomeHold":{"stage":"account_settlement","cursor":12,"reason":"paused","previousError":"original","previousNativeFailureHold":{"present":true,"value":"previous"}}}"#,
        )
        .expect("stored agent");
        super::clear_hold(&mut agent);
        assert_eq!(
            agent.extra.get("error"),
            Some(&serde_json::json!("original"))
        );
        assert_eq!(
            agent.extra.get("nativeFailureHold"),
            Some(&serde_json::json!("previous"))
        );
        assert!(agent.start_outcome_hold.is_none());
    }

    /// Account-scoped agent reads search the account index instead of scanning agents.
    #[test]
    fn account_agents_search_the_account_scope_index() {
        let reader = crate::fresh_state::test_canvas();
        let plan = reader
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                super::account_agents_sql("1")
            ))
            .expect("plan")
            .query_map(["default"], |row| row.get::<_, String>(3))
            .expect("plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan text");
        assert!(
            plan.iter()
                .any(|step| step.contains("USING INDEX runtime_agent_account ")),
            "{plan:?}"
        );
    }
}
