//! Durable pending-event selection and claim primitives for the scheduler.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{EventWriteRepository, QueueEvent, QueueEventId, StoreError, StoreResult};

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
        let Some(mut agent) =
            AgentRepository::get(transaction, &AgentId::from(identity.agent_id.as_str()))?
        else {
            return Ok(());
        };
        let Some(attempt) = agent.extra.get("startAttempt") else {
            return Ok(());
        };
        if attempt["id"].as_str() != Some(&identity.attempt_id)
            || attempt["epoch"].as_i64() != Some(identity.epoch)
            || attempt["turnId"].as_str().is_some()
            || attempt["executionOutcome"].as_str().is_some()
        {
            return Ok(());
        }
        if outcome == "uncertain"
            && (attempt["submitted"] != true || attempt["requestTicket"].as_str().is_none())
        {
            return Err(StoreError::SchemaMismatch(
                "unknown submission has no durable ticket".into(),
            ));
        }
        let stopped = agent.epoch != identity.epoch || agent.status == AgentStatus::Paused;
        let status = if outcome == "uncertain" {
            "uncertain"
        } else if stopped {
            "cancelled"
        } else {
            "pending"
        };
        let events = attempt["events"]
            .as_array()
            .ok_or_else(|| invalid_agent(&identity.agent_id))?;
        for event in events {
            transaction.execute("UPDATE runtime_events SET status=?1,error=NULL WHERE id=?2 AND agent=?3 AND epoch=?4 AND status IN ('reserved','dispatching','uncertain')",
                params![status, event.as_str(), identity.agent_id, identity.epoch])?;
        }
        if outcome != "uncertain" {
            agent.extra.remove("startAttempt");
            agent.extra.insert("inFlight".into(), json!(false));
        }
        if !stopped {
            agent.status = if outcome == "pending" {
                AgentStatus::Queued
            } else {
                AgentStatus::Waiting
            };
            if let Some(reason) = reason {
                agent.extra.insert("nativeFailureHold".into(), json!(true));
                agent.extra.insert("error".into(), json!(reason));
                agent.extra.insert("startOutcomeHold".into(), json!({"stage":"held", "attemptId":identity.attempt_id,"epoch":identity.epoch,"reason":reason}));
            }
        }
        publish_agent(transaction, &agent)
    }

    /// An activity notice is not a receipt for the input batch. It may still require interruption.
    pub fn observe_started(
        transaction: &Transaction<'_>,
        identity: &TurnAttemptIdentity,
        turn_id: &str,
    ) -> StoreResult<Option<bool>> {
        let Some(mut agent) =
            AgentRepository::get(transaction, &AgentId::from(identity.agent_id.as_str()))?
        else {
            return Ok(None);
        };
        if attempt_events(&agent, identity.epoch, &identity.attempt_id)?.is_none()
            || completed_turn(transaction, &identity.agent_id, turn_id)?
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
        agent.extra.insert("lastAnswer".into(), json!(""));
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
        for mut agent in account_agents(transaction, account)? {
            let Some(attempt) = agent.extra.get("startAttempt") else {
                continue;
            };
            if attempt["submitted"] != true
                || attempt["connectionGeneration"]
                    .as_u64()
                    .is_some_and(|known| known != generation)
                || attempt["executionOutcome"].as_str().is_some()
            {
                continue;
            }
            let receipt = json!({"epoch":agent.epoch,"accountKey":account,
                "threadId":agent.extra.get("threadId"),"turnId":agent.extra.get("turnId"),
                "startAttempt":attempt,"connectionId":agent.extra.get("connectionId"),
                "autoWake":agent.extra.get("autoWake"),"supervisor":{"generation":generation},"at":at});
            if agent.status != AgentStatus::Paused {
                // Preserve the original permission across repeated disconnects.
                if !agent.extra.contains_key("disconnectRecovery") {
                    agent
                        .extra
                        .insert("disconnectRecovery".into(), receipt.clone());
                }
                if restart {
                    let mut marker = agent
                        .extra
                        .get("disconnectRecovery")
                        .cloned()
                        .unwrap_or(receipt);
                    marker["stage"] = json!("pending");
                    agent.extra.insert("restartRecovery".into(), marker);
                    if let Some(receipt) = agent.extra.get_mut("disconnectRecovery") {
                        receipt["source"] = json!("restart");
                    }
                }
                agent.status = AgentStatus::Interrupted;
                agent.extra.insert("autoWake".into(), json!(false));
                if agent
                    .extra
                    .get("startOutcomeHold")
                    .is_none_or(|hold| hold["stage"] != "account_settlement")
                {
                    agent.extra.insert(
                        "error".into(),
                        json!("Codex disconnected. Review the transcript before resuming."),
                    );
                }
            }
            agent.extra.insert("inFlight".into(), json!(false));
            transaction.execute("UPDATE runtime_events SET status='uncertain' WHERE agent=?1 AND epoch=?2 AND status='dispatching'", params![agent.id.0, agent.epoch])?;
            publish_agent(transaction, &agent)?;
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
        for agent in account_agents(transaction, account)? {
            let Some(old) = agent
                .extra
                .get("startAttempt")
                .and_then(|a| a["connectionGeneration"].as_u64())
            else {
                continue;
            };
            if old == generation {
                continue;
            }
            Self::disconnect_account(transaction, account, old, at, true)?;
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
        for mut agent in account_agents(transaction, account)? {
            if !agent.extra.contains_key("startAttempt") && agent.status != AgentStatus::Queued {
                continue;
            }
            let previous_error = agent
                .extra
                .get("startOutcomeHold")
                .filter(|hold| hold["stage"] == "account_settlement")
                .and_then(|hold| hold.get("previousError"))
                .cloned()
                .unwrap_or_else(|| agent.extra.get("error").cloned().unwrap_or(Value::Null));
            agent.extra.insert("nativeFailureHold".into(), json!(true));
            agent.extra.insert("error".into(), json!(reason));
            agent.extra.insert("startOutcomeHold".into(), json!({"stage":"account_settlement","cursor":cursor,"reason":reason,"previousError":previous_error}));
            publish_agent(transaction, &agent)?;
        }
        Ok(())
    }

    pub fn clear_account_hold(transaction: &Transaction<'_>, account: &str) -> StoreResult<()> {
        for mut agent in account_agents(transaction, account)? {
            if agent
                .extra
                .get("startOutcomeHold")
                .is_none_or(|h| h["stage"] != "account_settlement")
            {
                continue;
            }
            let hold = agent
                .extra
                .remove("startOutcomeHold")
                .unwrap_or(Value::Null);
            agent.extra.remove("nativeFailureHold");
            // A later stop error supersedes the settlement error.
            if agent.extra.get("error") == hold.get("reason") {
                agent.extra.insert(
                    "error".into(),
                    hold.get("previousError").cloned().unwrap_or(Value::Null),
                );
            }
            publish_agent(transaction, &agent)?;
        }
        Ok(())
    }

    /// Confirm a claimed attempt is still authorized before submitting user input.
    pub fn attempt_can_submit(
        connection: &Connection,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
    ) -> StoreResult<bool> {
        let Some(agent) = AgentRepository::get(connection, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
            return Ok(false);
        };
        Ok(agent.epoch == epoch
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch)
            && agent.status == AgentStatus::Starting
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
            && agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true)
            && attempt.get("id").and_then(Value::as_str) == Some(attempt_id)
            && attempt.get("epoch").and_then(Value::as_i64) == Some(epoch)
            && attempt.get("submitted").and_then(Value::as_bool) == Some(true))
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
        generation: u64,
    ) -> StoreResult<Vec<SubmittedStartRequest>> {
        let generation = i64::try_from(generation).map_err(|_| {
            StoreError::SchemaMismatch("provider generation exceeds SQLite integer range".into())
        })?;
        let mut statement = connection.prepare(
            "SELECT id,json_extract(record,'$.startAttempt.epoch'), \
                    json_extract(record,'$.startAttempt.id'), \
                    json_extract(record,'$.startAttempt.requestTicket'), \
                    json_extract(record,'$.startAttempt.threadId') \
             FROM runtime_agents \
             WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 \
             AND json_extract(record,'$.startAttempt.submitted')=1 \
             AND json_extract(record,'$.startAttempt.connectionGeneration')=?2 \
             AND json_extract(record,'$.startAttempt.turnId') IS NULL \
             AND json_extract(record,'$.startAttempt.requestTicket') IS NOT NULL \
             AND json_extract(record,'$.status') IN ('starting','running','waiting','paused','interrupted') \
             AND json_extract(record,'$.deletedAt') IS NULL \
             ORDER BY rowid",
        )?;
        let rows = statement.query_map(params![account_key, generation], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(None);
        };
        if agent.id.0 != agent_id
            || agent.epoch != epoch
            || agent.status != AgentStatus::Queued
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
            EventWriteRepository::set_agent_sync_lifecycle(
                transaction,
                agent_id,
                "waiting",
                false,
                None,
            )?;
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
                "events": event_ids,
                "submitted": false,
                "created": attempt_created
            }),
        );
        AgentRepository::put(transaction, &agent)?;
        EventWriteRepository::set_agent_sync_lifecycle(
            transaction,
            agent_id,
            "starting",
            true,
            None,
        )?;
        Ok(Some(ClaimedEvents {
            agent,
            events,
            metadata,
        }))
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
        for event_id in agent.extra["startAttempt"]["events"]
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
        let event_count = agent.extra["startAttempt"]["events"]
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
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let attempt = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(agent_id))?;
        if attempt.get("id").and_then(Value::as_str) != Some(attempt_id)
            || attempt.get("epoch").and_then(Value::as_i64) != Some(epoch)
            || attempt.get("submitted").and_then(Value::as_bool) != Some(false)
            || agent.epoch != epoch
        {
            return Ok(false);
        }
        let changed = Self::mark_submitted(transaction, agent_id, epoch, attempt_id)?;
        if !changed {
            return Ok(false);
        }
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let attempt = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.insert(
            "requestTicket".to_owned(),
            Value::String(request_id.to_owned()),
        );
        attempt.insert(
            "nativeOperationId".to_owned(),
            Value::String(request_id.to_owned()),
        );
        attempt.insert(
            "accountKey".to_owned(),
            Value::String(account_key.to_owned()),
        );
        attempt.insert("connectionGeneration".to_owned(), Value::from(generation));
        attempt.insert("threadId".to_owned(), Value::String(thread_id.to_owned()));
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

    /// Cancel only the exact reservation when a stop revoked it before submission.
    pub fn cancel_unsubmitted_claim(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        event_ids: &[String],
    ) -> StoreResult<bool> {
        let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
            return Ok(false);
        };
        let Some(attempt_events) = attempt.get("events").and_then(Value::as_array) else {
            return Err(invalid_agent(agent_id));
        };
        let mut persisted_ids = Vec::with_capacity(attempt_events.len());
        for event in attempt_events {
            let Some(id) = event.as_str() else {
                return Err(invalid_agent(agent_id));
            };
            persisted_ids.push(id.to_owned());
        }
        let mut expected_ids = event_ids.to_vec();
        persisted_ids.sort_unstable();
        expected_ids.sort_unstable();
        if attempt.get("id").and_then(Value::as_str) != Some(attempt_id)
            || attempt.get("epoch").and_then(Value::as_i64) != Some(epoch)
            || attempt.get("submitted").and_then(Value::as_bool) != Some(false)
            || persisted_ids != expected_ids
        {
            return Ok(false);
        }
        let mut cancelled = 0;
        for id in &expected_ids {
            cancelled += transaction.execute(
                "UPDATE runtime_events SET status='cancelled' \
                 WHERE id=?1 AND agent=?2 AND epoch=?3 AND status='reserved'",
                params![id, agent_id, epoch],
            )?;
        }
        if cancelled != expected_ids.len() {
            return Err(StoreError::SchemaMismatch(
                "scheduler reservation changed before stop cancellation".to_owned(),
            ));
        }
        Ok(true)
    }

    /// Return only event reservations whose persisted attempt proves no turn was submitted.
    pub fn recover_unsubmitted(transaction: &Transaction<'_>) -> StoreResult<usize> {
        let mut statement = transaction.prepare(
            "SELECT e.id,e.agent,e.epoch,a.record FROM runtime_events e \
             JOIN runtime_agents a ON a.id=e.agent WHERE e.status='reserved' ORDER BY e.rowid",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut candidates = BTreeMap::<(String, i64), (String, Vec<String>)>::new();
        for (event_id, agent_id, epoch, record) in rows {
            let candidate = candidates
                .entry((agent_id, epoch))
                .or_insert_with(|| (record, Vec::new()));
            candidate.1.push(event_id);
        }

        let mut recovered = 0;
        for ((agent_id, epoch), (record, reserved_ids)) in candidates {
            let mut agent: AgentRecord =
                serde_json::from_str(&record).map_err(|_| StoreError::InvalidSyncObject {
                    scope: "runtime_agents".to_owned(),
                    id: agent_id.clone(),
                })?;
            if agent.id.0 != agent_id {
                return Err(StoreError::InvalidSyncObject {
                    scope: "runtime_agents".to_owned(),
                    id: agent_id,
                });
            }
            let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
                return Err(invalid_agent(&agent_id));
            };
            let Some(events) = attempt.get("events").and_then(Value::as_array) else {
                return Err(invalid_agent(&agent_id));
            };
            let mut attempt_ids = Vec::with_capacity(events.len());
            for event in events {
                let Some(id) = event.as_str() else {
                    return Err(invalid_agent(&agent_id));
                };
                attempt_ids.push(id.to_owned());
            }
            attempt_ids.sort_unstable();
            if attempt_ids.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(invalid_agent(&agent_id));
            }
            let mut reserved = reserved_ids.clone();
            reserved.sort_unstable();
            if attempt.get("submitted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if attempt.get("submitted").and_then(Value::as_bool) != Some(false) {
                return Err(invalid_agent(&agent_id));
            }
            let exact_unsubmitted_attempt = attempt.get("epoch").and_then(Value::as_i64)
                == Some(epoch)
                && attempt_ids == reserved;
            if !exact_unsubmitted_attempt {
                if agent.epoch == epoch {
                    return Err(invalid_agent(&agent_id));
                }
                continue;
            }
            if agent.epoch != epoch {
                let mut cancelled = 0;
                for id in &attempt_ids {
                    cancelled += transaction.execute(
                        "UPDATE runtime_events SET status='cancelled' \
                         WHERE id=?1 AND agent=?2 AND epoch=?3 AND status='reserved'",
                        params![id, agent_id, epoch],
                    )?;
                }
                if cancelled != attempt_ids.len() {
                    return Err(StoreError::SchemaMismatch(
                        "stale scheduler reservation changed before stop cancellation".to_owned(),
                    ));
                }
                continue;
            }
            if agent.status != AgentStatus::Starting
                || agent.extra.get("autoWake").and_then(Value::as_bool) != Some(true)
                || agent.extra.get("inFlight").and_then(Value::as_bool) != Some(true)
            {
                continue;
            }
            let mut restored = 0;
            for id in &attempt_ids {
                restored += transaction.execute(
                    "UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL \
                     WHERE id=?1 AND agent=?2 AND epoch=?3 AND status='reserved'",
                    params![id, agent_id, epoch],
                )?;
            }
            if restored != 0 {
                agent.extra.remove("startAttempt");
                agent
                    .extra
                    .insert("inFlight".to_owned(), Value::Bool(false));
                agent.status = AgentStatus::Queued;
                AgentRepository::put(transaction, &agent)?;
                EventWriteRepository::set_agent_sync_lifecycle(
                    transaction,
                    &agent_id,
                    "queued",
                    false,
                    None,
                )?;
                recovered += restored;
            }
        }
        Ok(recovered)
    }

    /// Restore Python-shaped scoped receipts; the account worker will reattach without replay.
    pub fn recover_submitted(transaction: &Transaction<'_>) -> StoreResult<usize> {
        let mut statement = transaction.prepare("SELECT DISTINCT COALESCE(json_extract(record,'$.accountKey'),'default'), COALESCE(json_extract(record,'$.startAttempt.connectionGeneration'),0) FROM runtime_agents WHERE json_extract(record,'$.startAttempt.submitted')=1 AND json_extract(record,'$.startAttempt.executionOutcome') IS NULL")?;
        let accounts = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut recovered = 0;
        for (account, generation) in accounts {
            let generation = u64::try_from(generation)
                .map_err(|_| StoreError::SchemaMismatch("invalid provider generation".into()))?;
            Self::clear_account_hold(transaction, &account)?;
            Self::disconnect_account(
                transaction,
                &account,
                generation,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64()),
                true,
            )?;
            for mut agent in account_agents(transaction, &account)? {
                let Some(attempt) = agent.extra.get("startAttempt") else {
                    continue;
                };
                if attempt["submitted"] != true || attempt["executionOutcome"].as_str().is_some() {
                    continue;
                }
                if attempt["turnId"].as_str().is_none() {
                    let hold = json!({"stage":"held","attemptId":attempt["id"],"epoch":attempt["epoch"],"reason":"submitted input has no confirmed turn identity"});
                    agent.extra.insert("startOutcomeHold".into(), hold);
                    if let Some(marker) = agent.extra.get_mut("restartRecovery") {
                        marker["stage"] = json!("held");
                        marker["reason"] =
                            json!("Native input submission has no confirmed turn identity.");
                    }
                    publish_agent(transaction, &agent)?;
                }
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
        let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(None);
        };
        let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
            return Ok(None);
        };
        if attempt.get("id").and_then(Value::as_str) != Some(attempt_id)
            || attempt.get("epoch").and_then(Value::as_i64) != Some(epoch)
            || attempt.get("requestTicket").and_then(Value::as_str) != Some(request_id)
            || attempt.get("accountKey").and_then(Value::as_str) != Some(account_key)
            || attempt.get("connectionGeneration").and_then(Value::as_u64) != Some(generation)
            || attempt.get("threadId").and_then(Value::as_str) != Some(thread_id)
        {
            return Ok(None);
        }
        let interrupt = agent.status == AgentStatus::Paused || agent.epoch != epoch;
        Ok(
            Self::settle_started(transaction, agent_id, epoch, attempt_id, turn_id)?
                .then_some(interrupt),
        )
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
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let Some(event_ids) = attempt_events(&agent, epoch, attempt_id)? else {
            return Ok(false);
        };
        let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
            return Ok(false);
        };
        if attempt.get("requestTicket").and_then(Value::as_str) != Some(request_id)
            || attempt.get("accountKey").and_then(Value::as_str) != Some(account_key)
            || attempt.get("connectionGeneration").and_then(Value::as_u64) != Some(generation)
            || attempt.get("threadId").and_then(Value::as_str) != Some(thread_id)
        {
            return Ok(false);
        }
        if attempt.get("executionOutcome").and_then(Value::as_str) == Some("failed") {
            return Ok(true);
        }
        let stopped_after_submit = agent.epoch != epoch
            && agent.status == AgentStatus::Paused
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(false)
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch);
        let current_attempt = agent.epoch == epoch
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch)
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true);
        if !stopped_after_submit && !current_attempt && !recovering(&agent, epoch) {
            return Ok(false);
        }
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider rejected turn/start");
        for event_id in &event_ids {
            let changed = transaction.execute(
                "UPDATE runtime_events SET status='failed',turn_id=NULL,error=?1 \
                 WHERE id=?2 AND agent=?3 AND epoch=?4 AND status IN ('reserved','dispatching','uncertain')",
                params![message, event_id, agent_id, epoch],
            )?;
            if changed != 1 {
                return Err(StoreError::SchemaMismatch(
                    "start rejection no longer matches its submitted events".to_owned(),
                ));
            }
        }
        if let Some(attempt) = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
        {
            attempt.insert(
                "executionOutcome".to_owned(),
                Value::String("failed".into()),
            );
            attempt.insert("error".to_owned(), error.clone());
        }
        agent
            .extra
            .insert("inFlight".to_owned(), Value::Bool(false));
        agent.extra.insert("turnId".to_owned(), Value::Null);
        agent.extra.insert(
            "lastCompletedTurnStatus".to_owned(),
            Value::String("failed".into()),
        );
        agent
            .extra
            .insert("lastCompletedTurnError".to_owned(), error.clone());
        if current_attempt || recovering(&agent, epoch) {
            agent.status = AgentStatus::Failed;
            agent.extra.insert("error".to_owned(), error.clone());
            agent
                .extra
                .insert("nativeFailureHold".to_owned(), Value::Bool(true));
            agent.extra.insert(
                "startOutcomeHold".to_owned(),
                json!({
                    "stage":"held","attemptId":attempt_id,"epoch":epoch,"reason":message
                }),
            );
        }
        publish_agent(transaction, &agent)?;
        Ok(true)
    }

    pub fn settle_started(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        turn_id: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let Some(event_ids) = attempt_events(&agent, epoch, attempt_id)? else {
            return Ok(false);
        };
        if completed_turn(transaction, agent_id, turn_id)? {
            return Ok(false);
        }
        let stopped_after_submit = agent.epoch != epoch
            && agent.status == AgentStatus::Paused
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(false)
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch);
        let current_attempt = agent.epoch == epoch
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch)
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
            && matches!(
                agent.status,
                AgentStatus::Starting | AgentStatus::Waiting | AgentStatus::Running
            );
        if !stopped_after_submit && !current_attempt && !recovering(&agent, epoch) {
            return Ok(false);
        }
        if agent
            .extra
            .get("startAttempt")
            .and_then(Value::as_object)
            .and_then(|attempt| attempt.get("turnId"))
            .and_then(Value::as_str)
            .is_some_and(|existing| existing != turn_id)
            || agent
                .extra
                .get("turnId")
                .and_then(Value::as_str)
                .is_some_and(|existing| existing != turn_id)
        {
            return Ok(false);
        }
        if !can_deliver_attempt_events(transaction, agent_id, epoch, &event_ids, turn_id)? {
            return Ok(false);
        }
        deliver_attempt_events(transaction, agent_id, epoch, &event_ids, turn_id)?;

        let attempt = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.insert("turnId".to_owned(), Value::String(turn_id.to_owned()));
        attempt.insert(
            "observedTurnId".to_owned(),
            Value::String(turn_id.to_owned()),
        );
        if agent.epoch == epoch {
            attempt.remove("executionOutcome");
        }
        agent
            .extra
            .insert("turnId".to_owned(), Value::String(turn_id.to_owned()));
        agent.extra.insert("lastAnswer".into(), json!(""));
        agent.extra.insert("inFlight".to_owned(), Value::Bool(true));
        if current_attempt || recovering(&agent, epoch) {
            if current_attempt {
                agent.status = AgentStatus::Running;
            }
            if agent
                .extra
                .get("startOutcomeHold")
                .and_then(Value::as_object)
                .and_then(|hold| hold.get("attemptId"))
                .and_then(Value::as_str)
                == Some(attempt_id)
            {
                agent.extra.remove("startOutcomeHold");
                if agent
                    .extra
                    .get("nativeFailureHold")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    agent
                        .extra
                        .insert("nativeFailureHold".to_owned(), Value::Bool(false));
                }
                agent.extra.insert("error".to_owned(), Value::Null);
            }
        }
        publish_agent(transaction, &agent)?;
        Ok(true)
    }

    /// Apply one terminal provider outcome, ignoring stale attempt or epoch callbacks.
    pub fn settle_terminal(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        turn_id: &str,
        status: AgentStatus,
        error: Option<Value>,
    ) -> StoreResult<bool> {
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
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Ok(false);
        };
        let Some(event_ids) = attempt_events(&agent, epoch, attempt_id)? else {
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
        let stopped_after_submit = agent.epoch != epoch
            && agent.status == AgentStatus::Paused
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(false)
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch);
        let current_attempt = agent.epoch == epoch
            && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(epoch)
            && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(true)
            && matches!(
                agent.status,
                AgentStatus::Starting | AgentStatus::Waiting | AgentStatus::Running
            );
        if !stopped_after_submit && !current_attempt && !recovering(&agent, epoch) {
            return Ok(false);
        }
        if agent
            .extra
            .get("startAttempt")
            .and_then(Value::as_object)
            .and_then(|attempt| attempt.get("turnId"))
            .and_then(Value::as_str)
            .is_some_and(|existing| existing != turn_id)
            || agent
                .extra
                .get("turnId")
                .and_then(Value::as_str)
                .is_some_and(|existing| existing != turn_id)
        {
            return Ok(false);
        }
        if !can_deliver_attempt_events(transaction, agent_id, epoch, &event_ids, turn_id)? {
            return Ok(false);
        }
        deliver_attempt_events(transaction, agent_id, epoch, &event_ids, turn_id)?;

        {
            let attempt = agent
                .extra
                .get_mut("startAttempt")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| invalid_agent(agent_id))?;
            attempt.insert("turnId".to_owned(), Value::String(turn_id.to_owned()));
            attempt.insert(
                "executionOutcome".to_owned(),
                Value::String(outcome.to_owned()),
            );
        }
        let clears_unknown = agent
            .extra
            .get("startOutcomeHold")
            .and_then(Value::as_object)
            .and_then(|hold| hold.get("attemptId"))
            .and_then(Value::as_str)
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
            .insert("lastAnswer".into(), Value::String(String::new()));
        agent.extra.insert(
            "lastCompletedTurn".to_owned(),
            Value::String(turn_id.to_owned()),
        );
        agent.extra.insert(
            "lastCompletedTurnStatus".to_owned(),
            Value::String(outcome.to_owned()),
        );
        agent.extra.insert(
            "lastCompletedTurnError".to_owned(),
            error.clone().unwrap_or(Value::Null),
        );
        agent.extra.insert("turnId".to_owned(), Value::Null);
        agent.extra.insert("activity".to_owned(), Value::Null);
        agent
            .extra
            .insert("activeTools".to_owned(), serde_json::json!([]));
        agent
            .extra
            .insert("inFlight".to_owned(), Value::Bool(false));
        if current_attempt || recovering(&agent, epoch) {
            if let Some(error) = error {
                agent.extra.insert("error".to_owned(), error);
            } else {
                agent.extra.insert("error".to_owned(), Value::Null);
            }
            if status == AgentStatus::Failed {
                agent
                    .extra
                    .insert("nativeFailureHold".to_owned(), Value::Bool(true));
            } else if clears_unknown {
                agent
                    .extra
                    .insert("nativeFailureHold".to_owned(), Value::Bool(false));
            }
            agent.status = status.clone();
        }
        if current_attempt && status == AgentStatus::Completed
            && !agent.extra.get("nativeFailureHold").is_some_and(truthy)
            && transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending')",
                params![agent_id, epoch],
                |row| row.get::<_, bool>(0),
            )?
        {
            agent.status = AgentStatus::Queued;
        }
        publish_agent(transaction, &agent)?;
        Ok(true)
    }

    pub fn has_unsubmitted_claim(
        transaction: &Transaction<'_>,
        agent: &str,
        attempt_id: &str,
    ) -> StoreResult<bool> {
        Ok(transaction
            .query_row(
                "SELECT 1 FROM runtime_agents WHERE id=?1 AND json_extract(record,'$.startAttempt.id')=?2 \
                 AND json_extract(record,'$.startAttempt.submitted')=0",
                params![agent, attempt_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

fn account_agents(connection: &Connection, account: &str) -> StoreResult<Vec<AgentRecord>> {
    let mut statement = connection.prepare("SELECT record FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.deletedAt') IS NULL")?;
    let rows = statement.query_map([account], |row| row.get::<_, String>(0))?;
    rows.map(|row| {
        serde_json::from_str(&row?)
            .map_err(|_| StoreError::SchemaMismatch("invalid account agent".into()))
    })
    .collect()
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

fn publish_agent(transaction: &Transaction<'_>, agent: &AgentRecord) -> StoreResult<()> {
    AgentRepository::put(transaction, agent)?;
    let status = serde_json::to_value(&agent.status).map_err(|_| invalid_agent(&agent.id.0))?;
    EventWriteRepository::set_agent_sync_lifecycle(
        transaction,
        &agent.id.0,
        status.as_str().ok_or_else(|| invalid_agent(&agent.id.0))?,
        agent
            .extra
            .get("inFlight")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        agent.extra.get("turnId").and_then(Value::as_str),
    )
}

fn attempt_events(
    agent: &AgentRecord,
    epoch: i64,
    attempt_id: &str,
) -> StoreResult<Option<Vec<String>>> {
    let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
        return Ok(None);
    };
    if attempt.get("id").and_then(Value::as_str) != Some(attempt_id)
        || attempt.get("epoch").and_then(Value::as_i64) != Some(epoch)
        || attempt.get("submitted").and_then(Value::as_bool) != Some(true)
    {
        return Ok(None);
    }
    let Some(events) = attempt.get("events").and_then(Value::as_array) else {
        return Err(invalid_agent(&agent.id.0));
    };
    let mut event_ids = Vec::with_capacity(events.len());
    for event in events {
        let Some(id) = event.as_str() else {
            return Err(invalid_agent(&agent.id.0));
        };
        event_ids.push(id.to_owned());
    }
    event_ids.sort_unstable();
    if event_ids.is_empty() || event_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid_agent(&agent.id.0));
    }
    Ok(Some(event_ids))
}

fn can_deliver_attempt_events(
    transaction: &Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    event_ids: &[String],
    turn_id: &str,
) -> StoreResult<bool> {
    for event_id in event_ids {
        let row = transaction
            .query_row(
                "SELECT status,turn_id FROM runtime_events WHERE id=?1 AND agent=?2 AND epoch=?3",
                params![event_id, agent_id, epoch],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((status, existing_turn)) = row else {
            return Ok(false);
        };
        if !matches!(status.as_str(), "reserved" | "dispatching" | "uncertain")
            && !(status == "delivered" && existing_turn.as_deref() == Some(turn_id))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn deliver_attempt_events(
    transaction: &Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    event_ids: &[String],
    turn_id: &str,
) -> StoreResult<()> {
    for event_id in event_ids {
        let changed = transaction.execute(
            "UPDATE runtime_events SET status='delivered',turn_id=?1,error=NULL \
             WHERE id=?2 AND agent=?3 AND epoch=?4 \
             AND (status IN ('reserved','dispatching','uncertain') \
                  OR (status='delivered' AND turn_id=?1))",
            params![turn_id, event_id, agent_id, epoch],
        )?;
        if changed != 1 {
            return Err(StoreError::SchemaMismatch(
                "scheduler event changed during settlement transaction".to_owned(),
            ));
        }
    }
    Ok(())
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
    use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus};
    use crate::{QueueEvent, QueueEventId};
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);
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
        assert!(
            SchedulerRepository::has_unsubmitted_claim(&transaction, "agent", "attempt")
                .expect("claim marker reads")
        );
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
        let projected_status: String = connection
            .query_row(
                "SELECT json_extract(payload,'$.value.status') FROM sync_entities WHERE collection='agent' AND id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent sync status reads");
        assert_eq!(projected_status, "starting");
        let projected_in_flight: bool = connection
            .query_row(
                "SELECT json_extract(payload,'$.value.inFlight') FROM sync_entities WHERE collection='agent' AND id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent sync activity reads");
        assert!(projected_in_flight);
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
        let projected_in_flight: bool = connection
            .query_row(
                "SELECT json_extract(payload,'$.value.inFlight') FROM sync_entities WHERE collection='agent' AND id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("recovered agent sync activity reads");
        assert!(!projected_in_flight);
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
        let claimed = SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 1)
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
            !SchedulerRepository::mark_submitted(&transaction, "agent", 4, "attempt")
                .expect("revoked claim cannot submit")
        );
        let ids = claimed
            .events
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>();
        assert!(
            SchedulerRepository::cancel_unsubmitted_claim(
                &transaction,
                "agent",
                4,
                "attempt",
                &ids,
            )
            .expect("exact revoked reservation cancels")
        );
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
            SchedulerRepository::mark_submitted(&transaction, "agent", 4, "attempt")
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
        let claimed = SchedulerRepository::claim(&transaction, "agent", 4, "held", 10.0, 1)
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
            !SchedulerRepository::mark_submitted(&transaction, "agent", 4, "held")
                .expect("hold refuses submission")
        );
        let ids = claimed
            .events
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>();
        assert!(
            SchedulerRepository::cancel_unsubmitted_claim(&transaction, "agent", 4, "held", &ids,)
                .expect("held pre-submit claim cancels")
        );
        transaction.commit().expect("hold cancellation commits");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='first'",
                [],
                |row| row.get(0),
            )
            .expect("held event status reads");
        assert_eq!(status, "cancelled");
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
}
