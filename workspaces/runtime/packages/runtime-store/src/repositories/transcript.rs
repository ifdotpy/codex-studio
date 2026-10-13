//! Read-only access to Python-written transcript rows.

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use thiserror::Error;

const CONDITIONAL_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_item_fulltext",
    columns: &["id", "body"],
}];
const LEGACY_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_search",
    columns: &["id", "body"],
}];
const LEGACY_ADDRESS_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_search_rows",
    columns: &["id", "search_rowid"],
}];
const SEARCH_INDEXED_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_search_indexed",
    columns: &["id"],
}];
const SEARCH_NEXT_META_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_search_next_meta",
    columns: &["id", "agent", "kind", "search_rowid"],
}];
const SEARCH_ROLLOUT_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_search_rollout",
    columns: &["id", "phase", "cursor", "updated"],
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[
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
            table_exists: "runtime_item_fulltext",
            tables: CONDITIONAL_TABLES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_search",
            tables: LEGACY_TABLES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_search_rows",
            tables: LEGACY_ADDRESS_TABLES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_search_indexed",
            tables: SEARCH_INDEXED_TABLES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_search_next_meta",
            tables: SEARCH_NEXT_META_TABLES,
        },
        ConditionalTableRequirement {
            table_exists: "runtime_search_rollout",
            tables: SEARCH_ROLLOUT_TABLES,
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
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
        let record = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [agent],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| map_column_error(error, "runtime_agents"))?;
        record
            .map(|raw| decode_object(&raw, "runtime_agents"))
            .transpose()
    }

    pub fn item(
        connection: &Connection,
        agent: &str,
        id: &str,
    ) -> TranscriptStoreResult<Option<TranscriptItem>> {
        let item = connection
            .query_row(
                "SELECT id,record FROM runtime_items WHERE id=?1 AND agent=?2",
                params![id, agent],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| map_column_error(error, "runtime_items"))?;
        item.map(|(id, raw)| {
            Ok(TranscriptItem {
                id,
                record: decode_object(&raw, "runtime_items")?,
            })
        })
        .transpose()
    }

    pub fn items_for_agent(
        connection: &Connection,
        agent: &str,
    ) -> TranscriptStoreResult<Vec<TranscriptItem>> {
        Self::items_for_agent_with_restore(connection, agent, false)
    }

    pub fn items_for_agent_including_restored(
        connection: &Connection,
        agent: &str,
    ) -> TranscriptStoreResult<Vec<TranscriptItem>> {
        Self::items_for_agent_with_restore(connection, agent, true)
    }

    fn items_for_agent_with_restore(
        connection: &Connection,
        agent: &str,
        include_restored: bool,
    ) -> TranscriptStoreResult<Vec<TranscriptItem>> {
        let mut statement = connection.prepare(
            "SELECT id,record FROM runtime_items WHERE agent=?1 ORDER BY created DESC,id DESC",
        )?;
        let rows = statement.query_map([agent], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (id, raw) = row.map_err(|error| map_column_error(error, "runtime_items"))?;
            let record = decode_object(&raw, "runtime_items")?;
            if include_restored || visible(&record) {
                items.push(TranscriptItem { id, record });
            }
        }
        Ok(items)
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
        if table_exists(connection, "runtime_item_fulltext")? {
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

    /// Persist a Python-shaped transcript row and keep whichever search
    /// projections are present in the same transaction.
    pub fn put_item(
        transaction: &Transaction<'_>,
        id: &str,
        agent: &str,
        record: &Value,
        created: f64,
        text: &str,
        index_search: bool,
    ) -> TranscriptStoreResult<()> {
        if record.get("id").and_then(Value::as_str) != Some(id)
            || !matches!(
                record.get("role").and_then(Value::as_str),
                Some("assistant" | "user" | "system")
            )
        {
            return Err(TranscriptStoreError::InvalidStoredObject("runtime_items"));
        }
        let has_fulltext = table_exists(transaction, "runtime_item_fulltext")?;
        if text.chars().count() > 20_000 && !has_fulltext {
            return Err(TranscriptStoreError::InvalidStoredObject(
                "runtime_item_fulltext",
            ));
        }
        let owner = transaction
            .query_row("SELECT agent FROM runtime_items WHERE id=?1", [id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if owner.as_deref().is_some_and(|owner| owner != agent) {
            return Err(TranscriptStoreError::InvalidStoredObject("runtime_items"));
        }
        transaction.execute(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,?4) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![
                id,
                agent,
                serde_json::to_string(record)
                    .map_err(|_| TranscriptStoreError::InvalidStoredObject("runtime_items"))?,
                created
            ],
        )?;
        if text.chars().count() > 20_000 {
            transaction.execute(
                "INSERT INTO runtime_item_fulltext(id,body) VALUES(?1,?2) \
                 ON CONFLICT(id) DO UPDATE SET body=excluded.body",
                params![id, text],
            )?;
        } else if has_fulltext {
            transaction.execute("DELETE FROM runtime_item_fulltext WHERE id=?1", [id])?;
        }
        if index_search {
            index_search_item(
                transaction,
                id,
                agent,
                record.get("role").and_then(Value::as_str).unwrap_or(""),
                text,
            )?;
        } else {
            delete_search_item(transaction, id)?;
        }
        Ok(())
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

fn decode_object(raw: &str, object: &'static str) -> TranscriptStoreResult<Value> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| TranscriptStoreError::InvalidStoredObject(object))?;
    if !value.is_object() {
        return Err(TranscriptStoreError::InvalidStoredObject(object));
    }
    Ok(value)
}

fn table_exists(connection: &Connection, table: &str) -> TranscriptStoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
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

fn index_search_item(
    transaction: &Transaction<'_>,
    id: &str,
    agent: &str,
    kind: &str,
    body: &str,
) -> TranscriptStoreResult<()> {
    let phase = search_phase(transaction)?;
    if !matches!(phase.as_str(), "active" | "dropping" | "complete")
        && table_exists(transaction, "runtime_search")?
    {
        let previous = if table_exists(transaction, "runtime_search_rows")? {
            transaction
                .query_row(
                    "SELECT search_rowid FROM runtime_search_rows WHERE id=?1",
                    [id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
        } else {
            None
        };
        if let Some(rowid) = previous {
            transaction.execute("DELETE FROM runtime_search WHERE rowid=?1", [rowid])?;
        } else if !table_exists(transaction, "runtime_search_rows")? {
            transaction.execute("DELETE FROM runtime_search WHERE id=?1", [id])?;
        }
        transaction.execute(
            "INSERT INTO runtime_search(id,agent,kind,body) VALUES(?1,?2,?3,?4)",
            params![id, agent, kind, body],
        )?;
        let rowid = transaction.last_insert_rowid();
        if table_exists(transaction, "runtime_search_rows")? {
            transaction.execute(
                "INSERT INTO runtime_search_rows(id,search_rowid) VALUES(?1,?2) \
                 ON CONFLICT(id) DO UPDATE SET search_rowid=excluded.search_rowid",
                params![id, rowid],
            )?;
        }
        if table_exists(transaction, "runtime_search_indexed")? {
            transaction.execute(
                "INSERT OR IGNORE INTO runtime_search_indexed(id) VALUES(?1)",
                [id],
            )?;
        }
    }
    if matches!(
        phase.as_str(),
        "building" | "active" | "dropping" | "complete"
    ) && table_exists(transaction, "runtime_search_next")?
        && table_exists(transaction, "runtime_search_next_meta")?
    {
        let previous = transaction
            .query_row(
                "SELECT search_rowid FROM runtime_search_next_meta WHERE id=?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let rowid = if let Some(rowid) = previous {
            transaction.execute("DELETE FROM runtime_search_next WHERE rowid=?1", [rowid])?;
            transaction.execute(
                "UPDATE runtime_search_next_meta SET agent=?1,kind=?2 WHERE id=?3",
                params![agent, kind, id],
            )?;
            rowid
        } else {
            let rowid: i64 = transaction.query_row(
                "SELECT coalesce(max(search_rowid),0)+1 FROM runtime_search_next_meta",
                [],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT INTO runtime_search_next_meta(id,agent,kind,search_rowid) VALUES(?1,?2,?3,?4)",
                params![id, agent, kind, rowid],
            )?;
            rowid
        };
        transaction.execute(
            "INSERT INTO runtime_search_next(rowid,body) VALUES(?1,?2)",
            params![rowid, body],
        )?;
    }
    Ok(())
}

fn delete_search_item(transaction: &Transaction<'_>, id: &str) -> TranscriptStoreResult<()> {
    let phase = search_phase(transaction)?;
    if !matches!(phase.as_str(), "active" | "dropping" | "complete")
        && table_exists(transaction, "runtime_search_rows")?
    {
        let rowid = transaction
            .query_row(
                "SELECT search_rowid FROM runtime_search_rows WHERE id=?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(rowid) = rowid {
            if table_exists(transaction, "runtime_search")? {
                transaction.execute("DELETE FROM runtime_search WHERE rowid=?1", [rowid])?;
            }
            transaction.execute("DELETE FROM runtime_search_rows WHERE id=?1", [id])?;
            if table_exists(transaction, "runtime_search_indexed")? {
                transaction.execute("DELETE FROM runtime_search_indexed WHERE id=?1", [id])?;
            }
        }
    }
    if matches!(
        phase.as_str(),
        "building" | "active" | "dropping" | "complete"
    ) && table_exists(transaction, "runtime_search_next")?
        && table_exists(transaction, "runtime_search_next_meta")?
    {
        let rowid = transaction
            .query_row(
                "SELECT search_rowid FROM runtime_search_next_meta WHERE id=?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(rowid) = rowid {
            transaction.execute("DELETE FROM runtime_search_next WHERE rowid=?1", [rowid])?;
            transaction.execute("DELETE FROM runtime_search_next_meta WHERE id=?1", [id])?;
        }
    }
    Ok(())
}

fn search_phase(connection: &Connection) -> TranscriptStoreResult<String> {
    if !table_exists(connection, "runtime_search_rollout")? {
        return Ok("legacy".to_owned());
    }
    Ok(connection
        .query_row(
            "SELECT phase FROM runtime_search_rollout WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "legacy".to_owned()))
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
