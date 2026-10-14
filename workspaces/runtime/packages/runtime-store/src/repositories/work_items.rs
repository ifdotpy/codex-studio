//! Read and write persisted work-item, plan, and queue state.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::records::{
    AgentId, AgentRecord, EventMetadataRecord, ExecutionRunRecord, JsonRecord, PlanRecord,
    QueueEventId, Table, TaskId, TranscriptAssetId, TranscriptAssetRecord, WorkItemRecord,
};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_work",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_plans",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_event_meta",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_assets",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_events",
        columns: &[
            "id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error",
        ],
    },
];

const EXECUTION_RUNS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_execution_runs",
    columns: &[
        "id", "agent", "account", "epoch", "thread", "turn", "created", "record",
    ],
}];
const CONDITIONAL_TABLES: &[ConditionalTableRequirement] = &[ConditionalTableRequirement {
    table_exists: "runtime_execution_runs",
    tables: EXECUTION_RUNS,
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TABLES[4]],
    conditional_tables: CONDITIONAL_TABLES,
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TABLES[0], TABLES[1], TABLES[2], TABLES[3]],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct StoredQueueEvent {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub status: String,
    pub created: f64,
    pub error: Option<String>,
    pub metadata: Option<Value>,
    pub valid: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredRecord {
    pub id: String,
    pub record: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkItemsRepository;

impl WorkItemsRepository {
    /// Read the active execution identity the Python writer would attach to a work result.
    /// The execution table is feature-created and therefore optional on older state.
    pub fn submission_identity(
        connection: &Connection,
        agent: &AgentRecord,
    ) -> StoreResult<Option<(String, Option<String>)>> {
        let table_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_execution_runs')",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(None);
        }

        let account = agent
            .extra
            .get("accountKey")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let thread = agent
            .extra
            .get("threadId")
            .and_then(serde_json::Value::as_str);
        let turn = agent
            .extra
            .get("turnId")
            .and_then(serde_json::Value::as_str);
        let row: Option<(String, String)> = if let Some(turn) = turn {
            let mut statement = connection.prepare(
                "SELECT id, record FROM runtime_execution_runs \
                 WHERE agent=?1 AND account=?2 AND thread IS ?3 AND turn=?4 LIMIT 2",
            )?;
            let rows = statement
                .query_map(params![agent.id.0, account, thread, turn], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (rows.len() == 1).then(|| rows.into_iter().next()).flatten()
        } else {
            connection
                .query_row(
                    "SELECT id, record FROM runtime_execution_runs \
                     WHERE agent=?1 AND account=?2 AND epoch=?3 AND turn IS NULL \
                     ORDER BY created DESC LIMIT 1",
                    params![agent.id.0, account, agent.epoch],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
        };
        let Some((id, record)) = row else {
            return Ok(None);
        };
        let typed = serde_json::from_str::<ExecutionRunRecord>(&record).map_err(|_| {
            crate::StoreError::InvalidStoredObject {
                table: "runtime_execution_runs",
                id: id.clone(),
            }
        })?;
        if typed.id().as_ref() != id
            || Table::<ExecutionRunRecord>::new(connection)
                .validate(&typed)
                .is_err()
        {
            return Err(crate::StoreError::InvalidStoredObject {
                table: "runtime_execution_runs",
                id,
            });
        }
        let value = typed.value();
        if turn.is_none()
            && matches!(
                value.get("status").and_then(serde_json::Value::as_str),
                Some("completed" | "failed" | "interrupted" | "cancelled")
            )
        {
            return Ok(None);
        }
        let attempt_id = value
            .get("latestAttemptId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(Some((id, attempt_id)))
    }

    pub fn work_record(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Ok(Table::<WorkItemRecord>::new(connection)
            .get(&TaskId::from(id))?
            .map(|record| record.value().clone()))
    }

    /// Select only current-team items that depend on the accepted task.
    pub fn dependent_work(
        connection: &Connection,
        root_id: &str,
        task_id: &str,
    ) -> StoreResult<Vec<StoredRecord>> {
        let mut statement = connection.prepare(
            "SELECT id, record FROM runtime_work \
             WHERE json_extract(record,'$.rootId')=?1 \
             AND EXISTS (SELECT 1 FROM json_each(record,'$.dependencies') AS dependency \
                         WHERE dependency.value=?2) ORDER BY id",
        )?;
        statement
            .query_map(params![root_id, task_id], |row| {
                Ok(StoredRecord {
                    id: row.get(0)?,
                    record: Some(row.get(1)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn put_work(connection: &Connection, id: &str, value: &Value) -> StoreResult<bool> {
        let record = WorkItemRecord::from_value(value.clone())?;
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: WorkItemRecord::TABLE,
                id: id.to_owned(),
            });
        }
        Table::<WorkItemRecord>::new(connection).insert_new(&record)
    }

    pub fn update_work(connection: &Connection, id: &str, value: &Value) -> StoreResult<()> {
        let record = WorkItemRecord::from_value(value.clone())?;
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: WorkItemRecord::TABLE,
                id: id.to_owned(),
            });
        }
        Table::<WorkItemRecord>::new(connection).update(&record)?;
        Ok(())
    }

    /// Release unfinished work owned by a failed worker and return each updated record.
    pub fn release_failed_work(
        connection: &Connection,
        owner_id: &str,
        reason: &str,
        now: f64,
    ) -> StoreResult<Vec<Value>> {
        let mut statement = connection.prepare(
            "SELECT id, record FROM runtime_work \
             WHERE json_extract(record,'$.owner')=?1 \
             AND json_extract(record,'$.status') IN ('ready','running','blocked')",
        )?;
        let rows = statement
            .query_map([owner_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let reason: String = reason.chars().take(500).collect();
        let mut released = Vec::with_capacity(rows.len());
        for (id, raw) in rows {
            let mut record: Value =
                serde_json::from_str(&raw).map_err(|_| crate::StoreError::InvalidStoredObject {
                    table: "runtime_work",
                    id: id.clone(),
                })?;
            if record.get("id").and_then(Value::as_str) != Some(id.as_str())
                || record.get("owner").and_then(Value::as_str) != Some(owner_id)
            {
                return Err(crate::StoreError::InvalidStoredObject {
                    table: "runtime_work",
                    id,
                });
            }
            let status = record
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| crate::StoreError::InvalidStoredObject {
                    table: "runtime_work",
                    id: id.clone(),
                })?;
            let version = record
                .get("version")
                .and_then(Value::as_i64)
                .and_then(|version| version.checked_add(1))
                .ok_or_else(|| crate::StoreError::InvalidStoredObject {
                    table: "runtime_work",
                    id: id.clone(),
                })?;
            if record
                .get("releases")
                .is_some_and(|value| !value.is_array())
            {
                return Err(crate::StoreError::InvalidStoredObject {
                    table: "runtime_work",
                    id: id.clone(),
                });
            }
            record
                .as_object_mut()
                .ok_or_else(|| crate::StoreError::InvalidStoredObject {
                    table: "runtime_work",
                    id: id.clone(),
                })?
                .entry("releases")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .expect("validated release history")
                .push(serde_json::json!({
                    "agent": owner_id,
                    "reason": reason,
                    "created": now,
                }));
            record["owner"] = Value::Null;
            record["status"] = Value::String(
                if status == "blocked" {
                    "blocked"
                } else {
                    "ready"
                }
                .to_owned(),
            );
            record["version"] = Value::from(version);
            record["updated"] = Value::from(now);
            Self::update_work(connection, &id, &record)?;
            released.push(record);
        }
        Ok(released)
    }

    pub fn validate_work(connection: &Connection, id: &str, value: &Value) -> StoreResult<()> {
        let record = WorkItemRecord::from_value(value.clone())?;
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: WorkItemRecord::TABLE,
                id: id.to_owned(),
            });
        }
        Table::<WorkItemRecord>::new(connection).validate(&record)
    }

    pub fn close_review_event(
        connection: &Connection,
        agent_id: &str,
        result_id: &str,
    ) -> StoreResult<()> {
        connection.execute(
            "UPDATE runtime_events SET status='stored_only',error=?1 \
             WHERE id=?2 AND kind='work_review' AND status='pending' AND agent=?3",
            params![
                "The exact result already has a decision",
                format!("work-result:{result_id}"),
                agent_id
            ],
        )?;
        Ok(())
    }

    pub fn retire_pending_work_ready(
        connection: &Connection,
        owner_id: &str,
        task_id: &str,
        reason: &'static str,
    ) -> StoreResult<()> {
        connection.execute(
            "UPDATE runtime_events SET status='stored_only',error=?3 \
             WHERE agent=?1 AND kind='work_ready' AND status='pending' \
             AND CASE WHEN json_valid(text) THEN json_extract(text,'$.task') END=?2",
            params![owner_id, task_id, reason],
        )?;
        Ok(())
    }

    pub fn work(connection: &Connection) -> StoreResult<Vec<StoredRecord>> {
        Table::<WorkItemRecord>::new(connection)
            .list()?
            .into_iter()
            .map(|record| {
                Ok(StoredRecord {
                    id: record.id().as_ref().to_owned(),
                    record: Some(serde_json::to_string(record.value())?),
                })
            })
            .collect::<StoreResult<Vec<_>>>()
    }

    pub fn plan(connection: &Connection, id: &str) -> StoreResult<Option<StoredRecord>> {
        Table::<PlanRecord>::new(connection)
            .get(&AgentId::from(id))?
            .map(|record| {
                Ok::<_, StoreError>(StoredRecord {
                    id: id.to_owned(),
                    record: Some(serde_json::to_string(record.value())?),
                })
            })
            .transpose()
    }

    pub fn queue(
        connection: &Connection,
        agent: &str,
        epoch: i64,
    ) -> StoreResult<Vec<StoredQueueEvent>> {
        let mut statement = connection.prepare(
            "SELECT id,kind,text,status,created,error FROM runtime_events \
             WHERE agent=?1 AND epoch=?2 AND status='pending' \
             AND kind IN ('user','followup') ORDER BY created,rowid",
        )?;
        let rows = statement.query_map(params![agent, epoch], |row| {
            let (id, id_valid) = row.get::<_, String>(0).map_or_else(
                |_| ("<invalid id>".to_owned(), false),
                |value| (value, true),
            );
            let (kind, kind_valid) = row
                .get::<_, String>(1)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            let (text, text_valid) = row
                .get::<_, String>(2)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            let (status, status_valid) = row
                .get::<_, String>(3)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            let (created, created_valid) = row
                .get::<_, f64>(4)
                .map_or_else(|_| (f64::NAN, false), |value| (value, value.is_finite()));
            let (error, error_valid) = row.get_ref(5).map(optional_text).unwrap_or((None, false));
            Ok(StoredQueueEvent {
                id,
                kind,
                text,
                status,
                created,
                error,
                metadata: None,
                valid: id_valid
                    && kind_valid
                    && text_valid
                    && status_valid
                    && created_valid
                    && error_valid,
            })
        })?;
        let mut events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let metadata = Table::<EventMetadataRecord>::new(connection);
        for event in &mut events {
            if let Some(record) = metadata.get(&QueueEventId::from(event.id.as_str()))? {
                event.metadata = Some(record.value().clone());
            }
        }
        Ok(events)
    }

    pub fn assets(connection: &Connection, ids: &[String]) -> StoreResult<Vec<Value>> {
        let table = Table::<TranscriptAssetRecord>::new(connection);
        let mut result = Vec::with_capacity(ids.len());
        let mut seen = std::collections::BTreeSet::new();
        for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
            if let Some(record) = table.get(&TranscriptAssetId::from(id.as_str()))? {
                result.push(record.value().clone());
            }
        }
        Ok(result)
    }
}

fn optional_text(value: rusqlite::types::ValueRef<'_>) -> (Option<String>, bool) {
    match value {
        rusqlite::types::ValueRef::Null => (None, true),
        rusqlite::types::ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => (Some(text.to_owned()), true),
            Err(_) => (None, false),
        },
        _ => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::{REQUIREMENTS, WorkItemsRepository};
    use crate::{AgentRecord, StoreError};
    use rusqlite::Connection;
    use serde_json::json;

    #[test]
    fn bootstrapped_tables_are_required_and_empty_reads_succeed() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        for table in REQUIREMENTS
            .tables
            .iter()
            .chain(REQUIREMENTS.bootstrap_tables)
        {
            let columns = table.columns.join(" TEXT,");
            connection
                .execute_batch(&format!("CREATE TABLE {}({columns} TEXT)", table.name))
                .expect("required family creates");
        }
        assert_eq!(REQUIREMENTS.conditional_tables.len(), 1);
        assert_eq!(
            REQUIREMENTS.conditional_tables[0].table_exists,
            "runtime_execution_runs"
        );
        assert!(
            WorkItemsRepository::work(&connection)
                .expect("empty work list")
                .is_empty()
        );
        assert!(
            WorkItemsRepository::plan(&connection, "agent-a")
                .expect("missing plan row")
                .is_none()
        );
        assert!(
            WorkItemsRepository::queue(&connection, "agent-a", 0)
                .expect("empty queue")
                .is_empty()
        );
    }

    #[test]
    fn submission_identity_locates_invalid_execution_run_records() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection.execute_batch(
            "CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);",
        ).expect("execution table creates");
        let agent = AgentRecord::from_value(json!({
            "id":"worker","rootId":"lead","epoch":4,"status":"paused"
        }))
        .expect("agent record parses");
        for record in [
            json!({"id":"run-1","agent":"worker","accountKey":"default","epoch":4,"created":1.0}),
            json!({"id":"run-1","agent":"worker","accountKey":"default","epoch":4,"status":"running","created":1.0,"latestAttemptId":5}),
        ] {
            connection.execute(
                "INSERT INTO runtime_execution_runs VALUES ('run-1','worker','default',4,NULL,NULL,1.0,?1)",
                [record.to_string()],
            ).expect("execution row inserts");
            assert!(matches!(
                WorkItemsRepository::submission_identity(&connection, &agent),
                Err(StoreError::InvalidStoredObject { table: "runtime_execution_runs", id }) if id == "run-1"
            ));
            connection
                .execute("DELETE FROM runtime_execution_runs", [])
                .expect("invalid row deletes");
        }
    }

    #[test]
    fn release_failed_work_updates_only_unfinished_items_and_keeps_blocked_status() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            .expect("work table creates");
        for (id, status) in [
            ("running-work", "running"),
            ("blocked-work", "blocked"),
            ("review-work", "review"),
        ] {
            let record = json!({
                "id":id,"rootId":"lead","owner":"worker","status":status,
                "title":"Work","created":1.0,"updated":1.0,"version":3,
                "dependencies":[],"results":[],"decisions":[]
            });
            connection
                .execute(
                    "INSERT INTO runtime_work(id,record) VALUES (?1,?2)",
                    rusqlite::params![id, record.to_string()],
                )
                .expect("work record inserts");
        }

        let reason = "x".repeat(510);
        let released =
            WorkItemsRepository::release_failed_work(&connection, "worker", &reason, 12.5)
                .expect("unfinished items release");
        assert_eq!(released.len(), 2);
        let by_id = released
            .into_iter()
            .map(|record| (record["id"].as_str().unwrap_or_default().to_owned(), record))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(by_id["running-work"]["status"], "ready");
        assert_eq!(by_id["blocked-work"]["status"], "blocked");
        for record in by_id.values() {
            assert_eq!(record["owner"], serde_json::Value::Null);
            assert_eq!(record["version"], 4);
            assert_eq!(record["updated"], 12.5);
            assert_eq!(record["releases"][0]["reason"], "x".repeat(500));
        }
        assert!(
            WorkItemsRepository::work_record(&connection, "review-work")
                .expect("review item reads")
                .is_some_and(|record| record["status"] == "review")
        );
    }

    #[test]
    fn work_creation_refuses_an_existing_id_without_overwriting_it() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);")
            .expect("work family creates");
        let first = serde_json::json!({"id":"work-1","status":"ready","title":"original"});
        let replacement =
            serde_json::json!({"id":"work-1","status":"blocked","title":"replacement"});
        assert!(
            WorkItemsRepository::put_work(&connection, "work-1", &first)
                .expect("new work item inserts")
        );
        assert!(
            !WorkItemsRepository::put_work(&connection, "work-1", &replacement)
                .expect("duplicate is refused")
        );
        assert_eq!(
            WorkItemsRepository::work_record(&connection, "work-1")
                .expect("original reads")
                .expect("original remains")["title"],
            "original"
        );
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "work_items",
        requirements: &REQUIREMENTS,
    }
}
