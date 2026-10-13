//! Durable pending-event selection and claim primitives for the scheduler.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

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
    /// Set when persisted inputs require a scheduler policy that Rust does not yet provide.
    pub refusal_reason: Option<String>,
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

        let (complaints, work) = load_policy_records(transaction, &events)?;
        let mut prepared = prepare_pending_batch(&agent, events, metadata, &complaints);
        apply_preparation(transaction, &prepared)?;

        if prepared.events.is_empty() {
            set_waiting(transaction, &mut agent, agent_id)?;
            return Ok(None);
        }

        let mut events = select_dispatch_batch(
            &agent,
            prepared.events.clone(),
            &prepared.metadata,
            limit,
            attempt_created,
        );
        if events.is_empty() {
            return Ok(None);
        }

        loop {
            let obsolete_reviews = events
                .iter()
                .filter(|event| {
                    event.kind == "work_review" && work_review_is_obsolete(&agent, event, &work)
                })
                .map(|event| event.id.0.clone())
                .collect::<BTreeSet<_>>();
            if obsolete_reviews.is_empty() {
                break;
            }
            for id in &obsolete_reviews {
                let changed = transaction.execute(
                    "UPDATE runtime_events SET status='stored_only',error=?1 \
                     WHERE id=?2 AND agent=?3 AND epoch=?4 AND status='pending'",
                    params![
                        "The exact result already has a decision",
                        id,
                        agent_id,
                        epoch
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::SchemaMismatch(
                        "scheduler work review changed during pending reconciliation".to_owned(),
                    ));
                }
            }
            prepared
                .events
                .retain(|event| !obsolete_reviews.contains(&event.id.0));
            events = select_dispatch_batch(
                &agent,
                prepared.events.clone(),
                &prepared.metadata,
                limit,
                attempt_created,
            );
            if events.is_empty() {
                let pending = transaction
                    .query_row(
                        "SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending' LIMIT 1",
                        params![agent_id, epoch],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !pending {
                    set_waiting(transaction, &mut agent, agent_id)?;
                }
                return Ok(None);
            }
        }

        let policy_refusal = events
            .iter()
            .find_map(|event| {
                event_policy_dependency(&event.kind)
                    .map(|dependency| (event.kind.clone(), dependency))
            })
            .or_else(|| {
                events.iter().find_map(|event| {
                    prepared
                        .metadata
                        .get(&event.id.0)
                        .and_then(|value| value.get("radioAnswerTurnId"))
                        .map(|_| (event.kind.clone(), "radio-policy"))
                })
            });
        if let Some((kind, dependency)) = policy_refusal {
            return Ok(Some(ClaimedEvents {
                agent,
                events: Vec::new(),
                metadata: prepared.metadata,
                refusal_reason: Some(format!(
                    "scheduler cannot claim {kind}: requires {dependency}"
                )),
            }));
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
            metadata: prepared.metadata,
            refusal_reason: None,
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

struct PreparedBatch {
    events: Vec<QueueEvent>,
    metadata: BTreeMap<String, Value>,
    metadata_updates: BTreeMap<String, Value>,
    retired: BTreeMap<String, String>,
    cancelled: BTreeMap<String, String>,
}

fn load_policy_records(
    transaction: &Transaction<'_>,
    events: &[QueueEvent],
) -> StoreResult<(BTreeMap<String, Value>, BTreeMap<String, Value>)> {
    let mut complaints = BTreeMap::new();
    let mut work = BTreeMap::new();
    let has_complaints = events.iter().any(|event| event.kind == "complaint")
        && table_exists(transaction, "runtime_complaints")?;
    let has_work = events.iter().any(|event| event.kind == "work_review")
        && table_exists(transaction, "runtime_work")?;

    for event in events {
        if event.kind == "complaint"
            && has_complaints
            && let Some(ids) = complaint_ids(&event.text)
        {
            for id in ids {
                if complaints.contains_key(&id) {
                    continue;
                }
                if let Some(value) = read_json_record(transaction, "runtime_complaints", &id)? {
                    complaints.insert(id, value);
                }
            }
        }
        if event.kind == "work_review"
            && has_work
            && let Some((task_id, _)) = work_review_ids(event)
            && !work.contains_key(&task_id)
            && let Some(value) = read_json_record(transaction, "runtime_work", &task_id)?
        {
            work.insert(task_id, value);
        }
    }
    Ok((complaints, work))
}

fn table_exists(transaction: &Transaction<'_>, table: &str) -> StoreResult<bool> {
    Ok(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn read_json_record(
    transaction: &Transaction<'_>,
    table: &str,
    id: &str,
) -> StoreResult<Option<Value>> {
    let sql = match table {
        "runtime_complaints" => "SELECT record FROM runtime_complaints WHERE id=?1",
        "runtime_work" => "SELECT record FROM runtime_work WHERE id=?1",
        _ => return Ok(None),
    };
    let raw = transaction
        .query_row(sql, [id], |row| row.get::<_, String>(0))
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&raw).map_err(|_| StoreError::InvalidSyncObject {
        scope: table.to_owned(),
        id: id.to_owned(),
    })?;
    if !value.is_object() {
        return Err(StoreError::InvalidSyncObject {
            scope: table.to_owned(),
            id: id.to_owned(),
        });
    }
    Ok(Some(value))
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
            entry
                .get("complaint_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

fn complaint_needs_response(complaint: &Value) -> Option<bool> {
    let status = complaint.get("status")?.as_str()?;
    if matches!(status, "resolved" | "declined") {
        return Some(false);
    }
    if complaint.get("sourceType").and_then(Value::as_str) == Some("user_task") && status == "open"
    {
        return Some(true);
    }
    let responsible = if complaint.get("recipient")?.as_str()? == "user" {
        "user"
    } else {
        complaint.get("leadId")?.as_str()?
    };
    let responses = complaint.get("responses")?.as_array()?;
    Some(
        !responses
            .iter()
            .any(|response| response.get("author").and_then(Value::as_str) == Some(responsible)),
    )
}

fn complaints_resolved(
    agent: &AgentRecord,
    event: &QueueEvent,
    records: &BTreeMap<String, Value>,
) -> bool {
    complaint_ids(&event.text).is_some_and(|ids| {
        ids.iter().all(|id| {
            records.get(id).is_some_and(|complaint| {
                complaint.get("leadId").and_then(Value::as_str) == Some(agent.id.0.as_str())
                    && complaint_needs_response(complaint) == Some(false)
            })
        })
    })
}

fn progress_update(text: &str) -> Option<((String, String, String), u64)> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("importance")?.as_str()? != "progress" {
        return None;
    }
    let sender = value.get("sender")?.as_str()?.to_owned();
    let room = value.get("room")?.as_str()?.to_owned();
    let key = value.get("progress_key")?.as_str()?.to_owned();
    if sender.is_empty() || room.is_empty() || key.is_empty() {
        return None;
    }
    let version = value.get("progress_version")?.as_u64().or_else(|| {
        value
            .get("progress_version")?
            .as_i64()
            .and_then(|v| u64::try_from(v).ok())
    })?;
    Some(((sender, room, key), version))
}

fn prepare_pending_batch(
    agent: &AgentRecord,
    events: Vec<QueueEvent>,
    metadata: BTreeMap<String, Value>,
    complaints: &BTreeMap<String, Value>,
) -> PreparedBatch {
    let mut metadata = metadata;
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

    let mut groups = BTreeMap::<(String, String, String), Vec<(usize, u64)>>::new();
    let mut preserve = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let saved = metadata
            .get(&event.id.0)
            .and_then(|value| value.get("preserveProgress"))
            .is_some_and(truthy);
        preserve.push(saved);
        if event.kind == "agent_message"
            && let Some((key, version)) = progress_update(&event.text)
        {
            groups.entry(key).or_default().push((index, version));
        }
    }

    for group in groups.values() {
        let duplicate_version = group
            .iter()
            .map(|(_, version)| *version)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != group.len();
        if duplicate_version || group.iter().any(|(index, _)| preserve[*index]) {
            for (index, _) in group {
                let id = events[*index].id.0.clone();
                let value = metadata.entry(id.clone()).or_insert_with(|| json!({}));
                if let Some(object) = value.as_object_mut() {
                    object.insert("preserveProgress".to_owned(), Value::Bool(true));
                    metadata_updates.insert(id, value.clone());
                }
            }
            continue;
        }
        if group.len() < 2 {
            continue;
        }
        let Some((latest_index, _)) = group.iter().max_by_key(|(_, version)| *version) else {
            continue;
        };
        let latest_id = events[*latest_index].id.0.clone();
        let previous_count = group.iter().try_fold(0_u64, |sum, (index, _)| {
            match metadata
                .get(&events[*index].id.0)
                .and_then(|value| value.get("earlierProgressUpdates"))
            {
                None | Some(Value::Null) => Some(sum),
                Some(Value::Number(number)) => number
                    .as_u64()
                    .or_else(|| number.as_i64().and_then(|value| u64::try_from(value).ok()))
                    .and_then(|value| sum.checked_add(value)),
                _ => None,
            }
        });
        let Some(previous_count) = previous_count else {
            for (index, _) in group {
                let id = events[*index].id.0.clone();
                let value = metadata.entry(id.clone()).or_insert_with(|| json!({}));
                if let Some(object) = value.as_object_mut() {
                    object.insert("preserveProgress".to_owned(), Value::Bool(true));
                    metadata_updates.insert(id, value.clone());
                }
            }
            continue;
        };
        let Some(count) = previous_count.checked_add((group.len() - 1) as u64) else {
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
        if let Some(object) = value.as_object_mut() {
            object.insert("earlierProgressUpdates".to_owned(), json!(count));
            metadata_updates.insert(latest_id, value.clone());
        }
    }

    let prepared_events = events
        .into_iter()
        .filter_map(|mut event| {
            if retired.contains_key(&event.id.0) || cancelled.contains_key(&event.id.0) {
                return None;
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
                    Value::String(
                        "Read earlier progress with orchestration_chat_read in this room."
                            .to_owned(),
                    ),
                );
                event.text = Value::Object(value).to_string();
            }
            Some(event)
        })
        .collect();

    PreparedBatch {
        events: prepared_events,
        metadata,
        metadata_updates,
        retired,
        cancelled,
    }
}

fn apply_preparation(transaction: &Transaction<'_>, prepared: &PreparedBatch) -> StoreResult<()> {
    for (id, reason) in &prepared.retired {
        let changed = transaction.execute(
            "UPDATE runtime_events SET status='stored_only',error=?1 WHERE id=?2 AND status='pending'",
            params![reason, id],
        )?;
        if changed != 1 {
            return Err(StoreError::SchemaMismatch(
                "scheduler event changed during pending preparation".to_owned(),
            ));
        }
    }
    for (id, reason) in &prepared.cancelled {
        let changed = transaction.execute(
            "UPDATE runtime_events SET status='cancelled',error=?1 WHERE id=?2 AND status='pending'",
            params![reason, id],
        )?;
        if changed != 1 {
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
                params![id, serde_json::to_string(value)?],
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

fn work_review_is_obsolete(
    agent: &AgentRecord,
    event: &QueueEvent,
    work: &BTreeMap<String, Value>,
) -> bool {
    let Some((task_id, result_id)) = work_review_ids(event) else {
        return false;
    };
    work.get(&task_id).is_some_and(|record| {
        record.get("rootId").and_then(Value::as_str) == Some(agent.id.0.as_str())
            && record
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| {
                    results.iter().any(|result| {
                        result.get("id").and_then(Value::as_str) == Some(result_id.as_str())
                    })
                })
            && record
                .get("decisions")
                .and_then(Value::as_array)
                .is_some_and(|decisions| {
                    decisions.iter().any(|decision| {
                        decision.get("resultId").and_then(Value::as_str) == Some(result_id.as_str())
                    })
                })
    })
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

fn select_dispatch_batch(
    agent: &AgentRecord,
    events: Vec<QueueEvent>,
    metadata: &BTreeMap<String, Value>,
    limit: usize,
    now: f64,
) -> Vec<QueueEvent> {
    let maximum = limit.min(MAX_EVENTS_PER_TURN);
    let mut page = events.iter().take(maximum).cloned().collect::<Vec<_>>();
    if progress_only(&page) {
        if let Some(urgent) = events
            .iter()
            .skip(maximum)
            .find(|event| !is_progress(event))
        {
            page = std::iter::once(urgent.clone())
                .chain(page.into_iter().take(maximum.saturating_sub(1)))
                .collect();
        } else if let Some(oldest) = page.iter().map(|event| event.created).reduce(f64::min)
            && now - oldest < 1.0
        {
            return Vec::new();
        }
    }
    select_turn_batch(agent, page, metadata, maximum)
}

fn progress_only(events: &[QueueEvent]) -> bool {
    !events.is_empty() && events.iter().all(is_progress)
}

fn is_progress(event: &QueueEvent) -> bool {
    if event.kind != "agent_message" {
        return false;
    }
    serde_json::from_str::<Value>(&event.text)
        .ok()
        .and_then(|value| {
            value
                .get("importance")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|importance| importance == "progress")
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
    use super::{
        SchedulerRepository, prepare_pending_batch, select_dispatch_batch, select_turn_batch,
    };
    use crate::records::{AgentId, AgentRecord, AgentRepository, AgentStatus};
    use crate::{QueueEvent, QueueEventId};
    use rusqlite::Connection;
    use serde_json::{Value, json};
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
    fn protected_event_kinds_fail_closed_with_named_policy_and_no_claim() {
        for (kind, policy) in [
            ("agent_message", "team-isolation-policy"),
            ("complaint_response", "team-isolation-policy"),
            ("followup", "team-isolation-policy"),
            ("radio", "radio-policy"),
            ("radio_turn", "radio-policy"),
        ] {
            let mut connection = database();
            connection
                .execute("UPDATE runtime_events SET kind=?1 WHERE id='first'", [kind])
                .expect("sensitive event writes");
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
            let refused = SchedulerRepository::claim(&transaction, "agent", 4, "attempt", 10.0, 32)
                .expect("unsupported policy kind returns a named refusal")
                .expect("protected event returns a refusal receipt");
            assert!(refused.events.is_empty());
            assert_eq!(
                refused.refusal_reason.as_deref(),
                Some(format!("scheduler cannot claim {kind}: requires {policy}").as_str())
            );
            transaction.commit().expect("refusal commits");
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
            assert_eq!(after_agent, before_agent, "agent changed for {kind}");
            assert_eq!(after_events, before_events, "events changed for {kind}");
        }
    }

    #[test]
    fn removed_chat_reviews_are_cancelled_with_python_reason() {
        let mut connection = database();
        connection
            .execute_batch(
                "UPDATE runtime_events SET kind='chat_review' WHERE id='first';
                 DELETE FROM runtime_events WHERE id!='first';",
            )
            .expect("removed review event writes");
        let transaction = connection.transaction().expect("claim transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "review", 10.0, 32)
                .expect("removed review is reconciled")
                .is_none()
        );
        transaction.commit().expect("review retirement commits");
        let (status, error): (String, String) = connection
            .query_row(
                "SELECT status,error FROM runtime_events WHERE id='first'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("retired review state reads");
        assert_eq!(status, "cancelled");
        assert_eq!(error, "Scheduled chat reviews have been removed");
    }

    #[test]
    fn complaint_preparation_retires_resolved_and_claims_unresolved_records() {
        let mut resolved = database();
        resolved
            .execute_batch(
                "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 DELETE FROM runtime_events;
                 INSERT INTO runtime_complaints VALUES(
                   'settled','{\"leadId\":\"agent\",\"status\":\"resolved\",\"responses\":[]}');
                 INSERT INTO runtime_events VALUES(
                   'settled-notice','agent','complaint',
                   '{\"complaints\":[{\"complaint_id\":\"settled\"}]}',
                   'pending',1.0,4,NULL,NULL);",
            )
            .expect("resolved complaint fixture writes");
        let transaction = resolved.transaction().expect("claim transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "resolved", 10.0, 32)
                .expect("resolved complaint reconciles")
                .is_none()
        );
        transaction.commit().expect("resolved complaint commits");
        let (status, error): (String, String) = resolved
            .query_row(
                "SELECT status,error FROM runtime_events WHERE id='settled-notice'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("resolved notice state reads");
        assert_eq!(status, "stored_only");
        assert_eq!(error, "Every referenced complaint already has a response");

        let mut unresolved = database();
        unresolved
            .execute_batch(
                "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 DELETE FROM runtime_events;
                 INSERT INTO runtime_complaints VALUES(
                   'open','{\"leadId\":\"agent\",\"status\":\"open\",\"recipient\":\"lead\",\"responses\":[]}');
                 INSERT INTO runtime_events VALUES(
                   'open-notice','agent','complaint',
                   '{\"complaints\":[{\"complaint_id\":\"open\"}]}',
                   'pending',1.0,4,NULL,NULL);",
            )
            .expect("unresolved complaint fixture writes");
        let transaction = unresolved.transaction().expect("claim transaction begins");
        let claimed = SchedulerRepository::claim(&transaction, "agent", 4, "open", 10.0, 32)
            .expect("unresolved complaint claim executes")
            .expect("unresolved complaint remains actionable");
        assert_eq!(claimed.events[0].id.0, "open-notice");
        assert_eq!(claimed.events[0].kind, "complaint");
        assert_eq!(claimed.refusal_reason, None);
        transaction
            .commit()
            .expect("unresolved complaint claim commits");
    }

    #[test]
    fn work_review_with_exact_existing_decision_is_retired() {
        let mut connection = database();
        connection
            .execute_batch(
                "CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 DELETE FROM runtime_events;
                 INSERT INTO runtime_work VALUES(
                   'task','{\"rootId\":\"agent\",\"results\":[{\"id\":\"result\"}],\"decisions\":[{\"resultId\":\"result\"}]}');
                 INSERT INTO runtime_events VALUES(
                   'review','agent','work_review',
                   '{\"task\":\"task\",\"result\":{\"id\":\"result\"}}',
                   'pending',1.0,4,NULL,NULL);",
            )
            .expect("work review fixture writes");
        let transaction = connection.transaction().expect("claim transaction begins");
        assert!(
            SchedulerRepository::claim(&transaction, "agent", 4, "review", 10.0, 32)
                .expect("obsolete work review reconciles")
                .is_none()
        );
        transaction
            .commit()
            .expect("work review retirement commits");
        let (status, error): (String, String) = connection
            .query_row(
                "SELECT status,error FROM runtime_events WHERE id='review'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("work review state reads");
        assert_eq!(status, "stored_only");
        assert_eq!(error, "The exact result already has a decision");
    }

    #[test]
    fn obsolete_work_review_does_not_stall_other_pending_events() {
        let mut connection = database();
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("work table creates");
        connection
            .execute(
                "UPDATE runtime_events SET kind='work_review',text=?1 WHERE id='first'",
                [r#"{"task":"task","result":{"id":"result"}}"#],
            )
            .expect("obsolete work review event writes");
        connection
            .execute(
                "INSERT INTO runtime_work VALUES('task',?1)",
                [r#"{"rootId":"agent","results":[{"id":"result"}],"decisions":[{"resultId":"result"}]}"#],
            )
            .expect("obsolete work record writes");
        let transaction = connection.transaction().expect("claim transaction begins");
        let claimed = SchedulerRepository::claim(&transaction, "agent", 4, "review", 10.0, 32)
            .expect("obsolete review is retired before claim")
            .expect("remaining pending events continue in this claim");
        assert_eq!(
            claimed
                .events
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>(),
            ["tie", "later"]
        );
        transaction.commit().expect("remaining claim commits");
        let (status, error): (String, String) = connection
            .query_row(
                "SELECT status,error FROM runtime_events WHERE id='first'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("obsolete review state reads");
        assert_eq!(status, "stored_only");
        assert_eq!(error, "The exact result already has a decision");
    }

    #[test]
    fn progress_preparation_persists_fold_before_team_policy_refusal() {
        let mut connection = database();
        connection
            .execute_batch(
                "DELETE FROM runtime_events;
                 INSERT INTO runtime_events VALUES
                   ('progress-old','agent','agent_message',
                    '{\"importance\":\"progress\",\"sender\":\"worker\",\"room\":\"team\",\"progress_key\":\"build\",\"progress_version\":1}',
                    'pending',1.0,4,NULL,NULL),
                   ('progress-latest','agent','agent_message',
                    '{\"importance\":\"progress\",\"sender\":\"worker\",\"room\":\"team\",\"progress_key\":\"build\",\"progress_version\":2}',
                    'pending',2.0,4,NULL,NULL);",
            )
            .expect("progress fixture writes");
        let transaction = connection.transaction().expect("claim transaction begins");
        let refused = SchedulerRepository::claim(&transaction, "agent", 4, "progress", 10.0, 32)
            .expect("progress is refused until team isolation exists")
            .expect("protected progress returns a refusal receipt");
        assert_eq!(
            refused.refusal_reason.as_deref(),
            Some("scheduler cannot claim agent_message: requires team-isolation-policy")
        );
        transaction.commit().expect("progress preparation commits");
        let (status, error): (String, String) = connection
            .query_row(
                "SELECT status,error FROM runtime_events WHERE id='progress-old'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("folded progress state reads");
        assert_eq!(status, "stored_only");
        assert_eq!(error, "Superseded by progress event progress-latest");
        let latest: String = connection
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id='progress-latest'",
                [],
                |row| row.get(0),
            )
            .expect("latest progress metadata reads");
        let latest: Value = serde_json::from_str(&latest).expect("metadata remains JSON");
        assert_eq!(latest["earlierProgressUpdates"], 1);
    }

    #[test]
    fn pending_preparation_folds_unique_progress_and_preserves_duplicate_versions() {
        let agent = AgentRecord {
            id: AgentId::from("agent"),
            root_id: AgentId::from("agent"),
            epoch: 4,
            status: AgentStatus::Queued,
            extra: BTreeMap::new(),
        };
        let events = vec![
            selected_event(
                "progress-1",
                "agent_message",
                r#"{"importance":"progress","sender":"worker","room":"team","progress_key":"build","progress_version":1}"#,
            ),
            selected_event(
                "progress-2",
                "agent_message",
                r#"{"importance":"progress","sender":"worker","room":"team","progress_key":"build","progress_version":2}"#,
            ),
            selected_event(
                "duplicate-a",
                "agent_message",
                r#"{"importance":"progress","sender":"worker","room":"team","progress_key":"test","progress_version":1}"#,
            ),
            selected_event(
                "duplicate-b",
                "agent_message",
                r#"{"importance":"progress","sender":"worker","room":"team","progress_key":"test","progress_version":1}"#,
            ),
        ];
        let prepared = prepare_pending_batch(&agent, events, BTreeMap::new(), &BTreeMap::new());
        assert_eq!(
            prepared
                .events
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>(),
            ["progress-2", "duplicate-a", "duplicate-b"]
        );
        assert_eq!(
            prepared.retired.get("progress-1").map(String::as_str),
            Some("Superseded by progress event progress-2")
        );
        assert_eq!(prepared.metadata["progress-2"]["earlierProgressUpdates"], 1);
        assert_eq!(prepared.metadata["duplicate-a"]["preserveProgress"], true);
        assert_eq!(prepared.metadata["duplicate-b"]["preserveProgress"], true);
        let projected: Value = serde_json::from_str(&prepared.events[0].text)
            .expect("folded model projection remains JSON");
        assert_eq!(projected["earlierProgressUpdates"], 1);
        assert_eq!(
            projected["history"],
            "Read earlier progress with orchestration_chat_read in this room."
        );
    }

    #[test]
    fn dispatch_boundary_waits_for_old_progress_and_moves_urgent_event_ahead() {
        let agent = AgentRecord {
            id: AgentId::from("agent"),
            root_id: AgentId::from("agent"),
            epoch: 4,
            status: AgentStatus::Queued,
            extra: BTreeMap::new(),
        };
        let progress = |id: String, created| QueueEvent {
            id: QueueEventId::from(id),
            agent: AgentId::from("agent"),
            kind: "agent_message".to_owned(),
            text: r#"{"importance":"progress"}"#.to_owned(),
            status: "pending".to_owned(),
            created,
            epoch: 4,
            turn_id: None,
            error: None,
        };
        let mut events = (0..32)
            .map(|index| progress(format!("progress-{index}"), 9.5))
            .collect::<Vec<_>>();
        assert!(
            select_dispatch_batch(&agent, events.clone(), &BTreeMap::new(), 32, 10.0).is_empty()
        );
        events[0].created = 8.0;
        let selected = select_dispatch_batch(&agent, events, &BTreeMap::new(), 32, 10.0);
        assert_eq!(selected.len(), 32);

        let mut recent = (0..32)
            .map(|index| progress(format!("recent-{index}"), 9.5))
            .collect::<Vec<_>>();
        recent.push(selected_event("urgent", "user", "urgent"));
        let selected = select_dispatch_batch(&agent, recent, &BTreeMap::new(), 32, 10.0);
        assert_eq!(selected.len(), 32);
        assert_eq!(selected[0].id.0, "urgent");
        assert_eq!(selected[1].id.0, "recent-0");
    }

    #[test]
    fn work_review_obsolescence_requires_exact_root_result_and_decision() {
        let agent = AgentRecord {
            id: AgentId::from("agent"),
            root_id: AgentId::from("root"),
            epoch: 4,
            status: AgentStatus::Queued,
            extra: BTreeMap::new(),
        };
        let event = selected_event(
            "review",
            "work_review",
            r#"{"task":"task","result":{"id":"result"}}"#,
        );
        let record = json!({
            "rootId":"agent",
            "results":[{"id":"result"}],
            "decisions":[{"resultId":"result"}]
        });
        assert!(super::work_review_is_obsolete(
            &agent,
            &event,
            &BTreeMap::from([("task".to_owned(), record.clone())])
        ));
        let mut wrong_root = record.clone();
        wrong_root["rootId"] = json!("other");
        assert!(!super::work_review_is_obsolete(
            &agent,
            &event,
            &BTreeMap::from([("task".to_owned(), wrong_root)])
        ));
        let mut no_decision = record;
        no_decision["decisions"] = json!([]);
        assert!(!super::work_review_is_obsolete(
            &agent,
            &event,
            &BTreeMap::from([("task".to_owned(), no_decision)])
        ));
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
