//! Read persisted rule and owner records for the rules list.

use rusqlite::{Connection, Transaction};
use serde::Serialize;

use crate::StoreResult;
use crate::records::{AgentId, AgentRecord, RuleRecord, Table};
use crate::repositories::sync_documents::SyncDocumentsRepository;
use crate::schema::{SchemaRequirements, TableRequirement};

const RULE_TABLE: TableRequirement = TableRequirement {
    name: "runtime_rules",
    columns: &["id", "record"],
};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[
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
    bootstrap_tables: &[RULE_TABLE],
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

    pub fn publish_entity(transaction: &Transaction<'_>, record: &RuleRecord) -> StoreResult<()> {
        put_rule_entity(
            transaction,
            record.id(),
            Some(&record.agent),
            Some(record.name.as_str()),
            false,
        )
    }

    pub fn put(transaction: &Transaction<'_>, record: &RuleRecord) -> StoreResult<()> {
        Self::publish_entity(transaction, record)?;
        Table::<RuleRecord>::new(transaction).put(record)
    }

    pub fn delete(transaction: &Transaction<'_>, id: &str) -> StoreResult<()> {
        put_rule_entity(transaction, id, None, None, true)?;
        Table::<RuleRecord>::new(transaction).delete(&id.to_owned())?;
        Ok(())
    }

    pub fn active_check_ids(connection: &Connection, rule_id: &str) -> StoreResult<Vec<String>> {
        let mut statement = connection.prepare(
            "SELECT id FROM runtime_monitors WHERE \
             CASE WHEN json_valid(record) THEN \
             json_extract(record,'$.ruleId')=?1 AND \
             json_extract(record,'$.status') IN ('running','starting','approval') \
             ELSE 0 END ORDER BY rowid",
        )?;
        statement
            .query_map([rule_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

fn put_rule_entity(
    transaction: &Transaction<'_>,
    id: &str,
    agent: Option<&str>,
    name: Option<&str>,
    deleted: bool,
) -> StoreResult<()> {
    #[derive(Serialize)]
    struct RuleProjection<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
    }
    #[derive(Serialize)]
    struct Envelope<'a> {
        collection: &'static str,
        id: &'a str,
        value: RuleProjection<'a>,
    }
    let value = if deleted {
        RuleProjection {
            id: None,
            agent: None,
            name: None,
        }
    } else {
        RuleProjection {
            id: Some(id),
            agent,
            name,
        }
    };
    let payload = serde_json::to_string(&Envelope {
        collection: "rule",
        id,
        value,
    })?;
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
            &crate::agent_columns::fixture_sql("CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_rules VALUES('r2','{\"id\":\"r2\",\"agent\":\"a1\",\"epoch\":0,\"name\":\"Rule\",\"kind\":\"interval\",\"status\":\"active\",\"intervalSeconds\":60,\"nextAt\":1,\"created\":1,\"command\":\"\",\"text\":\"\",\"checks\":0,\"wakes\":0,\"inFlight\":false,\"fileGeneration\":0,\"stallTimeoutSeconds\":1800,\"stallWakeGeneration\":-1,\"livenessCommand\":\"\",\"event\":\"\"}');
             INSERT INTO runtime_rules VALUES('r1','{\"id\":\"r1\",\"agent\":\"a1\",\"epoch\":0,\"name\":\"Rule\",\"kind\":\"interval\",\"status\":\"active\",\"intervalSeconds\":60,\"nextAt\":1,\"created\":1,\"command\":\"\",\"text\":\"\",\"checks\":0,\"wakes\":0,\"inFlight\":false,\"fileGeneration\":0,\"stallTimeoutSeconds\":1800,\"stallWakeGeneration\":-1,\"livenessCommand\":\"\",\"event\":\"\"}');
             INSERT INTO runtime_agents(id,record) VALUES('a1','{\"id\":\"a1\",\"provider\":\"codex\",\"accountKey\":\"default\",\"rootId\":\"a1\",\"epoch\":0,\"status\":\"idle\",\"compactions\":0}');"),
        )
        .expect("declared tables create");
        db.execute_batch(crate::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");

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
                .bootstrap_tables
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

        let mut db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch(
            "CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,
                 PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
             CREATE TABLE durable_rows(seq INTEGER); CREATE TABLE sync_generation(id INTEGER PRIMARY KEY,value INTEGER NOT NULL); INSERT INTO sync_generation VALUES(1,0);",
        )
        .expect("rule and sync tables create");
        {
            let transaction = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("writer transaction opens");
            RulesRepository::put(
                &transaction,
                &serde_json::from_str::<RuleRecord>(r#"{"id":"r1","agent":"a1","epoch":0,"name":"Check","kind":"interval","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","checks":0,"wakes":0,"inFlight":false,"fileGeneration":0,"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"livenessCommand":"","event":""}"#)
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
    fn failed_rule_write_rolls_back_its_sync_entity_in_the_same_transaction() {
        use crate::records::RuleRecord;
        use rusqlite::OptionalExtension;

        let mut db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch(
            "CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,
                 PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
             CREATE TABLE durable_rows(seq INTEGER); CREATE TABLE sync_generation(id INTEGER PRIMARY KEY,value INTEGER NOT NULL); INSERT INTO sync_generation VALUES(1,0);
             CREATE TRIGGER fail_rule_insert BEFORE INSERT ON runtime_rules
                 BEGIN SELECT RAISE(ABORT,'injected'); END;",
        )
        .expect("rule and sync tables and failure trigger create");

        let transaction = db.transaction().expect("writer transaction opens");
        let error = RulesRepository::put(
            &transaction,
            &serde_json::from_str::<RuleRecord>(r#"{"id":"rollback-rule","agent":"a1","epoch":0,"name":"Rollback","kind":"interval","status":"active","intervalSeconds":60,"nextAt":1,"created":1,"command":"","text":"","checks":0,"wakes":0,"inFlight":false,"fileGeneration":0,"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"livenessCommand":"","event":""}"#)
            .expect("rule has a valid ID"),
        )
        .expect_err("injected rule storage failure is returned");
        assert!(matches!(error, crate::StoreError::Sqlite(_)));
        drop(transaction);

        let rule: Option<i64> = db
            .query_row(
                "SELECT 1 FROM runtime_rules WHERE id='rollback-rule'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("rule table remains readable");
        let sync_entity: Option<i64> = db
            .query_row(
                "SELECT 1 FROM sync_entities WHERE collection='rule' AND id='rollback-rule'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("sync entity table remains readable");
        assert_eq!(rule, None);
        assert_eq!(sync_entity, None);
    }

    #[test]
    fn active_check_lookup_is_filtered_and_ignores_unrelated_invalid_rows() {
        let db = Connection::open_in_memory().expect("in-memory database opens");
        db.execute_batch(
            "CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0);
             INSERT INTO runtime_monitors(id,record) VALUES
                 ('m1','{\"id\":\"m1\",\"agent\":\"a1\",\"ruleId\":\"r1\",\"status\":\"running\",\"created\":1}'),
                 ('unrelated','{\"id\":\"unrelated\",\"status\":false}');",
        )
        .expect("monitor table creates");
        assert_eq!(
            RulesRepository::active_check_ids(&db, "r1").expect("active check is found"),
            ["m1"]
        );
        assert!(
            RulesRepository::active_check_ids(&db, "r2")
                .expect("other rule has no check")
                .is_empty()
        );
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "rules",
        requirements: &REQUIREMENTS,
    }
}
