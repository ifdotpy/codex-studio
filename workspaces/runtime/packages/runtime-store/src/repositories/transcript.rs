//! Read-only access to Python-written transcript rows.

use crate::records::{
    AgentId, AgentRecord, RecordOrder, RecordPredicate, Table, TranscriptItemId,
    TranscriptItemRecord,
};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use thiserror::Error;

const LEGACY_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_search",
    columns: &["id", "body"],
}];
const LEGACY_ADDRESS_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_search_rows",
    columns: &["id", "search_rowid"],
}];
const BOOTSTRAP_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_item_fulltext",
    columns: &["id", "body"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[
        TableRequirement {
            name: "runtime_item_fulltext",
            columns: &["id", "body"],
        },
        TableRequirement {
            name: "runtime_items",
            columns: &["id", "agent", "record", "created"],
        },
        TableRequirement {
            name: "runtime_agents",
            columns: &["id", "record"],
        },
        TableRequirement {
            name: "runtime_events",
            columns: &["id", "agent", "text"],
        },
    ],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_search",
            tables: LEGACY_TABLES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_search_rows",
            tables: LEGACY_ADDRESS_TABLES,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: BOOTSTRAP_TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Debug, Error)]
pub enum TranscriptStoreError {
    #[error("SQLite operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid stored object {0}")]
    InvalidStoredObject(&'static str),
}

pub type TranscriptStoreResult<T> = Result<T, TranscriptStoreError>;

#[derive(Clone, Debug)]
pub struct TranscriptItem {
    pub id: String,
    pub record: Value,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TranscriptRepository;

impl TranscriptRepository {
    pub fn agent(connection: &Connection, agent: &str) -> TranscriptStoreResult<Option<Value>> {
        Table::<AgentRecord>::new(connection)
            .get(&AgentId::from(agent))
            .map(|record| record.map(|record| record.value()))
            .map_err(|error| map_store_error(error, "runtime_agents"))
    }

    pub fn item(
        connection: &Connection,
        agent: &str,
        id: &str,
    ) -> TranscriptStoreResult<Option<TranscriptItem>> {
        Table::<TranscriptItemRecord>::new(connection)
            .get_matching(
                &TranscriptItemId::from(id),
                Some(RecordPredicate::PhysicalAgentEquals(agent)),
            )
            .map(|item| {
                item.map(|item| TranscriptItem {
                    id: item.id().as_ref().to_owned(),
                    record: item.value().clone(),
                })
            })
            .map_err(|error| map_store_error(error, "runtime_items"))
    }

    pub fn items_for_agent(
        connection: &Connection,
        agent: &str,
    ) -> TranscriptStoreResult<Vec<TranscriptItem>> {
        Table::<TranscriptItemRecord>::new(connection)
            .list_matching(
                Some(RecordPredicate::PhysicalAgentEquals(agent)),
                RecordOrder::PhysicalCreatedDescIdDesc,
            )
            .map(|records| {
                records
                    .into_iter()
                    .filter(|record| visible(record.value()))
                    .map(|record| TranscriptItem {
                        id: record.id().as_ref().to_owned(),
                        record: record.value().clone(),
                    })
                    .collect()
            })
            .map_err(|error| map_store_error(error, "runtime_items"))
    }

    pub fn event_text(
        connection: &Connection,
        agent: &str,
        event: &str,
    ) -> TranscriptStoreResult<Option<String>> {
        let text = connection
            .query_row(
                "SELECT text FROM runtime_events WHERE id=?1 AND agent=?2",
                params![event, agent],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| map_column_error(error, "runtime_events"))?;
        Ok(text.flatten())
    }

    pub fn full_text(
        connection: &Connection,
        item: &TranscriptItem,
    ) -> TranscriptStoreResult<Option<String>> {
        let full = connection
            .query_row(
                "SELECT body FROM runtime_item_fulltext WHERE id=?1",
                [&item.id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| map_column_error(error, "runtime_item_fulltext"))?;
        if let Some(Some(full)) = full {
            return Ok(Some(full));
        }
        if item.record.get("truncated").and_then(Value::as_bool) == Some(true)
            && table_exists(connection, "runtime_search")?
        {
            let full = if table_exists(connection, "runtime_search_rows")? {
                connection
                    .query_row(
                        "SELECT s.body FROM runtime_search_rows r \
                         JOIN runtime_search s ON s.rowid=r.search_rowid WHERE r.id=?1",
                        [&item.id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map_err(|error| map_column_error(error, "runtime_search"))?
                    .flatten()
            } else {
                connection
                    .query_row(
                        "SELECT body FROM runtime_search WHERE id=?1",
                        [&item.id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map_err(|error| map_column_error(error, "runtime_search"))?
                    .flatten()
            };
            if let Some(full) = full {
                return Ok(Some(full));
            }
        }
        if let Some(text) = item.record.get("text") {
            let text = text
                .as_str()
                .ok_or(TranscriptStoreError::InvalidStoredObject("runtime_items"))?;
            if item.record.get("truncated").and_then(Value::as_bool) != Some(true)
                && (item.record.get("inputs").is_none() || !text.is_empty())
            {
                return Ok(Some(text.to_owned()));
            }
        }
        Ok(None)
    }

    pub fn resolve_item(
        connection: &Connection,
        agent: &str,
        identity: &str,
    ) -> TranscriptStoreResult<Option<TranscriptItem>> {
        if let Some(item) = Self::item(connection, agent, identity)?
            && visible(&item.record)
        {
            return Ok(Some(item));
        }
        for item in Self::items_for_agent(connection, agent)? {
            let inputs = match item.record.get("inputs") {
                None | Some(Value::Null) => continue,
                Some(Value::Array(inputs)) => inputs,
                _ => return Err(TranscriptStoreError::InvalidStoredObject("runtime_items")),
            };
            for (index, input) in inputs.iter().enumerate() {
                let Some(fields) = input.as_object() else {
                    return Err(TranscriptStoreError::InvalidStoredObject("runtime_items"));
                };
                let input_id = match fields.get("id") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(id)) => Some(id.as_str()),
                    _ => return Err(TranscriptStoreError::InvalidStoredObject("runtime_items")),
                };
                let display_id = input_id.map_or_else(
                    || format!("{}:{index}", item.id),
                    |id| format!("{agent}:{id}"),
                );
                if display_id == identity {
                    return Ok(Some(item));
                }
            }
        }
        Ok(None)
    }
}

fn table_exists(connection: &Connection, table: &str) -> TranscriptStoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn map_store_error(error: crate::StoreError, object: &'static str) -> TranscriptStoreError {
    match error {
        crate::StoreError::Sqlite(error) => TranscriptStoreError::Sqlite(error),
        _ => TranscriptStoreError::InvalidStoredObject(object),
    }
}

fn map_column_error(error: rusqlite::Error, object: &'static str) -> TranscriptStoreError {
    match error {
        rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..) => {
            TranscriptStoreError::InvalidStoredObject(object)
        }
        error => TranscriptStoreError::Sqlite(error),
    }
}

fn visible(record: &Value) -> bool {
    record
        .get("afterRestore")
        .is_none_or(|after_restore| after_restore.is_null())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{TranscriptRepository, TranscriptStoreError};

    #[test]
    fn event_text_rejects_a_non_text_column_with_its_object_name() {
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        connection
            .execute_batch("CREATE TABLE runtime_events (id TEXT, agent TEXT, text); INSERT INTO runtime_events VALUES ('e','a',x'01');")
            .expect("event fixture is stored");

        assert!(matches!(
            TranscriptRepository::event_text(&connection, "a", "e"),
            Err(TranscriptStoreError::InvalidStoredObject("runtime_events"))
        ));
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "transcript",
        requirements: &REQUIREMENTS,
    }
}
