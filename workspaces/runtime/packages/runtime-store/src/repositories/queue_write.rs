//! Queue event mutations used by the queue-write service.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::{
    StoreResult,
    schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement},
};

const EVENT_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_events",
    columns: &[
        "id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error",
    ],
}];
const META_TABLE: &[TableRequirement] = &[TableRequirement {
    name: "runtime_event_meta",
    columns: &["id", "record"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_events",
            tables: EVENT_TABLE,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_event_meta",
            tables: META_TABLE,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, PartialEq)]
pub struct PendingEvent {
    pub id: String,
    pub kind: String,
    pub created: f64,
    pub valid: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QueueWriteRepository;

impl QueueWriteRepository {
    pub fn table_exists(transaction: &Transaction<'_>, table: &str) -> StoreResult<bool> {
        Ok(transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 COLLATE NOCASE)",
            [table],
            |row| row.get(0),
        )?)
    }

    pub fn pending_events(
        transaction: &Transaction<'_>,
        agent: &str,
        epoch: i64,
    ) -> StoreResult<Vec<PendingEvent>> {
        let mut statement = transaction.prepare(
            "SELECT id,kind,created FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending' ORDER BY created,rowid",
        )?;
        let rows = statement.query_map(params![agent, epoch], |row| {
            let (id, id_valid) = row.get::<_, String>(0).map_or_else(
                |_| ("<invalid id>".to_owned(), false),
                |value| (value, true),
            );
            let (kind, kind_valid) = row
                .get::<_, String>(1)
                .map_or_else(|_| (String::new(), false), |value| (value, true));
            let (created, created_valid) = row
                .get::<_, f64>(2)
                .map_or_else(|_| (f64::NAN, false), |value| (value, value.is_finite()));
            Ok(PendingEvent {
                id,
                kind,
                created,
                valid: id_valid && kind_valid && created_valid,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn update_created(
        transaction: &Transaction<'_>,
        id: &str,
        created: f64,
    ) -> StoreResult<()> {
        transaction.execute(
            "UPDATE runtime_events SET created=?1 WHERE id=?2",
            params![created, id],
        )?;
        Ok(())
    }

    pub fn update_status(transaction: &Transaction<'_>, id: &str, status: &str) -> StoreResult<()> {
        transaction.execute(
            "UPDATE runtime_events SET status=?1 WHERE id=?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn update_text(transaction: &Transaction<'_>, id: &str, text: &str) -> StoreResult<()> {
        transaction.execute(
            "UPDATE runtime_events SET text=?1 WHERE id=?2",
            params![text, id],
        )?;
        Ok(())
    }

    pub fn metadata(transaction: &Transaction<'_>, id: &str) -> StoreResult<Option<String>> {
        Ok(transaction
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn put_metadata(
        transaction: &Transaction<'_>,
        id: &str,
        metadata: &Value,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO runtime_event_meta(id,record) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, serde_json::to_string(metadata)?],
        )?;
        Ok(())
    }
}
