//! Durable pending-event selection and claim primitives for the scheduler.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::records::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, EventMetadataRecord, Table,
};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{EventWriteRepository, QueueEvent, QueueEventId, StoreError, StoreResult};

const MAX_EVENTS_PER_TURN: usize = 32;
const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_events",
        columns: &[
            "id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error",
        ],
    },
    TableRequirement {
        name: "runtime_event_meta",
        columns: &["id", "record"],
    },
];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: TABLES,
    conditional_tables: &[],
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

#[derive(Clone, Copy, Debug, Default)]
pub struct SchedulerRepository;

impl SchedulerRepository {
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
        for event in &events {
            if let Some(record) = Table::<EventMetadataRecord>::new(transaction).get(&event.id)? {
                let value = record.value().clone();
                metadata.insert(event.id.0.clone(), value);
            }
        }

        if events
            .iter()
            .any(|event| requires_event_policy(&event.kind))
            || metadata
                .values()
                .any(|value| value.get("radioAnswerTurnId").is_some())
        {
            return Ok(None);
        }
        let selected = select_turn_batch(&agent, events, &metadata, limit);
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
            "SELECT e.id,e.agent,e.epoch FROM runtime_events e \
             JOIN runtime_agents a ON a.id=e.agent WHERE e.status='reserved' ORDER BY e.rowid",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut candidates = BTreeMap::<(String, i64), Vec<String>>::new();
        for (event_id, agent_id, epoch) in rows {
            let candidate = candidates.entry((agent_id, epoch)).or_default();
            candidate.push(event_id);
        }

        let mut recovered = 0;
        for ((agent_id, epoch), reserved_ids) in candidates {
            let mut agent = AgentRepository::get(transaction, &AgentId::from(agent_id.as_str()))?
                .ok_or_else(|| invalid_agent(&agent_id))?;
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

    pub fn has_unsubmitted_claim(
        transaction: &Transaction<'_>,
        agent: &str,
        attempt_id: &str,
    ) -> StoreResult<bool> {
        let Some(record) = AgentRepository::get(transaction, &AgentId::from(agent))? else {
            return Ok(false);
        };
        Ok(record
            .extra
            .get("startAttempt")
            .and_then(Value::as_object)
            .is_some_and(|attempt| {
                attempt.get("id").and_then(Value::as_str) == Some(attempt_id)
                    && attempt.get("submitted").is_some_and(|value| {
                        value.as_bool() == Some(false) || value.as_i64() == Some(0)
                    })
            }))
    }
}

fn requires_event_policy(kind: &str) -> bool {
    matches!(
        kind,
        "agent_message"
            | "chat_review"
            | "complaint"
            | "complaint_response"
            | "followup"
            | "radio_turn"
            | "work_review"
    )
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
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
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
    fn unsupported_event_policy_kind_is_not_claimed_or_changed() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_events SET kind='agent_message' WHERE id='first'",
                [],
            )
            .expect("sensitive event writes");
        let before: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |row| row.get(0),
            )
            .expect("agent snapshot reads");
        let transaction = connection
            .transaction()
            .expect("refusal transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 32)
                .expect("unsupported policy kind is refused")
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
        assert_eq!(after, before);
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
