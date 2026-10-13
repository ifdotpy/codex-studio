//! Read and write persisted work-item, plan, and queue state.

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::records::{
    EventMetadataRecord, JsonRecord, QueueEventId, Table, TaskId, TranscriptAssetId,
    TranscriptAssetRecord, WorkItemRecord,
};
use crate::schema::{SchemaRequirements, TableRequirement};
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

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: TABLES,
    conditional_tables: &[],
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

#[derive(Clone, Copy, Debug, Default)]
pub struct WorkItemsRepository;

impl WorkItemsRepository {
    pub fn work_record(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Ok(Table::<WorkItemRecord>::new(connection)
            .get(&TaskId::from(id))?
            .map(|record| record.value().clone()))
    }

    pub fn put_work(connection: &Connection, id: &str, value: &Value) -> StoreResult<()> {
        let record = WorkItemRecord::from_value(value.clone())?;
        if record.id().as_ref() != id {
            return Err(StoreError::InvalidStoredObject {
                table: WorkItemRecord::TABLE,
                id: id.to_owned(),
            });
        }
        Table::<WorkItemRecord>::new(connection).put(&record)
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

    pub fn work(connection: &Connection) -> StoreResult<Vec<Value>> {
        Ok(Table::<WorkItemRecord>::new(connection)
            .list()?
            .into_iter()
            .map(|record| record.value().clone())
            .collect())
    }

    pub fn plan(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Ok(Table::<crate::PlanRecord>::new(connection)
            .get(&crate::AgentId::from(id))?
            .map(|record| record.value().clone()))
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
    use rusqlite::Connection;

    #[test]
    fn bootstrapped_tables_are_required_and_empty_reads_succeed() {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        for table in REQUIREMENTS.tables {
            let columns = table.columns.join(" TEXT,");
            connection
                .execute_batch(&format!("CREATE TABLE {}({columns} TEXT)", table.name))
                .expect("required family creates");
        }
        assert!(REQUIREMENTS.conditional_tables.is_empty());
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
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "work_items",
        requirements: &REQUIREMENTS,
    }
}
