//! Atomic persistence for question and complaint mutations.

use rusqlite::{OptionalExtension, Transaction, params};

use crate::StoreResult;
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};

const REQUESTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_requests",
    columns: &["id", "record"],
}];
const COMPLAINTS: &[TableRequirement] = &[TableRequirement {
    name: "runtime_complaints",
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
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct QuestionsWriteRepository;

impl QuestionsWriteRepository {
    pub fn request(transaction: &Transaction<'_>, id: &str) -> StoreResult<Option<String>> {
        Ok(transaction
            .query_row(
                "SELECT record FROM runtime_requests WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn put_request(transaction: &Transaction<'_>, id: &str, record: &str) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO runtime_requests(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record],
        )?;
        Ok(())
    }

    pub fn complaint(transaction: &Transaction<'_>, id: &str) -> StoreResult<Option<String>> {
        Ok(transaction
            .query_row(
                "SELECT record FROM runtime_complaints WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn put_complaint(transaction: &Transaction<'_>, id: &str, record: &str) -> StoreResult<()> {
        let typed: crate::ComplaintRecord = serde_json::from_str(record)?;
        if typed.id() != id {
            return Err(crate::StoreError::InvalidStoredObject {
                table: "runtime_complaints",
                id: id.to_owned(),
            });
        }
        transaction.execute(
            "INSERT INTO runtime_complaints(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record],
        )?;
        Ok(())
    }

    pub fn put_typed_complaint(
        transaction: &Transaction<'_>,
        record: &crate::ComplaintRecord,
    ) -> StoreResult<()> {
        let encoded = studio_runtime_schema::json_compat::python_json_serialized(record)?;
        transaction.execute(
            "INSERT INTO runtime_complaints(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![record.id(), encoded],
        )?;
        Ok(())
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "questions_write",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod complaint_tests {
    use super::QuestionsWriteRepository;
    use crate::ComplaintRecord;
    use rusqlite::Connection;

    #[test]
    fn typed_complaint_write_keeps_python_fingerprint_bytes() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .unwrap();
        let raw = r#"{"id":"c","leadId":"l","author":"user","text":"é😀","recipient":"lead","version":1,"status":"open","created":1.0,"updated":1.0,"readAt":null,"responses":[]}"#;
        let complaint: ComplaintRecord = serde_json::from_str(raw).unwrap();
        let tx = db.transaction().unwrap();
        QuestionsWriteRepository::put_typed_complaint(&tx, &complaint).unwrap();
        let stored: String = tx
            .query_row(
                "SELECT record FROM runtime_complaints WHERE id='c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            r#"{"author": "user", "created": 1.0, "id": "c", "leadId": "l", "readAt": null, "recipient": "lead", "responses": [], "status": "open", "text": "\u00e9\ud83d\ude00", "updated": 1.0, "version": 1}"#
        );
    }
}
