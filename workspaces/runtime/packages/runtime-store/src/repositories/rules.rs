//! Read persisted rule and owner records for the rules list.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::repositories::sync_documents::SyncDocumentsRepository;
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

    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<StoredRule>> {
        connection
            .query_row(
                "SELECT id,record FROM runtime_rules WHERE id=?1",
                [id],
                |row| {
                    Ok(StoredRule {
                        id: row.get(0)?,
                        record: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn agent(connection: &Connection, id: &str) -> StoreResult<Option<StoredAgent>> {
        connection
            .query_row(
                "SELECT id,record FROM runtime_agents WHERE id=?1",
                [id],
                |row| {
                    Ok(StoredAgent {
                        id: row.get(0)?,
                        record: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn put(transaction: &Transaction<'_>, id: &str, record: &str) -> StoreResult<()> {
        let value = serde_json::from_str::<Value>(record)?;
        if value.get("id").and_then(Value::as_str) != Some(id) {
            return Err(crate::StoreError::SchemaMismatch(
                "rule record identity does not match its key".to_owned(),
            ));
        }
        put_rule_entity(
            transaction,
            id,
            value.get("agent"),
            value.get("name"),
            false,
        )?;
        transaction.execute(
            "INSERT INTO runtime_rules(id,record) VALUES(?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record],
        )?;
        Ok(())
    }

    pub fn delete(transaction: &Transaction<'_>, id: &str) -> StoreResult<()> {
        put_rule_entity(transaction, id, None, None, true)?;
        transaction.execute("DELETE FROM runtime_rules WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn has_active_check(connection: &Connection, rule_id: &str) -> StoreResult<bool> {
        let has_table: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_monitors')",
            [],
            |row| row.get(0),
        )?;
        if !has_table {
            return Ok(false);
        }
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_monitors \
                 WHERE json_extract(record,'$.ruleId')=?1 \
                 AND json_extract(record,'$.status') IN ('running','starting','approval'))",
                [rule_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }
}

fn put_rule_entity(
    transaction: &Transaction<'_>,
    id: &str,
    agent: Option<&Value>,
    name: Option<&Value>,
    deleted: bool,
) -> StoreResult<()> {
    let mut value = serde_json::Map::new();
    if !deleted {
        value.insert("id".to_owned(), Value::String(id.to_owned()));
        if let Some(agent) = agent.filter(|value| value.is_string()) {
            value.insert("agent".to_owned(), agent.clone());
        }
        if let Some(name) = name.filter(|value| value.is_string()) {
            value.insert("name".to_owned(), name.clone());
        }
    }
    let payload = serde_json::to_string(&json!({
        "collection": "rule",
        "id": id,
        "value": value,
    }))?;
    SyncDocumentsRepository::put_entity(transaction, "rule", id, &payload, deleted)?;
    Ok(())
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

    #[test]
    fn rule_mutations_keep_the_renderer_entity_in_the_same_transaction() {
        use rusqlite::TransactionBehavior;
        use serde_json::json;

        let mut db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch(
            "CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,
                 PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
             CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);",
        )
        .expect("rule and sync tables create");
        {
            let transaction = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("writer transaction opens");
            RulesRepository::put(
                &transaction,
                "r1",
                &json!({"id":"r1","agent":"a1","name":"Check"}).to_string(),
            )
            .expect("rule is persisted");
            transaction.commit().expect("rule transaction commits");
        }
        let stored: Option<String> = db
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='rule' AND id='r1' AND deleted=0",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(
            stored.as_deref(),
            Some(
                r#"{"collection":"rule","id":"r1","value":{"agent":"a1","id":"r1","name":"Check"}}"#
            )
        );
        {
            let transaction = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("writer transaction opens");
            RulesRepository::delete(&transaction, "r1").expect("rule is removed");
            transaction.commit().expect("delete transaction commits");
        }
        let deleted: Option<i64> = db
            .query_row(
                "SELECT deleted FROM sync_entities WHERE collection='rule' AND id='r1'",
                [],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(deleted, Some(1));
        assert_eq!(
            RulesRepository::get(&db, "r1").expect("rule query works"),
            None
        );
    }

    #[test]
    fn active_check_lookup_is_optional_until_monitors_are_initialized() {
        let db = Connection::open_in_memory().expect("in-memory database opens");
        assert!(!RulesRepository::has_active_check(&db, "r1").expect("missing table is allowed"));
        db.execute_batch(
            "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_monitors VALUES
                 ('m1','{\"ruleId\":\"r1\",\"status\":\"running\"}');",
        )
        .expect("monitor table creates");
        assert!(RulesRepository::has_active_check(&db, "r1").expect("active check is found"));
        assert!(!RulesRepository::has_active_check(&db, "r2").expect("other rule has no check"));
    }
}
