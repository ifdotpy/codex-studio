//! Read persisted rule and owner records for the rules list.

use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};

use crate::StoreResult;
use crate::records::{AgentId, AgentRecord, MonitorRecord, RuleRecord, Table};
use crate::repositories::sync_documents::SyncDocumentsRepository;
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
        TableRequirement {
            name: "runtime_monitors",
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

#[derive(Clone, Copy, Debug, Default)]
pub struct RulesRepository;

impl RulesRepository {
    pub fn list(connection: &Connection) -> StoreResult<(Vec<RuleRecord>, Vec<AgentRecord>)> {
        Ok((
            Table::<RuleRecord>::new(connection).list()?,
            Table::<AgentRecord>::new(connection).list()?,
        ))
    }

    pub fn get(connection: &Connection, id: &str) -> StoreResult<Option<RuleRecord>> {
        Table::<RuleRecord>::new(connection).get(&id.to_owned())
    }

    pub fn agent(connection: &Connection, id: &str) -> StoreResult<Option<AgentRecord>> {
        Table::<AgentRecord>::new(connection).get(&AgentId::from(id))
    }

    pub fn put(transaction: &Transaction<'_>, record: &RuleRecord) -> StoreResult<()> {
        let id = record.id().as_ref();
        let value = record.value();
        put_rule_entity(
            transaction,
            id,
            value.get("agent"),
            value.get("name"),
            false,
        )?;
        Table::<RuleRecord>::new(transaction).put(record)
    }

    pub fn delete(transaction: &Transaction<'_>, id: &str) -> StoreResult<()> {
        put_rule_entity(transaction, id, None, None, true)?;
        Table::<RuleRecord>::new(transaction).delete(&id.to_owned())?;
        Ok(())
    }

    pub fn has_active_check(connection: &Connection, rule_id: &str) -> StoreResult<bool> {
        Ok(Table::<MonitorRecord>::new(connection)
            .list()?
            .iter()
            .any(|monitor| {
                monitor.get("ruleId").and_then(Value::as_str) == Some(rule_id)
                    && monitor
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| matches!(status, "running" | "starting" | "approval"))
            }))
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
    SyncDocumentsRepository::put_entity_payload(transaction, "rule", id, &payload, deleted)?;
    Ok(())
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
             INSERT INTO runtime_rules VALUES('r2','{\"id\":\"r2\",\"kind\":\"interval\"}');
             INSERT INTO runtime_rules VALUES('r1','{\"id\":\"r1\",\"kind\":\"interval\"}');
             INSERT INTO runtime_agents VALUES('a1','{\"id\":\"a1\",\"rootId\":\"a1\",\"epoch\":0,\"status\":\"idle\"}');",
        )
        .expect("declared tables create");

        let (rules, agents) = RulesRepository::list(&db).expect("records read");
        assert_eq!(
            rules
                .iter()
                .map(|item| item.id().as_str())
                .collect::<Vec<_>>(),
            ["r2", "r1"]
        );
        assert_eq!(agents[0].id().0, "a1");
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
        use crate::records::RuleRecord;
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
                &RuleRecord::from_value(
                    json!({"id":"r1","agent":"a1","name":"Check","kind":"interval"}),
                )
                .expect("rule has an id"),
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
    fn active_check_lookup_uses_typed_monitor_records() {
        let db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch(
            "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_monitors VALUES
                 ('m1','{\"id\":\"m1\",\"agent\":\"a1\",\"ruleId\":\"r1\",\"status\":\"running\",\"created\":1}');",
        )
        .expect("monitor table creates");
        assert!(RulesRepository::has_active_check(&db, "r1").expect("active check is found"));
        assert!(!RulesRepository::has_active_check(&db, "r2").expect("other rule has no check"));
    }
}
