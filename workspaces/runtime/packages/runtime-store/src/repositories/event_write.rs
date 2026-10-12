//! Atomic persistence primitives for plan and annotation requests.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{QueueEvent, StoreError, StoreResult};

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

    /// Keep the renderer projection in step with the scheduler's queued state.
    pub fn set_agent_sync_status(
        transaction: &Transaction<'_>,
        agent_id: &str,
        status: &str,
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
        let value = payload
            .get_mut("value")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| StoreError::SchemaMismatch("agent sync projection is invalid".into()))?;
        if value.get("id").and_then(Value::as_str) != Some(agent_id) {
            return Err(StoreError::SchemaMismatch(
                "agent sync projection identity is invalid".to_owned(),
            ));
        }
        if value.get("status").and_then(Value::as_str) == Some(status) {
            return Ok(());
        }
        value.insert("status".to_owned(), Value::String(status.to_owned()));
        let payload = serde_json::to_string(&payload)?;
        let digest = Sha256::digest(payload.as_bytes());
        let mut hash = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut hash, "{byte:02x}").map_err(|_| {
                StoreError::SchemaMismatch("cannot encode sync entity hash".to_owned())
            })?;
        }
        let sequence: i64 = transaction.query_row(
            "SELECT MAX(entity_seq,document_seq,version_seq)+1 FROM ( \
             SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities), \
             (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents), \
             (SELECT COALESCE(MAX(seq),0) AS version_seq FROM sync_versions)",
            [],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE sync_entities SET seq=?1,hash=?2,payload=?3 \
             WHERE collection='agent' AND id=?4 AND deleted=0",
            params![sequence, hash, payload, agent_id],
        )?;
        Ok(())
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
                 INSERT INTO sync_entities VALUES('agent','a',1,'old', '{\"collection\":\"agent\",\"id\":\"a\",\"value\":{\"id\":\"a\",\"status\":\"idle\"}}',0);",
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
                "collection":"agent","id":"a","value":{"id":"a","status":"queued"}
            })
        );
    }
}
