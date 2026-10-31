//! Transcript item/search readers and durable sync projection from committed state.

use crate::event_write::truthy;
use crate::submit_gates::Deletion;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{
    AgentId, ComplaintRecord, PlanRecord, RuntimeStore, StoreError, Table, TaskId, TranscriptItem,
    TranscriptRepository, TranscriptStoreError, WorkItemRecord, WorkspaceViewsRepository,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranscriptError {
    #[error("unknown message in this conversation")]
    UnknownMessage,
    #[error("this conversation was deleted")]
    DeletedConversation,
    #[error("This agent was deleted")]
    DeletedAssetOwner,
    #[error("Unknown managed agent")]
    UnknownAssetOwner,
    #[error("full transcript content is unavailable")]
    MissingContent,
    #[error("invalid stored object {table}/{id}")]
    InvalidStoredObject { table: &'static str, id: String },
    #[error("invalid stored object {table}/{id}")]
    InvalidStoredState { table: String, id: String },
    #[error("transcript storage failed")]
    Store,
    #[error("this search result is unavailable")]
    SearchResultUnavailable,
    #[error("this chat is unavailable")]
    ChatUnavailable,
    #[error("use one history cursor")]
    HistoryCursor,
    #[error("search query is invalid")]
    InvalidSearch,
    #[error("search context changed; retry the read")]
    SnapshotChanged,
    #[error("read-only snapshot requires transcript cache repair")]
    ReadOnlyRepair,
    #[error("transcript operation requires {0}")]
    RequiresDependency(&'static str),
}

impl From<TranscriptStoreError> for TranscriptError {
    fn from(error: TranscriptStoreError) -> Self {
        match error {
            TranscriptStoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ReadOnly =>
            {
                Self::ReadOnlyRepair
            }
            TranscriptStoreError::InvalidStoredObject { table, id } => {
                Self::InvalidStoredObject { table, id }
            }
            TranscriptStoreError::Store(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                error,
                _,
            ))) if error.code == rusqlite::ErrorCode::ReadOnly => Self::ReadOnlyRepair,
            TranscriptStoreError::Store(_) => Self::Store,
            TranscriptStoreError::Sqlite(_) => Self::Store,
        }
    }
}

impl From<StoreError> for TranscriptError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::InvalidStoredObject { table, id } => {
                Self::InvalidStoredObject { table, id }
            }
            StoreError::TranscriptFullTextUnavailable { id } => Self::InvalidStoredObject {
                table: "runtime_item_fulltext",
                id,
            },
            StoreError::TranscriptInputsIncomplete { id } => Self::InvalidStoredObject {
                table: "runtime_items",
                id,
            },
            StoreError::InvalidSyncObject { scope, id }
            | StoreError::InvalidSyncObjectField { scope, id, .. } => {
                Self::InvalidStoredState { table: scope, id }
            }
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ReadOnly =>
            {
                Self::ReadOnlyRepair
            }
            StoreError::StateDirectory(_)
            | StoreError::AlreadyOwned
            | StoreError::MissingDatabase(_)
            | StoreError::Migration { .. }
            | StoreError::UnsafeStateDirectory
            | StoreError::SchemaMismatch(_)
            | StoreError::InvalidStoredRecord { .. }
            | StoreError::SchemaObjectMismatch { .. }
            | StoreError::Sqlite(_)
            | StoreError::CommitOutcomeUnknown(_)
            | StoreError::RequestResult(_)
            | StoreError::Outbox(_)
            | StoreError::Json(_)
            | StoreError::SchemaValidation(_)
            | StoreError::RecordContract(_)
            | StoreError::WriterPoisoned
            | StoreError::WriterActorInitialization(_)
            | StoreError::WriterActorControl(_)
            | StoreError::CommitActorOutcomeUnknown(_)
            | StoreError::ReadTaskFailed => Self::Store,
        }
    }
}
impl From<studio_runtime_store::analytics::AnalyticsError> for TranscriptError {
    fn from(error: studio_runtime_store::analytics::AnalyticsError) -> Self {
        match error {
            studio_runtime_store::analytics::AnalyticsError::InvalidStoredRow { table, id } => {
                invalid_stored(table, &id)
            }
            studio_runtime_store::analytics::AnalyticsError::InvalidRequest(_)
            | studio_runtime_store::analytics::AnalyticsError::InvalidStoredObject(_)
            | studio_runtime_store::analytics::AnalyticsError::UnknownAgent
            | studio_runtime_store::analytics::AnalyticsError::Read(_)
            | studio_runtime_store::analytics::AnalyticsError::Sqlite(_)
            | studio_runtime_store::analytics::AnalyticsError::Serialization(_) => Self::Store,
        }
    }
}
impl From<rusqlite::Error> for TranscriptError {
    // rusqlite errors are non-exhaustive; only a read-only failure has a dedicated remedy.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(error, _)
                if error.code == rusqlite::ErrorCode::ReadOnly =>
            {
                Self::ReadOnlyRepair
            }
            _ => Self::Store,
        }
    }
}

/// The sync document of one conversation. After the migration that retired the copies
/// the page is built from `runtime_items` at read time and nothing is written: a delta
/// when the page at `after` was sent before, otherwise the full page. Without the
/// migration check the cached copy in `sync_entities` is kept (see
/// `retire_transcript_copies`).
pub async fn pull_scope(
    store: Arc<RuntimeStore>,
    scope: &str,
    agent: &str,
    after: i64,
) -> Result<Value, TranscriptError> {
    let scope = scope.to_owned();
    let agent = agent.to_owned();
    let now = crate::transcript_runtime::observed_at()?;
    let read_scope = scope.clone();
    let read_agent = agent.clone();
    let cached = store
        .read_async("runtime-services/transcript_scope", move |transaction| {
            if studio_runtime_store::durable_seq::transcript_copies_retired(transaction)? {
                return read_time_cursor(transaction, &read_scope, &read_agent, after)
                    .map(ReadStep::Cursor);
            }
            pull_scope_cached_in(transaction, &read_scope, &read_agent, after).map(ReadStep::Copy)
        })
        .await;
    match cached {
        Ok(ReadStep::Cursor(ReadTimeCursor::Done(result))) => return Ok(result),
        Ok(ReadStep::Cursor(ReadTimeCursor::Page {
            through,
            workspace_id,
        })) => {
            let payload =
                crate::transcript_runtime::page_at(&store, agent.clone(), Default::default(), now)
                    .await?;
            return read_time_documents(
                store.root(),
                &scope,
                &agent,
                after.max(0),
                through,
                workspace_id,
                &payload,
            );
        }
        Ok(ReadStep::Copy(Some(result))) => return Ok(result),
        Ok(ReadStep::Copy(None)) | Err(TranscriptError::ReadOnlyRepair) => {}
        Err(error) => return Err(error),
    }

    let transaction_scope = scope;
    let transaction_agent = agent;
    let write_store = Arc::clone(&store);
    store
        .transaction_with(move |transaction| {
            pull_scope_in(
                transaction,
                &write_store,
                &transaction_scope,
                &transaction_agent,
                after,
                now,
            )
        })
        .await
}

/// Ids of the most recently changed conversations that have items, newest first,
/// without those in `exclude`.
pub async fn recent_chats(
    store: &RuntimeStore,
    limit: usize,
    exclude: Vec<String>,
) -> Result<Vec<String>, TranscriptError> {
    store
        .read_async("runtime-services/transcript_recent", move |transaction| {
            let wanted = (limit + exclude.len()) as i64;
            let ids = transaction
                .prepare_cached(
                    "SELECT a.id FROM runtime_agents a
                     JOIN durable_rows d ON d.tbl='runtime_agents' AND d.id=a.id
                     WHERE json_extract(a.record,'$.deletedAt') IS NULL
                       AND EXISTS(SELECT 1 FROM runtime_items i WHERE i.agent=a.id)
                     ORDER BY d.seq DESC LIMIT ?1",
                )?
                .query_map([wanted], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, TranscriptError>(
                ids.into_iter()
                    .filter(|id| !exclude.contains(id))
                    .take(limit)
                    .collect(),
            )
        })
        .await
}

/// How a page looked at a change number, to answer a cursor with a delta instead of a
/// full page. It lives in memory only and is not a copy of the conversation: a lost
/// entry costs one full page. Entries are keyed by state directory and agent.
struct PageIndex {
    items: std::collections::BTreeMap<String, String>,
    order: Vec<String>,
    meta: String,
}

impl PageIndex {
    fn of(payload: &Value) -> Result<(Self, Vec<&Value>), TranscriptError> {
        let items = payload
            .get("items")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut index = PageIndex {
            items: Default::default(),
            order: Vec::with_capacity(items.len()),
            meta: String::new(),
        };
        for item in items {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored("runtime_items", "pull"))?;
            index.order.push(id.to_owned());
            index.items.insert(
                id.to_owned(),
                studio_runtime_store::sha256_hex(item.to_string().as_bytes()),
            );
        }
        let meta = payload
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| key.as_str() != "items")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        index.meta = studio_runtime_store::sha256_hex(Value::Object(meta).to_string().as_bytes());
        Ok((index, items.iter().collect()))
    }
}

/// Largest number of remembered pages, in pages. Each holds one hash for each item.
const PAGE_HISTORY_ENTRIES: usize = 256;
type PageHistory = Vec<(std::path::PathBuf, String, i64, Arc<PageIndex>)>;
static PAGE_HISTORY: std::sync::Mutex<PageHistory> = std::sync::Mutex::new(Vec::new());

fn page_history() -> std::sync::MutexGuard<'static, PageHistory> {
    PAGE_HISTORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The sync documents for a page built at read time: a delta when the page at `after`
/// is remembered, a full page otherwise, and none when the page did not change.
fn read_time_documents(
    root: &std::path::Path,
    scope: &str,
    agent: &str,
    after: i64,
    through: i64,
    workspace_id: String,
    payload: &Value,
) -> Result<Value, TranscriptError> {
    let (index, items) = PageIndex::of(payload)?;
    let index = Arc::new(index);
    let prior = if after > 0 {
        page_history()
            .iter()
            .find(|(path, id, seq, _)| path == root && id == agent && *seq == after)
            .map(|entry| Arc::clone(&entry.3))
    } else {
        None
    };
    {
        let mut history = page_history();
        history.retain(|(path, id, seq, _)| !(path == root && id == agent && *seq == through));
        history.push((
            root.to_path_buf(),
            agent.to_owned(),
            through,
            Arc::clone(&index),
        ));
        if history.len() > PAGE_HISTORY_ENTRIES {
            history.remove(0);
        }
    }
    let encode = |value: &Value| serde_json::to_string(value).map_err(|_| TranscriptError::Store);
    let Some(prior) = prior else {
        let document =
            json!({"id": scope, "payload": encode(payload)?, "seq": through, "_deleted": false});
        return Ok(json!({
            "workspaceId": workspace_id,
            "documents": [document],
            "checkpoint": {"seq": through}
        }));
    };
    let changed = items
        .into_iter()
        .filter(|item| {
            item["id"]
                .as_str()
                .is_some_and(|id| prior.items.get(id) != index.items.get(id))
        })
        .cloned()
        .collect::<Vec<_>>();
    let removed = prior
        .items
        .keys()
        .filter(|id| !index.items.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    let order_changed = prior.order != index.order;
    if changed.is_empty() && removed.is_empty() && !order_changed && prior.meta == index.meta {
        return Ok(json!({
            "workspaceId": workspace_id,
            "documents": [],
            "checkpoint": {"seq": after}
        }));
    }
    let mut delta = payload.as_object().cloned().unwrap_or_default();
    delta.insert("delta".to_owned(), Value::Bool(true));
    delta.insert("items".to_owned(), Value::Array(changed));
    delta.insert("removed".to_owned(), json!(removed));
    if order_changed {
        delta.insert("order".to_owned(), json!(index.order));
    }
    let document = json!({"id": scope, "payload": encode(&Value::Object(delta))?, "seq": through, "_deleted": false});
    Ok(json!({
        "workspaceId": workspace_id,
        "documents": [document],
        "checkpoint": {"seq": through}
    }))
}

enum ReadStep {
    Cursor(ReadTimeCursor),
    Copy(Option<Value>),
}

enum ReadTimeCursor {
    /// Nothing newer than the cursor, or a deleted conversation.
    Done(Value),
    /// The conversation changed; build its page.
    Page { through: i64, workspace_id: String },
}

/// Decide from the change number alone whether a page must be built.
fn read_time_cursor(
    transaction: &Transaction<'_>,
    scope: &str,
    agent: &str,
    after: i64,
) -> Result<ReadTimeCursor, TranscriptError> {
    let after = after.max(0);
    let actor = TranscriptRepository::agent(transaction, agent)?;
    let deleted = actor.as_ref().is_none_or(|a| a.is_deleted());
    let through = studio_runtime_store::durable_seq::transcript_change_cursor(transaction, agent)?;
    let workspace_id = studio_runtime_store::SyncDocumentsRepository::workspace_id(transaction)?;
    if after > 0 && after >= through {
        return Ok(ReadTimeCursor::Done(json!({
            "workspaceId": workspace_id,
            "documents": [],
            "checkpoint": {"seq": through}
        })));
    }
    if deleted {
        return Ok(ReadTimeCursor::Done(json!({
            "workspaceId": workspace_id,
            "documents": [{"id": scope, "payload": "{}", "seq": through, "_deleted": true}],
            "checkpoint": {"seq": through}
        })));
    }
    Ok(ReadTimeCursor::Page {
        through,
        workspace_id,
    })
}

/// Read-only pulls stay cheap when the requested revision is already cached.
/// A missing or stale payload returns `None` so it can be built with the
/// current revision inside the writer transaction.
fn pull_scope_cached_in(
    transaction: &Transaction<'_>,
    scope: &str,
    agent: &str,
    after: i64,
) -> Result<Option<Value>, TranscriptError> {
    let actor = TranscriptRepository::agent(transaction, agent)?;
    let deleted = actor
        .as_ref()
        .is_none_or(studio_runtime_store::AgentRecord::is_deleted);
    let revision = TranscriptRepository::revision(transaction, agent)?;
    let high = transaction.query_row(
        "SELECT coalesce(max(seq),0) FROM sync_entities WHERE collection=?1",
        [scope],
        |r| r.get::<_, i64>(0),
    )?;
    let hash = transaction
        .query_row(
            "SELECT hash FROM sync_entities WHERE collection=?1 AND id='@revision' AND deleted=0",
            [scope],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if after > 0
        && after >= high
        && hash.as_deref()
            == Some(studio_runtime_store::sha256_hex(revision.to_string().as_bytes()).as_str())
    {
        return Ok(Some(json!({
            "workspaceId": studio_runtime_store::SyncDocumentsRepository::workspace_id(transaction)?,
            "documents": [],
            "checkpoint": {"seq": high}
        })));
    }
    let cache = transaction
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection=?1 AND id='@revision' AND hash=?2 AND deleted=0",
            params![scope, studio_runtime_store::sha256_hex(revision.to_string().as_bytes())],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(raw) = cache else {
        return Ok(None);
    };
    let payload: Value =
        serde_json::from_str(&raw).map_err(|_| invalid_stored("sync_entities", scope))?;
    if !deleted {
        validate_cached_payload(scope, &payload)?;
    }
    Ok(Some(TranscriptRepository::pull_payload(
        transaction,
        scope,
        after.max(0),
        &payload,
        deleted,
        Some(revision),
    )?))
}

fn pull_scope_in(
    transaction: &Transaction<'_>,
    store: &RuntimeStore,
    scope: &str,
    agent: &str,
    after: i64,
    now: f64,
) -> Result<Value, TranscriptError> {
    if let Some(result) = pull_scope_cached_in(transaction, scope, agent, after)? {
        return Ok(result);
    }
    let actor = TranscriptRepository::agent(transaction, agent)?;
    let deleted = actor
        .as_ref()
        .is_none_or(studio_runtime_store::AgentRecord::is_deleted);
    let revision = TranscriptRepository::revision(transaction, agent)?;
    let payload = if deleted {
        json!({})
    } else {
        crate::transcript_runtime::page_in_transaction(
            store,
            transaction,
            agent,
            &crate::transcript_runtime::PageRequest::default(),
            now,
        )?
    };
    let response = TranscriptRepository::pull_payload(
        transaction,
        scope,
        after.max(0),
        &payload,
        deleted,
        Some(revision),
    )?;
    // Keep the bounded result on the existing revision entity. A lost
    // response can replay any original cursor without observing time again.
    let encoded = serde_json::to_string(&payload).map_err(|_| TranscriptError::Store)?;
    transaction.execute(
        "UPDATE sync_entities SET payload=?1 WHERE collection=?2 AND id='@revision'",
        params![encoded, scope],
    )?;
    Ok(response)
}

fn validate_cached_payload(scope: &str, payload: &Value) -> Result<(), TranscriptError> {
    let cache_id = format!("{scope}/@revision");
    let items = payload
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_stored("sync_entities", &cache_id))?;
    if payload.get("truncated").and_then(Value::as_bool).is_none() {
        return Err(invalid_stored("sync_entities", &cache_id));
    }
    for item in items {
        item["id"]
            .as_str()
            .ok_or_else(|| invalid_stored("sync_entities", &cache_id))?;
    }
    Ok(())
}

pub async fn item(
    store: &RuntimeStore,
    agent: String,
    identity: String,
) -> Result<Value, TranscriptError> {
    store
        .read_async("runtime-services/transcript", move |connection| {
            item_in(connection, &agent, &identity)
        })
        .await
}

fn item_in(connection: &Connection, agent: &str, identity: &str) -> Result<Value, TranscriptError> {
    let actor =
        TranscriptRepository::agent(connection, agent)?.ok_or(TranscriptError::UnknownMessage)?;
    if actor.is_deleted() {
        return Err(TranscriptError::DeletedConversation);
    }
    let row = TranscriptRepository::resolve_item(connection, agent, identity)?
        .ok_or(TranscriptError::UnknownMessage)?;
    validate_parent(&row)?;
    if let Some(inputs) = row
        .record
        .inputs()
        .value()
        .filter(|inputs| !inputs.is_empty())
    {
        for (index, input) in inputs.iter().enumerate() {
            let candidate = hydrate_input(&row, input, index, agent, Some(identity), |id| {
                Ok(TranscriptRepository::event_text(connection, agent, id)?)
            })?;
            if let Some(candidate) = candidate {
                return Ok(candidate);
            }
        }
    }
    let full = TranscriptRepository::full_text(connection, &row)?;
    parent_projection(&row, agent, full, |id| {
        Ok(TranscriptRepository::event_text(connection, agent, id)?)
    })
}

pub async fn search(
    store: &RuntimeStore,
    agent: String,
    query: String,
    limit: i64,
) -> Result<Value, TranscriptError> {
    store
        .read_async("runtime-services/transcript", move |connection| {
            search_in(connection, &agent, &query, limit)
        })
        .await
}

fn search_in(
    connection: &Connection,
    agent: &str,
    query: &str,
    limit: i64,
) -> Result<Value, TranscriptError> {
    let query = query.trim();
    let limit = limit.clamp(1, 500) as usize;
    let actor =
        TranscriptRepository::agent(connection, agent)?.ok_or(TranscriptError::UnknownMessage)?;
    if actor.is_deleted() {
        return Err(TranscriptError::DeletedConversation);
    }
    if query.is_empty() {
        return Ok(json!({"results": [], "truncated": false}));
    }
    let mut results = Vec::new();
    for row in TranscriptRepository::items_for_agent(connection, agent)? {
        let candidates = if let Some(inputs) = row.record.inputs().value() {
            let mut candidates = Vec::with_capacity(inputs.len());
            for (index, input) in inputs.iter().enumerate() {
                let input_id = input.id();
                let Some(mut candidate) = hydrate_input(&row, input, index, agent, None, |id| {
                    Ok(TranscriptRepository::event_text(connection, agent, id)?)
                })?
                else {
                    continue;
                };
                candidate["clientMessageId"] =
                    input_id.map_or(Value::Null, |id| Value::String(id.to_owned()));
                candidates.push(candidate);
            }
            candidates
        } else {
            let full = TranscriptRepository::full_text(connection, &row)?;
            vec![parent_projection(&row, agent, full, |id| {
                Ok(TranscriptRepository::event_text(connection, agent, id)?)
            })?]
        };
        for candidate in candidates {
            let text = candidate["text"]
                .as_str()
                .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
            let Some(position) = find_lowercase(text, query) else {
                continue;
            };
            let characters = text.chars().collect::<Vec<_>>();
            let start = position.saturating_sub(80).min(characters.len());
            let end = position
                .saturating_add(query.chars().count())
                .saturating_add(160)
                .min(characters.len());
            let excerpt = characters[start.min(end)..end].iter().collect::<String>();
            results.push(json!({
                "id": candidate["id"],
                "sourceId": candidate["sourceId"],
                "clientMessageId": candidate["clientMessageId"],
                "role": candidate["role"],
                "turnId": candidate["turnId"],
                "text": candidate["text"],
                "agent": agent,
                "excerpt": excerpt,
            }));
            if results.len() > limit {
                results.truncate(limit);
                return Ok(json!({"results": results, "truncated": true}));
            }
        }
    }
    Ok(json!({"results": results, "truncated": false}))
}

pub async fn search_item(store: &RuntimeStore, identity: String) -> Result<Value, TranscriptError> {
    let directory = crate::sync_entity_context::RoomDirectory::read_and_prepare(store).await?;
    store
        .read_async("runtime-services/transcript", move |connection| {
            search_item_in(connection, &directory, &identity)
        })
        .await
}

fn search_item_in(
    connection: &Connection,
    directory: &crate::sync_entity_context::RoomDirectory,
    identity: &str,
) -> Result<Value, TranscriptError> {
    if !directory.matches(connection)? {
        return Err(TranscriptError::SnapshotChanged);
    }
    if let Some(row) = TranscriptRepository::item_any_agent(connection, identity)? {
        let actor = directory
            .agents()
            .iter()
            .find(|agent| agent.id.as_ref() == Some(&row.agent))
            .ok_or(TranscriptError::SearchResultUnavailable)?;
        if actor.deleted_at.as_ref().is_some_and(Deletion::is_deleted)
            || row
                .record
                .after_restore()
                .is_some_and(|value| !value.is_empty())
        {
            return Err(TranscriptError::SearchResultUnavailable);
        }
        let text = searchable_text(connection, &row)?;
        let mut result = row
            .record
            .value()
            .as_object()
            .cloned()
            .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
        result.insert("text".to_owned(), Value::String(text));
        result.insert("agent".to_owned(), Value::String(row.agent));
        result.insert("kind".to_owned(), Value::String("message".to_owned()));
        return Ok(Value::Object(result));
    }

    if let Some((id, room, sender, text, sequence, created, deliveries)) = connection
        .query_row(
            "SELECT id,room,sender,text,seq,created,deliveries FROM runtime_chat_messages WHERE id=?1",
            [identity],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, i64>(4)?, row.get::<_, f64>(5)?, row.get::<_, String>(6)?)),
        )
        .optional()
        .map_err(|_| TranscriptError::Store)?
    {
        let room_record = studio_runtime_store::system_reads::get(connection, "runtime_rooms", &room)?
            .ok_or(TranscriptError::ChatUnavailable)?;
        if room_view(connection, directory, &room_record)?.is_none() {
            return Err(TranscriptError::ChatUnavailable);
        }
        serde_json::from_str::<Value>(&deliveries)
            .map_err(|_| invalid_stored("runtime_chat_messages", &id))?;
        return Ok(json!({"id":id,"room":room,"sender":sender,"text":text,
            "seq":sequence,"created":created,"deliveries":deliveries,
            "agent":sender,"kind":"room"}));
    }

    let result = if let Some(row) = Table::<WorkItemRecord>::new(connection)
        .get(&TaskId::from(identity))
        .map_err(|_| TranscriptError::Store)?
    {
        Some((row.into_value(), "work"))
    } else if let Some(row) = Table::<PlanRecord>::new(connection)
        .get(&AgentId::from(identity))
        .map_err(|_| TranscriptError::Store)?
    {
        Some((row.value(), "plan"))
    } else {
        Table::<ComplaintRecord>::new(connection)
            .get(&String::from(identity))
            .map_err(|_| TranscriptError::Store)?
            .map(|row| (row.into_value(), "complaint"))
    };
    let Some((mut record, kind)) = result else {
        return Err(TranscriptError::SearchResultUnavailable);
    };
    let owner = record
        .get("rootId")
        .filter(|value| truthy(Some(value)))
        .or_else(|| record.get("leadId").filter(|value| truthy(Some(value))))
        .or_else(|| record.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_stored("runtime_search", identity))?;
    directory
        .agents()
        .iter()
        .find(|actor| {
            actor.id.as_ref() == Some(&owner)
                && !actor.deleted_at.as_ref().is_some_and(Deletion::is_deleted)
        })
        .ok_or(TranscriptError::SearchResultUnavailable)?;
    let text = if let Some(text) = record
        .get("text")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        text.to_owned()
    } else {
        let table = match kind {
            "work" => "runtime_work",
            "plan" => "runtime_plans",
            _ => "runtime_complaints",
        };
        let raw: String = connection.query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [identity],
            |r| r.get(0),
        )?;
        let ordered: crate::turn_prepare::OrderedJson =
            serde_json::from_str(&raw).map_err(|_| invalid_stored(table, identity))?;
        serde_json::to_string_pretty(&ordered).map_err(|_| invalid_stored(table, identity))?
    };
    let object = record
        .as_object_mut()
        .ok_or_else(|| invalid_stored("runtime_search", identity))?;
    object.insert("agent".to_owned(), Value::String(owner));
    object.insert("kind".to_owned(), Value::String(kind.to_owned()));
    object.insert("text".to_owned(), Value::String(text));
    Ok(record)
}

fn room_view(
    connection: &Connection,
    directory: &crate::sync_entity_context::RoomDirectory,
    room: &Value,
) -> Result<Option<Value>, TranscriptError> {
    if room["kind"] == "federated" && !connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_federation_rooms WHERE id=?1 AND json_extract(record,'$.status')='approved' AND json_extract(record,'$.localApproved')=1 AND json_extract(record,'$.remoteApproved')=1)",
        [room["id"].as_str()], |r| r.get::<_, bool>(0),
    )? { return Ok(None); }
    let context = directory.for_room(connection, room, false)?;
    Ok(crate::sync_views::room_entity_view(room, &context))
}

pub async fn search_work(
    store: &RuntimeStore,
    query: String,
    agent_filter: Option<String>,
    limit: i64,
) -> Result<Value, TranscriptError> {
    let directory = crate::sync_entity_context::RoomDirectory::read_and_prepare(store).await?;
    store
        .read_async("runtime-services/transcript", move |connection| {
            search_work_in_transaction(
                connection,
                &directory,
                &query,
                agent_filter.as_deref(),
                limit,
            )
        })
        .await
}

/// Transaction-aware search for callback dispatch. The room directory is
/// captured before opening the writer transaction and compared against rows
/// again here before any search result is returned.
pub(crate) fn search_work_in_transaction(
    connection: &Connection,
    directory: &crate::sync_entity_context::RoomDirectory,
    query: &str,
    agent_filter: Option<&str>,
    limit: i64,
) -> Result<Value, TranscriptError> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 500 {
        return Err(TranscriptError::InvalidSearch);
    }
    let limit = limit.clamp(1, 100) as usize;
    if !directory.matches(connection)? {
        return Err(TranscriptError::SnapshotChanged);
    }
    let agents = directory.agents();
    let caller_root = if let Some(agent) = agent_filter {
        let caller = agents
            .iter()
            .find(|caller| caller.id.as_deref() == Some(agent))
            .ok_or(TranscriptError::SearchResultUnavailable)?;
        if caller.deleted_at.as_ref().is_some_and(Deletion::is_deleted) {
            return Err(TranscriptError::DeletedConversation);
        }
        caller.root_id.as_deref()
    } else {
        None
    };
    let allowed = agents
        .iter()
        .filter(|agent| !agent.deleted_at.as_ref().is_some_and(Deletion::is_deleted))
        .filter(|agent| caller_root.is_none_or(|root| agent.root_id.as_deref() == Some(root)))
        .filter_map(|agent| agent.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let match_query = query
        .split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ");
    let mut found = Vec::new();
    for (id, agent, kind) in TranscriptRepository::search_hits(connection, &match_query)? {
        if !allowed.contains(agent.as_str()) || agent_filter.is_some_and(|caller| caller != agent) {
            continue;
        }
        let Some(item) = TranscriptRepository::item(connection, &agent, &id)? else {
            continue;
        };
        let body = searchable_text(connection, &item)?;
        let excerpt = excerpt(&body, query);
        found.push(json!({"id":id,"agent":agent,"type":"message","kind":kind,"reference":id,"excerpt":excerpt,"text":excerpt}));
    }
    for (table, kind, field) in [
        ("runtime_work", "work", "title"),
        ("runtime_complaints", "complaint", "title"),
        ("runtime_plans", "plan", "text"),
    ] {
        let records = match table {
            "runtime_work" => WorkspaceViewsRepository::search_records::<
                studio_runtime_store::WorkItemRecord,
            >(connection)?
            .into_iter()
            .map(|(id, record)| (id, record.into_value()))
            .collect::<Vec<_>>(),
            "runtime_complaints" => WorkspaceViewsRepository::search_records::<
                studio_runtime_store::ComplaintRecord,
            >(connection)?
            .into_iter()
            .map(|(id, record)| (id, record.into_value()))
            .collect(),
            "runtime_plans" => WorkspaceViewsRepository::search_records::<
                studio_runtime_store::PlanRecord,
            >(connection)?
            .into_iter()
            .map(|(id, record)| (id, record.into_value()))
            .collect(),
            _ => unreachable!("declared search tables"),
        };
        for (id, record) in records {
            let root = record
                .get("rootId")
                .filter(|value| truthy(Some(value)))
                .or_else(|| record.get("leadId").filter(|value| truthy(Some(value))))
                .or_else(|| record.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored(table, &id))?;
            if !allowed.contains(root) || caller_root.is_some_and(|caller| caller != root) {
                continue;
            }
            let serialized = serde_json::to_string(&record).map_err(|_| TranscriptError::Store)?;
            if find_lowercase(&serialized, query).is_none() {
                continue;
            }
            let excerpt = record
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect::<String>();
            found.push(json!({
                "id": id, "agent": root, "type": kind, "kind": kind,
                "excerpt": excerpt, "reference": id, "text": excerpt
            }));
        }
    }
    let mut readable = std::collections::BTreeSet::new();
    for room in studio_runtime_store::system_reads::all(connection, "runtime_rooms")? {
        let Some(view) = room_view(connection, directory, &room)? else {
            continue;
        };
        if let Some(caller) = agent_filter {
            let members = view["members"].as_array().cloned().unwrap_or_default();
            if !members.iter().any(|id| id == caller) {
                continue;
            }
            if room["kind"] != "federated" {
                if caller_root.is_none() || room["rootId"] == "all" {
                    continue;
                }
                let foreign = members
                    .iter()
                    .any(|id| !allowed.contains(id.as_str().unwrap_or_default()));
                let direct = room["radio"]["direct"] == true
                    && members.iter().all(|id| {
                        directory.agents().iter().any(|a| {
                            a.id.as_deref() == id.as_str()
                                && a.shared_room_id.as_deref() == room["id"].as_str()
                                && a.cwd.as_deref() == room["projectPath"].as_str()
                        })
                    });
                if foreign
                    && !(room["kind"] == "private"
                        && members.len() == 2
                        && (view.get("peerTeamId").is_some() || direct))
                {
                    continue;
                }
            }
        }
        if let Some(id) = room["id"].as_str() {
            readable.insert(id.to_owned());
        }
    }
    let mut messages=connection.prepare("SELECT id,room,sender,text FROM runtime_chat_messages WHERE instr(lower(text),lower(?1))>0 ORDER BY seq DESC LIMIT 1000")?;
    for row in messages.query_map([query], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (id, room, sender, text) = row?;
        if readable.contains(&room) {
            let excerpt = text.chars().take(500).collect::<String>();
            found.push(json!({"id":id,"room":room,"agent":sender,"type":"room","kind":"Agent chat","excerpt":excerpt,"reference":id,"text":excerpt}));
        }
    }
    found.truncate(limit);
    Ok(json!({"results":found,"query":query,"limit":limit}))
}

fn searchable_text(
    connection: &Connection,
    row: &TranscriptItem,
) -> Result<String, TranscriptError> {
    let mut full = TranscriptRepository::full_text(connection, row)?;
    if full.as_deref() == Some("") && row.record.inputs().value().is_some_and(|v| !v.is_empty()) {
        // The repository's fallback may be the empty legacy parent, not a body.
        let persisted = studio_runtime_store::text_blobs::reference(connection, &row.id)?
            .is_some()
            || connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_item_fulltext WHERE id=?1 AND body IS NOT NULL)",
                [&row.id],
                |r| r.get::<_, bool>(0),
            )?;
        if !persisted {
            full = None;
        }
    }
    let value = parent_projection(row, &row.agent, full, |id| {
        Ok(TranscriptRepository::event_text(
            connection, &row.agent, id,
        )?)
    })?;
    Ok(value["text"].as_str().unwrap_or_default().to_owned())
}

fn excerpt(body: &str, query: &str) -> String {
    const TOKEN_LIMIT: usize = 30;
    // Use the same Unicode lowercase rule as transcript search. Full casefold
    // (for example Straße/STRASSE) is deliberately unsupported by both readers.
    let token = |ch: char| ch.is_alphanumeric();
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, ch) in body.char_indices() {
        if token(ch) {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(begin) = start.take() {
            tokens.push((begin, index));
        }
    }
    if let Some(begin) = start {
        tokens.push((begin, body.len()));
    }
    if tokens.len() <= TOKEN_LIMIT {
        return body.to_owned();
    }
    let terms = query
        .split(|ch| !token(ch))
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect::<std::collections::BTreeSet<_>>();
    let center = tokens
        .iter()
        .position(|(start, end)| terms.contains(&body[*start..*end].to_lowercase()))
        .unwrap_or(0);
    let start = center
        .saturating_sub(TOKEN_LIMIT / 2 - 1)
        .min(tokens.len() - TOKEN_LIMIT);
    let end = (start + TOKEN_LIMIT).min(tokens.len());
    format!(
        "{}{}{}",
        if start > 0 { " … " } else { "" },
        &body[tokens[start].0..tokens[end - 1].1],
        if end < tokens.len() { " … " } else { "" }
    )
}

fn project(
    row: &TranscriptItem,
    input: Option<&studio_runtime_schema::transcript_record::TranscriptInput>,
    source_id: &str,
    identity: &str,
    agent: &str,
    text: String,
    complete: bool,
) -> Result<Value, TranscriptError> {
    validate_parent(row)?;
    let parent_role = row.record.role();
    let role = Value::String(
        input
            .map_or(parent_role, |input| input.projected_role(parent_role))
            .as_str()
            .to_owned(),
    );
    let mut projected = row
        .record
        .value()
        .as_object()
        .cloned()
        .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
    if let Some(input) = input {
        let input_value = input.value();
        let fields = input_value
            .as_object()
            .ok_or_else(|| invalid_stored("runtime_items", &row.id))?;
        projected.extend(fields.clone());
    }
    projected.insert("id".to_owned(), Value::String(identity.to_owned()));
    projected.insert("sourceId".to_owned(), Value::String(source_id.to_owned()));
    projected.insert("agent".to_owned(), Value::String(agent.to_owned()));
    projected.insert("role".to_owned(), role);
    projected.insert("text".to_owned(), Value::String(text));
    projected.insert("truncated".to_owned(), Value::Bool(!complete));
    Ok(Value::Object(projected))
}

fn hydrate_input<F>(
    row: &TranscriptItem,
    input: &studio_runtime_schema::transcript_record::TranscriptInput,
    index: usize,
    agent: &str,
    requested_identity: Option<&str>,
    event_lookup: F,
) -> Result<Option<Value>, TranscriptError>
where
    F: FnOnce(&str) -> Result<Option<String>, TranscriptError>,
{
    let truncated = input.effective_truncated();
    let input_id = input.id();
    let identity = input_id.map_or_else(
        || format!("{}:{index}", row.id),
        |id| format!("{agent}:{id}"),
    );
    if requested_identity.is_some_and(|requested| requested != identity) {
        return Ok(None);
    }
    let event = input_id.map(event_lookup).transpose()?.flatten();
    if truncated && event.is_none() {
        return Err(TranscriptError::MissingContent);
    }
    let complete = !truncated || event.is_some();
    let text = input.projected_text(event);
    project(row, Some(input), &row.id, &identity, agent, text, complete).map(Some)
}

fn parent_projection<F>(
    row: &TranscriptItem,
    agent: &str,
    mut full: Option<String>,
    mut event_lookup: F,
) -> Result<Value, TranscriptError>
where
    F: FnMut(&str) -> Result<Option<String>, TranscriptError>,
{
    let truncated = validate_parent(row)?;
    if full.is_none()
        && let Some(inputs) = row.record.inputs().value()
    {
        let mut pieces = Vec::new();
        for input in inputs {
            let input_id = input.id();
            let event = input_id.map(&mut event_lookup).transpose()?.flatten();
            if let Some(event) = event {
                pieces.push(event);
            } else if input_id.is_none()
                && !input.effective_truncated()
                && let Some(text) = input.text()
            {
                pieces.push(text.to_owned());
            }
        }
        if !pieces.is_empty() {
            full = Some(pieces.join("\n\n"));
        }
    }
    if truncated && full.as_deref().is_none_or(str::is_empty) {
        return Err(TranscriptError::MissingContent);
    }
    let complete = !truncated || full.is_some();
    let text = full
        .or(row.record.text().value().cloned())
        .unwrap_or_default();
    project(row, None, &row.id, &row.id, agent, text, complete)
}

fn validate_parent(row: &TranscriptItem) -> Result<bool, TranscriptError> {
    if row.record.id().as_ref() != row.id {
        return Err(invalid_stored("runtime_items", &row.id));
    }
    Ok(row.record.truncated().unwrap_or(false))
}

fn invalid_stored(table: &'static str, id: &str) -> TranscriptError {
    TranscriptError::InvalidStoredObject {
        table,
        id: id.to_owned(),
    }
}

fn find_lowercase(text: &str, query: &str) -> Option<usize> {
    let lowered_text = text.to_lowercase();
    let lowered_query = query.to_lowercase();
    let start = lowered_text.find(&lowered_query)?;
    Some(lowered_text[..start].chars().count())
}

#[cfg(test)]
mod tests {
    use super::{hydrate_input, parent_projection};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use studio_runtime_store::{
        AgentRecord, AgentRepository, RuntimeStore, TranscriptItem, TranscriptItemRecord,
    };

    #[test]
    fn global_excerpt_uses_thirty_words_and_shared_lowercase_limit() {
        let body = (0..60)
            .map(|n| format!("word{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        for (query, expected) in [
            // The match is the fifteenth of thirty words; both ends are cut.
            (
                "WORD40",
                " … word26 word27 word28 word29 word30 word31 word32 word33 word34 word35 word36 word37 word38 word39 word40 word41 word42 word43 word44 word45 word46 word47 word48 word49 word50 word51 word52 word53 word54 word55 … ",
            ),
            // A match near the end keeps the last thirty words and no trailing mark.
            (
                "WORD58",
                " … word30 word31 word32 word33 word34 word35 word36 word37 word38 word39 word40 word41 word42 word43 word44 word45 word46 word47 word48 word49 word50 word51 word52 word53 word54 word55 word56 word57 word58 word59",
            ),
            // No match keeps the first thirty words.
            (
                "zzz",
                "word0 word1 word2 word3 word4 word5 word6 word7 word8 word9 word10 word11 word12 word13 word14 word15 word16 word17 word18 word19 word20 word21 word22 word23 word24 word25 word26 word27 word28 word29 … ",
            ),
        ] {
            assert_eq!(super::excerpt(&body, query), expected, "{query}");
        }
        assert_eq!(
            super::excerpt(
                "word0 word1 word2 word3 word4 word5 word6 word7 word8 word9 word10 word11 word12 word13 word14 word15 word16 word17 word18 word19 word20 word21 word22 word23 word24 word25 word26 word27 word28 word29",
                "word5"
            ),
            "word0 word1 word2 word3 word4 word5 word6 word7 word8 word9 word10 word11 word12 word13 word14 word15 word16 word17 word18 word19 word20 word21 word22 word23 word24 word25 word26 word27 word28 word29"
        );
        assert_eq!(
            super::find_lowercase("Straße", "STRASSE"),
            None,
            "named limit: Unicode lowercase is not Python full casefold"
        );
        assert_eq!(super::find_lowercase("ÉTÉ", "été"), Some(0));
    }

    #[test]
    fn legacy_parent_text_is_hydrated_before_projection() {
        for text in [None, Some(Value::Null)] {
            let mut stored = json!({"id":"item-1","role":"assistant","truncated":true});
            if let Some(text) = text {
                stored["text"] = text;
            }
            let record: TranscriptItemRecord = serde_json::from_str(&stored.to_string())
                .expect("persisted transcript permits absent or null parent text");
            let row = TranscriptItem {
                id: record.id().as_ref().to_owned(),
                agent: "agent-1".to_owned(),
                record,
            };

            let projected = parent_projection(
                &row,
                "agent-1",
                Some("hydrated from fulltext".to_owned()),
                |_| Ok(None),
            )
            .expect("legacy content is hydrated before response projection");
            assert_eq!(projected["text"], "hydrated from fulltext");
            assert_eq!(projected["truncated"], false);
        }
    }

    #[test]
    fn legacy_transcript_inputs_allow_missing_and_null_optional_fields() {
        for input in [
            json!({"id":"input-1"}),
            json!({"id":"input-1","kind":null,"text":null,"truncated":null}),
        ] {
            let record: TranscriptItemRecord = serde_json::from_str(
                &json!({
                    "id":"item-1", "role":"assistant", "text":null,
                    "inputs":[input]
                })
                .to_string(),
            )
            .expect("persisted transcript permits absent or null optional input fields");
            let row = TranscriptItem {
                id: record.id().as_ref().to_owned(),
                agent: "agent-1".to_owned(),
                record,
            };
            let projected = hydrate_input(
                &row,
                &row.record.inputs().value().unwrap()[0],
                0,
                "agent-1",
                Some("agent-1:input-1"),
                |_| Ok(Some("hydrated input text".to_owned())),
            )
            .expect("legacy input fields are hydrated")
            .expect("requested input is present");
            assert_eq!(projected["text"], "hydrated input text");
            assert_eq!(projected["role"], "assistant");
        }
    }

    #[tokio::test]
    async fn pull_rebuilds_stale_transcript_cache_from_current_revision() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store"),
        );
        store
            .transaction(|transaction| {
                AgentRepository::put(
                    transaction,
                    &AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,
                        "id":"agent", "rootId":"agent", "epoch":0, "status":"idle"
                    }))?,
                )?;
                transaction.execute(
                    "INSERT INTO runtime_items(id,agent,created,record) VALUES ('agent:item','agent',1.0,?1)",
                    [json!({"id":"agent:item","role":"assistant","text":"before"}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("seed transcript");

        let baseline = super::pull_scope(Arc::clone(&store), "transcript:agent", "agent", 0)
            .await
            .expect("initial transcript snapshot");
        let prior_checkpoint = baseline["checkpoint"]["seq"]
            .as_i64()
            .expect("baseline checkpoint");
        store
            .transaction(|transaction| {
                transaction.execute(
                    "UPDATE runtime_items SET record=json_set(record,'$.text','after') WHERE id='agent:item'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("ordinary transcript update");
        let pulled = super::pull_scope(
            Arc::clone(&store),
            "transcript:agent",
            "agent",
            prior_checkpoint,
        )
        .await
        .expect("incremental pull rebuilds stale cache from current revision");
        let document = pulled["documents"]
            .as_array()
            .expect("documents")
            .iter()
            .find(|document| document["id"] == "transcript:agent")
            .expect("transcript delta");
        let payload: Value = serde_json::from_str(document["payload"].as_str().expect("payload"))
            .expect("delta JSON");
        assert_eq!(payload["delta"], true);
        assert_eq!(payload["items"][0]["id"], "agent:item");
        assert_eq!(payload["items"][0]["text"], "after");
        assert!(pulled["checkpoint"]["seq"].as_i64().expect("checkpoint") > prior_checkpoint);

        let sync_state = || {
            let connection = store.test_reader().expect("reader");
            let mut statement = connection
                .prepare("SELECT id,seq,hash,payload,deleted FROM sync_entities WHERE collection='transcript:agent' ORDER BY id")
                .expect("sync ledger snapshot query");
            statement
                .query_map([], |row| {
                    Ok(json!([
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?
                    ]))
                })
                .expect("sync ledger rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("sync ledger snapshot")
        };
        let committed_sync_state = sync_state();

        let replay = super::pull_scope(
            Arc::clone(&store),
            "transcript:agent",
            "agent",
            prior_checkpoint,
        )
        .await
        .expect("same cursor replays exact incremental response");
        assert_eq!(replay, pulled);
        assert_eq!(
            sync_state(),
            committed_sync_state,
            "replay must not add ledger writes"
        );
    }

    /// A state with `count` items for the chat `agent`. `retired` selects the writer that
    /// keeps no transcript copy (read-time pages) or the one that keeps them.
    async fn chat(retired: bool, count: usize) -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store"),
        );
        store
            .transaction(move |transaction| {
                if !retired {
                    transaction.execute(
                        "DELETE FROM sync_entity_meta WHERE key='transcript_copies'",
                        [],
                    )?;
                }
                AgentRepository::put(
                    transaction,
                    &AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,
                        "id":"agent", "rootId":"agent", "epoch":0, "status":"idle"
                    }))?,
                )?;
                for index in 0..count {
                    transaction.execute(
                        "INSERT INTO runtime_items(id,agent,created,record) VALUES (?1,'agent',?2,?3)",
                        rusqlite::params![
                            format!("agent:{index:03}"),
                            index as f64,
                            json!({"id":format!("agent:{index:03}"),"role":"assistant","text":format!("text {index}")}).to_string()
                        ],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seed transcript");
        (root, store)
    }

    fn payload(pulled: &Value) -> Value {
        let document = &pulled["documents"][0];
        serde_json::from_str(document["payload"].as_str().expect("payload")).expect("JSON")
    }

    fn transcript_rows(store: &RuntimeStore) -> i64 {
        store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT COUNT(*) FROM sync_entities WHERE collection LIKE 'transcript:%'",
                [],
                |row| row.get(0),
            )
            .expect("count")
    }

    #[tokio::test]
    async fn a_page_built_from_the_items_equals_the_old_cached_page() {
        let (_old_root, old) = chat(false, 5).await;
        let (_new_root, new) = chat(true, 5).await;
        let cached = super::pull_scope(Arc::clone(&old), "transcript:agent", "agent", 0)
            .await
            .expect("cached page");
        let read_time = super::pull_scope(Arc::clone(&new), "transcript:agent", "agent", 0)
            .await
            .expect("read-time page");
        assert_eq!(
            cached["documents"][0]["payload"], read_time["documents"][0]["payload"],
            "the same items give the same page"
        );
        assert_eq!(
            payload(&read_time)["items"].as_array().map(Vec::len),
            Some(5)
        );

        // The same change gives the same delta in both writers.
        let change = |transaction: &rusqlite::Transaction<'_>| {
            transaction.execute(
                "UPDATE runtime_items SET record=json_set(record,'$.text','after') WHERE id='agent:002'",
                [],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        };
        old.transaction(move |transaction| change(transaction))
            .await
            .expect("old change");
        new.transaction(move |transaction| change(transaction))
            .await
            .expect("new change");
        let old_after = cached["checkpoint"]["seq"].as_i64().expect("cursor");
        let new_after = read_time["checkpoint"]["seq"].as_i64().expect("cursor");
        let old_delta = super::pull_scope(Arc::clone(&old), "transcript:agent", "agent", old_after)
            .await
            .expect("cached delta");
        let new_delta = super::pull_scope(Arc::clone(&new), "transcript:agent", "agent", new_after)
            .await
            .expect("read-time delta");
        assert_eq!(
            old_delta["documents"][0]["payload"], new_delta["documents"][0]["payload"],
            "the delta has the same items, removals and order"
        );
        assert_eq!(payload(&new_delta)["delta"], true);
        assert_eq!(payload(&new_delta)["items"][0]["text"], "after");
        assert_eq!(
            payload(&new_delta)["items"].as_array().map(Vec::len),
            Some(1)
        );
    }

    #[tokio::test]
    async fn the_reader_writes_no_transcript_copy_and_replays_a_cursor() {
        let (_root, store) = chat(true, 3).await;
        let first = super::pull_scope(Arc::clone(&store), "transcript:agent", "agent", 0)
            .await
            .expect("page");
        let cursor = first["checkpoint"]["seq"].as_i64().expect("cursor");
        let unchanged = super::pull_scope(Arc::clone(&store), "transcript:agent", "agent", cursor)
            .await
            .expect("unchanged");
        assert_eq!(unchanged["documents"], json!([]));
        assert_eq!(unchanged["checkpoint"]["seq"], cursor);
        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_items(id,agent,created,record) VALUES ('agent:new','agent',9,?1)",
                    [json!({"id":"agent:new","role":"assistant","text":"new"}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("new item");
        let delta = super::pull_scope(Arc::clone(&store), "transcript:agent", "agent", cursor)
            .await
            .expect("delta");
        let delta_payload = payload(&delta);
        assert_eq!(delta_payload["items"][0]["id"], "agent:new");
        assert_eq!(delta_payload["order"][3], "agent:new");
        assert_eq!(
            transcript_rows(&store),
            0,
            "no transcript copy is written for a page, a repeat or a delta"
        );
    }

    #[tokio::test]
    async fn older_items_come_with_a_scroll_request() {
        let (_root, store) = chat(true, 130).await;
        let first = super::pull_scope(Arc::clone(&store), "transcript:agent", "agent", 0)
            .await
            .expect("page");
        let page = payload(&first);
        assert_eq!(page["items"].as_array().map(Vec::len), Some(120));
        assert_eq!(page["truncated"], true);
        assert_eq!(page["items"][0]["id"], "agent:010");
        let older = crate::transcript_runtime::page(
            &store,
            "agent".to_owned(),
            crate::transcript_runtime::PageRequest {
                before: page["nextCursor"].as_str().map(str::to_owned),
                ..Default::default()
            },
        )
        .await
        .expect("older page");
        let ids = older["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["id"].as_str().expect("id").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 10);
        assert_eq!(ids[0], "agent:000");
        assert_eq!(ids[9], "agent:009");
    }

    #[tokio::test]
    async fn the_migration_keeps_the_copies_when_an_item_is_missing() {
        let (_root, store) = chat(false, 2).await;
        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES ('transcript:agent','item:gone',1,'h',NULL,0)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("copy row without its item");
        let missing = store
            .transaction(|transaction| {
                studio_runtime_store::durable_seq::retire_transcript_copies(transaction)
            })
            .await
            .expect("check");
        assert_eq!(missing, vec![("agent".to_owned(), "gone".to_owned())]);
        super::pull_scope(Arc::clone(&store), "transcript:agent", "agent", 0)
            .await
            .expect("page");
        assert!(
            transcript_rows(&store) > 1,
            "the writer still writes the copies, and the old row stays"
        );
    }
}
