//! Read persisted rule and owner records for the rules list.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StoreResult;
use crate::records::{AgentRecord, JsonRecord, Table};
use crate::schema::{SchemaRequirements, TableRequirement};

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

#[derive(Clone, Debug, PartialEq)]
struct RuleRecord {
    id: String,
    value: Value,
}

impl Serialize for RuleRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RuleRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("record id must be a string"))?;
        Ok(Self {
            id: id.to_owned(),
            value,
        })
    }
}

impl JsonRecord for RuleRecord {
    type Id = String;

    const TABLE: &'static str = "runtime_rules";
    const SCHEMA: &'static str = "JsonObject";

    fn id(&self) -> &Self::Id {
        &self.id
    }
}

impl RulesRepository {
    pub fn list(connection: &Connection) -> StoreResult<(Vec<StoredRule>, Vec<StoredAgent>)> {
        let rules = Table::<RuleRecord>::new(connection)
            .list()?
            .into_iter()
            .map(|record| {
                Ok(StoredRule {
                    id: record.id,
                    record: serde_json::to_string(&record.value)?,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        let agents = Table::<AgentRecord>::new(connection)
            .list()?
            .into_iter()
            .map(|record| {
                Ok(StoredAgent {
                    id: record.id().0.clone(),
                    record: serde_json::to_string(&record)?,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        Ok((rules, agents))
    }
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
