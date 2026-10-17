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
