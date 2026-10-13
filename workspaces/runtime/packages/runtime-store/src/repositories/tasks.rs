use rusqlite::Connection;

use crate::StoreResult;
use crate::records::{TaskId, TaskRecord, TaskRepository, get_json_record, put_json_record};
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
    pub fn get(connection: &Connection, id: &TaskId) -> StoreResult<Option<TaskRecord>> {
        get_json_record(connection, "runtime_tasks", &id.0)
    }

    pub fn put(connection: &Connection, record: &TaskRecord) -> StoreResult<()> {
        put_json_record(connection, "runtime_tasks", &record.id.0, record)
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "tasks",
        requirements: &REQUIREMENTS,
    }
}
