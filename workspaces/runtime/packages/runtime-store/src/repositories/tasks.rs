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
    pub fn get(connection: &Connection, id: &TaskId) -> StoreResult<Option<TaskRecord>> {
        Table::<TaskRecord>::new(connection).get(id)
    }

    pub fn put(connection: &Connection, record: &TaskRecord) -> StoreResult<()> {
        Table::<TaskRecord>::new(connection).put(record)
    }
}
