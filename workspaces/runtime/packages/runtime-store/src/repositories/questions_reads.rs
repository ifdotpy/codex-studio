//! Persisted questions, complaints, and workspace checkpoint records.

use rusqlite::{Connection, OptionalExtension};

use crate::records::get_raw_record;
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const REQUESTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_requests",
    columns: &["id", "record"],
}];
const COMPLAINTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_complaints",
    columns: &["id", "record"],
}];
const CHECKPOINTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_checkpoints",
    columns: &["id", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_requests",
            tables: REQUESTS,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_complaints",
            tables: COMPLAINTS,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_checkpoints",
            tables: CHECKPOINTS,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuestionsReadsRecord {
    pub id: String,
    pub record: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QuestionsReadsRepository;

type OptionalRecord = Option<QuestionsReadsRecord>;

impl QuestionsReadsRepository {
    pub fn agents(connection: &Connection) -> StoreResult<Vec<QuestionsReadsRecord>> {
        read_rows(connection, "runtime_agents")
    }

    pub fn questions(connection: &Connection) -> StoreResult<Vec<QuestionsReadsRecord>> {
        read_records(connection, "runtime_requests")
    }

    pub fn complaint(
        connection: &Connection,
        id: &str,
    ) -> StoreResult<Option<QuestionsReadsRecord>> {
        read_record(connection, "runtime_complaints", id)
    }

    pub fn checkpoints(connection: &Connection) -> StoreResult<Vec<QuestionsReadsRecord>> {
        read_records(connection, "runtime_checkpoints")
    }
}

fn read_records(connection: &Connection, table: &str) -> StoreResult<Vec<QuestionsReadsRecord>> {
    if table_exists(connection, table)?.is_none() {
        return Ok(Vec::new());
    }
    read_rows(connection, table)
}

fn read_rows(connection: &Connection, table: &str) -> StoreResult<Vec<QuestionsReadsRecord>> {
    let mut statement =
        connection.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    let rows = statement.query_map([], |row| {
        Ok(QuestionsReadsRecord {
            id: row.get(0)?,
            record: row.get(1)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn read_record(connection: &Connection, table: &str, id: &str) -> StoreResult<OptionalRecord> {
    if table_exists(connection, table)?.is_none() {
        return Ok(None);
    }
    Ok(
        get_raw_record(connection, table, id)?.map(|record| QuestionsReadsRecord {
            id: id.to_owned(),
            record,
        }),
    )
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<Option<()>> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |_| Ok(()),
        )
        .optional()
        .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use super::{QuestionsReadsRepository, REQUIREMENTS};
    use rusqlite::Connection;

    #[test]
    fn lazily_created_families_are_empty_when_absent() {
        let db = Connection::open_in_memory().expect("memory SQLite opens");
        assert!(
            QuestionsReadsRepository::questions(&db)
                .expect("absent requests are empty")
                .is_empty()
        );
        assert!(
            QuestionsReadsRepository::complaint(&db, "missing")
                .expect("absent complaints are empty")
                .is_none()
        );
        assert!(
            QuestionsReadsRepository::checkpoints(&db)
                .expect("absent checkpoints are empty")
                .is_empty()
        );
        assert_eq!(REQUIREMENTS.conditional_tables.len(), 3);
    }

    #[test]
    fn reads_records_and_filters_checkpoints_by_agent() {
        let db = Connection::open_in_memory().expect("memory SQLite opens");
        db.execute_batch(
            "CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_checkpoints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_requests VALUES('q','{}');
             INSERT INTO runtime_complaints VALUES('c','{}');
             INSERT INTO runtime_checkpoints VALUES('p1','{\"agent\":\"a\"}');
             INSERT INTO runtime_checkpoints VALUES('p2','{\"agent\":\"b\"}');",
        )
        .expect("families create");

        assert_eq!(
            QuestionsReadsRepository::questions(&db).expect("request records read")[0].id,
            "q"
        );
        assert_eq!(
            QuestionsReadsRepository::complaint(&db, "c")
                .expect("complaint read")
                .expect("complaint exists")
                .id,
            "c"
        );
        assert_eq!(
            QuestionsReadsRepository::checkpoints(&db).expect("checkpoint records read")[0].id,
            "p1"
        );
    }
}
