//! Read-only access to Python-written transcript rows.

use crate::records::{
    AgentId, AgentRecord, RecordOrder, RecordPredicate, Table, TranscriptItemId,
    TranscriptItemRecord,
};
use crate::repositories::sync_documents::SyncDocumentsRepository;
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
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
const TRANSCRIPT_MAX_TOMBSTONES: usize = 512;
const TRANSCRIPT_PAGE_SIZE: usize = 120;
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
    bootstrap_tables: BOOTSTRAP_TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Debug, Error)]
pub enum TranscriptStoreError {
    #[error("SQLite operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid stored object {table}/{id}")]
    InvalidStoredObject { table: &'static str, id: String },
    #[error("active transcript needs the event driver")]
    ActiveTurn,
    #[error(transparent)]
    Store(#[from] crate::StoreError),
}

pub type TranscriptStoreResult<T> = Result<T, TranscriptStoreError>;

#[derive(Clone, Debug)]
pub struct TranscriptItem {
    pub id: String,
    pub agent: String,
    pub record: Value,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TranscriptRepository;

impl TranscriptRepository {
    pub fn pull(
        transaction: &Transaction<'_>,
        scope: &str,
        agent: &str,
        after: i64,
    ) -> TranscriptStoreResult<Value> {
        let actor = Self::agent(transaction, agent)?;
        let deleted = actor
            .as_ref()
            .is_none_or(|record| stored_truthy(record.get("deletedAt")));
        if let Some(actor) = actor.as_ref()
            && matches!(
                actor.get("status").and_then(Value::as_str),
                Some("running" | "starting" | "approval")
            )
        {
            return Err(TranscriptStoreError::ActiveTurn);
        }
        let revision = if table_exists(transaction, "runtime_transcript_revisions")? {
            Some(
                transaction
                    .query_row(
                        "SELECT revision FROM runtime_transcript_revisions WHERE agent=?1",
                        [agent],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                    .unwrap_or(0),
            )
        } else {
            None
        };
        let payload = if deleted {
            json!({})
        } else {
            let actor = actor
                .as_ref()
                .ok_or_else(|| invalid_record("runtime_agents", agent))?;
            let records = Self::items_for_agent(transaction, agent)?;
            let mut items: Vec<Value> = records
                .iter()
                .take(TRANSCRIPT_PAGE_SIZE)
                .map(|item| item.record.clone())
                .collect();
            items.reverse();
            enrich_persisted_items(transaction, agent, actor, &mut items)?;
            let mut agent_view = serde_json::Map::new();
            for field in [
                "id",
                "status",
                "activity",
                "inFlight",
                "contextUsage",
                "compactions",
                "compactionsObservedOnly",
            ] {
                agent_view.insert(
                    field.to_owned(),
                    actor.get(field).cloned().unwrap_or(Value::Null),
                );
            }
            let thread = actor
                .get("threadId")
                .map(python_string)
                .unwrap_or_else(|| "None".to_owned());
            let restored = actor
                .get("restoredCheckpoint")
                .map(python_string)
                .unwrap_or_else(|| "None".to_owned());
            json!({
                "items": items,
                "truncated": records.len() > TRANSCRIPT_PAGE_SIZE,
                "nextCursor": null,
                "nextAfterCursor": null,
                "historyVersion": format!("{thread}:{restored}"),
                "unavailable": null,
                "agent": Value::Object(agent_view),
            })
        };
        transcript_pull_write(
            transaction,
            scope,
            after.max(0),
            &payload,
            deleted,
            revision,
        )
    }

    pub fn search_hits(
        connection: &Connection,
        match_query: &str,
    ) -> TranscriptStoreResult<Vec<(String, String, String)>> {
        let next_phase = connection
            .query_row(
                "SELECT phase FROM runtime_search_rollout WHERE id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if matches!(
            next_phase.as_deref(),
            Some("active" | "dropping" | "complete")
        ) {
            if !table_exists(connection, "runtime_search_next")? {
                return Ok(Vec::new());
            }
            let mut statement = connection.prepare(
                "SELECT m.id,m.agent,m.kind FROM runtime_search_next \
                 JOIN runtime_search_next_meta m ON m.search_rowid=runtime_search_next.rowid \
                 JOIN runtime_items i ON i.id=m.id \
                 WHERE runtime_search_next MATCH ?1 \
                   AND json_extract(i.record,'$.afterRestore') IS NULL \
                 ORDER BY rank LIMIT 1000",
            )?;
            let rows = statement.query_map([match_query], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            return rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        }
        if !table_exists(connection, "runtime_search")? {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare(
            "SELECT s.id,s.agent,s.kind FROM runtime_search s \
             JOIN runtime_items i ON i.id=s.id \
             WHERE runtime_search MATCH ?1 \
               AND json_extract(i.record,'$.afterRestore') IS NULL \
             ORDER BY rank LIMIT 1000",
        )?;
        let rows = statement.query_map([match_query], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn agent(connection: &Connection, agent: &str) -> TranscriptStoreResult<Option<Value>> {
        Table::<AgentRecord>::new(connection)
            .get(&AgentId::from(agent))
            .map(|record| record.map(|record| record.value()))
            .map_err(map_store_error)
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
                    agent: agent.to_owned(),
                    record: item.value().clone(),
                })
            })
            .map_err(map_store_error)
    }

    pub fn item_any_agent(
        connection: &Connection,
        id: &str,
    ) -> TranscriptStoreResult<Option<TranscriptItem>> {
        let agent = connection
            .query_row("SELECT agent FROM runtime_items WHERE id=?1", [id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        let Some(agent) = agent else {
            return Ok(None);
        };
        Table::<TranscriptItemRecord>::new(connection)
            .get(&TranscriptItemId::from(id))
            .map(|item| {
                item.map(|item| TranscriptItem {
                    id: item.id().as_ref().to_owned(),
                    agent: agent.clone(),
                    record: item.value().clone(),
                })
            })
            .map_err(map_store_error)
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
        Table::<TranscriptItemRecord>::new(connection)
            .list_matching(
                Some(RecordPredicate::PhysicalAgentEquals(agent)),
                RecordOrder::PhysicalCreatedDescIdDesc,
            )
            .map(|records| {
                records
                    .into_iter()
                    .filter(|record| include_restored || visible(record.value()))
                    .map(|record| TranscriptItem {
                        id: record.id().as_ref().to_owned(),
                        agent: agent.to_owned(),
                        record: record.value().clone(),
                    })
                    .collect()
            })
            .map_err(map_store_error)
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
            .map_err(|error| map_column_error(error, "runtime_events", event))?;
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
            .map_err(|error| map_column_error(error, "runtime_item_fulltext", &item.id))?;
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
                    .map_err(|error| map_column_error(error, "runtime_search", &item.id))?
                    .flatten()
            } else {
                connection
                    .query_row(
                        "SELECT body FROM runtime_search WHERE id=?1",
                        [&item.id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map_err(|error| map_column_error(error, "runtime_search", &item.id))?
                    .flatten()
            };
            if let Some(full) = full {
                return Ok(Some(full));
            }
        }
        if let Some(text) = item.record.get("text") {
            let text = text
                .as_str()
                .ok_or_else(|| invalid_record("runtime_items", &item.id))?;
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
        if !matches!(
            record.get("role").and_then(Value::as_str),
            Some("assistant" | "user" | "system")
        ) {
            return Err(invalid_record("runtime_items", id));
        }
        Self::put_notification_item(transaction, id, agent, record, created, text, index_search)
    }

    /// Notification tools additionally write Python's `output` transcript role.
    /// The existing text writer keeps its narrower accepted role set.
    pub fn put_notification_item(
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
                Some("assistant" | "user" | "system" | "output")
            )
        {
            return Err(invalid_record("runtime_items", id));
        }
        let has_fulltext = table_exists(transaction, "runtime_item_fulltext")?;
        if text.chars().count() > 20_000 && !has_fulltext {
            return Err(invalid_record("runtime_item_fulltext", id));
        }
        let owner = transaction
            .query_row("SELECT agent FROM runtime_items WHERE id=?1", [id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if owner.as_deref().is_some_and(|owner| owner != agent) {
            return Err(invalid_record("runtime_items", id));
        }
        transaction.execute(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,?4) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![
                id,
                agent,
                serde_json::to_string(record).map_err(|_| invalid_record("runtime_items", id))?,
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
                if record["role"] == "system" || record["role"] == "output" {
                    record
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("system")
                } else {
                    record.get("role").and_then(Value::as_str).unwrap_or("")
                },
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
                _ => return Err(invalid_record("runtime_items", &item.id)),
            };
            for (index, input) in inputs.iter().enumerate() {
                let Some(fields) = input.as_object() else {
                    return Err(invalid_record("runtime_items", &item.id));
                };
                let input_id = match fields.get("id") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(id)) => Some(id.as_str()),
                    _ => return Err(invalid_record("runtime_items", &item.id)),
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

fn map_store_error(error: crate::StoreError) -> TranscriptStoreError {
    match error {
        crate::StoreError::InvalidStoredObject { table, id } => {
            TranscriptStoreError::InvalidStoredObject { table, id }
        }
        other => TranscriptStoreError::Store(other),
    }
}

fn map_column_error(error: rusqlite::Error, table: &'static str, id: &str) -> TranscriptStoreError {
    match error {
        rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..) => {
            invalid_record(table, id)
        }
        error => TranscriptStoreError::Sqlite(error),
    }
}

fn invalid_record(table: &'static str, id: &str) -> TranscriptStoreError {
    TranscriptStoreError::InvalidStoredObject {
        table,
        id: id.to_owned(),
    }
}

fn visible(record: &Value) -> bool {
    record
        .get("afterRestore")
        .is_none_or(|after_restore| after_restore.is_null())
}

fn stored_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn enrich_persisted_items(
    connection: &Connection,
    agent: &str,
    actor: &Value,
    items: &mut [Value],
) -> TranscriptStoreResult<()> {
    let completed_table = table_exists(connection, "runtime_completed_turns")?;
    let event_meta_table = table_exists(connection, "runtime_event_meta")?;
    let mut represented = std::collections::BTreeMap::<String, Vec<(usize, Option<usize>)>>::new();
    for (item_index, item) in items.iter_mut().enumerate() {
        let turn_id = item
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(turn_id) = turn_id.as_deref() {
            if item.get("turnStatus").is_none()
                && completed_table
                && connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_completed_turns WHERE id=?1)",
                    [format!("{agent}:{turn_id}")],
                    |row| row.get::<_, bool>(0),
                )?
            {
                let status = actor
                    .get("turnStatus")
                    .and_then(Value::as_str)
                    .filter(|status| matches!(*status, "completed" | "failed" | "interrupted"))
                    .or_else(|| {
                        actor
                            .get("status")
                            .and_then(Value::as_str)
                            .filter(|status| {
                                matches!(*status, "completed" | "failed" | "interrupted")
                            })
                    })
                    .unwrap_or("ended");
                item["turnStatus"] = Value::String(status.to_owned());
            }
            if actor.get("status").and_then(Value::as_str) != Some("running")
                || actor.get("turnId").and_then(Value::as_str) != Some(turn_id)
            {
                if item.get("streaming").is_some() {
                    item["streaming"] = Value::Bool(false);
                }
                if item.get("toolStatus").and_then(Value::as_str) == Some("running") {
                    item["toolStatus"] = Value::String("interrupted".to_owned());
                }
            }
        }
        let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
        if let Some(inputs) = item.get_mut("inputs") {
            let Some(inputs) = inputs.as_array_mut() else {
                return Err(invalid_record("runtime_items", "pull"));
            };
            for (input_index, input) in inputs.iter_mut().enumerate() {
                let Some(fields) = input.as_object_mut() else {
                    return Err(invalid_record("runtime_items", "pull"));
                };
                if fields.get("kind").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                let event = fields
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        (input_index == 0)
                            .then_some(item_id.as_deref())
                            .flatten()
                            .and_then(|id| id.strip_prefix(&format!("{agent}:")))
                            .map(str::to_owned)
                    });
                if let Some(event) = event {
                    fields.insert(
                        "requestedDelivery".to_owned(),
                        Value::String("queue".to_owned()),
                    );
                    represented
                        .entry(event)
                        .or_default()
                        .push((item_index, Some(input_index)));
                }
            }
        } else if item.get("role").and_then(Value::as_str) == Some("user") {
            let event = item
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.strip_prefix(&format!("{agent}:")))
                .map(str::to_owned);
            if let Some(event) = event {
                item["requestedDelivery"] = Value::String("queue".to_owned());
                represented
                    .entry(event)
                    .or_default()
                    .push((item_index, None));
            }
        }
    }
    if represented.is_empty() {
        return Ok(());
    }
    let placeholders = vec!["?"; represented.len()].join(",");
    let sql = format!(
        "SELECT id,status,error FROM runtime_events WHERE agent=? AND kind='user' AND id IN ({placeholders})"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut values: Vec<rusqlite::types::Value> = vec![agent.to_owned().into()];
    values.extend(represented.keys().cloned().map(Into::into));
    let rows = statement
        .query_map(rusqlite::params_from_iter(values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (event, status, error) in rows {
        let metadata = if event_meta_table {
            connection
                .query_row(
                    "SELECT record FROM runtime_event_meta WHERE id=?1",
                    [&event],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|raw| {
                    serde_json::from_str::<Value>(&raw)
                        .map_err(|_| invalid_record("runtime_event_meta", &event))
                })
                .transpose()?
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let delivery = metadata
            .get("requestedDelivery")
            .or_else(|| metadata.get("delivery"))
            .and_then(Value::as_str)
            .unwrap_or("queue");
        for (item_index, input_index) in &represented[&event] {
            let target = match input_index {
                Some(input) => &mut items[*item_index]["inputs"][*input],
                None => &mut items[*item_index],
            };
            if let Some(object) = target.as_object_mut() {
                object.insert("clientMessageId".to_owned(), Value::String(event.clone()));
                object.insert("deliveryStatus".to_owned(), Value::String(status.clone()));
                object.insert(
                    "requestedDelivery".to_owned(),
                    Value::String(delivery.to_owned()),
                );
                object.insert("materialized".to_owned(), Value::Bool(true));
                object.insert(
                    "deliveryError".to_owned(),
                    error.clone().map_or(Value::Null, Value::String),
                );
                object.insert("pending".to_owned(), Value::Bool(status == "pending"));
            }
        }
    }
    Ok(())
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

fn transcript_pull_write(
    transaction: &Transaction<'_>,
    scope: &str,
    after: i64,
    payload: &Value,
    deleted: bool,
    revision: Option<i64>,
) -> TranscriptStoreResult<Value> {
    let items = payload
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let order = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>();
    let mut metadata = payload.as_object().cloned().unwrap_or_default();
    metadata.remove("items");
    let mut current = transaction
        .prepare(
            "SELECT id,seq,hash,deleted FROM sync_entities WHERE collection=?1 ORDER BY rowid",
        )?
        .query_map([scope], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ),
            ))
        })?
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    let prior_meta = current.get("@meta");
    let base_missing = prior_meta.is_none();
    let was_deleted = prior_meta.is_some_and(|row| row.2);

    if deleted {
        for (id, (_, hash, was_deleted)) in &current {
            if (id.starts_with("item:") || id == "@order") && !was_deleted {
                SyncDocumentsRepository::put_entity_hash(transaction, scope, id, hash, true)?;
            }
        }
        let empty_hash = canonical_hash(&json!({}));
        SyncDocumentsRepository::put_entity_hash(transaction, scope, "@meta", &empty_hash, true)?;
        if let Some(revision) = revision {
            let hash = canonical_hash(&json!(revision));
            SyncDocumentsRepository::put_entity_hash(
                transaction,
                scope,
                "@revision",
                &hash,
                false,
            )?;
        }
    } else {
        let mut live = std::collections::BTreeSet::new();
        for item in &items {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                return Err(invalid_record("runtime_items", "pull"));
            };
            let entity_id = format!("item:{id}");
            live.insert(entity_id.clone());
            let hash = canonical_hash(item);
            SyncDocumentsRepository::put_entity_hash(transaction, scope, &entity_id, &hash, false)?;
        }
        for (id, (_, hash, was_deleted)) in &current {
            if id.starts_with("item:") && !was_deleted && !live.contains(id) {
                SyncDocumentsRepository::put_entity_hash(transaction, scope, id, hash, true)?;
            }
        }
        let order_hash = canonical_hash(&json!(order));
        SyncDocumentsRepository::put_entity_hash(transaction, scope, "@order", &order_hash, false)?;
        let metadata_hash = canonical_hash(&Value::Object(metadata.clone()));
        SyncDocumentsRepository::put_entity_hash(
            transaction,
            scope,
            "@meta",
            &metadata_hash,
            false,
        )?;
        if let Some(revision) = revision {
            let hash = canonical_hash(&json!(revision));
            SyncDocumentsRepository::put_entity_hash(
                transaction,
                scope,
                "@revision",
                &hash,
                false,
            )?;
        }
    }

    let pruned = transaction
        .prepare(
            "SELECT id,seq FROM sync_entities WHERE collection=?1 AND deleted=1 \
             AND id LIKE 'item:%' ORDER BY seq DESC LIMIT -1 OFFSET ?2",
        )?
        .query_map(params![scope, TRANSCRIPT_MAX_TOMBSTONES as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if !pruned.is_empty() {
        for (id, _) in &pruned {
            transaction.execute(
                "DELETE FROM sync_entities WHERE collection=?1 AND id=?2",
                params![scope, id],
            )?;
        }
        let previous_floor = transaction
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection=?1 AND id='@floor'",
                [scope],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let floor_sequence = previous_floor.max(pruned.iter().map(|row| row.1).max().unwrap_or(0));
        SyncDocumentsRepository::put_entity_hash_at_sequence(
            transaction,
            scope,
            "@floor",
            &canonical_hash(&json!(floor_sequence)),
            false,
            floor_sequence,
        )?;
    }

    current = transaction
        .prepare(
            "SELECT id,seq,hash,deleted FROM sync_entities WHERE collection=?1 ORDER BY rowid",
        )?
        .query_map([scope], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ),
            ))
        })?
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    let high = current.values().map(|row| row.0).max().unwrap_or(0);
    let floor = current.get("@floor").map_or(0, |row| row.0);
    if !deleted && base_missing {
        SyncDocumentsRepository::put_entity_hash_at_sequence(
            transaction,
            scope,
            "@floor",
            &canonical_hash(&json!(high)),
            false,
            high,
        )?;
    }
    let high = transaction.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection=?1",
        [scope],
        |row| row.get::<_, i64>(0),
    )?;
    let current_revision_hash = current.get("@revision").map(|row| row.1.as_str());
    let revision_hash = revision.map(|revision| canonical_hash(&json!(revision)));
    if let Some(revision_hash) = revision_hash.as_deref()
        && after >= high
        && current_revision_hash == Some(revision_hash)
    {
        let workspace_id = SyncDocumentsRepository::workspace_id(transaction)?;
        return Ok(json!({"workspaceId":workspace_id,"documents":[],"checkpoint":{"seq":high}}));
    }
    let full = after == 0 || base_missing || was_deleted || after < floor;
    let documents = if full {
        let encoded = serde_json::to_string(payload).map_err(crate::StoreError::from)?;
        vec![json!({"id":scope,"payload":encoded,"seq":high,"_deleted":deleted})]
    } else if after < high && deleted {
        vec![json!({"id":scope,"payload":"{}","seq":high,"_deleted":true})]
    } else if after < high {
        let changed = transaction
            .prepare("SELECT id,hash,deleted FROM sync_entities WHERE collection=?1 AND seq>?2 AND id!='@floor' ORDER BY seq")?
            .query_map(params![scope, after], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut changed_items = std::collections::BTreeMap::new();
        let mut removed = Vec::new();
        let mut revisions = serde_json::Map::new();
        let mut order_changed = false;
        for (id, hash, is_deleted) in changed {
            if id == "@order" {
                order_changed = true;
            } else if id.starts_with("item:") {
                let item_id = id.trim_start_matches("item:").to_owned();
                if is_deleted {
                    removed.push(item_id);
                } else {
                    changed_items.insert(item_id.clone(), hash.clone());
                    revisions.insert(item_id, Value::String(hash));
                }
            }
        }
        let ids = changed_items
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let deltas = items
            .iter()
            .filter(|item| {
                item.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ids.contains(id))
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut delta = Value::Object(metadata);
        delta["delta"] = Value::Bool(true);
        delta["items"] = Value::Array(deltas);
        delta["itemRevisions"] = Value::Object(revisions);
        delta["removed"] = json!(removed);
        if order_changed {
            delta["order"] = json!(order);
        }
        let encoded = serde_json::to_string(&delta).map_err(crate::StoreError::from)?;
        vec![json!({"id":scope,"payload":encoded,"seq":high,"_deleted":false})]
    } else {
        Vec::new()
    };
    let checkpoint = if documents.is_empty() { after } else { high };
    let workspace_id = SyncDocumentsRepository::workspace_id(transaction)?;
    Ok(json!({"workspaceId":workspace_id,"documents":documents,"checkpoint":{"seq":checkpoint}}))
}

fn canonical_hash(value: &Value) -> String {
    crate::sha256_hex(value.to_string().as_bytes())
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
    fn event_text_rejects_a_non_text_column_with_its_table_and_id() {
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        connection
            .execute_batch("CREATE TABLE runtime_events (id TEXT, agent TEXT, text); INSERT INTO runtime_events VALUES ('e','a',x'01');")
            .expect("event fixture is stored");

        assert!(matches!(
            TranscriptRepository::event_text(&connection, "a", "e"),
            Err(TranscriptStoreError::InvalidStoredObject { table: "runtime_events", id }) if id == "e"
        ));
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "transcript",
        requirements: &REQUIREMENTS,
    }
}
