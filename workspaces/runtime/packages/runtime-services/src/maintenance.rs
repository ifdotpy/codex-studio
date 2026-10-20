//! Idempotent startup cleanup for retired review assignments.
//!
//! This mirrors `codex_state_cleanup.remove_review_assignments`: remove
//! `reviewSchedules` and `reviewTargets`, and cancel only pending `chat_review`
//! events (`workspaces/runtime/apps/server/src/codex_state_cleanup.py:6-20`).
//! Payload retention remains an optional manual operation; Rust reads every
//! coherent `payload-v1:tasks` phase. Rust provider stderr is drained to a sink
//! by `runtime-process::provider_stdio`; this module does not create log files.
//! Store record writers preserve validation and write hooks. The renderer DTO
//! excludes these retired keys; rebuilding a current projection is a no-op,
//! while a stale projection is repaired through the sequenced entity writer.

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use studio_runtime_store::{
    AgentRecord, AgentRepository, JsonRecord, RecordKeyMode, RecordQueryShape, RuntimeStore,
    StoreResult, SyncDocumentsRepository, Table,
};

/// Counts rows changed by the retired-review cleanup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ReviewCleanupCounts {
    pub agents: u64,
    pub rooms: u64,
    pub pending: u64,
}

/// Remove retired review assignments atomically during runtime startup.
pub async fn remove_review_assignments(store: &RuntimeStore) -> StoreResult<ReviewCleanupCounts> {
    store
        .transaction(remove_review_assignments_in_transaction)
        .await
}

fn remove_review_assignments_in_transaction(
    transaction: &Transaction<'_>,
) -> StoreResult<ReviewCleanupCounts> {
    let mut counts = ReviewCleanupCounts::default();
    if table_exists(transaction, "runtime_agents")? {
        let rows = records_with_field(transaction, "runtime_agents", "reviewSchedules")?;
        for (_, mut value) in rows {
            value
                .as_object_mut()
                .expect("stored agent records are JSON objects")
                .remove("reviewSchedules");
            let record = AgentRecord::from_value(value)?;
            AgentRepository::put(transaction, &record)?;
            remove_projection_field(transaction, "agent", &record.id.0, "reviewSchedules")?;
            counts.agents += 1;
        }
    }
    if table_exists(transaction, "runtime_rooms")? {
        let rows = records_with_field(transaction, "runtime_rooms", "reviewTargets")?;
        for (_, value) in rows {
            let mut record: MaintenanceRoomRecord = serde_json::from_value(value)?;
            record.fields.remove("reviewTargets");
            Table::<MaintenanceRoomRecord>::new(transaction).put(&record)?;
            remove_projection_field(transaction, "room", record.id.as_ref(), "reviewTargets")?;
            counts.rooms += 1;
        }
    }
    if table_exists(transaction, "runtime_events")? {
        counts.pending = transaction.execute(
            "UPDATE runtime_events
             SET status='cancelled',error='Scheduled chat reviews have been removed'
             WHERE kind='chat_review' AND status='pending'",
            [],
        )? as u64;
    }
    Ok(counts)
}

fn remove_projection_field(
    transaction: &Transaction<'_>,
    collection: &str,
    id: &str,
    field: &str,
) -> StoreResult<()> {
    let row = transaction
        .query_row(
            "SELECT payload,deleted FROM sync_entities WHERE collection=?1 AND id=?2",
            rusqlite::params![collection, id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((Some(payload), 0)) = row else {
        return Ok(());
    };
    let mut envelope: Value = serde_json::from_str(&payload)?;
    let Some(value) = envelope.get_mut("value").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if value.remove(field).is_some() {
        SyncDocumentsRepository::put_entity(
            transaction,
            collection,
            id,
            Value::Object(value.clone()),
            false,
        )?;
    }
    Ok(())
}

fn records_with_field(
    connection: &Connection,
    table: &'static str,
    field: &'static str,
) -> StoreResult<Vec<(String, Value)>> {
    let mut statement = connection.prepare(&format!(
        "SELECT id,record FROM {table} WHERE json_type(record,'$.{field}') IS NOT NULL ORDER BY id"
    ))?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (id, raw) = row?;
            let value: Value = serde_json::from_str(&raw).map_err(|_| {
                studio_runtime_store::StoreError::InvalidStoredRecord {
                    object: table,
                    id: id.clone(),
                }
            })?;
            Ok((id, value))
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct MaintenanceRoomId(String);

impl AsRef<str> for MaintenanceRoomId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for MaintenanceRoomId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MaintenanceRoomRecord {
    id: MaintenanceRoomId,
    #[serde(flatten)]
    fields: BTreeMap<String, Value>,
}

impl JsonRecord for MaintenanceRoomRecord {
    type Id = MaintenanceRoomId;

    const TABLE: &'static str = "runtime_rooms";
    const SCHEMA: &'static str = "PersistedRoomRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;

    fn record_key(&self) -> Option<&Self::Id> {
        Some(&self.id)
    }
}

fn table_exists(transaction: &Transaction<'_>, table: &str) -> StoreResult<bool> {
    Ok(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn seed(connection: &Connection) {
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(
                    id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,
                    created REAL,epoch INTEGER,turn_id TEXT,error TEXT
                 );
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,
                    deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY,scope TEXT,id TEXT,payload TEXT,
                    deleted INTEGER,UNIQUE(scope,id));
                 CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY,scope TEXT UNIQUE,hash TEXT,
                    deleted INTEGER,updated REAL);
                 CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                 CREATE TABLE runtime_agent_record_generation(id INTEGER PRIMARY KEY,value INTEGER NOT NULL);
                 INSERT INTO runtime_agent_record_generation VALUES(1,0);
                 CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents
                 BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; END;
                 CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE OF record ON runtime_agents
                 WHEN OLD.record IS NOT NEW.record
                 BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; END;
                 CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents
                 BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; END;",
            )
            .expect("fixture schema creates");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES (?1,?2), (?3,?4)",
                (
                    "agent-a",
                    json!({"id":"agent-a","rootId":"agent-a","epoch":0,"status":"idle","reviewSchedules":[{"id":"retired"}],"extra":1})
                        .to_string(),
                    "agent-b",
                    json!({"id":"agent-b","rootId":"agent-b","epoch":0,"status":"idle","extra":2}).to_string(),
                ),
            )
            .expect("Python-shaped agent records insert");
        connection
            .execute(
                "INSERT INTO runtime_rooms(id,record) VALUES (?1,?2)",
                (
                    "room-a",
                    json!({"id":"room-a","kind":"private","members":["agent-a"],"updated":1,"userHidden":false,"reviewTargets":null,"extra":3}).to_string(),
                ),
            )
            .expect("Python-shaped room record inserts");
        connection
            .execute_batch(
                "INSERT INTO runtime_events VALUES
                 ('pending','agent-a','chat_review','pending body','pending',1,0,NULL,NULL),
                 ('submitted','agent-a','chat_review','submitted body','submitted',2,0,NULL,'receipt'),
                 ('uncertain','agent-a','chat_review','uncertain body','uncertain',3,0,NULL,'unknown'),
                 ('other','agent-a','followup','other body','pending',4,0,NULL,NULL);",
            )
            .expect("fixture events insert");
    }

    fn snapshot(connection: &Connection) -> Value {
        let agents = connection
            .prepare("SELECT id,record FROM runtime_agents ORDER BY id")
            .expect("agents query prepares")
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("agents query runs")
            .map(|row| {
                let (id, record) = row.expect("agent row reads");
                json!({"id":id,"record":serde_json::from_str::<Value>(&record).expect("agent JSON reads")})
            })
            .collect::<Vec<_>>();
        let rooms = connection
            .prepare("SELECT id,record FROM runtime_rooms ORDER BY id")
            .expect("rooms query prepares")
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("rooms query runs")
            .map(|row| {
                let (id, record) = row.expect("room row reads");
                json!({"id":id,"record":serde_json::from_str::<Value>(&record).expect("room JSON reads")})
            })
            .collect::<Vec<_>>();
        let events = connection
            .prepare("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id")
            .expect("events query prepares")
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "agent": row.get::<_, String>(1)?,
                    "kind": row.get::<_, String>(2)?,
                    "text": row.get::<_, String>(3)?,
                    "status": row.get::<_, String>(4)?,
                    "created": row.get::<_, f64>(5)?,
                    "epoch": row.get::<_, i64>(6)?,
                    "turn_id": row.get::<_, Option<String>>(7)?,
                    "error": row.get::<_, Option<String>>(8)?,
                }))
            })
            .expect("events query runs")
            .map(|row| row.expect("event row reads"))
            .collect::<Vec<_>>();
        let entities = connection
            .prepare("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities ORDER BY collection,id")
            .expect("sync entities query prepares")
            .query_map([], |row| {
                Ok(json!({
                    "collection": row.get::<_, String>(0)?,
                    "id": row.get::<_, String>(1)?,
                    "seq": row.get::<_, i64>(2)?,
                    "hash": row.get::<_, String>(3)?,
                    "payload": row.get::<_, Option<String>>(4)?,
                    "deleted": row.get::<_, i64>(5)?,
                }))
            })
            .expect("sync entities query runs")
            .map(|row| row.expect("sync entity row reads"))
            .collect::<Vec<_>>();
        let documents = connection
            .prepare("SELECT seq,scope,id,payload,deleted FROM sync_documents ORDER BY seq")
            .expect("sync documents query prepares")
            .query_map([], |row| {
                Ok(json!({
                    "seq": row.get::<_, i64>(0)?,
                    "scope": row.get::<_, String>(1)?,
                    "id": row.get::<_, String>(2)?,
                    "payload": row.get::<_, String>(3)?,
                    "deleted": row.get::<_, i64>(4)?,
                }))
            })
            .expect("sync documents query runs")
            .map(|row| row.expect("sync document row reads"))
            .collect::<Vec<_>>();
        let versions = connection
            .prepare("SELECT seq,scope,hash,deleted,updated FROM sync_versions ORDER BY seq")
            .expect("sync versions query prepares")
            .query_map([], |row| {
                Ok(json!({
                    "seq": row.get::<_, i64>(0)?,
                    "scope": row.get::<_, String>(1)?,
                    "hash": row.get::<_, String>(2)?,
                    "deleted": row.get::<_, i64>(3)?,
                    "updated": row.get::<_, f64>(4)?,
                }))
            })
            .expect("sync versions query runs")
            .map(|row| row.expect("sync version row reads"))
            .collect::<Vec<_>>();
        let agent_generation: i64 = connection
            .query_row(
                "SELECT value FROM runtime_agent_record_generation WHERE id=1",
                [],
                |row| row.get(0),
            )
            .expect("agent generation reads");
        json!({"agents":agents,"rooms":rooms,"events":events,"sync_entities":entities,"sync_documents":documents,"sync_versions":versions,"agent_generation":agent_generation})
    }

    #[test]
    fn cleanup_preserves_non_pending_inputs_and_counts_removed_rows() {
        let directory = tempfile::tempdir().expect("temporary fixture directory");
        let mut connection =
            Connection::open(directory.path().join("rust.sqlite3")).expect("fixture opens");
        seed(&connection);
        let before = snapshot(&connection);
        let transaction = connection
            .transaction()
            .expect("cleanup transaction begins");
        let counts =
            remove_review_assignments_in_transaction(&transaction).expect("Rust cleanup succeeds");
        transaction.commit().expect("cleanup transaction commits");
        let after = snapshot(&connection);

        assert_eq!(
            counts,
            ReviewCleanupCounts {
                agents: 1,
                rooms: 1,
                pending: 1,
            }
        );
        assert_eq!(
            after["agent_generation"],
            before["agent_generation"]
                .as_i64()
                .expect("agent generation should be an integer")
                + 1,
            "the store record writer advances the persistent agent generation trigger"
        );
        assert_eq!(after["sync_documents"], before["sync_documents"]);
        assert_eq!(after["sync_versions"], before["sync_versions"]);
        let event = |snapshot: &Value, id: &str| {
            snapshot["events"]
                .as_array()
                .expect("event snapshot is an array")
                .iter()
                .find(|event| event["id"] == id)
                .unwrap_or_else(|| panic!("{id} input remains"))
                .clone()
        };
        for id in ["submitted", "uncertain"] {
            assert_eq!(event(&after, id), event(&before, id));
        }
    }

    #[test]
    fn cleanup_is_atomic_and_idempotent_across_restart() {
        let directory = tempfile::tempdir().expect("temporary fixture directory");
        let path = directory.path().join("canvas.sqlite3");
        let mut connection = Connection::open(&path).expect("temporary database opens");
        seed(&connection);
        let before = snapshot(&connection);
        connection
            .execute_batch(
                "CREATE TRIGGER fail_event_cleanup BEFORE UPDATE ON runtime_events
                 BEGIN SELECT RAISE(ABORT,'injected cleanup failure'); END;",
            )
            .expect("failpoint trigger installs");
        {
            let transaction = connection.transaction().expect("transaction begins");
            assert!(remove_review_assignments_in_transaction(&transaction).is_err());
            drop(transaction);
        }
        assert_eq!(
            snapshot(&connection),
            before,
            "failure after record writes rolls back the whole cleanup"
        );
        connection
            .execute_batch("DROP TRIGGER fail_event_cleanup;")
            .expect("failpoint trigger drops");

        let first = {
            let transaction = connection.transaction().expect("first transaction begins");
            let counts =
                remove_review_assignments_in_transaction(&transaction).expect("first cleanup runs");
            transaction.commit().expect("first cleanup commits");
            counts
        };
        drop(connection);
        let mut connection = Connection::open(&path).expect("database reopens after commit");
        let second = {
            let transaction = connection.transaction().expect("replay transaction begins");
            let counts = remove_review_assignments_in_transaction(&transaction)
                .expect("replayed cleanup runs");
            transaction.commit().expect("replay commits");
            counts
        };
        assert_eq!(first.agents, 1);
        assert_eq!(first.rooms, 1);
        assert_eq!(first.pending, 1);
        assert_eq!(second, ReviewCleanupCounts::default());
    }

    #[test]
    fn missing_optional_room_table_is_a_noop() {
        let mut connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(id TEXT,kind TEXT,status TEXT,error TEXT);",
            )
            .expect("minimal fixture schema creates");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        let transaction = connection.transaction().expect("transaction begins");
        assert_eq!(
            remove_review_assignments_in_transaction(&transaction)
                .expect("missing optional table is accepted"),
            ReviewCleanupCounts::default()
        );
        transaction.commit().expect("transaction commits");
    }
}
