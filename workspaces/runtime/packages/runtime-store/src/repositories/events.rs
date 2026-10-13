use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreResult;
use crate::records::{AgentId, QueueEvent, QueueEventId, QueueEventRepository};
use crate::schema::{SchemaRequirements, TableRequirement, TriggerRequirement};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[TableRequirement {
        name: "runtime_events",
        columns: &[
            "id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error",
        ],
    }],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TableRequirement {
        name: "sync_entities",
        columns: &["collection", "id", "seq", "hash", "payload", "deleted"],
    }],
    bootstrap_triggers: &[
        TriggerRequirement {
            name: "sync_entity_event_INSERT",
            table: "runtime_events",
        },
        TriggerRequirement {
            name: "sync_entity_event_UPDATE",
            table: "runtime_events",
        },
        TriggerRequirement {
            name: "sync_entity_event_DELETE",
            table: "runtime_events",
        },
    ],
    bootstrap_connection_functions: &["sync_entity_payload", "sync_entity_hash"],
};

impl QueueEventRepository {
    pub fn get(connection: &Connection, id: &QueueEventId) -> StoreResult<Option<QueueEvent>> {
        Ok(connection.query_row(
            "SELECT id, agent, kind, text, status, created, epoch, turn_id, error FROM runtime_events WHERE id = ?1",
            [&id.0],
            map_queue_event,
        ).optional()?)
    }

    pub fn put(connection: &Connection, event: &QueueEvent) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_events(id, agent, kind, text, status, created, epoch, turn_id, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent, kind=excluded.kind, text=excluded.text, status=excluded.status, created=excluded.created, epoch=excluded.epoch, turn_id=excluded.turn_id, error=excluded.error",
            params![event.id.0, event.agent.0, event.kind, event.text, event.status, event.created, event.epoch, event.turn_id, event.error],
        )?;
        Ok(())
    }
}

pub(crate) fn map_queue_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEvent> {
    Ok(QueueEvent {
        id: QueueEventId(row.get(0)?),
        agent: AgentId(row.get(1)?),
        kind: row.get(2)?,
        text: row.get(3)?,
        status: row.get(4)?,
        created: row.get(5)?,
        epoch: row.get(6)?,
        turn_id: row.get(7)?,
        error: row.get(8)?,
    })
}
