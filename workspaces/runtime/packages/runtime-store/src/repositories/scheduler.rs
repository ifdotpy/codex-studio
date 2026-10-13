//! Durable pending-event selection and claim primitives for the scheduler.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus, get_raw_record};
use crate::repositories::events::map_queue_event;
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{EventWriteRepository, QueueEvent, StoreError, StoreResult};

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
const COMPLAINTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_complaints",
    columns: &["id", "record"],
}];
const WORK: &[TableRequirement] = &[TableRequirement {
    name: "runtime_work",
    columns: &["id", "record"],
}];

pub type SubmittedStartRequest = (String, i64, String, String, String);

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: TABLES,
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_complaints",
            tables: COMPLAINTS,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_event_meta",
            tables: EVENT_META,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_work",
            tables: WORK,
        },
    ],
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
    pub refusal_reason: Option<String>,
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
        if agent
            .extra
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
        {
            return Err(invalid_agent(agent_id));
        }
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

        // Python gives radio turns priority over the ordinary queue prefix.
        if transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 \
             AND status='pending' AND kind IN ('radio','radio_turn'))",
            params![agent_id, epoch],
            |row| row.get::<_, bool>(0),
        )? {
            return Ok(Some(refused(
                &agent,
                "radio-turn requires radio-policy".to_owned(),
            )));
        }

        let prefix = limit.min(MAX_EVENTS_PER_TURN);
        let has_metadata = table_exists(transaction, "runtime_event_meta")?;
        if has_metadata {
            let radio = transaction
                .query_row(
                    "SELECT e.id,m.record FROM runtime_events e \
                     JOIN runtime_event_meta m ON m.id=e.id \
                     WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
                     AND json_valid(m.record) \
                     AND json_type(m.record,'$.radioAnswerTurnId') IS NOT NULL \
                     AND json_type(m.record,'$.radioAnswerTurnId')!='null' \
                     ORDER BY e.created,e.rowid LIMIT 1",
                    params![agent_id, epoch],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((id, raw)) = radio {
                let value: Value = serde_json::from_str(&raw)
                    .map_err(|_| invalid_object("runtime_event_meta", &id))?;
                if !value.get("radioAnswerTurnId").is_some_and(Value::is_string) {
                    return Err(invalid_object("runtime_event_meta", &id));
                }
                return Ok(Some(refused(
                    &agent,
                    format!("radio answer {id} requires radio-policy"),
                )));
            }
        }
        let sql = if has_metadata {
            "SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,m.record \
             FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id \
             WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
             ORDER BY e.created,e.rowid LIMIT ?3"
        } else {
            "SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,NULL \
             FROM runtime_events e WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
             ORDER BY e.created,e.rowid LIMIT ?3"
        };
        let mut statement = transaction.prepare(sql)?;
        let rows = statement.query_map(params![agent_id, epoch, prefix as i64], |row| {
            Ok((map_queue_event(row)?, row.get::<_, Option<String>>(9)?))
        })?;
        let joined = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if joined.is_empty() {
            return Ok(None);
        }
        let mut events = Vec::with_capacity(joined.len());
        let mut metadata = BTreeMap::new();
        for (event, raw) in joined {
            if let Some(raw) = raw {
                let value: Value = serde_json::from_str(&raw)
                    .map_err(|_| invalid_object("runtime_event_meta", &event.id.0))?;
                let valid = value.is_object()
                    && value.get("assets").is_none_or(Value::is_array)
                    && value
                        .get("radioAnswerTurnId")
                        .is_none_or(|turn| turn.is_null() || turn.is_string());
                if !valid {
                    return Err(invalid_object("runtime_event_meta", &event.id.0));
                }
                metadata.insert(event.id.0.clone(), value);
            }
            events.push(event);
        }

        if let Some(event) = events.iter().find(|event| {
            metadata
                .get(&event.id.0)
                .and_then(|value| value.get("radioAnswerTurnId"))
                .is_some_and(|turn| !turn.is_null())
        }) {
            return Ok(Some(refused(
                &agent,
                format!("radio answer {} requires radio-policy", event.id.0),
            )));
        }
        let (complaints, _work) = load_policy_records(transaction, &agent, &events)?;
        let obsolete_work = obsolete_work_review_ids(transaction, &agent, &events)?;
        let prepared =
            prepare_pending_batch(&agent, events, metadata, &complaints, &obsolete_work)?;
        if let Some((kind, dependency)) = prepared.events.iter().find_map(|event| {
            let dependency = event_policy_dependency(&event.kind).or(match event.kind.as_str() {
                "complaint" => Some("complaint-policy"),
                "work_review" => Some("work-policy"),
                _ => None,
            });
            dependency.map(|dependency| (event.kind.as_str(), dependency))
        }) {
            return Ok(Some(refused(
                &agent,
                format!("scheduler cannot claim {kind}: requires {dependency}"),
            )));
        }
        let mut events = select_turn_batch(&prepared.events, &prepared.metadata, prefix);

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
            events
                .iter()
                .find(|event| !matches!(event.kind.as_str(), "user" | "complaint"))
                .map(|_| "event kind is not supported by the Rust turn driver.")
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
        if events.is_empty() {
            apply_preparation(transaction, &prepared)?;
            set_waiting(transaction, &mut agent, agent_id)?;
            return Ok(None);
        }
        apply_preparation(transaction, &prepared)?;
        let metadata = prepared.metadata;

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
            refusal_reason: None,
        }))
    }

    /// Reconcile notifications that became obsolete after reservation, before native submission.
    /// A false result means the caller must stop and must not cross the submission boundary.
    pub fn reconcile_obsolete_before_submit(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Err(invalid_agent(agent_id));
        };
        if agent
            .extra
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
        {
            return Err(invalid_agent(agent_id));
        }
        let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
            return Ok(false);
        };
        if attempt.get("id").and_then(Value::as_str) != Some(attempt_id)
            || attempt.get("epoch").and_then(Value::as_i64) != Some(epoch)
            || attempt.get("submitted").and_then(Value::as_bool) != Some(false)
            || attempt.get("turnId").is_some_and(|value| !value.is_null())
            || attempt
                .get("observedTurnId")
                .is_some_and(|value| !value.is_null())
            || agent.epoch != epoch
        {
            return Ok(false);
        }
        let saved = attempt
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_agent(agent_id))?;
        let mut event_ids = Vec::with_capacity(saved.len());
        for id in saved {
            let Some(id) = id.as_str() else {
                return Err(invalid_agent(agent_id));
            };
            event_ids.push(id.to_owned());
        }
        if event_ids.is_empty() || event_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid_agent(agent_id));
        }
        let mut statement = transaction.prepare(
            "SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events \
             WHERE id IN (SELECT value FROM json_each(?1)) AND agent=?2 AND epoch=?3 ORDER BY created,rowid",
        )?;
        let payload = serde_json::to_string(&event_ids)?;
        let rows = statement.query_map(params![payload, agent_id, epoch], map_queue_event)?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if rows.len() != event_ids.len()
            || rows
                .iter()
                .any(|event| !matches!(event.status.as_str(), "reserved" | "dispatching"))
            || rows
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>()
                != event_ids.iter().map(String::as_str).collect::<Vec<_>>()
        {
            return Ok(false);
        }
        let (complaints, _work) = load_policy_records(transaction, &agent, &rows)?;
        let obsolete_work = obsolete_work_review_ids(transaction, &agent, &rows)?;
        let prepared = prepare_pending_batch(
            &agent,
            rows.clone(),
            BTreeMap::new(),
            &complaints,
            &obsolete_work,
        )?;
        let mut changes = BTreeMap::new();
        let mut retired = Vec::new();
        for event in &rows {
            let reason = prepared.retired.get(&event.id.0);
            if let Some(reason) = reason {
                retired.push(event.id.0.clone());
                changes.insert(
                    event.id.0.clone(),
                    json!({"status":"stored_only","error":reason}),
                );
            } else {
                changes.insert(event.id.0.clone(), json!({"status":"pending","error":null}));
            }
        }
        if retired.is_empty() {
            return Ok(true);
        }
        let payload = serde_json::to_string(&changes)?;
        let mut update = transaction.prepare(
            "UPDATE runtime_events SET status=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.status'), \
             error=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.error') \
             WHERE id IN (SELECT key FROM json_each(?1)) AND agent=?2 AND epoch=?3 \
             AND status IN ('reserved','dispatching') RETURNING id",
        )?;
        let updated = update
            .query_map(params![payload, agent_id, epoch], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<BTreeSet<_>, _>>()?;
        if updated.len() != changes.len() {
            return Err(StoreError::SchemaMismatch(
                "scheduler reservation changed during submission reconciliation".to_owned(),
            ));
        }
        let attempt = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.insert(
            "notSubmittedReason".to_owned(),
            json!("Notifications resolved before native submission"),
        );
        attempt.insert("retiredEvents".to_owned(), json!(retired));
        let active = attempt
            .get("activeAtReservation")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !active {
            let pending = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending')",
                params![agent_id, epoch], |row| row.get::<_, bool>(0),
            )?;
            agent.status = if pending {
                AgentStatus::Queued
            } else {
                AgentStatus::Waiting
            };
            agent
                .extra
                .insert("inFlight".to_owned(), Value::Bool(false));
            publish_agent(transaction, &agent)?;
        }
        Ok(false)
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

struct PreparedBatch {
    events: Vec<QueueEvent>,
    metadata: BTreeMap<String, Value>,
    metadata_updates: BTreeMap<String, Value>,
    retired: BTreeMap<String, String>,
    cancelled: BTreeMap<String, String>,
}

fn refused(agent: &AgentRecord, reason: String) -> ClaimedEvents {
    ClaimedEvents {
        agent: agent.clone(),
        events: Vec::new(),
        metadata: BTreeMap::new(),
        refusal_reason: Some(reason),
    }
}

fn invalid_object(table: &str, id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: table.to_owned(),
        id: id.to_owned(),
    }
}

fn table_exists(transaction: &Transaction<'_>, table: &str) -> StoreResult<bool> {
    Ok(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn load_policy_records(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    events: &[QueueEvent],
) -> StoreResult<(BTreeMap<String, Value>, BTreeMap<String, Value>)> {
    let mut complaints = BTreeMap::new();
    let mut work = BTreeMap::new();
    if table_exists(transaction, "runtime_complaints")? {
        for event in events.iter().filter(|event| event.kind == "complaint") {
            for id in complaint_ids(&event.text).unwrap_or_default() {
                if complaints.contains_key(&id) {
                    continue;
                }
                if let Some(raw) = get_raw_record(transaction, "runtime_complaints", &id)? {
                    let value: Value = serde_json::from_str(&raw)
                        .map_err(|_| invalid_object("runtime_complaints", &id))?;
                    let valid = value.get("leadId").and_then(Value::as_str).is_some()
                        && value.get("status").and_then(Value::as_str).is_some()
                        && value.get("recipient").and_then(Value::as_str).is_some()
                        && value
                            .get("responses")
                            .and_then(Value::as_array)
                            .is_some_and(|responses| {
                                responses.iter().all(|response| {
                                    response.get("author").and_then(Value::as_str).is_some()
                                })
                            });
                    if !valid {
                        return Err(invalid_object("runtime_complaints", &id));
                    }
                    if value["leadId"] != agent.id.0 {
                        continue;
                    }
                    complaints.insert(id, value);
                }
            }
        }
    }
    if table_exists(transaction, "runtime_work")? {
        for event in events.iter().filter(|event| event.kind == "work_review") {
            let Some((id, _)) = work_review_ids(event) else {
                continue;
            };
            if work.contains_key(&id) {
                continue;
            }
            if let Some(raw) = get_raw_record(transaction, "runtime_work", &id)? {
                let value: Value =
                    serde_json::from_str(&raw).map_err(|_| invalid_object("runtime_work", &id))?;
                let valid = value.get("rootId").and_then(Value::as_str).is_some()
                    && value
                        .get("results")
                        .and_then(Value::as_array)
                        .is_some_and(|results| {
                            results
                                .iter()
                                .all(|result| result.get("id").and_then(Value::as_str).is_some())
                        })
                    && value
                        .get("decisions")
                        .and_then(Value::as_array)
                        .is_some_and(|decisions| {
                            decisions.iter().all(|decision| {
                                decision.get("resultId").and_then(Value::as_str).is_some()
                            })
                        });
                if !valid {
                    return Err(invalid_object("runtime_work", &id));
                }
                work.insert(id, value);
            }
        }
    }
    Ok((complaints, work))
}

fn complaint_ids(text: &str) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let entries = value.get("complaints")?.as_array()?;
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .map(|entry| {
            let id = entry.get("complaint_id")?.as_str()?;
            (!id.is_empty()).then(|| id.to_owned())
        })
        .collect()
}

fn complaints_resolved(
    agent: &AgentRecord,
    event: &QueueEvent,
    records: &BTreeMap<String, Value>,
) -> bool {
    let Some(ids) = complaint_ids(&event.text) else {
        return false;
    };
    ids.iter().all(|id| {
        records.get(id).is_some_and(|record| {
            let status = record["status"].as_str().unwrap_or_default();
            if matches!(status, "resolved" | "declined") {
                return true;
            }
            if record["sourceType"] == "user_task" && status == "open" {
                return false;
            }
            let responsible = if record["recipient"] == "user" {
                "user"
            } else {
                agent.id.0.as_str()
            };
            record["responses"]
                .as_array()
                .is_some_and(|responses| responses.iter().any(|r| r["author"] == responsible))
        })
    })
}

fn progress_update(text: &str) -> Option<((String, String, String), u64)> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("importance")?.as_str()? != "progress" {
        return None;
    }
    let field = |key| {
        value
            .get(key)?
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let version = value.get("progress_version")?.as_u64()?;
    Some((
        (field("sender")?, field("room")?, field("progress_key")?),
        version,
    ))
}

fn prepare_pending_batch(
    agent: &AgentRecord,
    events: Vec<QueueEvent>,
    mut metadata: BTreeMap<String, Value>,
    complaints: &BTreeMap<String, Value>,
    obsolete_work: &BTreeSet<String>,
) -> StoreResult<PreparedBatch> {
    let mut metadata_updates = BTreeMap::new();
    let mut retired = BTreeMap::new();
    let mut cancelled = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == "chat_review") {
        cancelled.insert(
            event.id.0.clone(),
            "Scheduled chat reviews have been removed".to_owned(),
        );
    }
    for event in events.iter().filter(|event| event.kind == "complaint") {
        if complaints_resolved(agent, event, complaints) {
            retired.insert(
                event.id.0.clone(),
                "Every referenced complaint already has a response".to_owned(),
            );
        }
    }
    for event in events
        .iter()
        .filter(|event| obsolete_work.contains(&event.id.0))
    {
        retired.insert(
            event.id.0.clone(),
            "The exact result already has a decision".to_owned(),
        );
    }
    let mut groups: BTreeMap<(String, String, String), Vec<(usize, u64)>> = BTreeMap::new();
    let mut preserved = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let saved = metadata
            .get(&event.id.0)
            .and_then(|value| value.get("preserveProgress"))
            .is_some_and(truthy);
        preserved.push(saved);
        if event.kind == "agent_message"
            && let Some((key, version)) = progress_update(&event.text)
        {
            groups.entry(key).or_default().push((index, version));
        }
    }
    for group in groups.values() {
        let versions = group
            .iter()
            .map(|(_, version)| *version)
            .collect::<BTreeSet<_>>();
        if versions.len() != group.len() || group.iter().any(|(index, _)| preserved[*index]) {
            for (index, _) in group {
                let id = events[*index].id.0.clone();
                let value = metadata.entry(id.clone()).or_insert_with(|| json!({}));
                value
                    .as_object_mut()
                    .ok_or_else(|| invalid_object("runtime_event_meta", &id))?
                    .insert("preserveProgress".to_owned(), Value::Bool(true));
                metadata_updates.insert(id, value.clone());
            }
            continue;
        }
        if group.len() < 2 {
            continue;
        }
        let latest = group
            .iter()
            .max_by_key(|(_, version)| *version)
            .map(|(index, _)| *index)
            .unwrap_or_default();
        let latest_id = events[latest].id.0.clone();
        let previous = group.iter().try_fold(0_u64, |sum, (index, _)| {
            let count = metadata
                .get(&events[*index].id.0)
                .and_then(|meta| meta.get("earlierProgressUpdates"));
            match count {
                None | Some(Value::Null) => Some(sum),
                Some(Value::Number(number)) => {
                    number.as_u64().and_then(|count| sum.checked_add(count))
                }
                _ => None,
            }
        });
        let Some(count) = previous.and_then(|count| count.checked_add((group.len() - 1) as u64))
        else {
            continue;
        };
        for (index, _) in group {
            let id = events[*index].id.0.clone();
            if id != latest_id {
                retired.insert(id, format!("Superseded by progress event {latest_id}"));
            }
        }
        let value = metadata
            .entry(latest_id.clone())
            .or_insert_with(|| json!({}));
        value
            .as_object_mut()
            .ok_or_else(|| invalid_object("runtime_event_meta", &latest_id))?
            .insert("earlierProgressUpdates".to_owned(), json!(count));
        metadata_updates.insert(latest_id, value.clone());
    }
    let mut prepared_events = Vec::new();
    for mut event in events {
        if retired.contains_key(&event.id.0) || cancelled.contains_key(&event.id.0) {
            continue;
        }
        if let Some(count) = metadata
            .get(&event.id.0)
            .and_then(|value| value.get("earlierProgressUpdates"))
            .filter(|count| truthy(count))
            && let Ok(Value::Object(mut value)) = serde_json::from_str(&event.text)
        {
            value.insert("earlierProgressUpdates".to_owned(), count.clone());
            value.insert(
                "history".to_owned(),
                json!("Read earlier progress with orchestration_chat_read in this room."),
            );
            event.text = Value::Object(value).to_string();
        }
        prepared_events.push(event);
    }
    Ok(PreparedBatch {
        events: prepared_events,
        metadata,
        metadata_updates,
        retired,
        cancelled,
    })
}

fn apply_preparation(transaction: &Transaction<'_>, prepared: &PreparedBatch) -> StoreResult<()> {
    let changes = prepared
        .retired
        .iter()
        .map(|(id, reason)| (id, "stored_only", reason))
        .chain(
            prepared
                .cancelled
                .iter()
                .map(|(id, reason)| (id, "cancelled", reason)),
        )
        .map(|(id, status, reason)| (id.clone(), json!({"status":status,"error":reason})))
        .collect::<BTreeMap<_, _>>();
    if !changes.is_empty() {
        let payload = serde_json::to_string(&changes)?;
        let mut statement = transaction.prepare(
            "UPDATE runtime_events SET status=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.status'), \
             error=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.error') \
             WHERE id IN (SELECT key FROM json_each(?1)) AND status='pending' RETURNING id",
        )?;
        let changed = statement
            .query_map([payload], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        if changed.len() != changes.len() || changes.keys().any(|id| !changed.contains(id)) {
            return Err(StoreError::SchemaMismatch(
                "scheduler event changed during pending preparation".to_owned(),
            ));
        }
    }
    if !prepared.metadata_updates.is_empty() {
        if !table_exists(transaction, "runtime_event_meta")? {
            return Err(StoreError::SchemaMismatch(
                "scheduler progress preparation requires runtime_event_meta".to_owned(),
            ));
        }
        for (id, value) in &prepared.metadata_updates {
            transaction.execute(
                "INSERT OR REPLACE INTO runtime_event_meta(id,record) VALUES(?1,?2)",
                params![id, value.to_string()],
            )?;
        }
    }
    Ok(())
}

fn work_review_ids(event: &QueueEvent) -> Option<(String, String)> {
    let value: Value = serde_json::from_str(&event.text).ok()?;
    Some((
        value.get("task")?.as_str()?.to_owned(),
        value.get("result")?.get("id")?.as_str()?.to_owned(),
    ))
}

fn obsolete_work_review_ids(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    events: &[QueueEvent],
) -> StoreResult<BTreeSet<String>> {
    if !table_exists(transaction, "runtime_work")? {
        return Ok(BTreeSet::new());
    }
    let ids = serde_json::to_string(
        &events
            .iter()
            .map(|event| event.id.0.as_str())
            .collect::<Vec<_>>(),
    )?;
    let mut statement = transaction.prepare(
        "SELECT e.id FROM runtime_events e \
         JOIN json_each(?1) selected ON selected.value=e.id \
         WHERE e.kind='work_review' AND json_valid(e.text) \
         AND EXISTS(SELECT 1 FROM runtime_work w \
             WHERE w.id=json_extract(e.text,'$.task') \
             AND json_extract(w.record,'$.rootId')=?2 \
             AND EXISTS(SELECT 1 FROM json_each(w.record,'$.results') result \
                 WHERE json_extract(result.value,'$.id')=json_extract(e.text,'$.result.id')) \
             AND EXISTS(SELECT 1 FROM json_each(w.record,'$.decisions') decision \
                 WHERE json_extract(decision.value,'$.resultId')=json_extract(e.text,'$.result.id')))"
    )?;
    Ok(statement
        .query_map(params![ids, agent.id.0], |row| row.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?)
}

fn event_policy_dependency(kind: &str) -> Option<&'static str> {
    match kind {
        "agent_message" | "complaint_response" | "followup" => Some("team-isolation-policy"),
        "radio" | "radio_turn" => Some("radio-policy"),
        _ => None,
    }
}

fn set_waiting(
    transaction: &Transaction<'_>,
    agent: &mut AgentRecord,
    agent_id: &str,
) -> StoreResult<()> {
    agent.status = AgentStatus::Waiting;
    AgentRepository::put(transaction, agent)?;
    EventWriteRepository::set_agent_sync_lifecycle(transaction, agent_id, "waiting", false, None)
}

fn select_turn_batch(
    events: &[QueueEvent],
    metadata: &BTreeMap<String, Value>,
    limit: usize,
) -> Vec<QueueEvent> {
    let mut selected = Vec::new();
    let mut has_composer_input = false;
    for event in events.iter().take(limit.min(MAX_EVENTS_PER_TURN)) {
        if !matches!(event.kind.as_str(), "user" | "complaint") {
            break;
        }
        let meta = metadata.get(&event.id.0);
        let composer = event.kind == "user"
            && (meta
                .and_then(|value| value.get("delivery"))
                .and_then(Value::as_str)
                == Some("after_turn")
                || meta
                    .and_then(|value| value.get("sendNow"))
                    .is_some_and(truthy));
        if composer && has_composer_input {
            break;
        }
        has_composer_input |= composer;
        selected.push(event.clone());
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
#[path = "../../tests/support/scheduler_repository.rs"]
mod tests;
