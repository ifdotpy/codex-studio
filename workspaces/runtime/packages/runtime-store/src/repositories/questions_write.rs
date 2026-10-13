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
        transaction.execute(
            "INSERT INTO runtime_complaints(id,record) VALUES (?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record],
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
