//! Read persisted rule and owner records for the rules list.

use rusqlite::{Connection, params};

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[
        TableRequirement {
            name: "runtime_rules",
            columns: &["id", "record"],
        },
        TableRequirement {
            name: "runtime_agents",
            columns: &["id", "record"],
        },
    ],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct StoredRule {
    pub id: String,
    pub record: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredAgent {
    pub id: String,
    pub record: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RulesRepository;

impl RulesRepository {
    pub fn list(connection: &Connection) -> StoreResult<(Vec<StoredRule>, Vec<StoredAgent>)> {
        let rules = read_records(connection, "runtime_rules")?
            .into_iter()
            .map(|(id, record)| StoredRule { id, record })
            .collect();
        let agents = read_records(connection, "runtime_agents")?
            .into_iter()
            .map(|(id, record)| StoredAgent { id, record })
            .collect();
        Ok((rules, agents))
    }
}

fn read_records(connection: &Connection, table: &str) -> StoreResult<Vec<(String, String)>> {
    let mut statement =
        connection.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    let rows = statement.query_map(params![], |row| {
        let id: String = row.get(0)?;
        let record: String = row.get(1)?;
        Ok((id, record))
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use super::{REQUIREMENTS, RulesRepository};
    use rusqlite::Connection;

    #[test]
    fn rule_repository_reads_only_declared_tables_in_python_row_order() {
        let db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch(
            "CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_rules VALUES('r2','{\"id\":\"r2\"}');
             INSERT INTO runtime_rules VALUES('r1','{\"id\":\"r1\"}');
             INSERT INTO runtime_agents VALUES('a1','{\"id\":\"a1\"}');",
        )
        .expect("declared tables create");

        let (rules, agents) = RulesRepository::list(&db).expect("records read");
        assert_eq!(
            rules
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["r2", "r1"]
        );
        assert_eq!(agents[0].id, "a1");
        assert!(
            REQUIREMENTS
                .tables
                .iter()
                .any(|table| table.name == "runtime_rules")
        );
        assert!(
            REQUIREMENTS
                .tables
                .iter()
                .any(|table| table.name == "runtime_agents")
        );
    }
}
