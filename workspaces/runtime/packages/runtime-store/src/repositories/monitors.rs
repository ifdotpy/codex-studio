//! Read persisted monitor records.

use rusqlite::Connection;

use crate::StoreResult;
use crate::records::{MonitorRecord, Table};
use crate::schema::{SchemaRequirements, TableRequirement};

const MONITOR_TABLE: TableRequirement = TableRequirement {
    name: "runtime_monitors",
    columns: &["id", "record"],
};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[MONITOR_TABLE],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct MonitorsRepository;

impl MonitorsRepository {
    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<MonitorRecord>> {
        Table::<MonitorRecord>::new(connection).get(&id.to_owned())
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "monitors",
        requirements: &REQUIREMENTS,
    }
}
