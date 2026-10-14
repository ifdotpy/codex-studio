//! Persistence for provider notification dispatch.

use crate::schema::SchemaRequirements;
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

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
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
    /// Temporary base only when item/started replaced a flushed stream's text.
    pub fn stream_base(tx: &Transaction<'_>, id: &str) -> StoreResult<Option<String>> {
        Ok(EventWriteRepository::receipt(tx, id)?.map(|row| row.result))
    }
    pub fn put_stream_base(tx: &Transaction<'_>, id: &str, text: Option<&str>) -> StoreResult<()> {
        if let Some(text) = text {
            tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'stream-base',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result", params![id, serde_json::to_string(text).map_err(|_| invalid_record("runtime_operation_receipts", id))?])?;
        } else {
            tx.execute("DELETE FROM runtime_operation_receipts WHERE id=?1", [id])?;
        }
        Ok(())
    }

    pub fn agent_record(transaction: &Transaction<'_>, agent: &str) -> StoreResult<Option<Value>> {
        TranscriptRepository::agent(transaction, agent)
            .map_err(|error| map_transcript_error(error, "runtime_agents", agent))
    }

    pub fn related_record(
        transaction: &Transaction<'_>,
        table: &'static str,
        id: &str,
    ) -> StoreResult<Option<Value>> {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let raw: Option<String> = transaction
            .query_row(
                &format!("SELECT record FROM {table} WHERE id=?1"),
                [id],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|raw| {
            let value: Value = serde_json::from_str(&raw).map_err(|_| invalid_record(table, id))?;
            if !value.is_object() || value.get("id").and_then(Value::as_str) != Some(id) {
                return Err(invalid_record(table, id));
            }
            Ok(value)
        })
        .transpose()
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
        Self::read_item(transaction, agent, id, false)
    }

    /// Read a native notice or tool item through the same checked storage boundary.
    pub fn notification_item(
        transaction: &Transaction<'_>,
        agent: &str,
        id: &str,
    ) -> StoreResult<Option<(Value, f64)>> {
        Self::read_item(transaction, agent, id, true)
    }

    fn read_item(
        transaction: &Transaction<'_>,
        agent: &str,
        id: &str,
        include_notices: bool,
    ) -> StoreResult<Option<(Value, f64)>> {
        let item = TranscriptRepository::item(transaction, agent, id)
            .map_err(|error| map_transcript_error(error, "runtime_items", id))?;
        item.map(|item| {
            validate_item(&item, agent, include_notices)?;
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
                agent: transaction.query_row(
                    "SELECT agent FROM runtime_items WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?,
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

    /// Native notifications share transcript persistence and search projection rules.
    pub fn put_notification_item(
        transaction: &Transaction<'_>,
        id: &str,
        agent: &str,
        record: &Value,
        created: f64,
        text: &str,
        index_search: bool,
    ) -> StoreResult<()> {
        TranscriptRepository::put_notification_item(
            transaction,
            id,
            agent,
            record,
            created,
            text,
            index_search,
        )
        .map_err(|error| map_transcript_error(error, "runtime_items", id))
    }

    pub fn link_transcript_item_to_event(
        transaction: &Transaction<'_>,
        agent: &str,
        event_id: &str,
        item_id: &str,
    ) -> StoreResult<()> {
        let event_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE id=?1 AND agent=?2)",
            params![event_id, agent],
            |row| row.get(0),
        )?;
        if !event_exists {
            return Err(invalid_record("runtime_events", event_id));
        }
        let meta: Option<String> = transaction
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id=?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(meta) = meta {
            let parsed: Value = serde_json::from_str(&meta)
                .map_err(|_| invalid_record("runtime_event_meta", event_id))?;
            if !parsed.is_object() {
                return Err(invalid_record("runtime_event_meta", event_id));
            }
            transaction.execute(
                "UPDATE runtime_event_meta SET record=json_set(record,'$.transcriptItemId',?1,'$.modelEventProjection',1) WHERE id=?2",
                params![item_id, event_id],
            )?;
        } else {
            let record = serde_json::json!({"transcriptItemId":item_id,"modelEventProjection":1});
            transaction.execute(
                "INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2)",
                params![event_id, record.to_string()],
            )?;
        }
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

    pub fn event_metadata(
        transaction: &Transaction<'_>,
        event_id: &str,
    ) -> StoreResult<Option<Value>> {
        let has_table: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_event_meta')",
            [],
            |row| row.get(0),
        )?;
        if !has_table {
            return Err(invalid_record("runtime_event_meta", event_id));
        }
        let raw: Option<String> = transaction
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id=?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(&raw)
            .map_err(|_| invalid_record("runtime_event_meta", event_id))?;
        if !value.is_object() {
            return Err(invalid_record("runtime_event_meta", event_id));
        }
        Ok(Some(value))
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

fn validate_item(item: &TranscriptItem, agent: &str, include_notices: bool) -> StoreResult<()> {
    let role = item.record.get("role").and_then(Value::as_str);
    if ["truncated", "streaming"].iter().any(|key| {
        item.record
            .get(key)
            .is_some_and(|value| !value.is_boolean())
    }) || item.record.get("id").and_then(Value::as_str) != Some(item.id.as_str())
        || !(matches!(role, Some("assistant" | "user"))
            || include_notices && matches!(role, Some("system" | "output")))
        || item
            .record
            .get("text")
            .is_some_and(|text| !text.is_string())
        || item
            .record
            .get("assets")
            .is_some_and(|assets| !assets.is_array())
        || item.record.get("inputs").is_some_and(|inputs| {
            !inputs.as_array().is_some_and(|inputs| {
                inputs.iter().all(|input| {
                    input.is_object()
                        && input.get("id").and_then(Value::as_str).is_some()
                        && input
                            .get("at")
                            .and_then(Value::as_f64)
                            .is_some_and(f64::is_finite)
                        && input.get("kind").and_then(Value::as_str).is_some()
                        && input.get("text").and_then(Value::as_str).is_some()
                        && input.get("truncated").and_then(Value::as_bool).is_some()
                        && input.get("assets").is_some_and(Value::is_array)
                })
            })
        })
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
        TranscriptStoreError::Store(error) => error,
        TranscriptStoreError::Sqlite(error) => StoreError::Sqlite(error),
        TranscriptStoreError::InvalidStoredObject { .. } | TranscriptStoreError::ActiveTurn => {
            invalid_record(table, id)
        }
    }
}

fn invalid_record(table: &str, id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: table.into(),
        id: id.into(),
    }
}

inventory::submit! { crate::repositories::SchemaRegistration { repository: "notification_dispatch", requirements: &REQUIREMENTS } }
