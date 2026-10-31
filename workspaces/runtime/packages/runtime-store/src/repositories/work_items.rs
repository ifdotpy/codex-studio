//! Read and write persisted work-item, plan, and queue state.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::records::{
    AgentId, AgentRecord, EventMetadataRecord, JsonRecord, PlanRecord, QueueEventId, Table, TaskId,
    TranscriptAssetId, TranscriptAssetRecord, WorkItemRecord,
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
    pub metadata: Option<EventMetadataRecord>,
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
    /// The task submission counter belongs to the current owner execution pointer.
    /// Historical order cannot grant a new attempt identity after a source transition.
    pub fn submission_identity(
        connection: &Connection,
        agent: &AgentRecord,
    ) -> StoreResult<Option<(String, Option<String>)>> {
        let Some(pointer) = agent
            .extra
            .get("activeExecution")
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let invalid = || StoreError::InvalidStoredObject {
            table: "runtime_agents",
            id: agent.id.0.clone(),
        };
        let studio_runtime_schema::json_object::JsonObject(pointer) =
            <studio_runtime_schema::json_object::JsonObject<crate::records::ActiveExecutionView> as serde::Deserialize>::deserialize(pointer).map_err(|_| invalid())?;
        let id = pointer.run_id.as_deref().ok_or_else(invalid)?;
        let attempt = pointer.attempt_id;
        let raw: Option<String> = connection
            .query_row(
                "SELECT record FROM runtime_execution_runs WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let raw = raw.ok_or_else(invalid)?;
        let value: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        let studio_runtime_schema::json_object::JsonObject(run): studio_runtime_schema::json_object::JsonObject<super::execution_history::ExecutionRun<Value, studio_runtime_schema::AgentCounter>> =
            serde::Deserialize::deserialize(&value).map_err(|_| {
                // The older record owner reports malformed JSON or a missing ID at
                // the agent pointer; schema errors are located at the run row.
                match value.get("id").and_then(Value::as_str) {
                        Some(record_id) => StoreError::InvalidStoredObject { table: "runtime_execution_runs", id: record_id.into() },
                        None => invalid(),
                }
            })?;
        if run.id != id
            || run.agent != agent.id.0
            || run.account_key != agent.account_key
            || run.epoch.as_number().as_i64() != Some(agent.epoch)
        {
            return Err(invalid());
        }
        Ok(Some((id.into(), attempt)))
    }

    pub fn work_record(connection: &Connection, id: &str) -> StoreResult<Option<WorkItemRecord>> {
        Table::<WorkItemRecord>::new(connection).get(&TaskId::from(id))
    }

    /// Select only current-team items that depend on the accepted task.
    pub fn dependent_work(
        connection: &Connection,
        root_id: &str,
        task_id: &str,
    ) -> StoreResult<Vec<WorkItemRecord>> {
        let mut statement = connection.prepare_cached(
            "SELECT id, record FROM runtime_work \
             WHERE json_extract(record,'$.rootId')=?1 \
             AND EXISTS (SELECT 1 FROM json_each(record,'$.dependencies') AS dependency \
                         WHERE dependency.value=?2) ORDER BY id",
        )?;
        let rows = statement
            .query_map(params![root_id, task_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, raw)| WorkItemRecord::decode(&id, &raw))
            .collect()
    }

    pub fn put_work(connection: &Connection, id: &str, value: &Value) -> StoreResult<bool> {
        let record = WorkItemRecord::checked(value)?;
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: WorkItemRecord::TABLE,
                id: id.to_owned(),
            });
        }
        Table::<WorkItemRecord>::new(connection).insert_new(&record)
    }

    pub fn update_work(connection: &Connection, id: &str, value: &Value) -> StoreResult<()> {
        let record = WorkItemRecord::checked(value)?;
        Self::update_checked_work(connection, id, &record)
    }

    pub fn update_checked_work(
        connection: &Connection,
        id: &str,
        record: &WorkItemRecord,
    ) -> StoreResult<()> {
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: WorkItemRecord::TABLE,
                id: id.to_owned(),
            });
        }
        Table::<WorkItemRecord>::new(connection).update(record)?;
        Ok(())
    }

    /// Release unfinished work owned by a failed worker and return each updated record.
    pub fn release_failed_work(
        connection: &Connection,
        owner_id: &str,
        reason: &str,
        now: f64,
    ) -> StoreResult<Vec<Value>> {
        let mut statement = connection.prepare_cached(
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
            // Release historically repairs fields such as a negative version before
            // the checked write, so this mutation boundary must retain raw input.
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

    pub fn validate_work(_connection: &Connection, id: &str, value: &Value) -> StoreResult<()> {
        let record = WorkItemRecord::checked(value)?;
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: WorkItemRecord::TABLE,
                id: id.to_owned(),
            });
        }
        Ok(())
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

    pub fn work(connection: &Connection) -> StoreResult<Vec<WorkItemRecord>> {
        Table::<WorkItemRecord>::new(connection).list()
    }

    pub fn plan(connection: &Connection, id: &str) -> StoreResult<Option<PlanRecord>> {
        Table::<PlanRecord>::new(connection).get(&AgentId::from(id))
    }

    pub fn queue(
        connection: &Connection,
        agent: &str,
        epoch: i64,
    ) -> StoreResult<Vec<StoredQueueEvent>> {
        let mut statement = connection.prepare_cached(
            "SELECT id,kind,text,status,created,error FROM runtime_events \
             WHERE agent=?1 AND epoch=?2 AND status IN ('pending','reserved') \
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
            let (mut status, status_valid) = row
                .get::<_, String>(3)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            // Rust reserves accepted events internally before provider
            // submission; Python keeps these visible as pending in /api/queue.
            if status == "reserved" {
                status = "pending".to_owned();
            }
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
                event.metadata = Some(record);
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
                result.push(record.into_value());
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
        rusqlite::types::ValueRef::Integer(_)
        | rusqlite::types::ValueRef::Real(_)
        | rusqlite::types::ValueRef::Blob(_) => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::{REQUIREMENTS, WorkItemsRepository};
    use crate::{AgentRecord, StoreError};
    use rusqlite::Connection;
    use serde_json::json;

    #[test]
    fn validation_only_checks_once_without_encoding() {
        use crate::records::validation_counts::{CHECKS, ENCODINGS};
        let connection = Connection::open_in_memory().expect("database");
        let work =
            json!({"id":"task","rootId":"lead","status":"ready","owner":null,"dependencies":[]});
        CHECKS.with(|count| count.set(0));
        ENCODINGS.with(|count| count.set(0));
        WorkItemsRepository::validate_work(&connection, "task", &work).expect("valid work");
        assert_eq!(CHECKS.with(std::cell::Cell::get), 1);
        assert_eq!(ENCODINGS.with(std::cell::Cell::get), 0);
        let plan = crate::PlanRecord::from_value(
            json!({"id":"agent","rootId":"lead","text":"","version":0,"updated":null}),
        )
        .expect("plan");
        CHECKS.with(|count| count.set(0));
        ENCODINGS.with(|count| count.set(0));
        crate::Table::<crate::PlanRecord>::new(&connection)
            .validate_at(&crate::AgentId::from("agent"), &plan)
            .expect("valid plan");
        assert_eq!(CHECKS.with(std::cell::Cell::get), 1);
        assert_eq!(ENCODINGS.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn plan_returns_checked_value_without_a_text_round_trip() {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch("CREATE TABLE runtime_plans(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .expect("table");
        let value = json!({"id":"agent", "rootId":"root", "text":"plan", "version":1, "updated":null, "future":{"nested":[1,2]}});
        connection
            .execute(
                "INSERT INTO runtime_plans VALUES (?1,?2)",
                rusqlite::params!["agent", value.to_string()],
            )
            .expect("insert");
        assert_eq!(
            WorkItemsRepository::plan(&connection, "agent")
                .expect("read")
                .expect("plan")
                .into_value(),
            value
        );
        connection
            .execute(
                "UPDATE runtime_plans SET record=?1",
                [r#"{"id":"agent","version":"invalid"}"#],
            )
            .expect("corrupt");
        assert!(WorkItemsRepository::plan(&connection, "agent").is_err());
    }

    #[test]
    fn typed_work_repository_retains_payload_and_rejects_corrupt_dependents() {
        // These signatures prevent serialization/reparse adapters from returning.
        let _: fn(&Connection, &str) -> crate::StoreResult<Option<crate::PlanRecord>> =
            WorkItemsRepository::plan;
        let _: fn(&Connection) -> crate::StoreResult<Vec<crate::WorkItemRecord>> =
            WorkItemsRepository::work;
        let _: fn(&Connection, &str) -> crate::StoreResult<Option<crate::WorkItemRecord>> =
            WorkItemsRepository::work_record;
        let _: fn(&Connection, &str, &str) -> crate::StoreResult<Vec<crate::WorkItemRecord>> =
            WorkItemsRepository::dependent_work;
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .expect("table");
        let raw = json!({"id":"task","rootId":"lead","status":"ready","owner":null,"dependencies":["parent"],"future":{"nested":[null,42]}});
        WorkItemsRepository::put_work(&connection, "task", &raw).expect("insert");
        assert_eq!(
            WorkItemsRepository::work(&connection).expect("list")[0].value(),
            raw
        );
        assert_eq!(
            WorkItemsRepository::work_record(&connection, "task")
                .expect("get")
                .expect("task")
                .value(),
            raw
        );
        assert_eq!(
            WorkItemsRepository::dependent_work(&connection, "lead", "parent").expect("dependents")
                [0]
            .value(),
            raw
        );
        connection.execute("UPDATE runtime_work SET record=json_set(record,'$.status','future') WHERE id='task'",[]).expect("corrupt status");
        assert!(WorkItemsRepository::work(&connection).is_err());
        assert!(WorkItemsRepository::work_record(&connection, "task").is_err());
        assert!(WorkItemsRepository::dependent_work(&connection, "lead", "parent").is_err());
    }

    #[test]
    fn work_record_preserves_last_duplicate_and_checks_physical_identity() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        connection.execute("INSERT INTO runtime_work VALUES('task',?1)", [r#"{"id":"wrong","id":"task","status":"ready","owner":null,"version":1.0,"future":{"x":[9007199254740993]}}"#]).unwrap();
        let record = WorkItemsRepository::work_record(&connection, "task")
            .unwrap()
            .unwrap();
        assert_eq!(record.owner(), None);
        assert_eq!(record.version().unwrap().as_f64(), Some(1.0));
        assert_eq!(
            record.value()["future"]["x"][0],
            json!(9007199254740993_u64)
        );
        let pointer = record.extension("future").unwrap()["x"]
            .as_array()
            .unwrap()
            .as_ptr();
        let projected = record.into_value();
        assert_eq!(
            pointer,
            projected["future"]["x"].as_array().unwrap().as_ptr()
        );
        connection
            .execute(
                "UPDATE runtime_work SET record=?1",
                [r#"{"id":"other","status":"ready"}"#],
            )
            .unwrap();
        assert!(WorkItemsRepository::work_record(&connection, "task").is_err());
    }

    #[test]
    fn release_preserves_historical_negative_version_repair() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        let value = json!({"id":"task", "owner":"worker", "status":"running", "version":-1, "future":{"proof":true}});
        connection
            .execute(
                "INSERT INTO runtime_work VALUES('task',?1)",
                [value.to_string()],
            )
            .unwrap();
        let released =
            WorkItemsRepository::release_failed_work(&connection, "worker", "lost", 4.0).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0]["version"], 0);
        assert_eq!(released[0]["owner"], serde_json::Value::Null);
        assert_eq!(released[0]["future"], value["future"]);
        assert_eq!(
            WorkItemsRepository::work_record(&connection, "task")
                .unwrap()
                .unwrap()
                .into_value(),
            released[0]
        );
    }

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
        connection
            .execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) \
                 VALUES ('reserved-user','agent-a','user','held','reserved',1.0,0,NULL)",
                [],
            )
            .expect("reserved event inserts");
        let queue = WorkItemsRepository::queue(&connection, "agent-a", 0)
            .expect("reserved event remains visible");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, "reserved-user");
        assert_eq!(queue[0].status, "pending");
    }

    #[test]
    fn submission_identity_locates_invalid_execution_run_records() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection.execute_batch(
            "CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);",
        ).expect("execution table creates");
        let agent = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default",
            "id":"worker","provider":"codex","rootId":"lead","epoch":4,"status":"paused",
            "activeExecution":{"runId":"run-1","attemptId":null}
        }))
        .expect("agent record parses");
        for raw in ["{", r#"{"agent":"worker"}"#, r#"{"id":false}"#] {
            connection.execute("INSERT INTO runtime_execution_runs VALUES ('run-1','worker','default',4,NULL,NULL,1.0,?1)", [raw]).unwrap();
            assert!(
                matches!(WorkItemsRepository::submission_identity(&connection, &agent), Err(StoreError::InvalidStoredObject { table: "runtime_agents", ref id }) if id == "worker")
            );
            connection
                .execute("DELETE FROM runtime_execution_runs", [])
                .unwrap();
        }
        connection.execute("INSERT INTO runtime_execution_runs VALUES ('run-1','worker','default',4,NULL,NULL,1.0,?1)", [r#"{"id":"foreign"}"#]).unwrap();
        assert!(
            matches!(WorkItemsRepository::submission_identity(&connection, &agent), Err(StoreError::InvalidStoredObject { table: "runtime_execution_runs", ref id }) if id == "foreign")
        );
        connection
            .execute("DELETE FROM runtime_execution_runs", [])
            .unwrap();
        let decimal = r#"{"id":"run-1","agent":"worker","accountKey":"default","epoch":4.0,"status":"running","created":1}"#;
        connection.execute("INSERT INTO runtime_execution_runs VALUES ('run-1','worker','default',4,NULL,NULL,1.0,?1)", [decimal]).unwrap();
        assert!(
            matches!(WorkItemsRepository::submission_identity(&connection, &agent), Err(StoreError::InvalidStoredObject { table: "runtime_agents", ref id }) if id == "worker")
        );
        connection
            .execute("DELETE FROM runtime_execution_runs", [])
            .unwrap();
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
    fn submission_identity_uses_last_duplicate_record_values() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,record TEXT);")
            .unwrap();
        let agent = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","id":"worker","provider":"codex","rootId":"lead","epoch":4,"status":"paused","activeExecution":{"runId":"run","attemptId":null}})).unwrap();
        let raw = r#"{"id":"wrong","id":"run","agent":"wrong","agent":"worker","accountKey":"default","epoch":4.0,"epoch":4,"status":"bad","status":"running","created":1}"#;
        db.execute("INSERT INTO runtime_execution_runs VALUES('run',?1)", [raw])
            .unwrap();
        assert_eq!(
            WorkItemsRepository::submission_identity(&db, &agent).unwrap(),
            Some(("run".into(), None))
        );
        let reversed = raw.replace(r#""epoch":4.0,"epoch":4"#, r#""epoch":4,"epoch":4.0"#);
        db.execute("UPDATE runtime_execution_runs SET record=?1", [reversed])
            .unwrap();
        assert!(matches!(
            WorkItemsRepository::submission_identity(&db, &agent),
            Err(StoreError::InvalidStoredObject {
                table: "runtime_agents",
                ..
            })
        ));
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
                .is_some_and(|record| record.status() == crate::WorkStatus::Review)
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
                .expect("original remains")
                .title(),
            Some("original")
        );
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "work_items",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod receipt_domain_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn submission_identity_never_selects_history_without_current_owner() {
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT)").expect("history");
        let record = json!({"id":"old","agent":"agent","accountKey":"default","epoch":1,"threadId":null,"turnId":null,"created":1.0,"status":"pending","latestAttemptId":"old-attempt"});
        db.execute(
            "INSERT INTO runtime_execution_runs VALUES('old','agent','default',1,NULL,NULL,1,?1)",
            [record.to_string()],
        )
        .expect("historical execution");
        let mut agent = AgentRecord::from_value(
            json!({"compactions":0,"accountKey":"default","id":"agent","rootId":"agent","provider":"codex","status":"idle","epoch":1}),
        )
        .expect("agent");
        assert_eq!(
            WorkItemsRepository::submission_identity(&db, &agent).expect("no current owner"),
            None
        );
        agent.extra.insert(
            "activeExecution".into(),
            json!({"runId":"old","attemptId":"old-attempt"}),
        );
        assert_eq!(
            WorkItemsRepository::submission_identity(&db, &agent).expect("current identity"),
            Some(("old".into(), Some("old-attempt".into())))
        );
        agent.extra.insert(
            "activeExecution".into(),
            json!({"runId":"old","attemptId":null}),
        );
        assert_eq!(
            WorkItemsRepository::submission_identity(&db, &agent)
                .expect("current run without attempt"),
            Some(("old".into(), None))
        );
    }
}
