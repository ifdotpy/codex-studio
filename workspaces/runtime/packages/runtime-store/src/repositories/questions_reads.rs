//! Persisted questions, complaints, and workspace checkpoint records.

use crate::StoreResult;
use crate::records::{
    AgentId, AgentRecord, CheckpointRecord, ComplaintRecord, RecordOrder, RecordPredicate,
    RequestRecord, Table,
};
use crate::schema::{SchemaRequirements, TableRequirement};
use rusqlite::Connection;

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_agents",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_requests",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_complaints",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_checkpoints",
        columns: &["id", "record"],
    },
];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TABLES[0], TABLES[1], TABLES[2]],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TABLES[3]],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[cfg(test)]
mod tests {
    use super::{QuestionsReadsRepository, REQUIREMENTS};
    use crate::StoreResult;
    use rusqlite::Connection;
    use serde_json::json;

    /// The indexed family and request reads return what the old read-everything path
    /// returned for that family, and plan on the existing indexes.
    #[tokio::test]
    async fn family_reads_match_the_full_scan_and_use_indexes() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = crate::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        store
            .transaction_with(|tx| -> StoreResult<()> {
                for (id, root, deleted) in [
                    ("r1", "r1", false),
                    ("c1", "r1", false),
                    ("c2", "r1", true),
                    ("r2", "r2", false),
                    ("c3", "r2", false),
                ] {
                    let record = json!({"id": id, "rootId": root, "epoch": 0, "status": "idle",
                        "deletedAt": deleted.then_some(1)});
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        [id, &record.to_string()],
                    )?;
                }
                for (id, agent) in [
                    ("q1", "r1"),
                    ("q2", "c1"),
                    ("q3", "c2"),
                    ("q4", "r2"),
                    ("q5", "c3"),
                ] {
                    let record = json!({"id": id, "agent": agent, "method": "agent/asyncQuestion",
                        "status": "pending"});
                    tx.execute(
                        "INSERT INTO runtime_requests(id,record) VALUES(?1,?2)",
                        [id, &record.to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seed corpus");
        let connection = store.read().expect("reader");
        let family = QuestionsReadsRepository::agent_family(&connection, "c1").expect("family");
        let family_ids = family
            .iter()
            .map(|a| a.id().as_ref().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(family_ids, ["c1", "c2", "r1"]);
        assert!(
            QuestionsReadsRepository::agent_family(&connection, "missing")
                .expect("family")
                .is_empty()
        );
        let indexed = QuestionsReadsRepository::requests_of_agents(&connection, &family)
            .expect("indexed requests");
        let scanned = QuestionsReadsRepository::questions(&connection)
            .expect("all requests")
            .into_iter()
            .filter(|(request, _)| {
                request.value()["agent"]
                    .as_str()
                    .is_some_and(|agent| family_ids.iter().any(|id| id == agent))
            })
            .collect::<Vec<_>>();
        assert_eq!(indexed, scanned);
        for (sql, index) in [
            (
                "SELECT id,record FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1",
                "runtime_agent_root",
            ),
            (
                "SELECT id,record FROM runtime_requests WHERE json_extract(record,'$.agent') IN (?1,?2)",
                "runtime_request_agent_status",
            ),
        ] {
            let plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("plan prepares")
                .query_map(
                    rusqlite::params_from_iter(["a", "b"].iter().take(sql.matches('?').count())),
                    |row| row.get::<_, String>(3),
                )
                .expect("plan runs")
                .collect::<Result<Vec<_>, _>>()
                .expect("plan rows")
                .join(" ");
            assert!(plan.contains(index), "{plan}");
        }
    }

    #[test]
    fn bootstrapped_record_families_are_required() {
        assert_eq!(
            REQUIREMENTS
                .tables
                .iter()
                .map(|table| table.name)
                .collect::<Vec<_>>(),
            ["runtime_agents", "runtime_requests", "runtime_complaints"]
        );
        assert_eq!(REQUIREMENTS.bootstrap_tables[0].name, "runtime_checkpoints");
        assert!(REQUIREMENTS.conditional_tables.is_empty());
    }

    #[test]
    fn reads_validated_records_and_preserves_request_property_order() {
        let db = Connection::open_in_memory().expect("memory SQLite opens");
        db.execute_batch(
            "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_checkpoints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_agents VALUES('a','{\"id\":\"a\",\"rootId\":\"a\",\"epoch\":0,\"status\":\"idle\"}');
             INSERT INTO runtime_requests VALUES('q','{\"id\":\"q\",\"agent\":\"a\",\"method\":\"mcpServer/elicitation/request\",\"status\":\"pending\",\"params\":{\"mode\":\"form\",\"requestedSchema\":{\"properties\":{\"password\":{},\"color\":{}}}}}');
             INSERT INTO runtime_complaints VALUES('c','{\"id\":\"c\",\"leadId\":\"a\",\"author\":\"user\",\"text\":\"Review\",\"status\":\"open\",\"created\":1}');
             INSERT INTO runtime_checkpoints VALUES('p','{\"id\":\"p\",\"agent\":\"a\",\"created\":1,\"items\":[\"hidden\"]}');",
        )
        .expect("families create");

        assert_eq!(
            QuestionsReadsRepository::agents(&db).expect("agents read")[0]
                .id()
                .as_ref(),
            "a"
        );
        let questions = QuestionsReadsRepository::questions(&db).expect("questions read");
        let question = &questions[0].1;
        assert!(
            question.find("password").expect("password field")
                < question.find("color").expect("color field")
        );
        assert_eq!(
            QuestionsReadsRepository::complaint(&db, "c")
                .expect("complaint read")
                .expect("complaint exists")
                .value()["text"],
            json!("Review")
        );
        assert_eq!(
            QuestionsReadsRepository::checkpoints(&db).expect("checkpoint read")[0].id(),
            "p"
        );
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QuestionsReadsRepository;

impl QuestionsReadsRepository {
    pub fn agents(connection: &Connection) -> StoreResult<Vec<AgentRecord>> {
        Table::<AgentRecord>::new(connection).list()
    }

    pub fn questions(connection: &Connection) -> StoreResult<Vec<(RequestRecord, String)>> {
        Table::<RequestRecord>::new(connection).list_with_raw()
    }

    /// The agent and every agent sharing its root (deleted ones included), by the
    /// `runtime_agent_root` index; empty when the agent does not exist.
    pub fn agent_family(connection: &Connection, agent_id: &str) -> StoreResult<Vec<AgentRecord>> {
        let table = Table::<AgentRecord>::new(connection);
        let Some(owner) = table.get(&AgentId::from(agent_id.to_owned()))? else {
            return Ok(Vec::new());
        };
        let mut statement = connection.prepare_cached(
            "SELECT id,record FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1 ORDER BY id",
        )?;
        let rows = statement
            .query_map([owner.root_id.as_ref()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.iter()
            .map(|(id, encoded)| table.decode_stored(id, encoded))
            .collect()
    }

    /// Every stored request of the given agents, by the `runtime_request_agent_status` index.
    pub fn requests_of_agents(
        connection: &Connection,
        agents: &[AgentRecord],
    ) -> StoreResult<Vec<(RequestRecord, String)>> {
        let ids = agents
            .iter()
            .map(|agent| agent.id().as_ref().to_owned())
            .collect::<Vec<_>>();
        Table::<RequestRecord>::new(connection)
            .list_with_raw_matching(Some(RecordPredicate::JsonAgentIn(&ids)), RecordOrder::RowId)
    }

    pub fn complaint(connection: &Connection, id: &str) -> StoreResult<Option<ComplaintRecord>> {
        Table::<ComplaintRecord>::new(connection).get(&id.to_owned())
    }

    pub fn checkpoints(connection: &Connection) -> StoreResult<Vec<CheckpointRecord>> {
        Table::<CheckpointRecord>::new(connection).list()
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "questions_reads",
        requirements: &REQUIREMENTS,
    }
}
