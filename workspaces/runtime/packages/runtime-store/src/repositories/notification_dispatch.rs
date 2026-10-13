//! Persistence for provider notification dispatch.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::repositories::transcript::{TranscriptItem, TranscriptRepository, TranscriptStoreError};
use crate::{EventWriteReceipt, EventWriteRepository, StoreError, StoreResult};

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
        TranscriptRepository::agent(transaction, agent)
            .map_err(|error| map_transcript_error(error, "runtime_agents", agent))
    }

    pub fn put_agent_record(
        transaction: &Transaction<'_>,
        agent: &str,
        record: &Value,
    ) -> StoreResult<()> {
        if !record.is_object() || record.get("id").and_then(Value::as_str) != Some(agent) {
            return Err(invalid_record("runtime_agents", agent));
        }
        let changed = transaction.execute(
            "UPDATE runtime_agents SET record=?2 WHERE id=?1",
            params![agent, serde_json::to_string(record)?],
        )?;
        if changed != 1 {
            return Err(invalid_record("runtime_agents", agent));
        }
        Ok(())
    }

    pub fn transcript_item(
        transaction: &Transaction<'_>,
        agent: &str,
        id: &str,
    ) -> StoreResult<Option<(Value, f64)>> {
        let item = TranscriptRepository::item(transaction, agent, id)
            .map_err(|error| map_transcript_error(error, "runtime_items", id))?;
        item.map(|item| {
            validate_item(&item, agent)?;
            let created = transaction
                .query_row(
                    "SELECT created FROM runtime_items WHERE id=?1 AND agent=?2",
                    params![id, agent],
                    |row| row.get::<_, f64>(0),
                )
                .map_err(|_| invalid_record("runtime_items", id))?;
            if !created.is_finite() || created < 0.0 {
                return Err(invalid_record("runtime_items", id));
            }
            Ok((item.record, created))
        })
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
        .map_err(|error| map_transcript_error(error, "runtime_items", id))
    }

    pub fn put_transcript_item(
        transaction: &Transaction<'_>,
        id: &str,
        agent: &str,
        record: &Value,
        created: f64,
        text: &str,
        index_search: bool,
    ) -> StoreResult<()> {
        TranscriptRepository::put_item(transaction, id, agent, record, created, text, index_search)
            .map_err(|error| map_transcript_error(error, "runtime_items", id))
    }

    pub fn link_transcript_item_to_event(
        transaction: &Transaction<'_>,
        agent: &str,
        event_id: &str,
        item_id: &str,
    ) -> StoreResult<()> {
        let meta: Option<String> = transaction
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id=?1 AND EXISTS \
                 (SELECT 1 FROM runtime_events WHERE id=?1 AND agent=?2)",
                params![event_id, agent],
                |row| row.get(0),
            )
            .optional()?;
        let Some(meta) = meta else {
            return Err(invalid_record("runtime_event_meta", event_id));
        };
        let parsed: Value = serde_json::from_str(&meta)
            .map_err(|_| invalid_record("runtime_event_meta", event_id))?;
        if !parsed.is_object() {
            return Err(invalid_record("runtime_event_meta", event_id));
        }
        transaction.execute(
            "UPDATE runtime_event_meta SET record=json_set(record,'$.transcriptItemId',?1,'$.modelEventProjection',1) WHERE id=?2",
            params![item_id, event_id],
        )?;
        Ok(())
    }

    pub fn user_event(
        transaction: &Transaction<'_>,
        agent: &str,
        event_id: &str,
    ) -> StoreResult<Option<(String, String, f64)>> {
        let event = transaction
            .query_row(
                "SELECT kind,text,created FROM runtime_events WHERE id=?1 AND agent=?2",
                params![event_id, agent],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| match error {
                rusqlite::Error::InvalidColumnType(..)
                | rusqlite::Error::FromSqlConversionFailure(..) => {
                    invalid_record("runtime_events", event_id)
                }
                error => StoreError::Sqlite(error),
            })?;
        if event
            .as_ref()
            .is_some_and(|(_, _, created)| !created.is_finite() || *created < 0.0)
        {
            return Err(invalid_record("runtime_events", event_id));
        }
        Ok(event)
    }

    pub fn event_metadata(transaction: &Transaction<'_>, event_id: &str) -> StoreResult<Value> {
        let raw: String = transaction
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id=?1",
                [event_id],
                |row| row.get(0),
            )
            .map_err(|_| invalid_record("runtime_event_meta", event_id))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|_| invalid_record("runtime_event_meta", event_id))?;
        if !value.is_object() {
            return Err(invalid_record("runtime_event_meta", event_id));
        }
        Ok(value)
    }

    pub fn notification_receipt(
        transaction: &Transaction<'_>,
        id: &str,
    ) -> StoreResult<Option<EventWriteReceipt>> {
        EventWriteRepository::receipt(transaction, id)
    }

    pub fn put_notification_receipt(
        transaction: &Transaction<'_>,
        id: &str,
        signature: &str,
        result: &str,
    ) -> StoreResult<()> {
        EventWriteRepository::put_receipt(transaction, id, signature, result)
    }
}

fn validate_item(item: &TranscriptItem, agent: &str) -> StoreResult<()> {
    let role = item.record.get("role").and_then(Value::as_str);
    if item.record.get("id").and_then(Value::as_str) != Some(item.id.as_str())
        || !matches!(role, Some("assistant" | "user"))
        || item
            .record
            .get("text")
            .is_some_and(|text| !text.is_string())
    {
        return Err(invalid_record("runtime_items", &item.id));
    }
    let expected_prefix = format!("{agent}:");
    if !item.id.starts_with(&expected_prefix) {
        return Err(invalid_record("runtime_items", &item.id));
    }
    Ok(())
}

fn map_transcript_error(error: TranscriptStoreError, table: &'static str, id: &str) -> StoreError {
    match error {
        TranscriptStoreError::Sqlite(error) => StoreError::Sqlite(error),
        TranscriptStoreError::InvalidStoredObject(_) => invalid_record(table, id),
    }
}

fn invalid_record(table: &str, id: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("invalid stored object {table}:{id}"))
}
