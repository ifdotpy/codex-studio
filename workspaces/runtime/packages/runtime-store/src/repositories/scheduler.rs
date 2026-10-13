//! Durable pending-event selection and claim primitives for the scheduler.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{ExecutionHistoryRepository, QueueEvent, QueueEventId, StoreError, StoreResult};

const MAX_EVENTS_PER_TURN: usize = 32;
const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_completed_turns",
        columns: &["id"],
    },
    TableRequirement {
        name: "runtime_events",
        columns: &[
            "id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error",
        ],
    },
];
const EVENT_META: &[TableRequirement] = &[TableRequirement {
    name: "runtime_event_meta",
    columns: &["id", "record"],
}];

pub type SubmittedStartRequest = (String, i64, String, String, String);

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: TABLES,
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_event_meta",
        tables: EVENT_META,
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedEvents {
    pub agent: AgentRecord,
    pub events: Vec<QueueEvent>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnAttemptIdentity {
    pub agent_id: String,
    pub epoch: i64,
    pub attempt_id: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SchedulerRepository;

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
            if agent.extra.get("error") == agent.extra["startAttempt"].get("responseError")
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
            if agent.extra["startAttempt"]["connectionGeneration"]
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
                agent.extra.insert("connectionRecovery".into(),json!({"source":"native_thread_read","at":at,"turnId":agent.extra["turnId"],"outcome":"active","automatic":true,"previousError":previous_error}));
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
                        "source":"native_thread_read", "at":at, "turnId":agent.extra["turnId"],
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
            let receipt = &agent.extra["startAttempt"]["interrupt"];
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
        let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
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
                    agent_id: agent.id.0,
                    epoch,
                    attempt_id: attempt_id.to_owned(),
                },
                attempt.get("turnId").and_then(Value::as_str).is_some(),
            )),
        ))
    }

    /// Return queued local agents with pending events in the current epoch.
    pub fn pending_agents(connection: &Connection) -> StoreResult<Vec<(String, i64)>> {
        let mut statement = connection.prepare(
            "SELECT a.id,json_extract(a.record,'$.epoch') FROM runtime_agents a \
             WHERE json_extract(a.record,'$.status')='queued' \
             AND json_extract(a.record,'$.autoWake')=1 \
             AND COALESCE(json_extract(a.record,'$.inFlight'),0)=0 \
             AND COALESCE(json_extract(a.record,'$.nativeFailureHold'),0)=0 \
             AND COALESCE(json_extract(a.record,'$.remoteWorker'),0)=0 \
             AND COALESCE(json_extract(a.record,'$.remoteAnchor'),0)=0 \
             AND EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent=a.id \
                        AND e.epoch=json_extract(a.record,'$.epoch') AND e.status='pending') \
             ORDER BY a.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Claim the oldest pending events and persist the matching unsubmitted attempt atomically.
    pub fn claim(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        attempt_created: f64,
        limit: usize,
    ) -> StoreResult<Option<ClaimedEvents>> {
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(None);
        };
        if let Some(saved) = crate::ExecutionHistoryRepository::attempt(transaction, attempt_id)? {
            if saved.get("claimResult").is_none() {
                return Err(crate::ExecutionHistoryRepository::invalid(
                    "runtime_execution_attempts",
                    attempt_id,
                ));
            }
            return replay_claim(transaction, agent, epoch, &saved).map(Some);
        }
        if agent.id.0 != agent_id
            || agent.epoch != epoch
            || agent.status != AgentStatus::Queued
            || !continuation_allowed(&agent)
            || agent.extra.get("autoWake").and_then(Value::as_bool) != Some(true)
            || agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true)
            || agent.extra.get("nativeFailureHold").is_some_and(truthy)
            || agent.extra.get("remoteWorker").is_some_and(truthy)
            || agent.extra.get("remoteAnchor").is_some_and(truthy)
        {
            return Ok(None);
        }
        let unresolved = transaction
            .query_row(
                "SELECT 1 FROM runtime_events WHERE agent=?1 \
                 AND status IN ('reserved','dispatching','uncertain') LIMIT 1",
                [agent_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if unresolved {
            return Ok(None);
        }
        if limit == 0 {
            return Ok(None);
        }

        let mut statement = transaction.prepare(
            "SELECT id, agent, kind, text, status, created, epoch, turn_id, error \
             FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending' \
             ORDER BY created,rowid",
        )?;
        let rows = statement.query_map(params![agent_id, epoch], |row| {
            Ok(QueueEvent {
                id: QueueEventId(row.get(0)?),
                agent: AgentId(row.get(1)?),
                kind: row.get(2)?,
                text: row.get(3)?,
                status: row.get(4)?,
                created: row.get(5)?,
                epoch: row.get(6)?,
                turn_id: row.get(7)?,
                error: row.get(8)?,
            })
        })?;
        let events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if events.is_empty() {
            return Ok(None);
        }

        let mut metadata = BTreeMap::new();
        if transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_event_meta')",
            [],
            |row| row.get::<_, bool>(0),
        )? {
            for event in &events {
                let raw = transaction
                    .query_row(
                        "SELECT record FROM runtime_event_meta WHERE id=?1",
                        [&event.id.0],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if let Some(raw) = raw {
                    let value: Value = serde_json::from_str(&raw).map_err(|_| {
                        StoreError::InvalidSyncObject {
                            scope: "runtime_event_meta".to_owned(),
                            id: event.id.0.clone(),
                        }
                    })?;
                    if !value.is_object() {
                        return Err(StoreError::InvalidSyncObject {
                            scope: "runtime_event_meta".to_owned(),
                            id: event.id.0.clone(),
                        });
                    }
                    if value.get("assets").is_some_and(|assets| !assets.is_array()) {
                        return Err(StoreError::InvalidSyncObject {
                            scope: "runtime_event_meta".to_owned(),
                            id: event.id.0.clone(),
                        });
                    }
                    metadata.insert(event.id.0.clone(), value);
                }
            }
        }

        let unsupported = if agent.extra.get("pendingSettings").is_some_and(truthy) {
            Some("pendingSettings are not supported by the Rust turn driver; apply settings first.")
        } else if agent.extra.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
            && agent
                .extra
                .get("imageWorkspaceReady")
                .and_then(Value::as_bool)
                != Some(true)
        {
            Some("image workspace is not ready; prepare the workspace before submitting input.")
        } else {
            events.first().and_then(|event| {
                if event.kind != "user" {
                    Some("event kind is not supported by the Rust turn driver.")
                } else if metadata
                    .get(&event.id.0)
                    .and_then(|value| value.get("assets"))
                    .and_then(Value::as_array)
                    .is_some_and(|assets| !assets.is_empty())
                {
                    Some("event attachments are not supported by the Rust turn driver.")
                } else if metadata
                    .get(&event.id.0)
                    .is_some_and(|value| value.get("radioAnswerTurnId").is_some())
                {
                    Some("radio answer events are not supported by the Rust turn driver.")
                } else {
                    None
                }
            })
        };
        if let Some(reason) = unsupported {
            agent.status = AgentStatus::Waiting;
            agent
                .extra
                .insert("inFlight".to_owned(), Value::Bool(false));
            agent.extra.insert("turnId".to_owned(), Value::Null);
            agent
                .extra
                .insert("nativeFailureHold".to_owned(), Value::Bool(true));
            agent
                .extra
                .insert("error".to_owned(), Value::String(reason.to_owned()));
            agent.extra.insert(
                "startOutcomeHold".to_owned(),
                json!({"stage":"held","reason":reason}),
            );
            AgentRepository::put(transaction, &agent)?;
            return Ok(None);
        }
        // Process the supported FIFO prefix. Later unsupported events do not suppress
        // an earlier user message; they receive a named hold on the next admission.
        let supported_prefix = events
            .iter()
            .take_while(|event| {
                event.kind == "user"
                    && metadata
                        .get(&event.id.0)
                        .and_then(|value| value.get("assets"))
                        .and_then(Value::as_array)
                        .is_none_or(Vec::is_empty)
                    && !metadata
                        .get(&event.id.0)
                        .is_some_and(|value| value.get("radioAnswerTurnId").is_some())
            })
            .count();
        let selected = select_turn_batch(
            &agent,
            events.into_iter().take(supported_prefix).collect(),
            &metadata,
            limit,
        );
        let mut events = selected;
        if events.is_empty() {
            return Ok(None);
        }

        for event in &mut events {
            let changed = transaction.execute(
                "UPDATE runtime_events SET status='reserved' WHERE id=?1 AND agent=?2 \
                 AND epoch=?3 AND status='pending'",
                params![event.id.0, agent_id, epoch],
            )?;
            if changed != 1 {
                return Err(StoreError::SchemaMismatch(
                    "scheduler event claim changed during its write transaction".to_owned(),
                ));
            }
            event.status = "reserved".to_owned();
        }

        let event_ids = events
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>();
        mark_timing(
            transaction,
            &json!(event_ids).to_string(),
            "dispatchPickedAt",
        )?;
        agent.status = AgentStatus::Starting;
        agent.extra.insert("inFlight".to_owned(), Value::Bool(true));
        agent.extra.insert("turnId".to_owned(), Value::Null);
        agent.extra.insert("turnEpoch".to_owned(), json!(epoch));
        agent.extra.insert(
            "startAttempt".to_owned(),
            json!({
                "id": attempt_id,
                "epoch": epoch,
                "activeAtReservation": false,
                "accountKey":agent.extra.get("accountKey").cloned().unwrap_or(json!("default")),
                "events": event_ids,
                "submitted": false,
                "created": attempt_created
            }),
        );
        publish_agent(transaction, &agent)?;
        let identity = input_identity(&events, &metadata);
        crate::ExecutionHistoryRepository::save_claim(
            transaction,
            attempt_id,
            &identity,
            &json!({"agent":agent,"metadata":metadata}),
        )?;
        Ok(Some(ClaimedEvents {
            agent,
            events,
            metadata,
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
        let records = select_agents(
            transaction,
            "SELECT a.id,a.record FROM runtime_agents a WHERE EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent=a.id AND e.status='reserved')",
            [],
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

fn acceptance_input(agent: &str, epoch: i64, turn: &str) -> Value {
    json!({"agent":agent,"epoch":epoch,"accepted":turn})
}
const UNKNOWN_START_ERROR: &str = "turn/start response timed out; outcome unknown";
const NOT_SENT_ERROR: &str = "Provider did not send turn/start";
const WAITING_START: &str =
    "The native start result is unknown. Studio is checking the original input in native history.";

struct StartErrorDecision {
    event_status: &'static str,
    evidence: &'static str,
    visible: Option<AgentStatus>,
    in_flight: bool,
    return_busy_input: bool,
}
fn start_error_decision(
    agent: &AgentRecord,
    epoch: i64,
    unknown: bool,
    preparation: bool,
) -> StartErrorDecision {
    let attempt = &agent.extra["startAttempt"];
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
fn apply_start_error(
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
fn account_agents(
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
fn select_agents(
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

fn business_metadata(value: Option<&Value>) -> Value {
    let mut value = value.cloned().unwrap_or(json!({}));
    if let Some(map) = value.as_object_mut() {
        map.remove("timing");
    }
    value
}
fn input_identity(events: &[QueueEvent], metadata: &BTreeMap<String, Value>) -> Value {
    json!(events.iter().map(|e|json!({"id":e.id.0,"agent":e.agent.0,"kind":e.kind,"text":e.text,"created":e.created,"epoch":e.epoch,"error":e.error,"metadata":business_metadata(metadata.get(&e.id.0))})).collect::<Vec<_>>())
}
fn replay_claim(
    tx: &Transaction<'_>,
    agent: AgentRecord,
    epoch: i64,
    saved: &Value,
) -> StoreResult<ClaimedEvents> {
    let receipt_error = || {
        crate::ExecutionHistoryRepository::invalid(
            "runtime_execution_attempts",
            saved["id"].as_str().unwrap_or("unknown"),
        )
    };
    let original: AgentRecord = serde_json::from_value(saved["claimResult"]["agent"].clone())
        .map_err(|_| receipt_error())?;
    if original.id != agent.id || original.epoch != epoch || !same_native_scope(&original, &agent) {
        return Err(invalid_agent(&agent.id.0));
    }
    let identity = saved["inputIdentity"]
        .as_array()
        .ok_or_else(receipt_error)?;
    let has_metadata: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_event_meta')", [], |r| r.get(0))?;
    let metadata_column = if has_metadata { "m.record" } else { "NULL" };
    let metadata_join = if has_metadata {
        "LEFT JOIN runtime_event_meta m ON m.id=e.id"
    } else {
        ""
    };
    let mut statement = tx.prepare(&format!("SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,{metadata_column} FROM json_each(?1) j JOIN runtime_events e ON e.id=json_extract(j.value,'$.id') {metadata_join} ORDER BY j.key"))?;
    let mut events = Vec::new();
    let mut metadata = BTreeMap::new();
    for row in statement.query_map([saved["inputIdentity"].to_string()], |r| {
        Ok((
            crate::QueueEventRepository::from_row(r)?,
            r.get::<_, Option<String>>(9)?,
        ))
    })? {
        let (mut event, raw) = row?;
        event.status = "reserved".into();
        event.turn_id = None;
        if let Some(raw) = raw {
            metadata.insert(
                event.id.0.clone(),
                serde_json::from_str(&raw).map_err(|_| receipt_error())?,
            );
        }
        events.push(event);
    }
    if events.len() != identity.len() {
        return Err(receipt_error());
    }
    if input_identity(&events, &metadata) != saved["inputIdentity"] {
        return Err(invalid_agent(&agent.id.0));
    }
    let metadata = serde_json::from_value(saved["claimResult"]["metadata"].clone())
        .map_err(|_| receipt_error())?;
    Ok(ClaimedEvents {
        agent: original,
        events,
        metadata,
    })
}

fn record_notification(transaction: &Transaction<'_>, agent: &mut AgentRecord) -> StoreResult<()> {
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

fn timestamp() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
fn clear_hold(agent: &mut AgentRecord) {
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
fn disconnect_agent(
    transaction: &Transaction<'_>,
    agent: &mut AgentRecord,
    at: f64,
    restart: bool,
) -> StoreResult<()> {
    let attempt = agent
        .extra
        .get("startAttempt")
        .cloned()
        .unwrap_or(Value::Null);
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
        let mut marker = agent.extra["disconnectRecovery"].clone();
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
fn interrupt_uncertain(attempt: &Value, thread: &str, turn: &str) -> bool {
    let receipt = &attempt["interrupt"];
    receipt["threadId"] == thread
        && receipt["turnId"] == turn
        && matches!(receipt["outcome"].as_str(), Some("unknown" | "accepted"))
}
fn same_native_scope(a: &AgentRecord, b: &AgentRecord) -> bool {
    a.extra.get("accountKey").unwrap_or(&json!("default"))
        == b.extra.get("accountKey").unwrap_or(&json!("default"))
        && a.extra.get("threadId") == b.extra.get("threadId")
}
fn same_recovery_scope(a: &Value, b: &Value) -> bool {
    ["epoch", "accountKey", "threadId"]
        .iter()
        .all(|k| a[*k] == b[*k])
        && (a["turnId"].is_null() || b["turnId"].is_null() || a["turnId"] == b["turnId"])
        && a["startAttempt"]["id"] == b["startAttempt"]["id"]
        && a["startAttempt"]["connectionGeneration"] == b["startAttempt"]["connectionGeneration"]
}

fn stored_attempt(agent: &AgentRecord) -> Option<&Value> {
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
fn restart_continuation_allowed(agent: &AgentRecord) -> bool {
    let Some(marker) = agent.extra.get("restartRecovery") else {
        return false;
    };
    continuation_allowed(agent)
        && recovering(agent, agent.epoch)
        && matches!(marker["stage"].as_str(), Some("pending" | "superseded"))
        && agent.extra["disconnectRecovery"]["autoWake"] == true
        && marker["turnId"].is_string()
        && marker.get("turnId") == agent.extra.get("turnId")
        && marker["epoch"] == agent.epoch
        && ["accountKey", "threadId"]
            .iter()
            .all(|k| marker.get(*k) == agent.extra.get(*k))
}
fn transport_continuation_allowed(agent: &AgentRecord) -> bool {
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

fn continuation_allowed(agent: &AgentRecord) -> bool {
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
fn load_agent(db: &Connection, id: &str) -> StoreResult<Option<AgentRecord>> {
    db.query_row("SELECT record FROM runtime_agents WHERE id=?1", [id], |r| {
        r.get::<_, String>(0)
    })
    .optional()?
    .map(|raw| decode_agent(id, &raw))
    .transpose()
}
fn decode_agent(id: &str, raw: &str) -> StoreResult<AgentRecord> {
    let record: AgentRecord = serde_json::from_str(raw).map_err(|_| invalid_agent(id))?;
    if record.id.0 != id {
        return Err(invalid_agent(id));
    }
    Ok(record)
}
fn current_attempt(agent: &AgentRecord, epoch: i64) -> bool {
    agent.epoch == epoch
        && agent.extra.get("turnEpoch") == Some(&json!(epoch))
        && agent.extra.get("autoWake") == Some(&json!(true))
}
fn stopped_attempt(agent: &AgentRecord, epoch: i64) -> bool {
    agent.status == AgentStatus::Paused
        && agent.extra.get("autoWake") == Some(&json!(false))
        && agent.extra.get("turnEpoch") == Some(&json!(epoch))
}

fn recovering(agent: &AgentRecord, epoch: i64) -> bool {
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

fn mark_timing(tx: &Transaction<'_>, ids: &str, field: &str) -> StoreResult<()> {
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
fn publish_agent(transaction: &Transaction<'_>, agent: &AgentRecord) -> StoreResult<()> {
    crate::ExecutionHistoryRepository::record(transaction, agent)?;
    AgentRepository::put(transaction, agent)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptAuthority {
    Current,
    Stopped,
    Recovery,
    Deleted,
    Inactive,
}
fn attempt_authority(agent: &AgentRecord, epoch: i64) -> AttemptAuthority {
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
struct ValidatedAttempt {
    agent: AgentRecord,
    event_ids: (String, usize),
    authority: AttemptAuthority,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcceptanceDecision {
    Ignore,
    InterruptOnly,
    Bind { visible: bool, interrupt: bool },
}
fn acceptance_decision(
    agent: &AgentRecord,
    epoch: i64,
    turn: &str,
    authority: AttemptAuthority,
) -> AcceptanceDecision {
    if authority == AttemptAuthority::Deleted {
        return AcceptanceDecision::InterruptOnly;
    }
    if authority == AttemptAuthority::Inactive
        || agent.extra["startAttempt"]
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
fn load_attempt(
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

fn deliver_attempt_events(
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

fn require_membership(changed: usize, expected: usize, agent: &str) -> StoreResult<()> {
    if changed != expected {
        return Err(invalid_agent(agent));
    }
    Ok(())
}

fn validated_events(
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

fn select_turn_batch(
    agent: &AgentRecord,
    events: Vec<QueueEvent>,
    metadata: &BTreeMap<String, Value>,
    limit: usize,
) -> Vec<QueueEvent> {
    let mut selected = Vec::new();
    let mut asset_count = 0;
    let mut has_composer_input = false;
    for event in events.into_iter().take(limit.min(MAX_EVENTS_PER_TURN)) {
        let meta = metadata.get(&event.id.0);
        let assets = meta
            .and_then(|value| value.get("assets"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if asset_count + assets > 8 {
            break;
        }
        let composer_input = event.kind == "user"
            && (meta
                .and_then(|value| value.get("delivery"))
                .and_then(Value::as_str)
                == Some("after_turn")
                || meta
                    .and_then(|value| value.get("sendNow"))
                    .is_some_and(truthy));
        if composer_input && has_composer_input {
            break;
        }
        asset_count += assets;
        has_composer_input |= composer_input;
        selected.push(event);
    }
    if agent.extra.get("provider").and_then(Value::as_str) == Some("claude")
        && let Some(index) = selected.iter().position(|event| {
            event.kind == "user"
                && event.text.trim_start().starts_with(['/', '$'])
                && !event.text.contains('\n')
        })
    {
        selected.truncate(if index == 0 { 1 } else { index });
    }
    selected
}

fn invalid_agent(agent_id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: "runtime_agents".to_owned(),
        id: agent_id.to_owned(),
    }
}

fn completed_turn(connection: &Connection, agent: &str, turn: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_completed_turns WHERE id=?1)",
        [format!("{agent}:{turn}")],
        |row| row.get(0),
    )?)
}

fn truthy(value: &Value) -> bool {
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
mod tests {
    use super::{SchedulerRepository, select_turn_batch};
    use super::{interrupt_uncertain, same_recovery_scope};
    use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus};
    use crate::{QueueEvent, QueueEventId};
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn recovery_and_interrupt_policies_keep_exact_scope() {
        let a = json!({"epoch":1,"accountKey":"a","threadId":"t","turnId":"u","startAttempt":{"id":"one","connectionGeneration":3}});
        assert!(same_recovery_scope(&a, &a));
        let mut unknown = a.clone();
        unknown["turnId"] = serde_json::Value::Null;
        assert!(same_recovery_scope(&unknown, &a));
        for key in ["epoch", "accountKey", "threadId", "turnId"] {
            let mut changed = a.clone();
            changed[key] = json!("different");
            assert!(!same_recovery_scope(&a, &changed));
        }
        let mut changed = a.clone();
        changed["startAttempt"]["id"] = json!("two");
        assert!(!same_recovery_scope(&a, &changed));
        for (outcome, blocked) in [
            ("unknown", true),
            ("accepted", true),
            ("rejected", false),
            ("unsent", false),
        ] {
            let attempt = json!({"interrupt":{"threadId":"t","turnId":"u","outcome":outcome}});
            assert_eq!(interrupt_uncertain(&attempt, "t", "u"), blocked);
            assert!(!interrupt_uncertain(&attempt, "t", "other"));
        }
    }
    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);
                 CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
                 CREATE TABLE runtime_execution_inputs(run TEXT,event TEXT,PRIMARY KEY(run,event));
                 CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);
                 CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(
                   id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,
                   created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
                 CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,
                   PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER);",
            )
            .expect("scheduler tables create");
        let mut extra = BTreeMap::new();
        extra.insert("threadId".into(), json!("thread"));
        extra.insert("autoWake".to_owned(), json!(true));
        extra.insert("inFlight".to_owned(), json!(false));
        AgentRepository::put(
            &connection,
            &AgentRecord {
                id: AgentId::from("agent"),
                root_id: AgentId::from("agent"),
                epoch: 4,
                status: AgentStatus::Queued,
                extra,
            },
        )
        .expect("agent fixture writes");
        connection
            .execute(
                "INSERT INTO sync_entities VALUES('agent','agent',1,'hash',?1,0)",
                [json!({
                    "collection":"agent", "id":"agent",
                    "value":{"id":"agent", "status":"queued", "overview":{
                        "result":"", "resultTruncated":false, "resultTurnId":null
                    }}
                })
                .to_string()],
            )
            .expect("agent sync projection fixture writes");
        for (id, created) in [("later", 2.0), ("first", 1.0), ("tie", 1.0)] {
            connection
                .execute(
                    "INSERT INTO runtime_events VALUES(?1,'agent','user',?1,'pending',?2,4,NULL,NULL)",
                    rusqlite::params![id, created],
                )
                .expect("event fixture writes");
        }
        connection
    }

    #[test]
    fn claims_ordered_pending_events_and_updates_agent_in_one_transaction() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        let claimed = SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes")
            .expect("pending events exist");
        assert_eq!(
            claimed
                .events
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>(),
            ["first", "tie"]
        );
        assert_eq!(claimed.agent.extra["startAttempt"]["submitted"], false);
        transaction.commit().expect("claim commits");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        assert_eq!(agent.status, AgentStatus::Starting);
        assert_eq!(
            agent.extra["startAttempt"]["events"],
            json!(["first", "tie"])
        );
        let pending: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='pending'",
                [],
                |row| row.get(0),
            )
            .expect("pending count reads");
        assert_eq!(pending, 1);
    }

    #[test]
    fn restart_requeues_only_an_exact_unsubmitted_reservation() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        transaction.commit().expect("claim commits");
        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("recovery executes"),
            2
        );
        transaction.commit().expect("recovery commits");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        assert_eq!(agent.status, AgentStatus::Queued);
        assert!(!agent.extra.contains_key("startAttempt"));
        let reserved: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='reserved'",
                [],
                |row| row.get(0),
            )
            .expect("reserved count reads");
        assert_eq!(reserved, 0);
    }

    #[test]
    fn stop_revocation_cancels_only_the_exact_unsubmitted_claim() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        let _claimed = SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1)
            .expect("claim executes")
            .expect("pending event exists");
        transaction.commit().expect("claim commits");

        let transaction = connection.transaction().expect("stop transaction begins");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',5,'$.status','paused','$.autoWake',json('false'),'$.inFlight',json('false'))",
                [],
            )
            .expect("stop revokes agent epoch");
        assert!(
            !SchedulerRepository::mark_submitted_identity(
                &transaction,
                "agent",
                4,
                "attempt",
                "ticket-attempt",
                "default",
                1,
                "thread"
            )
            .expect("revoked claim cannot submit")
        );
        SchedulerRepository::settle_start_outcome(
            &transaction,
            &super::TurnAttemptIdentity {
                agent_id: "agent".into(),
                epoch: 4,
                attempt_id: "attempt".into(),
            },
            "pending",
            None,
        )
        .expect("unsent claim settles");
        transaction.commit().expect("stop cancellation commits");

        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("old event status reads");
        assert_eq!(status, "cancelled");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("stopped agent reads")
            .expect("stopped agent remains");
        assert_eq!(agent.epoch, 5);
        assert_eq!(agent.status, AgentStatus::Paused);

        let mut resumed = agent;
        resumed.status = AgentStatus::Queued;
        resumed.extra.insert("autoWake".to_owned(), json!(true));
        resumed.extra.insert("inFlight".to_owned(), json!(false));
        AgentRepository::put(&connection, &resumed).expect("resumed agent writes");
        connection
            .execute(
                "INSERT INTO runtime_events VALUES('resumed','agent','user','resumed','pending',3.0,5,NULL,NULL)",
                [],
            )
            .expect("resumed event writes");
        let transaction = connection.transaction().expect("resumed claim begins");
        let next = SchedulerRepository::claim(&transaction, "agent", 5, "next", 11.0, 1)
            .expect("resumed claim executes")
            .expect("resumed input is not blocked by old reservation");
        assert_eq!(next.events[0].id.0, "resumed");
        transaction.commit().expect("resumed claim commits");
    }

    #[test]
    fn submitted_attempt_is_not_recovered() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.submitted',json('true'))",
                [],
            )
            .expect("submitted marker writes");
        transaction.commit().expect("claim commits");
        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("recovery executes"),
            0
        );
        transaction.commit().expect("recovery commits");
        let event_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("reserved status reads");
        assert_eq!(event_status, "reserved");
    }

    #[test]
    fn submitted_claim_is_not_recovered_or_admitted_again() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        assert!(
            SchedulerRepository::mark_submitted_identity(
                &transaction,
                "agent",
                4,
                "attempt",
                "ticket-attempt",
                "default",
                1,
                "thread"
            )
            .expect("submitted marker persists")
        );
        transaction.commit().expect("claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("recovery executes"),
            0
        );
        transaction.commit().expect("recovery commits");

        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.status','queued','$.inFlight',0)",
                [],
            )
            .expect("agent is queued with unresolved attempt");
        connection
            .execute(
                "INSERT INTO runtime_events VALUES('latest','agent','user','latest','pending',3,4,NULL,NULL)",
                [],
            )
            .expect("later pending event writes");
        let transaction = connection
            .transaction()
            .expect("second claim transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "attempt-2", 11.0, 32)
                .expect("unresolved attempt blocks admission")
                .is_none()
        );
        transaction.commit().expect("second claim commits");
        let later_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='latest'",
                [],
                |row| row.get(0),
            )
            .expect("later event status reads");
        assert_eq!(later_status, "pending");
    }

    #[test]
    fn recovery_cancels_exact_stale_claim_and_recovers_other_agents() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "old-attempt", 10.0, 2)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',5,'$.status','queued','$.inFlight',0)",
                [],
            )
            .expect("agent epoch advances");
        let mut other_extra = BTreeMap::new();
        other_extra.insert("autoWake".to_owned(), json!(true));
        other_extra.insert("inFlight".to_owned(), json!(false));
        AgentRepository::put(
            &transaction,
            &AgentRecord {
                id: AgentId::from("other"),
                root_id: AgentId::from("other"),
                epoch: 1,
                status: AgentStatus::Queued,
                extra: other_extra,
            },
        )
        .expect("independent agent writes");
        transaction
            .execute(
                "INSERT INTO sync_entities VALUES('agent','other',2,'hash',?1,0)",
                [json!({
                    "collection":"agent", "id":"other",
                    "value":{"id":"other", "status":"queued", "overview":{
                        "result":"", "resultTruncated":false, "resultTurnId":null
                    }}
                })
                .to_string()],
            )
            .expect("independent sync projection writes");
        transaction
            .execute(
                "INSERT INTO runtime_events VALUES('other-first','other','user','input','pending',1,1,NULL,NULL)",
                [],
            )
            .expect("independent event writes");
        SchedulerRepository::claim(&transaction, "other", 1, "other-attempt", 11.0, 1)
            .expect("independent agent claim succeeds");
        transaction.commit().expect("old claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction).expect("stale recovery skips"),
            1
        );
        transaction.commit().expect("recovery commits");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("stale reservation status reads");
        assert_eq!(status, "cancelled");
        let other_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='other-first'",
                [],
                |row| row.get(0),
            )
            .expect("independent reservation recovers");
        assert_eq!(other_status, "pending");
    }

    #[test]
    fn native_failure_hold_blocks_claim_without_writes() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.nativeFailureHold',json('true'))",
                [],
            )
            .expect("native failure hold writes");
        let transaction = connection.transaction().expect("claim transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "held", 10.0, 2)
                .expect("held agent claim is refused")
                .is_none()
        );
        transaction
            .commit()
            .expect("refused claim commits without changes");
        let pending: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='pending'",
                [],
                |row| row.get(0),
            )
            .expect("pending events remain");
        assert_eq!(pending, 3);
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("held agent reads")
            .expect("held agent remains");
        assert_eq!(agent.status, AgentStatus::Queued);
        assert!(!agent.extra.contains_key("startAttempt"));
    }

    #[test]
    fn native_failure_hold_added_after_claim_blocks_submission() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        let _claimed = SchedulerRepository::claim(&transaction, "agent", 4, "held", 10.0, 1)
            .expect("initial claim executes")
            .expect("pending event exists");
        transaction.commit().expect("initial claim commits");

        let transaction = connection.transaction().expect("hold transaction begins");
        let mut agent = AgentRepository::get(&transaction, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        agent
            .extra
            .insert("nativeFailureHold".to_owned(), json!(true));
        AgentRepository::put(&transaction, &agent).expect("hold persists");
        assert!(
            !SchedulerRepository::mark_submitted_identity(
                &transaction,
                "agent",
                4,
                "held",
                "ticket-held",
                "default",
                1,
                "thread"
            )
            .expect("hold refuses submission")
        );
        SchedulerRepository::settle_start_outcome(
            &transaction,
            &super::TurnAttemptIdentity {
                agent_id: "agent".into(),
                epoch: 4,
                attempt_id: "held".into(),
            },
            "pending",
            None,
        )
        .expect("unsent claim settles");
        transaction.commit().expect("hold cancellation commits");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("held event status reads");
        assert_eq!(status, "pending");
    }

    #[test]
    fn recovery_does_not_override_a_same_epoch_stop_state() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 2)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.status','paused','$.autoWake',json('false'))",
                [],
            )
            .expect("stop state writes");
        transaction.commit().expect("claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert_eq!(
            SchedulerRepository::recover_unsubmitted(&transaction)
                .expect("stopped attempt is retained"),
            0
        );
        transaction.commit().expect("recovery commits");
        let agent = AgentRepository::get(&connection, &AgentId::from("agent"))
            .expect("agent reads")
            .expect("agent remains");
        assert_eq!(agent.status, AgentStatus::Paused);
        assert_eq!(agent.extra["autoWake"], false);
        let event_status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("event status reads");
        assert_eq!(event_status, "reserved");
    }

    #[test]
    fn malformed_attempt_event_ids_are_refused_without_requeueing() {
        let mut connection = database();
        let transaction = connection.transaction().expect("claim transaction begins");
        SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1)
            .expect("claim executes");
        transaction
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.events',json('[\"first\",null]'))",
                [],
            )
            .expect("malformed attempt writes");
        transaction.commit().expect("claim commits");

        let transaction = connection
            .transaction()
            .expect("recovery transaction begins");
        assert!(matches!(
            SchedulerRepository::recover_unsubmitted(&transaction),
            Err(crate::StoreError::InvalidSyncObject { .. })
        ));
        transaction.rollback().expect("refused recovery rolls back");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("reserved event remains");
        assert_eq!(status, "reserved");
    }

    #[test]
    fn refusal_to_claim_does_not_change_agent_or_events() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.autoWake',0)",
                [],
            )
            .expect("disabled autowake writes");
        let before_agent: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent snapshot reads");
        let before_events: Vec<(String, String)> = connection
            .prepare("SELECT id,status FROM runtime_events ORDER BY id")
            .expect("event snapshot prepares")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("event snapshot queries")
            .collect::<Result<_, _>>()
            .expect("event snapshot reads");
        let transaction = connection
            .transaction()
            .expect("refusal transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 32)
                .expect("claim refusal is not an error")
                .is_none()
        );
        transaction.commit().expect("refusal transaction commits");
        let after_agent: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent snapshot reads");
        let after_events: Vec<(String, String)> = connection
            .prepare("SELECT id,status FROM runtime_events ORDER BY id")
            .expect("event snapshot prepares")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("event snapshot queries")
            .collect::<Result<_, _>>()
            .expect("event snapshot reads");
        assert_eq!(after_agent, before_agent);
        assert_eq!(after_events, before_events);
    }

    #[test]
    fn unsupported_event_kind_is_held_with_a_named_refusal() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_events SET kind='agent_message' WHERE id='first'",
                [],
            )
            .expect("sensitive event writes");
        let transaction = connection
            .transaction()
            .expect("refusal transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 32)
                .expect("unsupported kind is refused")
                .is_none()
        );
        transaction.commit().expect("refusal commits");
        let after: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent snapshot reads");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("event status reads");
        let after: serde_json::Value = serde_json::from_str(&after).expect("agent record parses");
        assert_eq!(after["status"], "waiting");
        assert_eq!(
            after["error"],
            "event kind is not supported by the Rust turn driver."
        );
        assert_eq!(after["nativeFailureHold"], true);
        assert_eq!(status, "pending");
    }

    #[test]
    fn batch_selection_honors_composer_assets_and_claude_command_boundaries() {
        let mut agent = AgentRecord {
            id: AgentId::from("agent"),
            root_id: AgentId::from("agent"),
            epoch: 4,
            status: AgentStatus::Queued,
            extra: BTreeMap::new(),
        };
        let events = vec![
            selected_event("notice", "plan_update", "notice"),
            selected_event("after-turn", "user", "first composer input"),
            selected_event("send-now", "user", "second composer input"),
        ];
        let metadata = BTreeMap::from([
            (
                "after-turn".to_owned(),
                json!({"delivery":"after_turn", "assets":[]}),
            ),
            ("send-now".to_owned(), json!({"sendNow":true, "assets":[]})),
        ]);
        assert_eq!(
            select_turn_batch(&agent, events.clone(), &metadata, 32)
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>(),
            ["notice", "after-turn"]
        );

        agent.extra.insert("provider".to_owned(), json!("claude"));
        let slash = vec![
            selected_event("notice", "plan_update", "notice"),
            selected_event("command", "user", "  /review"),
            selected_event("later", "user", "later input"),
        ];
        assert_eq!(
            select_turn_batch(&agent, slash, &BTreeMap::new(), 32)
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>(),
            ["notice"]
        );

        let mut asset_metadata = BTreeMap::new();
        asset_metadata.insert("notice".to_owned(), json!({"assets":[1,2,3,4,5,6,7,8,9]}));
        assert!(select_turn_batch(&agent, events, &asset_metadata, 32).is_empty());
    }

    fn selected_event(id: &str, kind: &str, text: &str) -> QueueEvent {
        QueueEvent {
            id: QueueEventId::from(id),
            agent: AgentId::from("agent"),
            kind: kind.to_owned(),
            text: text.to_owned(),
            status: "pending".to_owned(),
            created: 1.0,
            epoch: 4,
            turn_id: None,
            error: None,
        }
    }
    #[test]
    fn claim_replays_saved_result_and_refuses_changed_input_after_recovery() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction");
        let first = SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1)
            .expect("claim")
            .expect("input");
        let retry = SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1)
            .expect("retry")
            .expect("saved claim");
        assert_eq!(retry, first);
        SchedulerRepository::recover_unsubmitted(&tx).expect("restore unsent");
        let same = SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1)
            .expect("recover retry")
            .expect("same input");
        assert_eq!(same, first);
        SchedulerRepository::recover_unsubmitted(&tx).expect("restore again");
        tx.execute(
            "UPDATE runtime_events SET text='changed input' WHERE id='first'",
            [],
        )
        .expect("changed fixture");
        let before: String = tx
            .query_row(
                "SELECT group_concat(record) FROM runtime_execution_attempts",
                [],
                |r| r.get(0),
            )
            .expect("before");
        assert!(matches!(
            SchedulerRepository::claim(&tx, "agent", 4, "stable", 10.0, 1),
            Err(crate::StoreError::InvalidSyncObject { .. })
        ));
        let after: String = tx
            .query_row(
                "SELECT group_concat(record) FROM runtime_execution_attempts",
                [],
                |r| r.get(0),
            )
            .expect("after");
        assert_eq!(before, after);
        let reserved: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='reserved'",
                [],
                |r| r.get(0),
            )
            .expect("no reservation");
        assert_eq!(reserved, 0);
    }
}
