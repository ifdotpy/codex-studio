//! Persisted questions, complaints, and workspace checkpoint records.

use crate::StoreResult;
use crate::records::{AgentRecord, CheckpointRecord, ComplaintRecord, RequestRecord, Table};
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
    use rusqlite::Connection;
    use serde_json::json;

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
