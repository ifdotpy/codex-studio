//! Narrow read projections used by the capabilities endpoint.

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{Connection, StoreResult};

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

pub struct CapabilityReadsRepository;

impl CapabilityReadsRepository {
    /// Return only the task names/types observed for one agent. History records are never decoded.
    pub fn observed_names(connection: &Connection, agent_id: &str) -> StoreResult<Vec<String>> {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_tasks')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let name = "CASE WHEN json_type(record,'$.name') IS NOT NULL THEN json_extract(record,'$.name') WHEN json_type(record,'$.type') IS NOT NULL THEN json_extract(record,'$.type') ELSE '' END";
        let sql = format!(
            "SELECT {name} FROM runtime_tasks INDEXED BY runtime_task_history WHERE json_extract(record,'$.status')!='running' AND json_extract(record,'$.agent')=? UNION SELECT {name} FROM runtime_tasks INDEXED BY runtime_task_status WHERE (json_extract(record,'$.status')='running' OR json_extract(record,'$.status') IS NULL) AND json_extract(record,'$.agent')=? ORDER BY 1"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map([agent_id, agent_id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "capability_reads",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::CapabilityReadsRepository;
    use crate::Connection;
    use serde_json::json;

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'),json_extract(record,'$.created'));
                 CREATE INDEX runtime_task_history ON runtime_tasks(json_extract(record,'$.created') DESC,json_extract(record,'$.agent')) WHERE json_extract(record,'$.status')!='running';",
            )
            .expect("task indexes create");
        connection
    }

    #[test]
    fn names_are_agent_scoped_projected_and_deduplicated_across_statuses() {
        let connection = database();
        for (id, record) in [
            (
                "a",
                json!({"agent":"selected","status":"completed","name":"old"}),
            ),
            (
                "b",
                json!({"agent":"selected","status":"running","name":"live"}),
            ),
            (
                "c",
                json!({"agent":"selected","status":null,"type":"fallback"}),
            ),
            (
                "d",
                json!({"agent":"other","status":"completed","name":"foreign","large":"x".repeat(100_000)}),
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO runtime_tasks(id,record) VALUES (?1,?2)",
                    (id, record.to_string()),
                )
                .expect("task record inserts");
        }
        assert_eq!(
            CapabilityReadsRepository::observed_names(&connection, "selected")
                .expect("names project"),
            vec!["fallback", "live", "old"]
        );
    }

    #[test]
    fn absent_task_table_is_an_empty_projection() {
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        assert_eq!(
            CapabilityReadsRepository::observed_names(&connection, "agent")
                .expect("missing table is empty"),
            Vec::<String>::new()
        );
    }
}
