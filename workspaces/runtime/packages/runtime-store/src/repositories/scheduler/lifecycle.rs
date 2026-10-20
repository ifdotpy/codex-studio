use super::*;
use crate::CachedSql;

impl SchedulerRepository {
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
        apply_start_error(
            transaction,
            agent,
            identity,
            outcome == "uncertain",
            outcome != "uncertain",
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
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(&identity.agent_id))?;
        if attempt
            .get("turnId")
            .and_then(Value::as_str)
            .is_some_and(|known| known != turn_id)
        {
            return Ok(None);
        }
        // Do not use observedTurnId to constrain a later correlated acceptance.
        attempt.insert("observedTurnId".into(), json!(turn_id));
        record_notification(transaction, &mut agent)?;
        agent.extra.insert(
            "activity".into(),
            json!({"phase":"thinking","at":timestamp()}),
        );
        agent.extra.insert("activeTools".into(), json!([]));
        agent.extra.insert("lastAnswer".into(), json!(""));
        agent.extra.insert("turnId".into(), json!(turn_id));
        for key in [
            "nativeStatus",
            "nativeTurnError",
            "nativeSafetyBuffering",
            "nativeResponseTurn",
        ] {
            agent.extra.remove(key);
        }
        if current_attempt(&agent, identity.epoch) {
            agent.status = AgentStatus::Running;
            agent.extra.insert("inFlight".into(), json!(true));
            if agent.extra.get("error")
                == agent
                    .extra
                    .get("startAttempt")
                    .unwrap_or(&Value::Null)
                    .get("responseError")
                || agent.extra.get("error") == Some(&json!(UNKNOWN_START_ERROR))
            {
                agent.extra.insert("error".into(), Value::Null);
            }
        }
        if agent
            .extra
            .get("startOutcomeHold")
            .is_some_and(|h| h["attemptId"] == identity.attempt_id)
        {
            agent.extra.remove("startOutcomeHold");
        }
        let interrupt = agent.status == AgentStatus::Paused || agent.epoch != identity.epoch;
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
            if stored_attempt(&agent).unwrap_or(&Value::Null)["connectionGeneration"]
                .as_u64()
                .is_none_or(|g| g == generation)
                && disconnect_agent(transaction, &mut agent, at, restart, restart)?
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
                || agent.extra.get("executionMode").and_then(Value::as_str) != Some("vm")
                || stored_attempt(&agent).unwrap_or(&Value::Null)["connectionGeneration"]
                    .as_u64()
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
            let known = stored_attempt(&agent).and_then(|a| a["connectionGeneration"].as_u64());
            if known != Some(generation) {
                if disconnect_agent(transaction, &mut agent, at, restart, false)?
                    && agent.status == AgentStatus::Interrupted
                {
                    interrupted.push(agent.id.0);
                }
            } else if restart_continuation_allowed(&agent) {
                let previous_error = agent.extra.get("error").cloned().unwrap_or(Value::Null);
                agent.status = AgentStatus::Running;
                agent.extra.insert("autoWake".into(), json!(true));
                agent.extra.insert("inFlight".into(), json!(true));
                agent.extra.insert("error".into(), Value::Null);
                agent.extra.insert("connectionRecovery".into(),json!({"source":"native_thread_read","at":at,"turnId":agent.extra.get("turnId").unwrap_or(&Value::Null),"outcome":"active","automatic":true,"previousError":previous_error}));
                if let Some(marker) = agent.extra.get_mut("restartRecovery") {
                    marker["stage"] = json!("continued");
                    marker["observedAt"] = json!(at);
                    marker["outcome"] = json!("active");
                }
                publish_agent(transaction, &agent)?;
            } else if transport_continuation_allowed(&agent) {
                let previous_error = agent.extra.get("error").cloned().unwrap_or(Value::Null);
                agent.extra.insert("autoWake".into(), json!(true));
                agent.extra.insert("inFlight".into(), json!(true));
                agent.extra.insert("error".into(), Value::Null);
                agent.status = AgentStatus::Running;
                agent.extra.insert(
                    "connectionRecovery".into(),
                    json!({
                        "source":"native_thread_read", "at":at, "turnId":agent.extra.get("turnId").unwrap_or(&Value::Null),
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
    ) -> StoreResult<Option<Value>> {
        if agent
            .extra
            .get("startAttempt")
            .is_some_and(|a| !a.is_null())
        {
            return Ok(None);
        }
        let disconnect = agent
            .extra
            .get("disconnectRecovery")
            .filter(|_| agent.status == AgentStatus::Interrupted);
        let restart = agent
            .extra
            .get("restartRecovery")
            .filter(|marker| restart_marker_open(marker) || marker["stage"] == "held");
        let Some(attempt) = disconnect
            .or(restart)
            .and_then(|receipt| receipt.get("startAttempt"))
            .filter(|attempt| attempt.is_object())
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
            let Some(id) = attempt["id"].as_str() else {
                continue;
            };
            if !recovery_attempt_ids(&agent).any(|saved| saved == id)
                || !settled_attempt(transaction, &agent, &attempt)?
            {
                continue;
            }
            // A later turn that is still open is not proven ended.
            if let Some(turn) = agent
                .extra
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|turn| !turn.is_empty())
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
            if agent
                .extra
                .get("startAttempt")
                .and_then(|a| a["id"].as_str())
                == Some(id)
                && (last.is_none() || bound_turn(&agent, &attempt) != last.as_deref())
            {
                agent.extra.remove("startAttempt");
                changed = true;
            }
            if let Some(marker) = agent.extra.get_mut("restartRecovery")
                && restart_marker_open(marker)
                && marker["startAttempt"]["id"] == id
            {
                marker["stage"] = json!("finished");
                marker["reconciledAt"] = json!(at);
                marker["outcome"] = json!("settled");
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

    /// The error and retained cursor use existing fields published to the UI.
    pub fn hold_account(
        transaction: &Transaction<'_>,
        account: &str,
        cursor: Option<u64>,
        reason: &str,
    ) -> StoreResult<()> {
        for mut agent in account_agents(
            transaction,
            account,
            "(json_extract(record,'$.startAttempt') IS NOT NULL OR json_extract(record,'$.status')='queued')",
        )? {
            let previous_error = agent
                .extra
                .get("startOutcomeHold")
                .filter(|hold| hold["stage"] == "account_settlement")
                .and_then(|hold| hold.get("previousError"))
                .cloned()
                .unwrap_or_else(|| agent.extra.get("error").cloned().unwrap_or(Value::Null));
            let previous_hold = agent.extra.get("startOutcomeHold").filter(|h| h["stage"] == "account_settlement")
                .and_then(|h| h.get("previousNativeFailureHold")).cloned()
                .unwrap_or_else(|| json!({"present":agent.extra.contains_key("nativeFailureHold"),"value":agent.extra.get("nativeFailureHold")}));
            agent.extra.insert("nativeFailureHold".into(), json!(true));
            agent.extra.insert("error".into(), json!(reason));
            agent.extra.insert("startOutcomeHold".into(), json!({"stage":"account_settlement","cursor":cursor,"reason":reason,"previousError":previous_error,"previousNativeFailureHold":previous_hold}));
            publish_agent(transaction, &agent)?;
        }
        Ok(())
    }

    pub fn clear_account_hold(transaction: &Transaction<'_>, account: &str) -> StoreResult<()> {
        for mut agent in account_agents(
            transaction,
            account,
            "json_extract(record,'$.startOutcomeHold.stage')='account_settlement'",
        )? {
            clear_hold(&mut agent);
            publish_agent(transaction, &agent)?;
        }
        Ok(())
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
        let Some(attempt) = agent.extra.get("startAttempt") else {
            return Ok(false);
        };
        let active_turn = !turn.is_empty()
            && ["turnId", "observedTurnId"]
                .iter()
                .any(|k| attempt[*k] == turn);
        let active_review = turn.is_empty()
            && agent.extra.get("nativeReview").is_some_and(truthy)
            && agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true)
            && attempt.get("action").and_then(Value::as_str) == Some("review")
            && attempt.get("submitted").and_then(Value::as_bool) == Some(true)
            && attempt.get("threadId").and_then(Value::as_str) == Some(thread);
        if agent.extra.get("threadId").and_then(Value::as_str) != Some(thread)
            || !(active_turn || active_review)
            || interrupt_uncertain(attempt, thread, turn)
        {
            return Ok(false);
        }
        if let Some(attempt) = agent.extra.get_mut("startAttempt") {
            attempt["interrupt"] = json!({"requestTicket":ticket,"generation":generation,"threadId":thread,"turnId":turn,"outcome":"unknown"});
        }
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
        let generation = i64::try_from(generation)
            .map_err(|_| StoreError::SchemaMismatch("invalid provider generation".into()))?;
        let records = select_agents(
            tx,
            "SELECT id,record FROM runtime_agents WHERE json_extract(record,'$.startAttempt.interrupt.requestTicket')=?1 AND json_extract(record,'$.startAttempt.interrupt.generation')=?2",
            params![ticket, generation],
        )?;
        for mut agent in records {
            let receipt = &agent.extra.get("startAttempt").unwrap_or(&Value::Null)["interrupt"];
            let previous = receipt["outcome"]
                .as_str()
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            if previous == outcome && receipt.get("responseError").and_then(Value::as_str) == error
            {
                continue;
            }
            if previous != "unknown"
                || (previous == outcome && receipt.get("responseError").is_some())
            {
                return Err(invalid_agent(&agent.id.0));
            }
            if let Some(attempt) = agent.extra.get_mut("startAttempt") {
                attempt["interrupt"]["outcome"] = json!(outcome);
                if let Some(error) = error {
                    attempt["interrupt"]["responseError"] = json!(error);
                }
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
        agent.extra.insert("autoWake".into(), json!(false));
        agent.extra.insert("error".into(), json!(reason));
        if let Some(mut park) = agent.extra.remove("parkReceipt") {
            park["cancelledAtEpoch"] = json!(agent.epoch);
            agent.extra.insert("cancelledPark".into(), park);
        }
        for field in ["parkedEvent", "parkAfterTurn"] {
            agent.extra.remove(field);
        }
        tx.execute_cached(
            "UPDATE runtime_events SET status='cancelled' WHERE agent=?1 AND status='pending'",
            [id],
        )?;
        publish_agent(tx, &agent)?;
        Ok(true)
    }

    /// Account sessions that must be reattached to observe any submitted turn.
    pub fn accounts_with_unknown_submitted_turns(
        connection: &Connection,
    ) -> StoreResult<Vec<String>> {
        let mut statement = connection.prepare_cached(
            "SELECT DISTINCT COALESCE(json_extract(record,'$.accountKey'),'default') \
             FROM runtime_agents \
             WHERE COALESCE(json_extract(record,'$.provider'),'codex')='codex' \
             AND json_extract(record,'$.startAttempt.submitted')=1 \
             AND json_extract(record,'$.startAttempt.connectionGeneration') IS NOT NULL \
             AND json_extract(record,'$.status') IN ('starting','running','waiting','paused','interrupted') \
             AND json_extract(record,'$.deletedAt') IS NULL \
             ORDER BY 1",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Submitted `turn/start` requests whose reply may still arrive after reattach.
    pub fn submitted_start_requests(
        connection: &Connection,
        account_key: &str,
        generation: Option<u64>,
    ) -> StoreResult<Vec<SubmittedStartRequest>> {
        let agents = account_agents(connection, account_key, &unresolved_sql())?;
        let mut result = Vec::new();
        for agent in agents {
            let Some(attempt) = stored_attempt(&agent) else {
                continue;
            };
            if attempt["turnId"].is_string()
                || attempt["requestTicket"].is_null()
                || generation.is_some_and(|g| attempt["connectionGeneration"].as_u64() != Some(g))
            {
                continue;
            }
            let invalid = || invalid_agent(&agent.id.0);
            result.push((
                agent.id.0.clone(),
                attempt["epoch"].as_i64().ok_or_else(invalid)?,
                attempt["id"].as_str().ok_or_else(invalid)?.into(),
                attempt["requestTicket"]
                    .as_str()
                    .ok_or_else(invalid)?
                    .into(),
                attempt["threadId"].as_str().ok_or_else(invalid)?.into(),
            ));
        }
        Ok(result)
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
        let mut statement = connection.prepare_cached(
            "SELECT id,record FROM runtime_agents \
             WHERE json_extract(record,'$.threadId')=?1 \
             AND COALESCE(json_extract(record,'$.accountKey'),'default')=?2 \
             ORDER BY rowid LIMIT 2",
        )?;
        let rows = statement
            .query_map(params![thread_id, account_key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let [(_, record)] = rows.as_slice() else {
            return Ok((!rows.is_empty(), None));
        };
        let agent: AgentRecord =
            serde_json::from_str(record).map_err(|_| StoreError::InvalidSyncObject {
                scope: "runtime_agents".to_owned(),
                id: rows[0].0.clone(),
            })?;
        let Some(attempt) = stored_attempt(&agent).and_then(Value::as_object) else {
            return Ok((true, None));
        };
        let Some(attempt_id) = attempt.get("id").and_then(Value::as_str) else {
            return Err(invalid_agent(&agent.id.0));
        };
        let Some(epoch) = attempt.get("epoch").and_then(Value::as_i64) else {
            return Err(invalid_agent(&agent.id.0));
        };
        let completed_turn = match turn_id {
            Some(turn) => completed_turn(connection, &agent.id.0, turn)?,
            None => false,
        };
        let current_attempt = agent.epoch == epoch
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch)
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
            && agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true);
        let stopped_after_submit = agent.status == AgentStatus::Paused
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(false)
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch);
        if (!current_attempt && !stopped_after_submit && !recovering(&agent, epoch))
            || attempt.get("submitted").and_then(Value::as_bool) != Some(true)
            || attempt.get("accountKey").and_then(Value::as_str) != Some(account_key)
            || attempt.get("threadId").and_then(Value::as_str) != Some(thread_id)
            || attempt.get("connectionGeneration").and_then(Value::as_u64) != Some(generation)
            || attempt
                .get("requestTicket")
                .and_then(Value::as_str)
                .is_none()
            || completed_turn
            || attempt
                .get("executionOutcome")
                .and_then(Value::as_str)
                .is_some_and(|v| v != "unknown")
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

    /// Persist the uncertainty boundary before calling the turn-driver port.
    pub fn mark_submitted(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let matches = agent.epoch == epoch
            && agent.status == AgentStatus::Starting
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
            && agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true)
            && !agent.extra.get("nativeFailureHold").is_some_and(truthy)
            && agent
                .extra
                .get("startAttempt")
                .and_then(Value::as_object)
                .is_some_and(|attempt| {
                    attempt.get("id").and_then(Value::as_str) == Some(attempt_id)
                        && attempt.get("epoch").and_then(Value::as_i64) == Some(epoch)
                        && attempt.get("submitted").and_then(Value::as_bool) == Some(false)
                });
        if !matches {
            return Ok(false);
        }
        let mut count = 0;
        for event_id in agent.extra.get("startAttempt").unwrap_or(&Value::Null)["events"]
            .as_array()
            .ok_or_else(|| invalid_agent(agent_id))?
        {
            let id = event_id.as_str().ok_or_else(|| invalid_agent(agent_id))?;
            count += transaction.execute_cached(
                "UPDATE runtime_events SET status='dispatching' WHERE id=?1 AND agent=?2 \
                 AND epoch=?3 AND status='reserved'",
                params![id, agent_id, epoch],
            )?;
        }
        let event_count = agent.extra.get("startAttempt").unwrap_or(&Value::Null)["events"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default();
        if count != event_count {
            return Err(StoreError::SchemaMismatch(
                "scheduler reservation changed before submission".to_owned(),
            ));
        }
        if let Some(attempt) = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
        {
            attempt.insert("submitted".to_owned(), Value::Bool(true));
        }
        AgentRepository::put(transaction, &agent)?;
        EventWriteRepository::set_agent_sync_lifecycle(
            transaction,
            agent_id,
            "starting",
            true,
            None,
        )?;
        Ok(true)
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
            "submission",
            &receipt_input,
        )? {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
        // SCHEDULER-POLICY INTEGRATION: call reconcile_obsolete_before_submit(
        // transaction, agent_id, epoch, attempt_id) here; false stops submission.
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(false);
        };
        let Some(attempt) = agent.extra.get("startAttempt") else {
            return Ok(false);
        };
        if agent.epoch != epoch
            || agent
                .extra
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != account_key
            || agent.extra.get("threadId").and_then(Value::as_str) != Some(thread_id)
            || agent.status != AgentStatus::Starting
            || agent.extra.get("autoWake") != Some(&json!(true))
            || agent.extra.get("inFlight") != Some(&json!(true))
            || agent.extra.get("nativeFailureHold").is_some_and(truthy)
            || attempt["id"] != attempt_id
            || attempt["epoch"] != epoch
            || attempt["submitted"] != false
        {
            return Ok(false);
        }
        if let Some(saved) = crate::ExecutionHistoryRepository::attempt(transaction, attempt_id)?
            && saved.get("claimResult").is_some()
        {
            replay_claim(transaction, agent.clone(), epoch, &saved)?;
        }
        let events = validated_events(transaction, &agent, attempt)?;
        let changed = transaction.execute_cached(
            "UPDATE runtime_events SET status='dispatching' WHERE agent=?1 AND epoch=?2 AND status='reserved' AND id IN (SELECT value FROM json_each(?3))",
            params![agent_id, epoch, events])?;
        require_membership(
            changed,
            attempt["events"].as_array().map_or(0, Vec::len),
            agent_id,
        )?;
        let attempt = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.extend(
            json!({"submitted":true,"requestTicket":request_id,"nativeOperationId":format!("turn:{agent_id}:{}:attempt:{attempt_id}",attempt["events"][0].as_str().unwrap_or(attempt_id)),"connectionId":connection_id,"supervisorIdentity":null,"settingsFixed":true,
            "accountKey":account_key,"connectionGeneration":generation,"threadId":thread_id})
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone())),
        );
        agent
            .extra
            .entry("complaintsPresented".into())
            .or_insert(json!([]));
        publish_agent(transaction, &agent)?;
        ExecutionHistoryRepository::save_receipt(
            transaction,
            attempt_id,
            "submission",
            receipt_input,
            json!(true),
        )?;
        Ok(true)
    }

    /// Return only event reservations whose persisted attempt proves no turn was submitted.
    pub fn recover_unsubmitted(transaction: &Transaction<'_>) -> StoreResult<usize> {
        // Only accounts with a reserved event can match the per-account
        // predicate below; the runtime_event_queue index finds them directly.
        let mut statement = transaction.prepare_cached("SELECT DISTINCT COALESCE(json_extract(a.record,'$.accountKey'),'default') FROM runtime_agents a WHERE a.id IN (SELECT agent FROM runtime_events WHERE status='reserved')")?;
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

    /// Return one unsubmitted start attempt's reserved input to the queue. Its
    /// input never reached `turn/start`, so the retry cannot repeat a message.
    pub fn requeue_unsubmitted_attempt(
        transaction: &Transaction<'_>,
        identity: &TurnAttemptIdentity,
    ) -> StoreResult<bool> {
        let Some(agent) = load_agent(transaction, &identity.agent_id)? else {
            return Ok(false);
        };
        if agent.extra.get("startAttempt").is_none_or(|attempt| {
            attempt["id"] != identity.attempt_id
                || attempt["epoch"] != identity.epoch
                || attempt["submitted"] != false
                // An earlier outcome of the same request (an uncertain write,
                // then the resolution when its provider session closes)
                // already settled this attempt and released its input.
                || attempt["executionOutcome"]
                    .as_str()
                    .is_some_and(|outcome| outcome != "unknown")
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
            let mut agent = match decode_agent(&id, &raw) {
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
            "startResult",
            &acceptance_input(agent_id, epoch, turn_id),
        )? {
            let saved = ExecutionHistoryRepository::attempt(transaction, attempt_id)?
                .ok_or_else(|| invalid_agent(agent_id))?;
            if saved["requestTicket"] != request_id
                || saved["accountKey"] != account_key
                || saved["connectionGeneration"].as_u64() != Some(generation)
                || saved["threadId"] != thread_id
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
            "startResult",
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
            "startResult",
            &receipt_input,
        )? {
            let saved = ExecutionHistoryRepository::attempt(transaction, attempt_id)?
                .ok_or_else(|| invalid_agent(agent_id))?;
            if saved["requestTicket"] != request_id
                || saved["accountKey"] != account_key
                || saved["connectionGeneration"].as_u64() != Some(generation)
                || saved["threadId"] != thread_id
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
        let attempt = agent.extra.get("startAttempt").unwrap_or(&Value::Null);
        if attempt["id"] != attempt_id
            || attempt["epoch"] != epoch
            || attempt["submitted"] != true
            || attempt["accountKey"] != account_key
            || attempt["threadId"] != thread_id
            || agent
                .extra
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != account_key
            || agent.extra.get("threadId").and_then(Value::as_str) != Some(thread_id)
            || attempt["requestTicket"] != request_id
            || attempt["connectionGeneration"].as_u64() != Some(generation)
            || attempt["events"]
                .as_array()
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
            "startResult",
            &receipt_input,
        )? {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(false);
        };
        let attempt = agent.extra.get("startAttempt").unwrap_or(&Value::Null);
        if attempt["id"] != attempt_id
            || attempt["epoch"] != epoch
            || attempt["submitted"] != true
            || attempt["accountKey"] != account_key
            || attempt["threadId"] != thread_id
            || attempt["requestTicket"] != request_id
            || attempt["connectionGeneration"].as_u64() != Some(generation)
            || attempt["events"]
                .as_array()
                .is_none_or(|events| !events.is_empty())
        {
            return Ok(false);
        }
        let decision = start_error_decision(&agent, epoch, false, false);
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
        agent.extra.insert("inFlight".into(), json!(false));
        agent.extra.remove("startOutcomeHold");
        agent
            .extra
            .get_mut("startAttempt")
            .ok_or_else(|| invalid_agent(agent_id))?["executionOutcome"] = json!("rejected");
        publish_agent(transaction, &agent)?;
        ExecutionHistoryRepository::save_receipt(
            transaction,
            attempt_id,
            "startResult",
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
            "startResult",
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
            false,
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
            "startResult",
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
    /// Recheck the exact held snapshot and uncertain event rows before the
    /// ordinary acceptance path writes any agent or event state.
    pub fn settle_restart_started(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        expected_attempt: &Value,
        expected_wait: &Value,
        turn_id: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(false);
        };
        let marker = agent.extra.get("restartRecovery").unwrap_or(&Value::Null);
        let attempt_id = expected_attempt["id"]
            .as_str()
            .ok_or_else(|| invalid_agent(agent_id))?;
        if agent.epoch != epoch
            || agent.extra.get("contextRepairWait") != Some(expected_wait)
            || marker["stage"] != "held"
            || marker["startAttempt"] != *expected_attempt
            || agent
                .extra
                .get("startAttempt")
                .is_some_and(|attempt| attempt != expected_attempt)
            || expected_attempt["id"] != attempt_id
            || expected_attempt["epoch"] != epoch
            || expected_attempt["submitted"] != true
            || expected_attempt["executionOutcome"] != "unknown"
            || expected_attempt["accountKey"]
                != agent
                    .extra
                    .get("accountKey")
                    .cloned()
                    .unwrap_or(json!("default"))
            || expected_attempt["threadId"]
                != agent.extra.get("threadId").cloned().unwrap_or(Value::Null)
        {
            return Ok(false);
        }
        let event_ids = validated_events(transaction, &agent, expected_attempt)?;
        let expected_count = expected_attempt["events"].as_array().map_or(0, Vec::len);
        let uncertain_count: i64 = transaction.query_row_cached(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND epoch=?2 AND id IN (SELECT value FROM json_each(?3)) AND status='uncertain' AND turn_id IS NULL",
            params![agent_id, epoch, event_ids],
            |row| row.get(0),
        )?;
        if usize::try_from(uncertain_count).ok() != Some(expected_count) {
            return Ok(false);
        }
        agent
            .extra
            .insert("startAttempt".into(), expected_attempt.clone());
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
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.insert("turnId".into(), json!(turn_id));
        if agent.epoch == epoch {
            attempt.remove("executionOutcome");
        }
        agent.extra.insert("turnId".into(), json!(turn_id));
        agent.extra.insert("inFlight".into(), json!(true));
        if current_attempt || recovering(&agent, epoch) {
            if current_attempt {
                agent.status = AgentStatus::Running;
                agent.extra.insert("error".into(), Value::Null);
            }
            if agent
                .extra
                .get("startOutcomeHold")
                .and_then(|h| h["attemptId"].as_str())
                == Some(attempt_id)
            {
                agent.extra.remove("startOutcomeHold");
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
            "startResult",
            receipt_input,
            json!({"bound":true,"interrupt":interrupt}),
        )?;
        Ok(true)
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
        let receipt_input = json!({"agent":agent_id,"epoch":epoch,"turn":turn_id,"status":status,"error":error,"nativeStatus":native_status,"nativeError":native_error});
        if let Some(result) =
            ExecutionHistoryRepository::replay(transaction, attempt_id, "terminal", &receipt_input)?
        {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
        Self::resolve_steers_at_terminal(transaction, agent_id, turn_id, unapplied_steers)?;
        let outcome = match status {
            AgentStatus::Completed => "completed",
            AgentStatus::Failed => "failed",
            AgentStatus::Interrupted => "interrupted",
            _ => {
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
            .extra
            .get("startAttempt")
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
                .extra
                .get_mut("startAttempt")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| invalid_agent(agent_id))?;
            attempt.insert("turnId".into(), json!(turn_id));
            if native_status == "completed" && outcome == "failed" {
                attempt.insert("executionOutcome".into(), json!(outcome));
            }
        }
        record_notification(transaction, &mut agent)?;
        if let Some(marker) = agent.extra.get_mut("restartRecovery")
            && marker["turnId"] == turn_id
            && matches!(marker["stage"].as_str(), Some("reattached" | "continued"))
        {
            marker["stage"] = json!("finished");
            marker["reconciledAt"] = json!(timestamp());
            marker["outcome"] = json!(native_status);
        }
        // A later turn of the same thread finished, so the restart turn can no
        // longer continue. An open marker would let the next restart interrupt
        // this idle agent again (rehearsal 9, agent 3a35babb).
        let thread = agent.extra.get("threadId").cloned();
        if let Some(marker) = agent.extra.get_mut("restartRecovery")
            && restart_marker_open(marker)
            && marker["turnId"].as_str().is_some_and(|old| old != turn_id)
            && marker["epoch"] == epoch
            && thread.is_some_and(|thread| marker["threadId"] == thread)
        {
            marker["stage"] = json!("finished");
            marker["reconciledAt"] = json!(timestamp());
            marker["outcome"] = json!("replaced");
            marker["replacedByTurn"] = json!(turn_id);
        }
        let clears_unknown = agent
            .extra
            .get("startOutcomeHold")
            .and_then(|h| h["attemptId"].as_str())
            == Some(attempt_id);
        if clears_unknown {
            agent.extra.remove("startOutcomeHold");
        }
        transaction.execute_cached(
            "INSERT INTO runtime_completed_turns(id) VALUES (?1)",
            [format!("{agent_id}:{turn_id}")],
        )?;
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
        agent.extra.insert("turnId".into(), Value::Null);
        agent.extra.insert("activity".into(), Value::Null);
        agent
            .extra
            .insert("activeTools".into(), serde_json::json!([]));
        agent.extra.insert("inFlight".into(), json!(false));
        if status == AgentStatus::Failed {
            agent.extra.insert("nativeFailureHold".into(), json!(true));
        }
        for field in [
            "nativeStatus",
            "nativeTurnError",
            "nativeSafetyBuffering",
            "nativeResponseTurn",
        ] {
            agent.extra.remove(field);
        }
        if let Some(error) = error.as_ref()
            && error["codexErrorInfo"] == "misalignmentPolicyViolation"
        {
            agent.extra.insert(
                "nativeThreadBlock".into(),
                json!({"threadId":agent.extra.get("threadId"),"error":error}),
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
            native_status,
            native_error,
        )?;
        ExecutionHistoryRepository::save_receipt(
            transaction,
            attempt_id,
            "terminal",
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
const UNKNOWN_START_ERROR: &str = "turn/start response timed out; outcome unknown";
const NOT_SENT_ERROR: &str = "Provider did not send turn/start";
const WAITING_START: &str =
    "The native start result is unknown. Studio is checking the original input in native history.";

pub(super) struct StartErrorDecision {
    event_status: &'static str,
    evidence: &'static str,
    visible: Option<AgentStatus>,
    in_flight: bool,
    return_busy_input: bool,
}
pub(super) fn start_error_decision(
    agent: &AgentRecord,
    epoch: i64,
    unknown: bool,
    preparation: bool,
) -> StartErrorDecision {
    let attempt = &agent.extra.get("startAttempt").unwrap_or(&Value::Null);
    let submitted = attempt["submitted"] == true;
    let busy = attempt["activeAtReservation"] == true;
    let current = agent.epoch == epoch;
    let visible = (current && agent.extra.get("autoWake").is_some_and(truthy)).then_some(if busy {
        AgentStatus::Running
    } else if unknown {
        AgentStatus::Waiting
    } else {
        AgentStatus::Failed
    });
    StartErrorDecision {
        event_status: if unknown {
            if submitted { "uncertain" } else { "reserved" }
        } else if !current {
            "cancelled"
        } else if preparation && !submitted {
            "pending"
        } else {
            "failed"
        },
        evidence: if unknown {
            "unknown"
        } else if submitted {
            "rejected"
        } else {
            "unsent"
        },
        visible,
        in_flight: unknown || busy,
        return_busy_input: !unknown
            && busy
            && current
            && agent.extra.get("autoWake").is_some_and(truthy),
    }
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
        "startResult",
        &receipt_input,
    )? {
        return result
            .as_bool()
            .ok_or_else(|| invalid_agent(&identity.agent_id));
    }
    let Some(attempt) = agent.extra.get("startAttempt") else {
        return Ok(false);
    };
    if attempt["activeAtReservation"] == true {
        return apply_start_error(tx, agent, identity, false, false, message, input);
    }
    if attempt["id"] != identity.attempt_id
        || attempt["epoch"] != identity.epoch
        || attempt["turnId"].is_string()
        || attempt["observedTurnId"].is_string()
        || agent.epoch != identity.epoch
        || !agent.extra.get("autoWake").is_some_and(truthy)
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
    agent.extra.insert("inFlight".into(), json!(false));
    agent.extra.remove("startAttempt");
    agent.extra.remove("startOutcomeHold");
    publish_agent(tx, &agent)?;
    ExecutionHistoryRepository::save_receipt(
        tx,
        &identity.attempt_id,
        "startResult",
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
    preparation: bool,
    message: &str,
    input: Value,
) -> StoreResult<bool> {
    let kind = if unknown { "unknown" } else { "startResult" };
    let receipt_input = json!({"agent":identity.agent_id,"epoch":identity.epoch,"input":input});
    if let Some(result) =
        ExecutionHistoryRepository::replay_settled(tx, &identity.attempt_id, kind, &receipt_input)?
    {
        return result
            .as_bool()
            .ok_or_else(|| invalid_agent(&identity.agent_id));
    }
    let Some(attempt) = agent.extra.get("startAttempt") else {
        return Ok(false);
    };
    if attempt["id"] != identity.attempt_id
        || attempt["epoch"] != identity.epoch
        || attempt["turnId"].is_string()
        || attempt["observedTurnId"].is_string()
        || attempt.get("accountKey").is_some_and(|v| {
            *v != agent
                .extra
                .get("accountKey")
                .cloned()
                .unwrap_or(json!("default"))
        })
    {
        return Ok(false);
    }
    let events = validated_events(tx, &agent, attempt)?;
    let count = attempt["events"].as_array().map_or(0, Vec::len);
    let decision = start_error_decision(&agent, identity.epoch, unknown, preparation);
    if decision.return_busy_input {
        ExecutionHistoryRepository::reject_attempt(tx, &identity.attempt_id, message)?;
        tx.execute_cached("UPDATE runtime_events SET status='pending',turn_id=NULL,error=CASE WHEN EXISTS(SELECT 1 FROM runtime_event_meta m WHERE m.id=runtime_events.id AND json_extract(m.record,'$.sendNow')=1) THEN ?1 ELSE NULL END WHERE agent=?2 AND epoch=?3 AND id IN(SELECT value FROM json_each(?4)) AND status IN ('reserved','dispatching','uncertain')",
            params![message, identity.agent_id, identity.epoch, events])?;
        agent.extra.insert(
            "steerRejectedTurnId".into(),
            agent
                .extra
                .get("turnId")
                .cloned()
                .filter(|v| v.is_string())
                .unwrap_or(json!("")),
        );
        agent.status = if agent.extra.get("inFlight").is_some_and(truthy) {
            AgentStatus::Running
        } else {
            AgentStatus::Queued
        };
        agent.extra.insert("error".into(), Value::Null);
        agent.extra.remove("startAttempt");
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
                .extra
                .get_mut("startAttempt")
                .ok_or_else(|| invalid_agent(&identity.agent_id))?["responseError"] =
                json!(WAITING_START);
        }
    }
    agent
        .extra
        .insert("inFlight".into(), json!(decision.in_flight));
    if !unknown {
        agent.extra.remove("startOutcomeHold");
    }
    agent
        .extra
        .get_mut("startAttempt")
        .ok_or_else(|| invalid_agent(&identity.agent_id))?["executionOutcome"] =
        json!(decision.evidence);
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
const COMPLETED_START_SQL: &str = "COALESCE(json_extract(record,'$.status'),'') IN ('completed','waiting','paused','parked','queued') AND COALESCE(json_extract(record,'$.inFlight'),0)=0 AND COALESCE(json_extract(record,'$.startAttempt.submitted'),0)=1 AND json_extract(record,'$.startAttempt.action') IS NULL AND COALESCE(json_extract(record,'$.startAttempt.activeAtReservation'),0)=0 AND json_extract(record,'$.startOutcomeHold') IS NULL AND COALESCE(json_extract(record,'$.startAttempt.responseError'),'')='' AND COALESCE(json_extract(record,'$.startAttempt.executionOutcome'),'')!='unknown' AND COALESCE(json_type(record,'$.startAttempt.turnId'),'')='text' AND COALESCE(json_extract(record,'$.startAttempt.turnId'),'')!='' AND json_extract(record,'$.startAttempt.turnId')=COALESCE(json_extract(record,'$.lastCompletedTurn'),'') AND COALESCE(json_extract(record,'$.lastCompletedTurnStatus'),'')='completed'";
// An idle agent whose attempt turn is its last finished turn has nothing to
// recover, also when the attempt was reserved during an active turn
// (`activeAtReservation`). Python restart recovery never touches an idle agent.
// A turn that ended `failed` or `interrupted` is settled too: a later backend
// stop or provider generation must not interrupt it again or replace its error
// (rehearsal 11, agent 841ebc0e). The startup scan and every disconnect path
// select through this rule.
const SETTLED_IDLE_TURN_SQL: &str = "COALESCE(json_extract(record,'$.status'),'') IN ('completed','waiting','paused','parked','queued','failed','interrupted') AND COALESCE(json_extract(record,'$.inFlight'),0)=0 AND COALESCE(json_extract(record,'$.startAttempt.turnId'),json_extract(record,'$.startAttempt.observedTurnId'),'')!='' AND COALESCE(json_extract(record,'$.startAttempt.turnId'),json_extract(record,'$.startAttempt.observedTurnId'))=json_extract(record,'$.lastCompletedTurn') AND COALESCE(json_extract(record,'$.lastCompletedTurnStatus'),'') IN ('completed','failed','interrupted')";
pub(super) fn unresolved_sql() -> String {
    format!(
        "{} AND NOT ({COMPLETED_START_SQL}) AND NOT ({SETTLED_IDLE_TURN_SQL})",
        unresolved_base_sql()
    )
}
/// An agent's account, written exactly as the `runtime_agent_account_scope`
/// index expression so SQLite searches that index instead of scanning agents.
const ACCOUNT_SCOPE_SQL: &str = "CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' ELSE json_extract(record,'$.accountKey') END";
fn account_agents_sql(predicate: &str) -> String {
    format!(
        "SELECT id,record FROM runtime_agents WHERE {ACCOUNT_SCOPE_SQL}=?1 AND json_extract(record,'$.deletedAt') IS NULL AND ({predicate})"
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
        match decode_agent(&id, &raw) {
            Ok(agent) => agents.push(agent),
            Err(error) => {
                let receipt_id = format!("{}{id}", crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX);
                let held = EventWriteRepository::receipt(connection, &receipt_id)?.is_some_and(
                    |receipt| receipt.signature == crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                );
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
        decode_agent(&id, &raw)
    })
    .collect()
}

pub(super) fn record_notification(
    transaction: &Transaction<'_>,
    agent: &mut AgentRecord,
) -> StoreResult<()> {
    agent.extra.insert(
        "events".into(),
        json!(
            agent
                .extra
                .get("events")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1
        ),
    );
    let at: String =
        transaction.query_row_cached("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| {
            r.get(0)
        })?;
    agent.extra.insert("lastEvent".into(), json!(at));
    Ok(())
}

pub(super) fn timestamp() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
pub(super) fn clear_hold(agent: &mut AgentRecord) {
    if agent
        .extra
        .get("startOutcomeHold")
        .is_none_or(|h| h["stage"] != "account_settlement")
    {
        return;
    }
    let hold = agent
        .extra
        .remove("startOutcomeHold")
        .unwrap_or(Value::Null);
    if hold["previousNativeFailureHold"]["present"] == true {
        agent.extra.insert(
            "nativeFailureHold".into(),
            hold["previousNativeFailureHold"]["value"].clone(),
        );
    } else {
        agent.extra.remove("nativeFailureHold");
    }
    if agent.extra.get("error") == hold.get("reason") {
        agent
            .extra
            .insert("error".into(), hold["previousError"].clone());
    }
}
/// Requeue an exact unsubmitted current attempt, or cancel a stale epoch's
/// reservation. Returns the requeued event count when the agent was requeued.
fn requeue_unsubmitted(
    transaction: &Transaction<'_>,
    mut agent: AgentRecord,
) -> StoreResult<Option<usize>> {
    let attempt = agent
        .extra
        .get("startAttempt")
        .ok_or_else(|| invalid_agent(&agent.id.0))?;
    if attempt["submitted"] == true {
        return Ok(None);
    }
    if attempt["submitted"] != false {
        return Err(invalid_agent(&agent.id.0));
    }
    let events = validated_events(transaction, &agent, attempt)?;
    let epoch = attempt["epoch"]
        .as_i64()
        .ok_or_else(|| invalid_agent(&agent.id.0))?;
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
            || agent.extra.get("autoWake") != Some(&json!(true))
            || agent.extra.get("inFlight") != Some(&json!(true)))
    {
        return Ok(None);
    }
    let count = transaction.execute_cached(
        "UPDATE runtime_events SET status=?1,turn_id=NULL,error=NULL WHERE agent=?2 AND epoch=?3 AND status='reserved' AND id IN(SELECT value FROM json_each(?4))",
        params![if current {"pending"} else {"cancelled"},agent.id.0,epoch,events])?;
    if !current {
        return Ok(None);
    }
    agent.extra.remove("startAttempt");
    agent.extra.insert("inFlight".into(), json!(false));
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
    let attempt = stored_attempt(agent).cloned().unwrap_or(Value::Null);
    if attempt_already_decided(agent, &attempt) {
        return Ok(false);
    }
    let epoch = if attempt.is_null() {
        agent.epoch
    } else {
        attempt["epoch"]
            .as_i64()
            .ok_or_else(|| invalid_agent(&agent.id.0))?
    };
    let paused = agent.status == AgentStatus::Paused;
    // A turn that accepted this attempt makes its outcome provable from native
    // history, whichever runtime submitted the input. Without an attempt (a
    // turn that Python's supervisor restored) the agent's turn is that identity.
    // An observed turn is activity, not a receipt for the input batch, so an
    // attempt without an accepted turn stays unknown.
    let turn = if attempt.is_null() {
        agent.extra.get("turnId").and_then(Value::as_str)
    } else {
        attempt["turnId"].as_str()
    }
    .filter(|turn| !turn.is_empty())
    .map(str::to_owned);
    let unknown = turn.is_none();
    // Without a turn, only a Rust receipt can still identify the request.
    let legacy = unknown
        && (attempt["connectionGeneration"].as_u64().is_none()
            || attempt["requestTicket"].as_str().is_none());
    let restart_input_wait = restart
        && auto_reconcile_unknown
        && !paused
        && agent.extra.get("autoWake").is_some_and(truthy)
        && attempt["submitted"] == true
        && unknown
        && agent
            .extra
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        && restart_input_events_valid(transaction, &agent.id.0, epoch, &attempt["events"])?;
    let receipt = json!({"epoch":epoch,"accountKey":agent.extra.get("accountKey").unwrap_or(&json!("default")),
        "threadId":agent.extra.get("threadId"),"turnId":turn,"startAttempt":attempt,
        "connectionId":agent.extra.get("connectionId"),"autoWake":agent.extra.get("autoWake"),
        "supervisor":attempt.get("supervisorIdentity").cloned().unwrap_or_else(||json!({"generation":attempt["connectionGeneration"]})),"at":at});
    if !agent
        .extra
        .get("disconnectRecovery")
        .is_some_and(|old| same_recovery_scope(old, &receipt))
    {
        agent
            .extra
            .insert("disconnectRecovery".into(), receipt.clone());
    } else if let Some(previous) = agent.extra.get_mut("disconnectRecovery") {
        // Binding a formerly unknown turn enriches the same receipt without revoking its saved authority.
        previous["turnId"] = receipt["turnId"].clone();
        previous["startAttempt"] = receipt["startAttempt"].clone();
    }
    if restart {
        if restart_input_wait {
            let mut marker = receipt.clone();
            if let Some(object) = marker.as_object_mut() {
                object.remove("connectionId");
                object.remove("supervisor");
            }
            marker["stage"] = json!("held");
            marker["reason"] = json!("Native input submission has no confirmed turn identity.");
            agent.extra.insert("restartRecovery".into(), marker);
            agent.extra.remove("disconnectRecovery");
            let event_ids = attempt["events"].clone();
            let first_event = event_ids
                .as_array()
                .and_then(|events| events.first())
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            let error =
                format!("Context repair waits for a confirmed input receipt: {first_event}");
            agent.extra.insert("contextRepairWait".into(), json!({
                "error":error,
                "scope":"native",
                "source":{"id":agent.id.0,"accountKey":agent.extra.get("accountKey").unwrap_or(&json!("default")),"epoch":agent.epoch,"threadId":agent.extra.get("threadId"),"attemptId":null},
                "events":event_ids,
                "nextCheckAt":0
            }));
        } else {
            let mut marker = agent
                .extra
                .get("disconnectRecovery")
                .unwrap_or(&Value::Null)
                .clone();
            marker["stage"] = json!(if unknown || legacy { "held" } else { "pending" });
            // The same recovery keeps the record of its one automatic history check.
            if let Some(check) = agent
                .extra
                .get("restartRecovery")
                .and_then(|previous| previous.get("automaticCheck"))
                .filter(|_| {
                    agent
                        .extra
                        .get("restartRecovery")
                        .is_some_and(|previous| same_recovery_scope(previous, &marker))
                })
                .cloned()
            {
                marker["automaticCheck"] = check;
            }
            if unknown || legacy {
                marker["reason"] = json!(if legacy {
                    LEGACY_RECOVERY
                } else {
                    "Native input submission has no confirmed turn identity."
                });
            }
            agent.extra.insert("restartRecovery".into(), marker);
        }
        if let Some(receipt) = agent.extra.get_mut("disconnectRecovery") {
            receipt["source"] = json!("restart");
        }
    }
    if restart_input_wait {
        agent.status = AgentStatus::Queued;
    } else if !paused {
        agent.status = AgentStatus::Interrupted;
    }
    agent
        .extra
        .insert("autoWake".into(), json!(restart_input_wait));
    if restart_input_wait {
        agent.extra.remove("nativeFailureHold");
        agent.extra.remove("startOutcomeHold");
        let wait_error = agent.extra["contextRepairWait"]["error"].clone();
        agent.extra.insert("error".into(), wait_error);
        agent.extra.remove("startAttempt");
    } else if legacy {
        agent.extra.insert("nativeFailureHold".into(), json!(true));
        agent.extra.insert(
            "startOutcomeHold".into(),
            json!({"stage":"recovery","epoch":epoch,"reason":LEGACY_RECOVERY}),
        );
        agent.extra.insert("error".into(), json!(LEGACY_RECOVERY));
    } else if !paused
        && agent
            .extra
            .get("startOutcomeHold")
            .is_none_or(|h| h["stage"] != "account_settlement")
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
    agent.extra.insert("inFlight".into(), json!(false));
    transaction.execute_cached("UPDATE runtime_events SET status='uncertain',error=?3 WHERE agent=?1 AND epoch=?2 AND status='dispatching'", params![agent.id.0,epoch,if restart {"Server restarted before delivery acknowledgement"} else {"Codex disconnected"}])?;
    if restart_input_wait {
        transaction.execute_cached("UPDATE runtime_events SET status='uncertain' WHERE agent=?1 AND epoch=?2 AND id IN (SELECT value FROM json_each(?3)) AND status IN ('reserved','dispatching','uncertain')", params![agent.id.0,epoch,attempt["events"].to_string()])?;
    }
    if !restart && !legacy {
        agent.extra.remove("startAttempt");
    }
    publish_agent(transaction, agent)?;
    Ok(true)
}

fn restart_input_events_valid(
    transaction: &Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    events: &Value,
) -> StoreResult<bool> {
    let Some(events) = events.as_array().filter(|events| !events.is_empty()) else {
        return Ok(false);
    };
    let mut seen = std::collections::BTreeSet::new();
    for event in events {
        let Some(id) = event.as_str().filter(|id| !id.is_empty()) else {
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

const LEGACY_RECOVERY: &str = "Python submission identity requires native history reconciliation; input will not be replayed.";
pub(super) fn interrupt_uncertain(attempt: &Value, thread: &str, turn: &str) -> bool {
    let receipt = &attempt["interrupt"];
    receipt["threadId"] == thread
        && receipt["turnId"] == turn
        && matches!(receipt["outcome"].as_str(), Some("unknown" | "accepted"))
}
pub(super) fn same_native_scope(a: &AgentRecord, b: &AgentRecord) -> bool {
    a.extra.get("accountKey").unwrap_or(&json!("default"))
        == b.extra.get("accountKey").unwrap_or(&json!("default"))
        && a.extra.get("threadId") == b.extra.get("threadId")
}
pub(super) fn same_recovery_scope(a: &Value, b: &Value) -> bool {
    ["epoch", "accountKey", "threadId"]
        .iter()
        .all(|k| a[*k] == b[*k])
        && (a["turnId"].is_null() || b["turnId"].is_null() || a["turnId"] == b["turnId"])
        && a["startAttempt"]["id"] == b["startAttempt"]["id"]
        && a["startAttempt"]["connectionGeneration"] == b["startAttempt"]["connectionGeneration"]
}

/// The attempt that a disconnect or restart may still have to resolve. Recovery
/// receipts count only while open; see `OPEN_RESTART_MARKER_SQL`.
pub(super) fn stored_attempt(agent: &AgentRecord) -> Option<&Value> {
    agent
        .extra
        .get("startAttempt")
        .or_else(|| {
            (agent.status == AgentStatus::Interrupted)
                .then(|| agent.extra.get("disconnectRecovery"))
                .flatten()
                .and_then(|r| r.get("startAttempt"))
        })
        .or_else(|| {
            agent
                .extra
                .get("restartRecovery")
                .filter(|marker| restart_marker_open(marker))
                .and_then(|r| r.get("startAttempt"))
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
    attempt: &Value,
) -> StoreResult<bool> {
    if let Some(turn) = bound_turn(agent, attempt).filter(|turn| !turn.is_empty())
        && (agent.extra.get("lastCompletedTurn").and_then(Value::as_str) == Some(turn)
            || completed_turn(connection, &agent.id.0, turn)?)
    {
        return Ok(true);
    }
    let Some(id) = attempt["id"].as_str() else {
        return Ok(false);
    };
    Ok(connection
        .query_row_cached(
            "SELECT json_type(record,'$.receipts.terminal') IS 'object' \
             FROM runtime_execution_attempts WHERE id=?1",
            [id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}
/// The id of the attempt that a recovery receipt of `agent` names.
fn recovery_attempt_ids(agent: &AgentRecord) -> impl Iterator<Item = &str> {
    ["disconnectRecovery", "restartRecovery"]
        .into_iter()
        .filter_map(|key| agent.extra.get(key)?["startAttempt"]["id"].as_str())
}
fn restart_marker_open(marker: &Value) -> bool {
    marker["stage"]
        .as_str()
        .is_some_and(|stage| OPEN_RESTART_MARKER_STAGES.contains(&stage))
}
/// A recovery decision already recorded for this exact attempt is final. A later
/// provider generation adds no evidence about it, and applying it again would
/// undo the user's next message. Python `restore` never revisits such a marker.
fn attempt_already_decided(agent: &AgentRecord, attempt: &Value) -> bool {
    agent.extra.get("restartRecovery").is_some_and(|marker| {
        marker.is_object()
            && !restart_marker_open(marker)
            && marker["startAttempt"]["id"].is_string()
            && marker["startAttempt"]["id"] == attempt["id"]
            && marker["epoch"] == attempt["epoch"]
    })
}
pub(super) fn restart_continuation_allowed(agent: &AgentRecord) -> bool {
    let Some(marker) = agent.extra.get("restartRecovery") else {
        return false;
    };
    continuation_allowed(agent)
        && recovering(agent, agent.epoch)
        && matches!(marker["stage"].as_str(), Some("pending" | "superseded"))
        && agent
            .extra
            .get("disconnectRecovery")
            .unwrap_or(&Value::Null)["autoWake"]
            == true
        && marker["turnId"].is_string()
        && marker.get("turnId") == agent.extra.get("turnId")
        && marker["epoch"] == agent.epoch
        && ["accountKey", "threadId"]
            .iter()
            .all(|k| marker.get(*k) == agent.extra.get(*k))
}
pub(super) fn transport_continuation_allowed(agent: &AgentRecord) -> bool {
    let Some(previous) = agent.extra.get("disconnectRecovery") else {
        return false;
    };
    let attempt = agent.extra.get("startAttempt");
    continuation_allowed(agent)
        && agent.status == AgentStatus::Interrupted
        && agent.extra.get("inFlight").is_none_or(|v| !truthy(v))
        && agent.extra.get("autoWake").is_none_or(|v| !truthy(v))
        && agent.extra.get("turnId").is_some_and(Value::is_string)
        && matches!(
            agent.extra.get("error").and_then(Value::as_str),
            Some(PROVIDER_DISCONNECTED_TURN_ERROR | VM_PROVIDER_DISCONNECTED_TURN_ERROR)
        )
        && !matches!(
            agent
                .extra
                .get("restartRecovery")
                .and_then(|v| v["stage"].as_str()),
            Some("pending" | "superseded")
        )
        && previous["source"] != "restart"
        && previous["autoWake"] == true
        && ["accountKey", "threadId", "turnId"]
            .iter()
            .all(|k| previous.get(*k) == agent.extra.get(*k))
        && previous["epoch"] == agent.epoch
        && agent
            .extra
            .get("turnEpoch")
            .is_none_or(|v| *v == agent.epoch)
        && attempt.is_none_or(|a| {
            a == &previous["startAttempt"]
                && a["submitted"] == true
                && a["epoch"] == agent.epoch
                && a.get("action").is_none_or(|v| !truthy(v))
        })
}

pub(super) fn continuation_allowed(agent: &AgentRecord) -> bool {
    ![
        "deletedAt",
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
            block["threadId"].is_string() && block.get("threadId") == agent.extra.get("threadId")
        })
        && !agent
            .extra
            .get("error")
            .is_some_and(|error| error["codexErrorInfo"] == "misalignmentPolicyViolation")
}
pub(super) fn load_agent(db: &Connection, id: &str) -> StoreResult<Option<AgentRecord>> {
    db.query_row_cached("SELECT record FROM runtime_agents WHERE id=?1", [id], |r| {
        r.get::<_, String>(0)
    })
    .optional()?
    .map(|raw| decode_agent(id, &raw))
    .transpose()
}
pub(super) fn decode_agent(id: &str, raw: &str) -> StoreResult<AgentRecord> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
        crate::records::invalid_agent_field(
            id,
            "",
            &format!("invalid_json; record length {} bytes", raw.len()),
            None,
        )
    })?;
    if !studio_runtime_schema::validate("PersistedAgentRecord", &value)
        .map_err(StoreError::SchemaValidation)?
    {
        let (pointer, reason) = studio_runtime_schema::first_error("PersistedAgentRecord", &value)
            .map_err(StoreError::SchemaValidation)?
            .unwrap_or_else(|| ("/".to_owned(), "schema_invalid".to_owned()));
        return Err(crate::records::invalid_agent_field(
            id,
            &pointer,
            &reason,
            Some(&value),
        ));
    }
    let record: AgentRecord = serde_json::from_value(value.clone())
        .map_err(|_| crate::records::invalid_agent_field(id, "", "record_decode", Some(&value)))?;
    if record.id.0 != id {
        return Err(crate::records::invalid_agent_field(
            id,
            "/id",
            "row_id_mismatch",
            Some(&value),
        ));
    }
    Ok(record)
}
pub(super) fn current_attempt(agent: &AgentRecord, epoch: i64) -> bool {
    agent.epoch == epoch
        && agent.extra.get("turnEpoch") == Some(&json!(epoch))
        && agent.extra.get("autoWake") == Some(&json!(true))
}
pub(super) fn stopped_attempt(agent: &AgentRecord, epoch: i64) -> bool {
    agent.status == AgentStatus::Paused
        && agent.extra.get("autoWake") == Some(&json!(false))
        && agent.extra.get("turnEpoch") == Some(&json!(epoch))
}

pub(super) fn recovering(agent: &AgentRecord, epoch: i64) -> bool {
    agent.extra.get("disconnectRecovery").is_some_and(|r| {
        r["epoch"].as_i64() == Some(epoch)
            && r["accountKey"]
                == agent
                    .extra
                    .get("accountKey")
                    .cloned()
                    .unwrap_or(json!("default"))
            && r["threadId"] == agent.extra.get("threadId").cloned().unwrap_or(Value::Null)
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
    crate::ExecutionHistoryRepository::record(transaction, agent)?;
    AgentRepository::put(transaction, agent)
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
    if agent.extra.get("deletedAt").is_some_and(truthy) {
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
pub(super) struct ValidatedAttempt {
    agent: AgentRecord,
    event_ids: (String, usize),
    authority: AttemptAuthority,
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
            .extra
            .get("startAttempt")
            .unwrap_or(&Value::Null)
            .get("turnId")
            .and_then(Value::as_str)
            .is_some_and(|old| old != turn)
    {
        return AcceptanceDecision::Ignore;
    }
    AcceptanceDecision::Bind {
        visible: authority == AttemptAuthority::Current,
        interrupt: agent.status == AgentStatus::Paused
            || agent.epoch != epoch
            || agent.extra.get("autoWake") != Some(&json!(true)),
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
    if !agent.extra.contains_key("startAttempt")
        && scope.is_some()
        && recovering(&agent, epoch)
        && let Some(attempt) = agent
            .extra
            .get("disconnectRecovery")
            .and_then(|r| r.get("startAttempt"))
            .cloned()
    {
        agent.extra.insert("startAttempt".into(), attempt);
    }
    let Some(attempt) = agent.extra.get("startAttempt") else {
        return Ok(None);
    };
    if attempt["id"] != attempt_id
        || attempt["epoch"] != epoch
        || attempt["submitted"] != true
        || attempt["accountKey"]
            != agent
                .extra
                .get("accountKey")
                .cloned()
                .unwrap_or(json!("default"))
        || attempt["threadId"] != agent.extra.get("threadId").cloned().unwrap_or(Value::Null)
    {
        return Ok(None);
    }
    if let Some((ticket, account, generation, thread)) = scope
        && (attempt["requestTicket"] != ticket
            || attempt["accountKey"] != account
            || attempt["connectionGeneration"].as_u64() != Some(generation)
            || attempt["threadId"] != thread)
    {
        return Ok(None);
    }
    let event_ids = (
        validated_events(tx, &agent, attempt)?,
        attempt["events"].as_array().map_or(0, Vec::len),
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
    let changed = transaction.execute_cached(
        "UPDATE runtime_events SET status='delivered',turn_id=?1,error=NULL WHERE agent=?2 AND epoch=?3 AND id IN (SELECT value FROM json_each(?4)) AND (status IN ('reserved','dispatching','uncertain') OR (status='delivered' AND turn_id=?1))",
        params![turn_id, agent_id, epoch, event_ids.0])?;
    require_membership(changed, event_ids.1, agent_id)?;
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
                || event.status != "delivered"
                || event.turn_id.as_deref() != Some(turn_id)
                || event.kind != "agent_message"
            {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<Value>(&event.text) else {
                continue;
            };
            let (Some(message_id), Some(room_id), Some(sender), Some(text)) = (
                payload.get("message_id").and_then(Value::as_str),
                payload.get("room").and_then(Value::as_str),
                payload.get("sender").and_then(Value::as_str),
                payload.get("text").and_then(Value::as_str),
            ) else {
                continue;
            };
            if message_id.is_empty()
                || room_id.is_empty()
                || sender.is_empty()
                || event.id.0 != format!("chat:{message_id}:{agent_id}")
            {
                continue;
            }
            let Some(mut message) = crate::MessageRepository::get(
                transaction,
                &crate::ChatMessageId::from(message_id),
            )?
            else {
                continue;
            };
            if message.room != room_id || message.sender != sender || message.text != text {
                continue;
            }
            if let Some(delivery) = message
                .deliveries
                .as_object_mut()
                .and_then(|deliveries| deliveries.get_mut(agent_id))
                && matches!(delivery.as_str(), Some("queued" | "pending"))
            {
                *delivery = json!("delivered");
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
    attempt: &Value,
) -> StoreResult<String> {
    let events = attempt
        .get("events")
        .filter(|v| v.is_array())
        .ok_or_else(|| invalid_agent(&agent.id.0))?
        .to_string();
    // A native action submits no queued input; every other attempt submits at
    // least one event.
    let empty_allowed = native_action_attempt(attempt);
    let valid: bool = transaction.query_row_cached(
        "SELECT (COUNT(*) > 0 OR ?2) AND COUNT(*)=COUNT(DISTINCT value) AND COUNT(*)=COALESCE(SUM(type='text'),0) FROM json_each(?1)",
        params![events, empty_allowed], |r|r.get(0))?;
    if !valid {
        return Err(invalid_agent(&agent.id.0));
    }
    Ok(events)
}

/// True for a `compact` or `review` attempt ([`NATIVE_ACTION_ATTEMPT_KINDS`]).
pub(super) fn native_action_attempt(attempt: &Value) -> bool {
    attempt
        .get("action")
        .and_then(Value::as_str)
        .is_some_and(|action| NATIVE_ACTION_ATTEMPT_KINDS.contains(&action))
}

/// The provider turn an attempt is bound to. An ordinary attempt is bound by
/// its own `turnId`. A native action attempt saved before its acceptance bound
/// `startAttempt.turnId` (live reviewers 8388eb70, 6f209a16 and d0e0015f,
/// 20 October 2026) is bound by the agent turn on the same thread, which its
/// review/start reply or turn/started notice set. Without this, the terminal
/// notice of that turn stays deferred and holds the account journal.
pub(super) fn bound_turn<'a>(agent: &'a AgentRecord, attempt: &'a Value) -> Option<&'a str> {
    attempt.get("turnId").and_then(Value::as_str).or_else(|| {
        (native_action_attempt(attempt)
            && attempt.get("threadId").is_some()
            && attempt.get("threadId") == agent.extra.get("threadId"))
        .then(|| agent.extra.get("turnId").and_then(Value::as_str))
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
    Ok(connection.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM runtime_completed_turns WHERE id=?1)",
        [format!("{agent}:{turn}")],
        |row| row.get(0),
    )?)
}

/// Python `codex_agent_management.parked_after_turn`: a park requested during
/// a turn takes effect when that turn ends; a stopped event waiter is parked.
fn parked_after_turn(agent: &mut AgentRecord) {
    if !agent.extra.get("parkedEvent").is_some_and(truthy) {
        return;
    }
    if agent.extra.get("parkAfterTurn").is_some_and(truthy) {
        agent.extra.remove("parkAfterTurn");
        agent.extra.insert("autoWake".into(), json!(false));
    }
    if agent.extra.get("autoWake") != Some(&Value::Bool(true)) {
        agent.status = AgentStatus::Parked;
    }
}

pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod account_scope_tests {
    /// Account-scoped agent reads search the account index instead of scanning agents.
    #[tokio::test]
    async fn account_agents_search_the_account_scope_index() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = crate::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        let reader = store.read().expect("reader");
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
                .any(|step| step.contains("runtime_agent_account_scope")),
            "{plan:?}"
        );
    }
}
