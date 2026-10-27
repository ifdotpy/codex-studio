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
            name: "sync_entity_event_UPDATE_WINDOW",
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
        let mut statement = connection.prepare_cached(
            "SELECT id, agent, kind, text, status, created, epoch, turn_id, error FROM runtime_events WHERE id = ?1",
        )?;
        statement
            .query_row([&id.0], Self::from_row)
            .optional()
            .map_err(|error| {
                // Row conversion failure is retained evidence; query/infrastructure failure stays fatal.
                if let rusqlite::Error::InvalidColumnType(index, _, _)
                | rusqlite::Error::FromSqlConversionFailure(index, _, _)
                | rusqlite::Error::IntegralValueOutOfRange(index, _)
                | rusqlite::Error::Utf8Error(index, _) = &error
                    && let Ok(column) = statement.column_name(*index)
                {
                    return crate::StoreError::InvalidSyncObjectField {
                        scope: "runtime_events".into(),
                        id: id.0.clone(),
                        pointer: format!("/{column}"),
                        reason: "stored column cannot be decoded as a queue event".into(),
                    };
                }
                error.into()
            })
    }

    pub(crate) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEvent> {
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

    pub fn put(connection: &Connection, event: &QueueEvent) -> StoreResult<()> {
        connection.execute(
            "INSERT INTO runtime_events(id, agent, kind, text, status, created, epoch, turn_id, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent, kind=excluded.kind, text=excluded.text, status=excluded.status, created=excluded.created, epoch=excluded.epoch, turn_id=excluded.turn_id, error=excluded.error",
            params![event.id.0, event.agent.0, event.kind, event.text, event.status, event.created, event.epoch, event.turn_id, event.error],
        )?;
        Ok(())
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "events",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_query_failures_stay_fatal_while_malformed_columns_are_located() {
        let connection = Connection::open_in_memory().expect("memory SQLite");
        let id = QueueEventId::from("input");
        assert!(
            matches!(
                QueueEventRepository::get(&connection, &id),
                Err(crate::StoreError::Sqlite(_))
            ),
            "missing table is an infrastructure failure"
        );
        connection.execute_batch("CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT); INSERT INTO runtime_events VALUES('input','owner','user','private-input','failed',1,'epoch-private-canary',NULL,NULL);").expect("retained source");
        let error = QueueEventRepository::get(&connection, &id).expect_err("malformed source");
        assert!(
            matches!(&error,crate::StoreError::InvalidSyncObjectField{scope,id,pointer,..} if scope=="runtime_events" && id=="input" && pointer=="/epoch")
        );
        assert!(!error.to_string().contains("private-canary"));
        connection
            .execute("UPDATE runtime_events SET epoch=0", [])
            .expect("healthy fixture");
        let event = QueueEventRepository::get(&connection, &id)
            .expect("healthy query")
            .expect("retained event");
        assert_eq!(event.epoch, 0);
        assert_eq!(event.text, "private-input");
    }
}
