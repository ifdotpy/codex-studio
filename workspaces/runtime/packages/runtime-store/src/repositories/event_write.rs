//! Atomic persistence primitives for plan and annotation requests.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use super::sync_documents::{hash_payload, next_sequence};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{AgentId, AgentRepository, QueueEvent, StoreError, StoreResult};

const PLAN_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_plans",
    columns: &["id", "record"],
}];
const ANNOTATION_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_annotations",
    columns: &["id", "record"],
}];
const RECEIPT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_operation_receipts",
    columns: &["id", "signature", "result"],
}];
const EVENT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_events",
    columns: &[
        "id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error",
    ],
}];
const EVENT_META_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_event_meta",
    columns: &["id", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_plans",
            tables: PLAN_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_annotations",
            tables: ANNOTATION_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_operation_receipts",
            tables: RECEIPT_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_events",
            tables: EVENT_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_event_meta",
            tables: EVENT_META_TABLE,
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
pub struct EventWriteReceipt {
    pub signature: String,
    pub result: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EventWriteRepository;

impl EventWriteRepository {
    pub fn plan(connection: &Connection, id: &str) -> StoreResult<Option<String>> {
        Ok(connection
            .query_row(
                "SELECT record FROM runtime_plans WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn put_plan(connection: &Connection, id: &str, record: &str) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_plans(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record],
        )?;
        Ok(())
    }

    pub fn put_annotation(connection: &Connection, id: &str, record: &str) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_annotations(id,record) VALUES (?1,?2)",
            params![id, record],
        )?;
        Ok(())
    }

    pub fn receipt(connection: &Connection, id: &str) -> StoreResult<Option<EventWriteReceipt>> {
        Ok(connection
            .query_row(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
                [id],
                |row| {
                    Ok(EventWriteReceipt {
                        signature: row.get(0)?,
                        result: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn put_receipt(
        connection: &Connection,
        id: &str,
        signature: &str,
        result: &str,
    ) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3)",
            params![id, signature, result],
        )?;
        Ok(())
    }

    pub fn event_exists(connection: &Connection, id: &str) -> StoreResult<bool> {
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn insert_event(connection: &Connection, event: &QueueEvent) -> StoreResult<bool> {
        let inserted = connection.execute(
            "INSERT OR IGNORE INTO runtime_events \
             (id,agent,kind,text,status,created,epoch,turn_id,error) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,NULL)",
            params![
                event.id.0,
                event.agent.0,
                event.kind,
                event.text,
                event.status,
                event.created,
                event.epoch
            ],
        )?;
        Ok(inserted != 0)
    }

    pub fn mark_event_enqueued(
        connection: &Connection,
        id: &str,
        monotonic_ns: u128,
    ) -> StoreResult<()> {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_event_meta')",
            [], |row| row.get(0),
        )?;
        if !exists {
            return Ok(());
        }
        let value = serde_json::json!({"timing":{"enqueuedAt":monotonic_ns}});
        connection.execute(
            "INSERT INTO runtime_event_meta(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.timing.enqueuedAt', \
             json_extract(excluded.record,'$.timing.enqueuedAt'))",
            params![id, value.to_string()],
        )?;
        Ok(())
    }

    /// Keep the renderer projection in step with the scheduler's queued state.
    pub fn set_agent_sync_status(
        transaction: &Transaction<'_>,
        agent_id: &str,
        status: &str,
    ) -> StoreResult<()> {
        Self::set_agent_sync_fields(transaction, agent_id, Some(status), None, None)
    }

    /// Keep renderer-visible lifecycle fields in step with a scheduler transition.
    pub fn set_agent_sync_lifecycle(
        transaction: &Transaction<'_>,
        agent_id: &str,
        status: &str,
        in_flight: bool,
        turn_id: Option<&str>,
    ) -> StoreResult<()> {
        Self::set_agent_sync_fields(
            transaction,
            agent_id,
            Some(status),
            Some((in_flight, turn_id)),
            None,
        )
    }

    fn set_agent_sync_fields(
        transaction: &Transaction<'_>,
        agent_id: &str,
        status: Option<&str>,
        lifecycle: Option<(bool, Option<&str>)>,
        name: Option<&str>,
    ) -> StoreResult<()> {
        let old = transaction
            .query_row(
                "SELECT payload,deleted FROM sync_entities WHERE collection='agent' AND id=?1",
                [agent_id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((Some(old_payload), 0)) = old else {
            return Err(StoreError::SchemaMismatch(
                "agent sync projection is missing or deleted".to_owned(),
            ));
        };
        let mut payload: Value = serde_json::from_str(&old_payload)?;
        if payload.get("collection").and_then(Value::as_str) != Some("agent")
            || payload.get("id").and_then(Value::as_str) != Some(agent_id)
        {
            return Err(StoreError::SchemaMismatch(
                "agent sync projection identity is invalid".to_owned(),
            ));
        }
        let before = payload.clone();
        {
            let value = payload
                .get_mut("value")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    StoreError::SchemaMismatch("agent sync projection is invalid".into())
                })?;
            if value.get("id").and_then(Value::as_str) != Some(agent_id) {
                return Err(StoreError::SchemaMismatch(
                    "agent sync projection identity is invalid".to_owned(),
                ));
            }
            if let Some(status) = status {
                value.insert("status".to_owned(), Value::String(status.to_owned()));
            }
            if let Some(name) = name {
                value.insert("name".into(), Value::String(name.into()));
                value.insert("manualName".into(), Value::Bool(true));
            }
            if let Some((in_flight, turn_id)) = lifecycle {
                value.insert("inFlight".to_owned(), Value::Bool(in_flight));
                value.insert(
                    "turnId".to_owned(),
                    turn_id.map_or(Value::Null, |id| Value::String(id.to_owned())),
                );
            }
            let has_runtime_agents = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents')",
                [],
                |row| row.get::<_, bool>(0),
            )?;
            if has_runtime_agents
                && let Some(agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))?
            {
                value.insert("epoch".to_owned(), Value::from(agent.epoch));
                for key in [
                    "turnEpoch",
                    "error",
                    "nativeFailureHold",
                    "activity",
                    "activeTools",
                    "startAttempt",
                    "turnId",
                    "inFlight",
                    "autoWake",
                    "lastCompletedTurn",
                    "lastCompletedTurnStatus",
                    "lastCompletedTurnError",
                    "startOutcomeHold",
                    "lastAnswer",
                    "disconnectRecovery",
                    "restartRecovery",
                    "nativeSafetyBuffering",
                    "nativeStatus",
                    "nativeTurnError",
                    "unhandledNotifications",
                    "lastEvent",
                    "tail",
                    "contextUsage",
                ] {
                    if let Some(field) = agent.extra.get(key) {
                        value.insert(key.to_owned(), field.clone());
                    } else {
                        value.remove(key);
                    }
                }
            }
        }
        {
            let value = payload
                .get_mut("value")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    StoreError::SchemaMismatch("agent sync projection is invalid".into())
                })?;
            if let Some(name) = name {
                value.insert("name".to_owned(), Value::String(name.to_owned()));
                value.insert("manualName".to_owned(), Value::Bool(true));
            }
            if status == Some("queued")
                && let Some(overview) = value.get_mut("overview").and_then(Value::as_object_mut)
            {
                overview.insert("result".into(), Value::String(String::new()));
                overview.insert("resultTruncated".into(), Value::Bool(false));
                overview.insert("resultTurnId".into(), Value::Null);
            }
        }
        if payload == before {
            return Ok(());
        }
        let payload = serde_json::to_string(&payload)?;
        let hash = hash_payload(&payload);
        let sequence = next_sequence(transaction)?;
        transaction.execute(
            "UPDATE sync_entities SET seq=?1,hash=?2,payload=?3 \
             WHERE collection='agent' AND id=?4 AND deleted=0",
            params![sequence, hash, payload, agent_id],
        )?;
        Ok(())
    }

    /// Update the renderer's persisted agent title after the agent record write.
    pub fn set_agent_sync_name(
        transaction: &Transaction<'_>,
        agent_id: &str,
        name: &str,
    ) -> StoreResult<()> {
        Self::set_agent_sync_fields(transaction, agent_id, None, None, Some(name))
    }
}

#[cfg(test)]
mod tests {
    use super::EventWriteRepository;
    use rusqlite::Connection;
    use serde_json::json;

    #[test]
    fn agent_projection_status_write_advances_its_sync_revision() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);
                 INSERT INTO sync_entities VALUES('agent','a',1,'old', '{\"collection\":\"agent\",\"id\":\"a\",\"value\":{\"id\":\"a\",\"status\":\"idle\",\"overview\":{\"result\":\"stale\",\"resultTruncated\":true,\"resultTurnId\":\"turn-old\",\"task\":\"keep\"}}}',0);",
            )
            .expect("projection fixture creates");
        let transaction = connection.transaction().expect("transaction begins");
        EventWriteRepository::set_agent_sync_status(&transaction, "a", "queued")
            .expect("agent projection updates");
        transaction.commit().expect("transaction commits");
        let (sequence, payload) = connection
            .query_row(
                "SELECT seq,payload FROM sync_entities WHERE collection='agent' AND id='a'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("updated projection exists");
        assert_eq!(sequence, 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload)
                .expect("updated projection remains JSON"),
            json!({
                "collection":"agent","id":"a","value":{"id":"a","status":"queued","overview":{"result":"","resultTruncated":false,"resultTurnId":null,"task":"keep"}}
            })
        );
    }

    #[test]
    fn agent_projection_lifecycle_write_updates_activity_and_turn_identity() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);
                 INSERT INTO sync_entities VALUES('agent','a',1,'old', '{\"collection\":\"agent\",\"id\":\"a\",\"value\":{\"id\":\"a\",\"status\":\"starting\",\"inFlight\":false,\"turnId\":null}}',0);",
            )
            .expect("projection fixture creates");
        connection
            .execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("runtime agent fixture creates");
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES('a',?1)",
                [
                    json!({"id":"a","rootId":"a","name":"Agent","status":"running","epoch":2,
                "inFlight":true,"turnId":"turn-1","lastAnswer":"",
                "disconnectRecovery":{"epoch":2,"turnId":"turn-1"},
                "restartRecovery":{"stage":"held"}})
                    .to_string(),
                ],
            )
            .expect("runtime lifecycle receipt persists");
        let transaction = connection.transaction().expect("transaction begins");
        EventWriteRepository::set_agent_sync_lifecycle(
            &transaction,
            "a",
            "running",
            true,
            Some("turn-1"),
        )
        .expect("lifecycle projection updates");
        transaction.commit().expect("transaction commits");
        let payload: String = connection
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id='a'",
                [],
                |row| row.get(0),
            )
            .expect("projection exists");
        let value = serde_json::from_str::<serde_json::Value>(&payload)
            .expect("projection remains JSON")["value"]
            .clone();
        assert_eq!(value["status"], "running");
        assert_eq!(value["inFlight"], true);
        assert_eq!(value["turnId"], "turn-1");
        assert_eq!(
            (
                &value["lastAnswer"],
                &value["disconnectRecovery"],
                &value["restartRecovery"]
            ),
            (
                &json!(""),
                &json!({"epoch":2,"turnId":"turn-1"}),
                &json!({"stage":"held"})
            )
        );
    }

    #[test]
    fn agent_projection_name_write_advances_revision_and_preserves_other_fields() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);
                 INSERT INTO sync_entities VALUES('agent','a',1,'old','{\"collection\":\"agent\",\"id\":\"a\",\"value\":{\"id\":\"a\",\"name\":\"Old\",\"manualName\":false,\"needsTitle\":true,\"role\":\"orchestrator\"}}',0);",
            )
            .expect("projection fixture creates");
        let transaction = connection.transaction().expect("transaction begins");
        EventWriteRepository::set_agent_sync_name(&transaction, "a", "New")
            .expect("agent title projection updates");
        transaction.commit().expect("transaction commits");
        let (sequence, hash, payload) = connection
            .query_row(
                "SELECT seq,hash,payload FROM sync_entities WHERE collection='agent' AND id='a'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .expect("updated projection exists");
        assert_eq!(sequence, 2);
        assert_eq!(hash, super::hash_payload(&payload));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload)
                .expect("updated projection remains JSON")["value"],
            json!({"id":"a","name":"New","manualName":true,"needsTitle":true,"role":"orchestrator"})
        );
    }

    #[test]
    fn enqueue_timing_is_written_when_metadata_table_exists() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
            )
            .expect("metadata table creates");
        EventWriteRepository::mark_event_enqueued(&connection, "e", 42).expect("timing writes");
        let record: String = connection
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id='e'",
                [],
                |row| row.get(0),
            )
            .expect("metadata exists");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&record).expect("metadata parses")["timing"]
                ["enqueuedAt"],
            42
        );
    }
}
