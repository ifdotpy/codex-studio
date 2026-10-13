//! Persistence for provider notification dispatch and transcript writes.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::repositories::transcript::{TranscriptItem, TranscriptRepository, TranscriptStoreError};
use crate::{StoreError, StoreResult};

const ITEM_PREVIEW_CHARS: usize = 20_000;
const EVENT_META_TABLE: &[crate::schema::TableRequirement] = &[crate::schema::TableRequirement {
    name: "runtime_event_meta",
    columns: &["id", "record"],
}];
const RECEIPT_TABLE: &[crate::schema::TableRequirement] = &[crate::schema::TableRequirement {
    name: "runtime_operation_receipts",
    columns: &["id", "signature", "result"],
}];

#[derive(Clone, Copy, Debug, Default)]
pub struct NotificationDispatchRepository;

pub(crate) const REQUIREMENTS: crate::schema::SchemaRequirements =
    crate::schema::SchemaRequirements {
        tables: &[
            crate::schema::TableRequirement {
                name: "runtime_agents",
                columns: &["id", "record"],
            },
            crate::schema::TableRequirement {
                name: "runtime_events",
                columns: &["id", "agent", "kind", "text", "created"],
            },
            crate::schema::TableRequirement {
                name: "runtime_items",
                columns: &["id", "agent", "record", "created"],
            },
        ],
        conditional_tables: &[
            crate::schema::ConditionalTableRequirement {
                table_exists: "runtime_event_meta",
                tables: EVENT_META_TABLE,
            },
            crate::schema::ConditionalTableRequirement {
                table_exists: "runtime_operation_receipts",
                tables: RECEIPT_TABLE,
            },
            crate::schema::ConditionalTableRequirement {
                table_exists: "runtime_item_fulltext",
                tables: &[crate::schema::TableRequirement {
                    name: "runtime_item_fulltext",
                    columns: &["id", "body"],
                }],
            },
        ],
        triggers: &[],
        conditional_triggers: &[],
        connection_functions: &[],
        bootstrap_tables: &[],
        bootstrap_triggers: &[],
        bootstrap_connection_functions: &[],
    };

impl NotificationDispatchRepository {
    pub fn agent_record(transaction: &Transaction<'_>, agent: &str) -> StoreResult<Option<Value>> {
        let raw = transaction
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [agent],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        raw.map(|raw| decode_object(&raw, "runtime_agents"))
            .transpose()
    }

    pub fn put_agent_record(
        transaction: &Transaction<'_>,
        agent: &str,
        record: &Value,
    ) -> StoreResult<()> {
        if !record.is_object() || record.get("id").and_then(Value::as_str) != Some(agent) {
            return Err(invalid_record("runtime_agents"));
        }
        transaction.execute(
            "UPDATE runtime_agents SET record=?2 WHERE id=?1",
            params![agent, serde_json::to_string(record)?],
        )?;
        Ok(())
    }

    pub fn transcript_item(
        transaction: &Transaction<'_>,
        agent: &str,
        id: &str,
    ) -> StoreResult<Option<(Value, f64)>> {
        let row = transaction
            .query_row(
                "SELECT record,created FROM runtime_items WHERE id=?1 AND agent=?2",
                params![id, agent],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
            )
            .optional()?;
        row.map(|(raw, created)| Ok((decode_object(&raw, "runtime_items")?, created)))
            .transpose()
    }

    pub fn full_text(
        transaction: &Transaction<'_>,
        id: &str,
        record: &Value,
    ) -> StoreResult<Option<String>> {
        TranscriptRepository::full_text(
            transaction,
            &TranscriptItem {
                id: id.to_owned(),
                record: record.clone(),
            },
        )
        .map_err(|error| match error {
            TranscriptStoreError::Sqlite(error) => StoreError::Sqlite(error),
            TranscriptStoreError::InvalidStoredObject(object) => invalid_record(object),
        })
    }

    pub fn put_transcript_item(
        transaction: &Transaction<'_>,
        id: &str,
        agent: &str,
        record: &Value,
        created: f64,
        text: &str,
    ) -> StoreResult<()> {
        let has_fulltext = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_item_fulltext')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if text.chars().count() > ITEM_PREVIEW_CHARS && !has_fulltext {
            return Err(StoreError::SchemaMismatch(
                "runtime_item_fulltext is required for a long transcript item".to_owned(),
            ));
        }
        let existing_agent = transaction
            .query_row("SELECT agent FROM runtime_items WHERE id=?1", [id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if existing_agent
            .as_deref()
            .is_some_and(|owner| owner != agent)
        {
            return Err(invalid_record("runtime_items"));
        }
        transaction.execute(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,?4) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record,created=excluded.created",
            params![id, agent, serde_json::to_string(record)?, created],
        )?;
        if text.chars().count() > ITEM_PREVIEW_CHARS {
            transaction.execute(
                "INSERT INTO runtime_item_fulltext(id,body) VALUES(?1,?2) \
                 ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                params![id, text],
            )?;
        } else if has_fulltext {
            transaction.execute("DELETE FROM runtime_item_fulltext WHERE id=?1", [id])?;
        }
        Ok(())
    }

    pub fn link_transcript_item_to_event(
        transaction: &Transaction<'_>,
        agent: &str,
        event_id: &str,
        item_id: &str,
    ) -> StoreResult<()> {
        let has_event_meta = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_event_meta')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if has_event_meta {
            transaction.execute(
                "UPDATE runtime_event_meta SET record=json_set(record,'$.transcriptItemId',?1) \
                 WHERE id=?2 AND EXISTS (SELECT 1 FROM runtime_events WHERE id=?2 AND agent=?3)",
                params![item_id, event_id, agent],
            )?;
        }
        Ok(())
    }

    pub fn user_event(
        transaction: &Transaction<'_>,
        agent: &str,
        event_id: &str,
    ) -> StoreResult<Option<(String, String, f64)>> {
        Ok(transaction
            .query_row(
                "SELECT kind,text,created FROM runtime_events WHERE id=?1 AND agent=?2",
                params![event_id, agent],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    pub fn notification_receipt(
        transaction: &Transaction<'_>,
        id: &str,
    ) -> StoreResult<Option<(String, String)>> {
        Ok(transaction
            .query_row(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    pub fn put_notification_receipt(
        transaction: &Transaction<'_>,
        id: &str,
        signature: &str,
        result: &str,
    ) -> StoreResult<()> {
        transaction.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
            params![id, signature, result],
        )?;
        Ok(())
    }
}

fn decode_object(raw: &str, object: &'static str) -> StoreResult<Value> {
    let value: Value = serde_json::from_str(raw)?;
    if !value.is_object() {
        return Err(invalid_record(object));
    }
    Ok(value)
}

fn invalid_record(object: &'static str) -> StoreError {
    StoreError::SchemaMismatch(format!("invalid stored object {object}"))
}
