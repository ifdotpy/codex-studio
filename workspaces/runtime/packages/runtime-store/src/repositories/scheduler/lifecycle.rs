use super::*;

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
    pub fn disconnect_account(
        transaction: &Transaction<'_>,
        account: &str,
        generation: u64,
        at: f64,
        restart: bool,
    ) -> StoreResult<()> {
        for mut agent in account_agents(transaction, account, UNRESOLVED_SQL)? {
            if stored_attempt(&agent).unwrap_or(&Value::Null)["connectionGeneration"]
                .as_u64()
                .is_none_or(|g| g == generation)
            {
                disconnect_agent(transaction, &mut agent, at, restart)?;
            }
        }
        Ok(())
    }

    /// A new supervisor generation cannot prove an old turn's outcome. History reconciliation
    /// is a named dependency; preserve the Python recovery receipt and hold the old input.
    pub fn reconcile_generation(
        transaction: &Transaction<'_>,
        account: &str,
        generation: u64,
        at: f64,
    ) -> StoreResult<()> {
        for mut agent in account_agents(transaction, account, UNRESOLVED_SQL)? {
            let known = stored_attempt(&agent).and_then(|a| a["connectionGeneration"].as_u64());
            if known != Some(generation) {
                disconnect_agent(transaction, &mut agent, at, true)?;
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
        Ok(())
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
        if agent.extra.get("threadId").and_then(Value::as_str) != Some(thread)
            || !["turnId", "observedTurnId"]
                .iter()
                .any(|k| attempt[*k] == turn)
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
        tx.execute(
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
        let mut statement = connection.prepare(
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
        let agents = account_agents(connection, account_key, UNRESOLVED_SQL)?;
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
        let mut statement = connection.prepare(
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
                attempt
                    .get("turnId")
                    .and_then(Value::as_str)
                    .is_some_and(|known| known != turn_id)
            })
        {
            return Ok((true, None));
        }
        Ok((
            true,
            Some((
                TurnAttemptIdentity {
                    agent_id: agent.id.0.clone(),
                    epoch,
                    attempt_id: attempt_id.to_owned(),
                },
                attempt.get("turnId").and_then(Value::as_str).is_some(),
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
            count += transaction.execute(
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
        thread_id: &str,
    ) -> StoreResult<bool> {
        let receipt_input = json!({"agent":agent_id,"epoch":epoch,"ticket":request_id,"account":account_key,"generation":generation,"thread":thread_id});
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
        let changed = transaction.execute(
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
            json!({"submitted":true,"requestTicket":request_id,"nativeOperationId":format!("turn:{agent_id}:{}:attempt:{attempt_id}",attempt["events"][0].as_str().unwrap_or(attempt_id)),"connectionId":null,"supervisorIdentity":null,"settingsFixed":true,
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
        let mut statement = transaction.prepare("SELECT DISTINCT COALESCE(json_extract(record,'$.accountKey'),'default') FROM runtime_agents")?;
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
        for mut agent in records {
            let attempt = agent
                .extra
                .get("startAttempt")
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            if attempt["submitted"] == true {
                continue;
            }
            if attempt["submitted"] != false {
                return Err(invalid_agent(&agent.id.0));
            }
            let events = validated_events(transaction, &agent, attempt)?;
            let epoch = attempt["epoch"]
                .as_i64()
                .ok_or_else(|| invalid_agent(&agent.id.0))?;
            let exact: bool = transaction.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='reserved' AND id NOT IN(SELECT value FROM json_each(?3))) AND (SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='reserved' AND id IN(SELECT value FROM json_each(?3)))=json_array_length(?3)",
                params![agent.id.0,epoch,events], |r|r.get(0))?;
            if !exact {
                if agent.epoch == epoch {
                    return Err(invalid_agent(&agent.id.0));
                }
                continue;
            }
            let current = agent.epoch == epoch;
            if current
                && (agent.status != AgentStatus::Starting
                    || agent.extra.get("autoWake") != Some(&json!(true))
                    || agent.extra.get("inFlight") != Some(&json!(true)))
            {
                continue;
            }
            let count = transaction.execute(
                "UPDATE runtime_events SET status=?1,turn_id=NULL,error=NULL WHERE agent=?2 AND epoch=?3 AND status='reserved' AND id IN(SELECT value FROM json_each(?4))",
                params![if current {"pending"} else {"cancelled"},agent.id.0,epoch,events])?;
            if current {
                agent.extra.remove("startAttempt");
                agent.extra.insert("inFlight".into(), json!(false));
                agent.status = AgentStatus::Queued;
                publish_agent(transaction, &agent)?;
                recovered += count;
            }
        }
        Ok(recovered)
    }

    /// Restore Python-shaped scoped receipts; the account worker will reattach without replay.
    pub fn recover_submitted(transaction: &Transaction<'_>) -> StoreResult<usize> {
        let records = select_agents(
            transaction,
            "SELECT id,record FROM runtime_agents WHERE COALESCE(json_extract(record,'$.remoteWorker'),0)=0 AND COALESCE(json_extract(record,'$.remoteAnchor'),0)=0 AND ((json_extract(record,'$.startAttempt.submitted')=1 AND COALESCE(json_extract(record,'$.startAttempt.executionOutcome'),'unknown')='unknown') OR (json_extract(record,'$.startAttempt') IS NULL AND (json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval'))))",
            [],
        )?;
        for mut agent in records.iter().cloned() {
            clear_hold(&mut agent);
            disconnect_agent(transaction, &mut agent, timestamp(), true)?;
        }
        Ok(records.len())
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
    ) -> StoreResult<bool> {
        let receipt_input = json!({"agent":agent_id,"epoch":epoch,"turn":turn_id,"status":status,"error":error,"nativeStatus":native_status,"nativeError":native_error});
        if let Some(result) =
            ExecutionHistoryRepository::replay(transaction, attempt_id, "terminal", &receipt_input)?
        {
            return result.as_bool().ok_or_else(|| invalid_agent(agent_id));
        }
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
            .and_then(|a| a["turnId"].as_str())
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
        let clears_unknown = agent
            .extra
            .get("startOutcomeHold")
            .and_then(|h| h["attemptId"].as_str())
            == Some(attempt_id);
        if clears_unknown {
            agent.extra.remove("startOutcomeHold");
        }
        transaction.execute(
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
        if current_attempt && continuation_allowed(&agent)
            && !agent.extra.get("nativeFailureHold").is_some_and(truthy)
            && transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending')",
                params![agent_id, epoch],
                |row| row.get::<_, bool>(0),
            )?
        {
            agent.status = AgentStatus::Queued;
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
        ExecutionHistoryRepository::replay(tx, &identity.attempt_id, kind, &receipt_input)?
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
        tx.execute("UPDATE runtime_events SET status='pending',turn_id=NULL,error=CASE WHEN EXISTS(SELECT 1 FROM runtime_event_meta m WHERE m.id=runtime_events.id AND json_extract(m.record,'$.sendNow')=1) THEN ?1 ELSE NULL END WHERE agent=?2 AND epoch=?3 AND id IN(SELECT value FROM json_each(?4)) AND status IN ('reserved','dispatching','uncertain')",
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
    let changed = tx.execute("UPDATE runtime_events SET status=?1,error=?2 WHERE agent=?3 AND epoch=?4 AND id IN(SELECT value FROM json_each(?5)) AND status IN ('pending','reserved','dispatching','uncertain')",
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

const UNRESOLVED_SQL: &str = "COALESCE(json_extract(record,'$.startAttempt.submitted'),json_extract(record,'$.disconnectRecovery.startAttempt.submitted'),json_extract(record,'$.restartRecovery.startAttempt.submitted'))=1 AND COALESCE(json_extract(record,'$.startAttempt.executionOutcome'),json_extract(record,'$.disconnectRecovery.startAttempt.executionOutcome'),json_extract(record,'$.restartRecovery.startAttempt.executionOutcome'),'unknown')='unknown'";
pub(super) fn account_agents(
    connection: &Connection,
    account: &str,
    predicate: &str,
) -> StoreResult<Vec<AgentRecord>> {
    select_agents(
        connection,
        &format!(
            "SELECT id,record FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.deletedAt') IS NULL AND ({predicate})"
        ),
        [account],
    )
}
pub(super) fn select_agents(
    db: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> StoreResult<Vec<AgentRecord>> {
    let mut statement = db.prepare(sql)?;
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
        transaction.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| {
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
pub(super) fn disconnect_agent(
    transaction: &Transaction<'_>,
    agent: &mut AgentRecord,
    at: f64,
    restart: bool,
) -> StoreResult<()> {
    let attempt = stored_attempt(agent).cloned().unwrap_or(Value::Null);
    let epoch = if attempt.is_null() {
        agent.epoch
    } else {
        attempt["epoch"]
            .as_i64()
            .ok_or_else(|| invalid_agent(&agent.id.0))?
    };
    let paused = agent.status == AgentStatus::Paused;
    let legacy = attempt["connectionGeneration"].as_u64().is_none()
        || (attempt["turnId"].as_str().is_none() && attempt["requestTicket"].as_str().is_none());
    let unknown = attempt["turnId"].as_str().is_none();
    let receipt = json!({"epoch":epoch,"accountKey":agent.extra.get("accountKey").unwrap_or(&json!("default")),
        "threadId":agent.extra.get("threadId"),"turnId":attempt.get("turnId").or_else(||agent.extra.get("turnId")),"startAttempt":attempt,
        "connectionId":agent.extra.get("connectionId"),"autoWake":agent.extra.get("autoWake"),
        "supervisor":attempt.get("supervisorIdentity").cloned().unwrap_or_else(||json!({"generation":attempt["connectionGeneration"]})),"at":at});
    if !agent
        .extra
        .get("disconnectRecovery")
        .is_some_and(|old| same_recovery_scope(old, &receipt))
    {
        agent.extra.insert("disconnectRecovery".into(), receipt);
    } else if let Some(previous) = agent.extra.get_mut("disconnectRecovery") {
        // Binding a formerly unknown turn enriches the same receipt without revoking its saved authority.
        previous["turnId"] = receipt["turnId"].clone();
        previous["startAttempt"] = receipt["startAttempt"].clone();
    }
    if restart {
        let mut marker = agent
            .extra
            .get("disconnectRecovery")
            .unwrap_or(&Value::Null)
            .clone();
        marker["stage"] = json!(if unknown || legacy { "held" } else { "pending" });
        if unknown || legacy {
            marker["reason"] = json!(if legacy {
                LEGACY_RECOVERY
            } else {
                "Native input submission has no confirmed turn identity."
            });
        }
        agent.extra.insert("restartRecovery".into(), marker);
        if let Some(receipt) = agent.extra.get_mut("disconnectRecovery") {
            receipt["source"] = json!("restart");
        }
    }
    if !paused {
        agent.status = AgentStatus::Interrupted;
    }
    agent.extra.insert("autoWake".into(), json!(false));
    if legacy {
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
                "Server restarted during a turn. Review history, then send a new instruction."
            } else {
                "Codex disconnected. Review the transcript before resuming."
            }),
        );
    }
    agent.extra.insert("inFlight".into(), json!(false));
    transaction.execute("UPDATE runtime_events SET status='uncertain',error=?3 WHERE agent=?1 AND epoch=?2 AND status='dispatching'", params![agent.id.0,epoch,if restart {"Server restarted before delivery acknowledgement"} else {"Codex disconnected"}])?;
    if !restart && !legacy {
        agent.extra.remove("startAttempt");
    }
    publish_agent(transaction, agent)
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

pub(super) fn stored_attempt(agent: &AgentRecord) -> Option<&Value> {
    agent
        .extra
        .get("startAttempt")
        .or_else(|| {
            agent
                .extra
                .get("disconnectRecovery")
                .and_then(|r| r.get("startAttempt"))
        })
        .or_else(|| {
            agent
                .extra
                .get("restartRecovery")
                .and_then(|r| r.get("startAttempt"))
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
            Some(
                "Codex disconnected. Review the transcript before resuming."
                    | "Linux VM provider disconnected. The turn outcome is unknown. Review the transcript before resuming."
            )
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
    db.query_row("SELECT record FROM runtime_agents WHERE id=?1", [id], |r| {
        r.get::<_, String>(0)
    })
    .optional()?
    .map(|raw| decode_agent(id, &raw))
    .transpose()
}
pub(super) fn decode_agent(id: &str, raw: &str) -> StoreResult<AgentRecord> {
    let record: AgentRecord = serde_json::from_str(raw).map_err(|_| invalid_agent(id))?;
    if record.id.0 != id {
        return Err(invalid_agent(id));
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
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='runtime_event_meta')",
        [],
        |r| r.get(0),
    )?;
    if exists {
        let path = format!("$.timing.{field}");
        tx.execute("INSERT INTO runtime_event_meta(id,record) SELECT value,json_set('{}',?1,?2) FROM json_each(?3) WHERE 1 ON CONFLICT(id) DO UPDATE SET record=json_set(runtime_event_meta.record,?1,?2)",params![path,timestamp(),ids])?;
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
    let changed = transaction.execute(
        "UPDATE runtime_events SET status='delivered',turn_id=?1,error=NULL WHERE agent=?2 AND epoch=?3 AND id IN (SELECT value FROM json_each(?4)) AND (status IN ('reserved','dispatching','uncertain') OR (status='delivered' AND turn_id=?1))",
        params![turn_id, agent_id, epoch, event_ids.0])?;
    require_membership(changed, event_ids.1, agent_id)
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
    let valid: bool = transaction.query_row(
        "SELECT COUNT(*) > 0 AND COUNT(*)=COUNT(DISTINCT value) AND COUNT(*)=SUM(type='text') FROM json_each(?1)",
        [&events], |r|r.get(0))?;
    if !valid {
        return Err(invalid_agent(&agent.id.0));
    }
    Ok(events)
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
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_completed_turns WHERE id=?1)",
        [format!("{agent}:{turn}")],
        |row| row.get(0),
    )?)
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
