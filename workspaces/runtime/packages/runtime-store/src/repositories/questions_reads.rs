//! Persisted questions, complaints, and workspace checkpoint records.

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::StoreResult;
use crate::records::{AgentRecord, CheckpointRecord, ComplaintRecord, RequestRecord, Table};
use crate::schema::{SchemaRequirements, TableRequirement};

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
    tables: TABLES,
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TABLES[3]],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct QuestionsReadsRecord {
    pub id: String,
    pub value: Map<String, Value>,
}

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
            [
                "runtime_agents",
                "runtime_requests",
                "runtime_complaints",
                "runtime_checkpoints"
            ]
        );
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
            QuestionsReadsRepository::agents(&db).expect("agents read")[0].id,
            "a"
        );
        let question = &QuestionsReadsRepository::questions(&db).expect("questions read")[0];
        assert!(
            question.raw.find("password").expect("password field")
                < question.raw.find("color").expect("color field")
        );
        assert_eq!(
            QuestionsReadsRepository::complaint(&db, "c")
                .expect("complaint read")
                .expect("complaint exists")
                .value["text"],
            json!("Review")
        );
        assert_eq!(
            QuestionsReadsRepository::checkpoints(&db).expect("checkpoint read")[0].id,
            "p"
        );
    }
}

pub struct QuestionsRequestRecord {
    pub id: String,
    pub raw: String,
    pub value: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QuestionsReadsRepository;

impl QuestionsReadsRepository {
    pub fn agents(connection: &Connection) -> StoreResult<Vec<QuestionsReadsRecord>> {
        Table::<AgentRecord>::new(connection)
            .list()?
            .into_iter()
            .map(|record| {
                let id = record.id().as_ref().to_owned();
                let value = record.value();
                let value = value.as_object().cloned().ok_or_else(|| {
                    crate::StoreError::InvalidStoredObject {
                        table: "runtime_agents",
                        id: id.clone(),
                    }
                })?;
                Ok(QuestionsReadsRecord { id, value })
            })
            .collect::<Result<_, crate::StoreError>>()
    }

    pub fn questions(connection: &Connection) -> StoreResult<Vec<QuestionsRequestRecord>> {
        Table::<RequestRecord>::new(connection)
            .list_with_raw()?
            .into_iter()
            .map(|(record, raw)| {
                let id = record.id().to_owned();
                let value = record.value().as_object().cloned().ok_or_else(|| {
                    crate::StoreError::InvalidStoredObject {
                        table: "runtime_requests",
                        id: id.clone(),
                    }
                })?;
                Ok(QuestionsRequestRecord { id, raw, value })
            })
            .collect::<Result<_, crate::StoreError>>()
    }

    pub fn complaint(
        connection: &Connection,
        id: &str,
    ) -> StoreResult<Option<QuestionsReadsRecord>> {
        let Some(record) = Table::<ComplaintRecord>::new(connection).get(&id.to_owned())? else {
            return Ok(None);
        };
        let value = record.value().as_object().cloned().ok_or_else(|| {
            crate::StoreError::InvalidStoredObject {
                table: "runtime_complaints",
                id: id.to_owned(),
            }
        })?;
        Ok(Some(QuestionsReadsRecord {
            id: record.id().to_owned(),
            value,
        }))
    }

    pub fn checkpoints(connection: &Connection) -> StoreResult<Vec<QuestionsReadsRecord>> {
        Table::<CheckpointRecord>::new(connection)
            .list()?
            .into_iter()
            .map(|record| {
                let id = record.id().to_owned();
                let value = record.value().as_object().cloned().ok_or_else(|| {
                    crate::StoreError::InvalidStoredObject {
                        table: "runtime_checkpoints",
                        id: id.clone(),
                    }
                })?;
                Ok(QuestionsReadsRecord { id, value })
            })
            .collect::<Result<_, crate::StoreError>>()
    }
}
