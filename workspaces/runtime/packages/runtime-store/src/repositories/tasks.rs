use rusqlite::Connection;

use crate::StoreResult;
use crate::records::{Table, TaskId, TaskRecord, TaskRepository};
use crate::schema::{SchemaRequirements, TableRequirement};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TableRequirement {
        name: "runtime_tasks",
        columns: &["id", "record"],
    }],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

impl TaskRepository {
    /// Create the task indexes Rust readers rely on. The newest-tasks-per-agent
    /// order serves storage retention and agent task history; Python startup
    /// creates the same index, so existing state directories keep theirs.
    pub(crate) fn prepare_indexes(transaction: &rusqlite::Transaction<'_>) -> StoreResult<()> {
        for definition in Self::index_definitions() {
            transaction.execute_batch(&definition.create_sql)?;
        }
        Ok(())
    }

    /// Canonical DDL for the task repository's owned indexes.
    #[doc(hidden)]
    pub fn index_definitions() -> Vec<crate::OwnedRuntimeIndexDefinition> {
        vec![
            crate::OwnedRuntimeIndexDefinition {
                name: "runtime_task_agent_created_id",
                table: "runtime_tasks",
                create_sql: "CREATE INDEX IF NOT EXISTS runtime_task_agent_created_id \
                    ON runtime_tasks(json_extract(record,'$.agent'), \
                    json_extract(record,'$.created') DESC, id DESC)"
                    .to_owned(),
            },
            // Running tasks and status reads of the renderer task window.
            crate::OwnedRuntimeIndexDefinition {
                name: "runtime_task_status",
                table: "runtime_tasks",
                create_sql: "CREATE INDEX IF NOT EXISTS runtime_task_status \
                    ON runtime_tasks(json_extract(record,'$.status'), \
                    json_extract(record,'$.created'))"
                    .to_owned(),
            },
            // Newest finished tasks of the renderer task window and history reads.
            crate::OwnedRuntimeIndexDefinition {
                name: "runtime_task_history",
                table: "runtime_tasks",
                create_sql: "CREATE INDEX IF NOT EXISTS runtime_task_history \
                    ON runtime_tasks(json_extract(record,'$.created') DESC, \
                    json_extract(record,'$.agent')) \
                    WHERE json_extract(record,'$.status')!='running'"
                    .to_owned(),
            },
        ]
    }

    /// Schema-checked running tasks for one agent, in stable row order.
    pub fn running_for_agent(connection: &Connection, agent: &str) -> StoreResult<Vec<TaskRecord>> {
        let mut statement = connection.prepare_cached(
            "SELECT id,record FROM runtime_tasks WHERE json_extract(record,'$.agent')=?1 \
             AND json_extract(record,'$.status')='running' ORDER BY id",
        )?;
        statement
            .query_map([agent], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, raw) = row?;
                Table::<TaskRecord>::new(connection).decode_stored(&id, &raw)
            })
            .collect()
    }

    pub fn get(connection: &Connection, id: &TaskId) -> StoreResult<Option<TaskRecord>> {
        Table::<TaskRecord>::new(connection).get(id)
    }

    pub fn put(connection: &Connection, record: &TaskRecord) -> StoreResult<()> {
        Table::<TaskRecord>::new(connection).put(record)
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "tasks",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod typed_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_rows_preserve_presence_extensions_and_check_identity() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        let value = json!({"id":"a","agent":"agent","created":1.0,"status":null,"kind":"future","tail":null,"output":{"data":[1]},"extension":true});
        let record = TaskRecord::from_value(value.clone()).unwrap();
        TaskRepository::put(&db, &record).unwrap();
        assert_eq!(
            crate::WorkspaceViewsRepository::task(&db, "a").unwrap(),
            Some(value.clone())
        );
        let page =
            crate::WorkspaceViewsRepository::task_page(&db, &["agent".into()], None, None, 10)
                .unwrap();
        assert_eq!(page.into_iter().next().unwrap().decode(&db).unwrap(), value);
        assert_eq!(
            TaskRepository::get(&db, &TaskId::from("a"))
                .unwrap()
                .unwrap()
                .into_value(),
            value
        );
        db.execute(
            "UPDATE runtime_tasks SET record=?1",
            [r#"{"id":"b","status":"running"}"#],
        )
        .unwrap();
        assert!(TaskRepository::get(&db, &TaskId::from("a")).is_err());
        db.execute(
            "UPDATE runtime_tasks SET record=?1",
            [r#"{"id":"a","created":-1}"#],
        )
        .unwrap();
        assert!(TaskRepository::get(&db, &TaskId::from("a")).is_err());
    }
}
