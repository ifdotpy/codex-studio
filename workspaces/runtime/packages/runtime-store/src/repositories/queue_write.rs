//! Queue event mutations used by the queue-write service.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::records::{EventMetadataRecord, QueueEventId, Table};
use crate::{
    StoreResult,
    schema::{SchemaRequirements, TableRequirement},
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
    tables: &[EVENT_TABLE[0]],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[META_TABLE[0]],
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
    pub fn pending_events(
        transaction: &Transaction<'_>,
        agent: &str,
        epoch: i64,
    ) -> StoreResult<Vec<PendingEvent>> {
        let mut statement = transaction.prepare_cached(
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

    /// Whether user message `id` of `agent` is held by a turn: claimed for a turn
    /// start or a steer, and not yet delivered.
    pub fn held_user_message(
        transaction: &Transaction<'_>,
        agent: &str,
        id: &str,
    ) -> StoreResult<bool> {
        Ok(transaction
            .query_row(
                "SELECT 1 FROM runtime_events WHERE id=?1 AND agent=?2 AND kind='user' \
                 AND status IN ('reserved','dispatching')",
                params![id, agent],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
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

    pub fn metadata(transaction: &Transaction<'_>, id: &str) -> StoreResult<Option<Value>> {
        Ok(Self::typed_metadata(transaction, id)?.map(EventMetadataRecord::into_value))
    }

    pub fn typed_metadata(
        transaction: &Transaction<'_>,
        id: &str,
    ) -> StoreResult<Option<EventMetadataRecord>> {
        Table::<EventMetadataRecord>::new(transaction).get(&QueueEventId::from(id))
    }

    pub fn put_typed_metadata(
        transaction: &Transaction<'_>,
        id: &str,
        metadata: &EventMetadataRecord,
    ) -> StoreResult<()> {
        Table::<EventMetadataRecord>::new(transaction).put_at(&QueueEventId::from(id), metadata)
    }

    pub fn put_metadata(
        transaction: &Transaction<'_>,
        id: &str,
        metadata: &Value,
    ) -> StoreResult<()> {
        let key = QueueEventId::from(id);
        let record = EventMetadataRecord::from_value(metadata.clone()).map_err(|_| {
            crate::StoreError::InvalidStoredObject {
                table: "runtime_event_meta",
                id: id.to_owned(),
            }
        })?;
        Table::<EventMetadataRecord>::new(transaction).put_at(&key, &record)
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "queue_write",
        requirements: &REQUIREMENTS,
    }
}
