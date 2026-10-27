//! Synchronous access to the durable sync document and entity tables.

use crate::CachedSql;
use rusqlite::{Connection, OptionalExtension, Transaction, named_params, params};
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    SYNC_BATCH_LIMIT, SYNC_ENTITY_PAGE_LIMIT, SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
};

use crate::schema::{SchemaRequirements, TableRequirement, TriggerRequirement};
use crate::{StoreError, StoreResult};

/// Non-transcript entity tombstones retained for incremental sync clients.
/// Older tombstones are pruned and the tombstone floor is raised. Unit: rows.
pub const ENTITY_TOMBSTONE_LIMIT: i64 = 10_000;
/// Tombstones removed by one short writer transaction. Unit: rows.
pub const ENTITY_TOMBSTONE_PRUNE_BATCH: i64 = 500;

/// Selection shared by complete fingerprints and bounded subscription pages.
/// Prefix of indexed scope visibility fence keys in sync_entity_meta. SQL
/// bootstrap triggers consume this contract; a fast parity witness checks them.
const ENTITY_SCOPE_VISIBILITY_KEY_PREFIX: &str = "entity_scope_visibility:";
/// Qualifier for direct/team-agent selection; absent from root-only fences.
const ENTITY_SCOPE_TEAM_PREDICATE: &str = "team";

#[cfg(test)]
const ENTITY_SCOPE_TOMBSTONE_KEY_PREFIX: &str = "entity_scope_tombstone:";

const ENTITY_SCOPE_AGENT_PREDICATE: &str = "(:agent IS NULL OR (:include_team_members AND (json_extract(payload,'$.value.agent')=:agent \
          OR json_extract(payload,'$.value.agent') IN (SELECT id FROM sync_entities WHERE collection='agentSummary' AND deleted=0 AND json_extract(payload,'$.value.rootId')=:agent))) \
          OR json_extract(payload,'$.value.rootId')=:agent OR deleted=1)";

/// Selection and initial list ordering shared by the page reader.
#[derive(Clone, Copy)]
struct EntityPageScope<'a> {
    collections: Option<&'a str>,
    priority_id: Option<&'a str>,
}

const BOOTSTRAP_TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "sync_identity",
        columns: &["id"],
    },
    TableRequirement {
        name: "sync_generation",
        columns: &["id", "value"],
    },
    TableRequirement {
        name: "sync_documents",
        columns: &["seq", "scope", "id", "payload", "deleted"],
    },
    TableRequirement {
        name: "sync_entities",
        columns: &["collection", "id", "seq", "hash", "payload", "deleted"],
    },
    TableRequirement {
        name: "sync_entity_meta",
        columns: &["key", "value"],
    },
    TableRequirement {
        name: "sync_versions",
        columns: &["seq", "scope", "hash", "deleted", "updated"],
    },
];
const BOOTSTRAP_TRIGGERS: &[TriggerRequirement] = &[
    TriggerRequirement {
        name: "sync_entity_scope_tombstone_update",
        table: "sync_entities",
    },
    TriggerRequirement {
        name: "sync_entity_scope_tombstone_delete",
        table: "sync_entities",
    },
    TriggerRequirement {
        name: "sync_entity_scope_visibility_insert",
        table: "sync_entities",
    },
    TriggerRequirement {
        name: "sync_entity_scope_visibility_update",
        table: "sync_entities",
    },
    TriggerRequirement {
        name: "sync_entity_tombstone_count_insert",
        table: "sync_entities",
    },
    TriggerRequirement {
        name: "sync_entity_tombstone_count_update",
        table: "sync_entities",
    },
    TriggerRequirement {
        name: "sync_entity_tombstone_count_delete",
        table: "sync_entities",
    },
];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &["sync_entity_hash", "sync_entity_payload"],
    bootstrap_tables: BOOTSTRAP_TABLES,
    bootstrap_triggers: BOOTSTRAP_TRIGGERS,
    bootstrap_connection_functions: &["sync_entity_hash", "sync_entity_payload"],
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncDocument {
    pub id: String,
    pub payload: String,
    pub sequence: i64,
    pub deleted: bool,
}

impl SyncDocument {
    pub fn response(&self) -> Value {
        json!({
            "id": self.id,
            "payload": self.payload,
            "seq": self.sequence,
            "_deleted": self.deleted,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntityRow {
    pub collection: String,
    pub id: String,
    pub sequence: i64,
    pub payload: Option<String>,
    pub deleted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntityPage {
    pub rows: Vec<EntityRow>,
    pub max_sequence: i64,
    pub tombstone_floor: i64,
    pub initial_high: i64,
    pub checkpoint: i64,
    pub reset: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncDocumentsRepository;

impl SyncDocumentsRepository {
    /// Merge fields into an existing sync entity value without rebuilding its envelope.
    pub fn patch_entity(
        transaction: &Transaction<'_>,
        collection: &str,
        id: &str,
        patch: Value,
    ) -> StoreResult<()> {
        let raw = transaction
            .query_row_cached(
                "SELECT payload FROM sync_entities WHERE collection=?1 AND id=?2 AND deleted=0",
                params![collection, id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let mut envelope: Value = match raw {
            Some(raw) => serde_json::from_str(&raw).map_err(|_| StoreError::InvalidSyncObject {
                scope: collection.to_owned(),
                id: id.to_owned(),
            })?,
            None => json!({"collection":collection,"id":id,"value":{}}),
        };
        if envelope.get("collection").and_then(Value::as_str) != Some(collection)
            || envelope.get("id").and_then(Value::as_str) != Some(id)
        {
            return Err(StoreError::InvalidSyncObject {
                scope: collection.to_owned(),
                id: id.to_owned(),
            });
        }
        let value = envelope
            .get_mut("value")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| StoreError::InvalidSyncObject {
                scope: collection.to_owned(),
                id: id.to_owned(),
            })?;
        let patch = patch
            .as_object()
            .ok_or_else(|| StoreError::InvalidSyncObject {
                scope: collection.to_owned(),
                id: id.to_owned(),
            })?;
        value.extend(patch.clone());
        Self::put_entity(
            transaction,
            collection,
            id,
            envelope["value"].clone(),
            false,
        )?;
        Ok(())
    }

    pub fn pull_entities(
        transaction: &Transaction<'_>,
        after: i64,
        limit: i64,
        fresh: bool,
        initial_high: i64,
        reset_support: bool,
        priority_id: Option<&str>,
    ) -> StoreResult<EntityPage> {
        Self::pull_entity_view(
            transaction,
            after,
            limit,
            fresh,
            initial_high,
            reset_support,
            EntityPageScope {
                collections: None,
                priority_id,
            },
        )
    }

    /// The global list view; panels enter the cache only through scoped subscriptions.
    pub fn pull_global_entities(
        transaction: &Transaction<'_>,
        after: i64,
        limit: i64,
        fresh: bool,
        initial_high: i64,
        reset_support: bool,
        priority_id: Option<&str>,
    ) -> StoreResult<EntityPage> {
        Self::pull_entity_view(
            transaction,
            after,
            limit,
            fresh,
            initial_high,
            reset_support,
            EntityPageScope {
                collections: Some(
                    studio_runtime_schema::sync_protocol::global_entity_collections_json(),
                ),
                priority_id,
            },
        )
    }

    fn pull_entity_view(
        transaction: &Transaction<'_>,
        after: i64,
        limit: i64,
        fresh: bool,
        initial_high: i64,
        reset_support: bool,
        scope: EntityPageScope<'_>,
    ) -> StoreResult<EntityPage> {
        let tombstone_floor = Self::entity_tombstone_floor(transaction)?;
        let stored_max = transaction.query_row_cached(
            "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection NOT LIKE 'transcript:%'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let max_sequence = stored_max.max(tombstone_floor);
        let initial_high = if fresh {
            if initial_high > 0 {
                max_sequence.min(initial_high)
            } else {
                max_sequence
            }
        } else {
            0
        };
        let after = after.max(0);
        if reset_support && !fresh && after > 0 && (after < tombstone_floor || after > max_sequence)
        {
            return Ok(EntityPage {
                rows: Vec::new(),
                max_sequence,
                tombstone_floor,
                initial_high,
                checkpoint: max_sequence,
                reset: true,
            });
        }

        let limit = limit.clamp(1, SYNC_ENTITY_PAGE_LIMIT);
        let scope = EntityPageScope {
            priority_id: (after == 0 && fresh).then_some(scope.priority_id).flatten(),
            ..scope
        };
        let rows = read_entity_page(
            transaction,
            after,
            tombstone_floor,
            fresh,
            initial_high,
            limit,
            scope,
        )?;
        let checkpoint = if rows.len() as i64 == limit {
            rows.last().map_or(after, |row| row.sequence)
        } else {
            max_sequence
        };
        Ok(EntityPage {
            rows,
            max_sequence,
            tombstone_floor,
            initial_high,
            checkpoint,
            reset: false,
        })
    }

    /// Read one ref's durable interval and watermark in the caller's SQLite snapshot.
    /// `reset` selects a replacement snapshot when the cursor is missing, expired,
    /// or ahead of this store. Snapshots omit tombstones; deltas retain removals.
    /// A checkpoint below `max_sequence` means the bounded page is incomplete.
    pub fn resume_entities(
        transaction: &Transaction<'_>,
        since: Option<i64>,
        collection: Option<&str>,
        id: Option<&str>,
        limit: i64,
    ) -> StoreResult<EntityPage> {
        Self::resume_entity_scope(
            transaction,
            since,
            collection.map(|c| json!([c]).to_string()).as_deref(),
            id,
            None,
            true,
            limit,
        )
    }

    /// Read a view's selected collections and optional row/agent identity. Scope
    /// filters run in SQL before the page bound, including for tombstones.
    pub fn resume_entity_scope(
        transaction: &Transaction<'_>,
        since: Option<i64>,
        collections_json: Option<&str>,
        id: Option<&str>,
        agent: Option<&str>,
        include_team_members: bool,
        limit: i64,
    ) -> StoreResult<EntityPage> {
        let tombstone_floor = Self::entity_tombstone_floor(transaction)?;
        let max_sequence = Self::entity_sequence(transaction)?.max(tombstone_floor);
        let reset = since.is_none_or(|seq| seq < tombstone_floor || seq > max_sequence || seq < 0);
        let after = if reset { 0 } else { since.unwrap_or(0) };
        let interval = EntityPage {
            rows: Vec::new(),
            max_sequence,
            tombstone_floor,
            initial_high: if reset { max_sequence } else { 0 },
            checkpoint: after,
            reset,
        };
        Self::continue_entity_scope(
            transaction,
            &interval,
            collections_json,
            id,
            agent,
            include_team_members,
            limit,
        )
    }

    /// Continue only inside the caller's original SQLite read transaction with
    /// the same scope. Initial external-cursor recovery is already decided by
    /// resume_entity_scope; a snapshot checkpoint may legitimately predate the
    /// tombstone floor and must never restart the first page.
    pub fn continue_entity_scope(
        transaction: &Transaction<'_>,
        interval: &EntityPage,
        collections_json: Option<&str>,
        id: Option<&str>,
        agent: Option<&str>,
        include_team_members: bool,
        limit: i64,
    ) -> StoreResult<EntityPage> {
        let after = interval.checkpoint;
        let max_sequence = interval.max_sequence;
        let reset = interval.reset;
        let tombstone_floor = interval.tombstone_floor;
        let limit = limit.clamp(1, SYNC_ENTITY_STREAM_MAX_DOCUMENTS);
        let collections = if collections_json.is_some() {
            "collection IN (SELECT value FROM json_each(:collections))"
        } else {
            ":collections IS NULL"
        };
        let identity = if id.is_some() {
            "id=:id"
        } else {
            ":id IS NULL"
        };
        let mut statement = transaction.prepare_cached(&format!(
            "SELECT collection,id,seq,payload,deleted FROM sync_entities \
             WHERE collection NOT LIKE 'transcript:%' AND {collections} AND {identity} \
               AND {ENTITY_SCOPE_AGENT_PREDICATE} AND seq>:after AND seq<=:through \
               AND (:snapshot=0 OR deleted=0) ORDER BY seq LIMIT :limit"
        ))?;
        let mut rows = statement
            .query_map(
                named_params! {
                    ":after": after,
                    ":through": max_sequence,
                    ":collections": collections_json,
                    ":id": id,
                    ":snapshot": i64::from(reset),
                    ":limit": limit + 1,
                    ":agent": agent,
                    ":include_team_members": include_team_members,
                },
                entity_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let checkpoint = if rows.len() as i64 > limit {
            rows.truncate(limit as usize);
            rows.last().map_or(after, |row| row.sequence)
        } else {
            max_sequence
        };
        Ok(EntityPage {
            rows,
            max_sequence,
            tombstone_floor,
            initial_high: if reset { max_sequence } else { 0 },
            checkpoint,
            reset,
        })
    }

    /// Latest relevant predicate movement; this is a bounded indexed metadata
    /// lookup, independent of selected history size. The SQL trigger owner records
    /// old/new direct selectors and resolved roots in the same source transaction.
    pub fn entity_scope_visibility_revision(
        transaction: &Transaction<'_>,
        collections_json: &str,
        agent: &str,
        include_team_members: bool,
    ) -> StoreResult<i64> {
        transaction.query_row_cached(&format!(
            "SELECT COALESCE(MAX(CAST(value AS INTEGER)),0) FROM sync_entity_meta WHERE key IN \
             (SELECT '{ENTITY_SCOPE_VISIBILITY_KEY_PREFIX}' || json_array(value,?2) FROM json_each(?1) \
              UNION SELECT '{ENTITY_SCOPE_VISIBILITY_KEY_PREFIX}' || json_array(value,?2,'{ENTITY_SCOPE_TEAM_PREDICATE}') FROM json_each(?1) WHERE ?3 \
              UNION SELECT '{ENTITY_SCOPE_VISIBILITY_KEY_PREFIX}' || json_array('*',?2) WHERE ?3)"
        ), params![collections_json,agent,include_team_members], |row| row.get(0)).map_err(StoreError::from)
    }

    /// Complete live selection, independent of the stream page bound. Stream
    /// ordered identities and sequences into a digest so changes to root/team
    /// membership are detected even when the row count and maximum are equal.
    /// No DTO payloads or unbounded result lists cross the store boundary.
    pub fn entity_scope_fingerprint(
        transaction: &Transaction<'_>,
        collections_json: Option<&str>,
        id: Option<&str>,
        agent: Option<&str>,
        include_team_members: bool,
    ) -> StoreResult<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let collections = if collections_json.is_some() {
            "collection IN (SELECT value FROM json_each(:collections))"
        } else {
            ":collections IS NULL"
        };
        let identity = if id.is_some() {
            "id=:id"
        } else {
            ":id IS NULL"
        };
        let scope = format!(
            "collection NOT LIKE 'transcript:%' AND deleted=0 AND {collections} AND {identity}"
        );
        if agent.is_some() {
            let invalid: bool = transaction.query_row_cached(
                &format!("SELECT EXISTS(SELECT 1 FROM sync_entities INDEXED BY sync_entities_invalid_payload WHERE {scope} AND NOT json_valid(payload)) OR (:team AND EXISTS(SELECT 1 FROM sync_entities INDEXED BY sync_entities_invalid_payload WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND NOT json_valid(payload) AND collection='agentSummary'))"),
                named_params! { ":collections": collections_json, ":id": id, ":team": include_team_members }, |row| row.get(0),
            )?;
            if invalid {
                return Err(StoreError::RecordContract("invalid scoped entity payload"));
            }
        }
        let agents = if let Some(agent) = agent.filter(|_| include_team_members) {
            let mut statement = transaction.prepare_cached("SELECT id FROM sync_entities INDEXED BY sync_entities_live_root_scope WHERE collection NOT LIKE 'transcript:%' AND deleted=0 AND json_valid(payload) AND collection='agentSummary' AND json_extract(payload,'$.value.rootId')=?1")?;
            let mut members = statement
                .query_map([agent], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            members.push(agent.to_owned());
            Some(serde_json::to_string(&members)?)
        } else {
            None
        };
        let sql = if agent.is_some() {
            let root = format!(
                "SELECT collection,id,seq FROM sync_entities INDEXED BY sync_entities_live_root_scope WHERE {scope} AND json_valid(payload) AND json_extract(payload,'$.value.rootId')=:agent"
            );
            if include_team_members {
                format!(
                    "{root} UNION SELECT collection,id,seq FROM sync_entities INDEXED BY sync_entities_live_agent_scope WHERE {scope} AND json_valid(payload) \
                    AND json_extract(payload,'$.value.agent') IN (SELECT value FROM json_each(:agents)) ORDER BY seq"
                )
            } else {
                format!("{root} ORDER BY seq")
            }
        } else {
            let index = if collections_json.is_some() && id.is_none() {
                " INDEXED BY sync_entities_collection_seq"
            } else {
                ""
            };
            format!("SELECT collection,id,seq FROM sync_entities{index} WHERE {scope} ORDER BY seq")
        };
        let mut statement = transaction.prepare_cached(&sql)?;
        let mut rows = if let (Some(agent), Some(agents)) = (agent, agents.as_deref()) {
            statement.query(named_params! { ":collections": collections_json, ":id": id, ":agent": agent, ":agents": agents })?
        } else if let Some(agent) = agent {
            statement.query(
                named_params! { ":collections": collections_json, ":id": id, ":agent": agent },
            )?
        } else {
            statement.query(named_params! { ":collections": collections_json, ":id": id })?
        };
        let mut digest = Sha256::new();
        while let Some(row) = rows.next()? {
            for column in [0, 1] {
                let text: String = row.get(column)?;
                digest.update((text.len() as u64).to_be_bytes());
                digest.update(text.as_bytes());
            }
            digest.update(row.get::<_, i64>(2)?.to_be_bytes());
        }
        Ok(digest.finalize().into())
    }

    pub fn draft(transaction: &Transaction<'_>, id: &str) -> StoreResult<Option<SyncDocument>> {
        transaction
            .query_row_cached(
                "SELECT seq,id,payload,deleted FROM sync_documents WHERE scope='drafts' AND id=?1",
                [id],
                |row| {
                    Ok(SyncDocument {
                        sequence: row.get(0)?,
                        id: row.get(1)?,
                        payload: row.get(2)?,
                        deleted: row.get::<_, i64>(3)? != 0,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn put_draft(
        transaction: &Transaction<'_>,
        id: &str,
        payload: &str,
        deleted: bool,
    ) -> StoreResult<bool> {
        if Self::draft(transaction, id)?
            .is_some_and(|current| current.payload == payload && current.deleted == deleted)
        {
            return Ok(false);
        }
        transaction.execute_cached(
            "INSERT INTO sync_documents(seq,scope,id,payload,deleted) VALUES(?1,'drafts',?2,?3,?4) \
             ON CONFLICT(scope,id) DO UPDATE SET seq=excluded.seq,payload=excluded.payload,deleted=excluded.deleted",
            params![next_sequence(transaction)?, id, payload, i64::from(deleted)],
        )?;
        Ok(true)
    }

    /// Publish the renderer-owned sync envelope for an entity inside its source write transaction.
    pub fn put_entity(
        transaction: &Transaction<'_>,
        collection: &str,
        id: &str,
        value: Value,
        deleted: bool,
    ) -> StoreResult<bool> {
        let value = if deleted { json!({}) } else { value };
        let payload = serde_json::to_string(&json!({
            "collection": collection,
            "id": id,
            "value": value,
        }))?;
        Self::put_entity_payload(transaction, collection, id, &payload, deleted)
    }

    pub fn entity_sequence(connection: &Connection) -> StoreResult<i64> {
        connection
            .query_row_cached(
                "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection NOT LIKE 'transcript:%'",
                [], |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn entities_after(
        connection: &Connection,
        after: i64,
        limit: i64,
    ) -> StoreResult<Vec<EntityRow>> {
        let mut statement = connection.prepare_cached(
            "SELECT collection,id,seq,payload,deleted FROM sync_entities \
             WHERE seq>?1 AND collection NOT LIKE 'transcript:%' ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                after,
                limit.clamp(
                    1,
                    SYNC_ENTITY_STREAM_MAX_DOCUMENTS
                        .max(SYNC_ENTITY_PAGE_LIMIT)
                        .saturating_add(1)
                )
            ],
            |row| {
                Ok(EntityRow {
                    collection: row.get(0)?,
                    id: row.get(1)?,
                    sequence: row.get(2)?,
                    payload: row.get(3)?,
                    deleted: row.get::<_, i64>(4)? != 0,
                })
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Whether entity tombstones exceed the retained limit by the prune slack,
    /// so a pruning run removes a full batch instead of one row per delete.
    pub fn entity_tombstone_prune_due(connection: &Connection) -> StoreResult<bool> {
        let count = connection
            .query_row_cached(
                "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(count) = count else {
            // A missing counter is rebuilt by the first pruning transaction.
            return Ok(true);
        };
        let count = count
            .parse::<i64>()
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
        Ok(count
            > ENTITY_TOMBSTONE_LIMIT
                + ENTITY_TOMBSTONE_PRUNE_BATCH.min(ENTITY_TOMBSTONE_LIMIT / 20))
    }

    /// Remove one batch of the oldest non-transcript entity tombstones above
    /// [`ENTITY_TOMBSTONE_LIMIT`] and raise the tombstone floor to the highest
    /// removed sequence, so clients behind the floor reset instead of missing a
    /// deletion. Returns the number of removed rows; zero means nothing is due.
    pub fn prune_entity_tombstones(transaction: &Transaction<'_>) -> StoreResult<usize> {
        transaction.execute_cached(
            "INSERT OR IGNORE INTO sync_entity_meta(key,value) \
             SELECT 'entity_tombstone_count',CAST(COUNT(*) AS TEXT) FROM sync_entities \
             WHERE deleted=1 AND collection NOT LIKE 'transcript:%'",
            [],
        )?;
        let count = transaction
            .query_row_cached(
                "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'",
                [],
                |row| row.get::<_, String>(0),
            )?
            .parse::<i64>()
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
        let excess = count - ENTITY_TOMBSTONE_LIMIT;
        if excess <= 0 {
            return Ok(0);
        }
        let rows = {
            let mut statement = transaction.prepare_cached(
                "SELECT collection,id,seq FROM sync_entities \
                 WHERE deleted=1 AND collection NOT LIKE 'transcript:%' \
                 ORDER BY seq,collection,id LIMIT ?1",
            )?;
            statement
                .query_map([ENTITY_TOMBSTONE_PRUNE_BATCH.min(excess)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let Some(highest) = rows.iter().map(|(_, _, seq)| *seq).max() else {
            return Ok(0);
        };
        let floor = Self::entity_tombstone_floor(transaction)?.max(highest);
        let mut delete = transaction.prepare_cached(
            "DELETE FROM sync_entities WHERE collection=?1 AND id=?2 AND deleted=1",
        )?;
        for (collection, id, _) in &rows {
            delete.execute(params![collection, id])?;
        }
        transaction.execute_cached(
            "INSERT INTO sync_entity_meta(key,value) VALUES('entity_tombstone_floor',?1) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [floor.to_string()],
        )?;
        Ok(rows.len())
    }

    pub fn entity_tombstone_floor(connection: &Connection) -> StoreResult<i64> {
        let value = connection
            .query_row_cached(
                "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_floor'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value.map_or(Ok(0), |text| {
            text.parse::<i64>()
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))
        })
    }

    pub fn workspace_id(connection: &Connection) -> StoreResult<String> {
        connection
            .query_row_cached("SELECT id FROM sync_identity LIMIT 1", [], |row| row.get(0))
            .map_err(StoreError::from)
    }

    /// Latest draft row sequence, including retained tombstones. Unrelated sync
    /// scopes share the allocator but never advance this content revision.
    pub fn draft_sequence(connection: &Connection) -> StoreResult<i64> {
        connection
            .query_row_cached(
                "SELECT COALESCE(MAX(seq),0) FROM sync_documents WHERE scope='drafts'",
                [],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn pull_drafts(
        connection: &Connection,
        after: i64,
        limit: i64,
    ) -> StoreResult<Vec<SyncDocument>> {
        Self::drafts_after(connection, after, limit.clamp(1, SYNC_BATCH_LIMIT))
    }

    /// Read a caller-bounded draft interval, shared by pull pages and stream batches.
    pub fn drafts_after(
        connection: &Connection,
        after: i64,
        limit: i64,
    ) -> StoreResult<Vec<SyncDocument>> {
        let mut statement = connection.prepare_cached(
            "SELECT seq,id,payload,deleted FROM sync_documents \
             WHERE scope='drafts' AND seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after.max(0), limit], |row| {
            Ok(SyncDocument {
                sequence: row.get(0)?,
                id: row.get(1)?,
                payload: row.get(2)?,
                deleted: row.get::<_, i64>(3)? != 0,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn preference_payload(connection: &Connection, scope: &str) -> StoreResult<Option<String>> {
        connection
            .query_row_cached(
                "SELECT payload FROM sync_entities \
                 WHERE collection='uiPreferences' AND id=?1 AND deleted=0",
                [scope],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn put_preferences(
        transaction: &Transaction<'_>,
        scope: &str,
        payload: &str,
    ) -> StoreResult<bool> {
        Self::put_entity_payload(transaction, "uiPreferences", scope, payload, false)
    }

    pub(crate) fn put_entity_payload(
        transaction: &Transaction<'_>,
        collection: &str,
        id: &str,
        payload: &str,
        deleted: bool,
    ) -> StoreResult<bool> {
        let hash = hash_payload(payload);
        Self::write_entity_hash(
            transaction,
            collection,
            id,
            &hash,
            deleted,
            Some(payload),
            None,
        )
    }

    /// Store an entity hash without retaining its payload.
    pub fn put_entity_hash(
        transaction: &Transaction<'_>,
        collection: &str,
        id: &str,
        hash: &str,
        deleted: bool,
    ) -> StoreResult<bool> {
        Self::write_entity_hash(transaction, collection, id, hash, deleted, None, None)
    }

    pub(crate) fn put_entity_hash_at_sequence(
        transaction: &Transaction<'_>,
        collection: &str,
        id: &str,
        hash: &str,
        deleted: bool,
        sequence: i64,
    ) -> StoreResult<bool> {
        Self::write_entity_hash(
            transaction,
            collection,
            id,
            hash,
            deleted,
            None,
            Some(sequence),
        )
    }

    fn write_entity_hash(
        transaction: &Transaction<'_>,
        collection: &str,
        id: &str,
        hash: &str,
        deleted: bool,
        payload: Option<&str>,
        sequence: Option<i64>,
    ) -> StoreResult<bool> {
        crate::entity_publication::mark_published(collection, id);
        let current = transaction
            .query_row_cached(
                "SELECT hash,deleted FROM sync_entities WHERE collection=?1 AND id=?2",
                params![collection, id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        if current
            .as_ref()
            .is_some_and(|(old_hash, old_deleted)| old_hash == hash && *old_deleted == deleted)
        {
            return Ok(false);
        }
        transaction.execute_cached(
            "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) \
             VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash, \
             payload=excluded.payload,deleted=excluded.deleted",
            params![
                collection,
                id,
                match sequence {
                    Some(sequence) => sequence,
                    None => next_sequence(transaction)?,
                },
                hash,
                payload,
                i64::from(deleted)
            ],
        )?;
        Ok(true)
    }
}

fn read_entity_page(
    transaction: &Transaction<'_>,
    after: i64,
    tombstone_floor: i64,
    fresh: bool,
    initial_high: i64,
    limit: i64,
    scope: EntityPageScope<'_>,
) -> StoreResult<Vec<EntityRow>> {
    let order = if scope.priority_id.is_some() {
        "CASE WHEN collection='agentSummary' AND id=?5 THEN 0 ELSE 1 END,seq"
    } else {
        "seq"
    };
    let mut statement = transaction.prepare_cached(&format!(
        "SELECT collection,id,seq,payload,deleted FROM sync_entities \
         WHERE collection NOT LIKE 'transcript:%' AND (?7 IS NULL OR collection IN (SELECT value FROM json_each(?7))) AND seq>?1 \
           AND (deleted=0 OR seq>?2) AND (?3=0 OR deleted=0 OR seq>?4) \
         ORDER BY {order} LIMIT ?6",
    ))?;
    statement
        .query_map(
            params![
                after,
                tombstone_floor,
                i64::from(fresh),
                initial_high,
                scope.priority_id,
                limit,
                scope.collections
            ],
            entity_row,
        )?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn entity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntityRow> {
    Ok(EntityRow {
        collection: row.get(0)?,
        id: row.get(1)?,
        sequence: row.get(2)?,
        payload: row.get(3)?,
        deleted: row.get::<_, i64>(4)? != 0,
    })
}

pub(crate) fn hash_payload(payload: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(payload.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn next_sequence(connection: &Connection) -> StoreResult<i64> {
    connection
        .query_row_cached(
            "SELECT MAX(entity_seq,document_seq,version_seq)+1 FROM ( \
             SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities), \
             (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents), \
             (SELECT COALESCE(MAX(seq),0) AS version_seq FROM sync_versions)",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "sync_documents",
        requirements: &REQUIREMENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::SyncDocumentsRepository;
    use rusqlite::Connection;

    fn page_scan_fixture() -> Connection {
        let db = Connection::open_in_memory().expect("memory SQLite");
        db.execute_batch("CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
            CREATE INDEX sync_entities_non_transcript_seq ON sync_entities(seq) WHERE collection NOT LIKE 'transcript:%';
            CREATE INDEX sync_entities_collection_seq ON sync_entities(collection,seq);
            CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT);
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000)
            INSERT INTO sync_entities SELECT 'other',printf('other-%d',x),x,'{}',0 FROM n;
            INSERT INTO sync_entities VALUES('task','selected',2001,'{}',0);").expect("page fixture");
        db
    }

    #[test]
    fn scoped_page_reads_only_its_collection_and_id() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mut db = page_scan_fixture();
        let tx = db.transaction().expect("transaction");
        let steps = Arc::new(AtomicUsize::new(0));
        let counter = steps.clone();
        tx.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .expect("count steps");
        let page = SyncDocumentsRepository::resume_entity_scope(
            &tx,
            Some(0),
            Some(r#"["task"]"#),
            Some("selected"),
            None,
            true,
            10,
        )
        .expect("scoped page");
        tx.progress_handler(0, None::<fn() -> bool>)
            .expect("stop counting");
        assert_eq!(page.rows.len(), 1);
        assert!(
            steps.load(Ordering::Relaxed) < 1000,
            "scoped page scans unrelated rows"
        );
    }

    #[test]
    fn pull_page_without_priority_stops_at_limit() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mut db = page_scan_fixture();
        let tx = db.transaction().expect("transaction");
        let steps = Arc::new(AtomicUsize::new(0));
        let counter = steps.clone();
        tx.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .expect("count steps");
        let rows = super::read_entity_page(
            &tx,
            0,
            0,
            false,
            0,
            10,
            super::EntityPageScope {
                collections: None,
                priority_id: None,
            },
        )
        .expect("page");
        tx.progress_handler(0, None::<fn() -> bool>)
            .expect("stop counting");
        assert_eq!(rows.len(), 10);
        assert!(
            steps.load(Ordering::Relaxed) < 1000,
            "unprioritized page sorts all rows"
        );
        tx.execute(
            "INSERT INTO sync_entities VALUES('agentSummary','priority',2002,'{}',0)",
            [],
        )
        .expect("priority row");
        let prioritized = super::read_entity_page(
            &tx,
            0,
            0,
            true,
            2002,
            10,
            super::EntityPageScope {
                collections: None,
                priority_id: Some("priority"),
            },
        )
        .expect("priority page");
        assert_eq!(prioritized[0].id, "priority");
    }

    #[test]
    fn scope_visibility_tracks_predicate_moves_atomically_without_replaying_appends() {
        use serde_json::json;
        let mut connection = Connection::open_in_memory().expect("memory SQLite");
        connection
            .execute_batch(
                include_str!("../sync_schema.sql")
                    .split("-- section: runtime_events")
                    .next()
                    .expect("common schema"),
            )
            .expect("bootstrap owner");
        let tx = connection.transaction().expect("transaction");
        for (id, root) in [("a", "alpha"), ("b", "beta")] {
            SyncDocumentsRepository::put_entity(
                &tx,
                "agentSummary",
                id,
                json!({"id":id,"rootId":root}),
                false,
            )
            .expect("member");
        }
        SyncDocumentsRepository::put_entity(
            &tx,
            "task",
            "task",
            json!({"agent":"a","status":"running"}),
            false,
        )
        .expect("task");
        let fence = |root| {
            SyncDocumentsRepository::entity_scope_visibility_revision(
                &tx,
                r#"["task"]"#,
                root,
                true,
            )
            .expect("indexed fence")
        };
        let alpha = fence("alpha");
        let beta = fence("beta");
        SyncDocumentsRepository::put_entity(
            &tx,
            "task",
            "task",
            json!({"agent":"a","status":"completed"}),
            false,
        )
        .expect("content only");
        SyncDocumentsRepository::put_entity(&tx, "task", "append", json!({"agent":"a"}), false)
            .expect("append");
        SyncDocumentsRepository::put_entity(&tx, "task", "append", json!({}), true)
            .expect("ordinary tombstone");
        SyncDocumentsRepository::put_entity(
            &tx,
            "agentSummary",
            "other",
            json!({"id":"other","rootId":"unrelated"}),
            false,
        )
        .expect("unrelated member");
        assert_eq!(fence("alpha"), alpha);
        assert_eq!(fence("beta"), beta);
        tx.execute_batch("SAVEPOINT visibility_move;")
            .expect("savepoint");
        tx.execute("UPDATE sync_entities SET payload=json_set(payload,'$.value.agent','b'),seq=seq+1000 WHERE collection='task' AND id='task'",[]).expect("raw source reassignment");
        assert!(fence("alpha") > alpha);
        assert!(fence("beta") > beta);
        assert!(fence("a") > 0);
        assert!(fence("b") > 0);
        tx.execute_batch("ROLLBACK TO visibility_move; RELEASE visibility_move;")
            .expect("rollback source and fence together");
        assert_eq!(fence("alpha"), alpha);
        assert_eq!(fence("beta"), beta);
        tx.execute("UPDATE sync_entities SET payload='{}',deleted=1,seq=seq+2000 WHERE collection='agentSummary' AND id='a'",[]).expect("retired member");
        let retired = fence("alpha");
        assert!(retired > alpha);
        assert_eq!(fence("beta"), beta);
        tx.execute("UPDATE sync_entities SET payload=json_object('value',json_object('rootId','alpha')),deleted=0,seq=seq+2000 WHERE collection='agentSummary' AND id='a'",[]).expect("restored member");
        assert!(fence("alpha") > retired);
    }

    #[test]
    fn scope_visibility_retains_deleted_owner_until_restore_and_ignores_team_for_root_only_work() {
        use serde_json::json;
        let mut connection = Connection::open_in_memory().expect("SQLite");
        connection
            .execute_batch(
                include_str!("../sync_schema.sql")
                    .split("-- section: runtime_events")
                    .next()
                    .expect("common sync schema section"),
            )
            .expect("bootstrap scope fixture schema");
        let tx = connection.transaction().expect("scope fixture transaction");
        for (id, root) in [("a", "alpha"), ("b", "beta")] {
            SyncDocumentsRepository::put_entity(
                &tx,
                "agentSummary",
                id,
                json!({"id":id,"rootId":root}),
                false,
            )
            .expect("publish team membership");
        }
        SyncDocumentsRepository::put_entity(&tx, "task", "moved", json!({"agent":"a"}), false)
            .expect("publish original task owner");
        let fence = || {
            SyncDocumentsRepository::entity_scope_visibility_revision(
                &tx,
                r#"["task"]"#,
                "alpha",
                true,
            )
            .expect("read alpha visibility fence")
        };
        let baseline = fence();
        SyncDocumentsRepository::put_entity(&tx, "task", "moved", json!({}), true)
            .expect("tombstone original task owner");
        assert_eq!(fence(), baseline, "ordinary deletion stays incremental");
        SyncDocumentsRepository::put_entity(&tx, "task", "moved", json!({"agent":"b"}), false)
            .expect("restore under beta owner");
        assert!(
            fence() > baseline,
            "coalesced delete/restore must retain old ownership"
        );
        SyncDocumentsRepository::put_entity(
            &tx,
            "workDetail",
            "work",
            json!({"rootId":"alpha"}),
            false,
        )
        .expect("publish root-only work record");
        let work_fence = || {
            SyncDocumentsRepository::entity_scope_visibility_revision(
                &tx,
                r#"["workDetail"]"#,
                "alpha",
                false,
            )
            .expect("read root-only work fence")
        };
        let baseline = work_fence();
        SyncDocumentsRepository::put_entity(
            &tx,
            "agentSummary",
            "new-child",
            json!({"rootId":"alpha"}),
            false,
        )
        .expect("publish new team child");
        assert_eq!(
            work_fence(),
            baseline,
            "root-only Work cannot replay history after a new child"
        );
        SyncDocumentsRepository::put_entity(
            &tx,
            "workDetail",
            "outside",
            json!({"rootId":"beta","agent":"alpha"}),
            false,
        )
        .expect("publish out-of-scope work record");
        let selected = SyncDocumentsRepository::resume_entity_scope(
            &tx,
            None,
            Some(r#"["workDetail"]"#),
            None,
            Some("alpha"),
            false,
            512,
        )
        .expect("read root-only work selection");
        assert_eq!(
            selected
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["work"],
            "root-only capability is enforced in SQL even with arbitrary agent properties"
        );
        let provenance_count = || {
            tx.query_row(
                "SELECT COUNT(*) FROM sync_entity_meta WHERE key LIKE 'entity_scope_tombstone:%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count retained tombstone provenance")
        };
        assert_eq!(provenance_count(), 0, "restoration consumes provenance");
        tx.execute_batch("SAVEPOINT deleted_owner")
            .expect("create provenance savepoint");
        SyncDocumentsRepository::put_entity(&tx, "task", "moved", json!({}), true)
            .expect("tombstone within savepoint");
        assert_eq!(provenance_count(), 1);
        tx.execute_batch("ROLLBACK TO deleted_owner; RELEASE deleted_owner")
            .expect("rollback source and provenance");
        assert_eq!(
            provenance_count(),
            0,
            "rollback retains source/provenance atomicity"
        );
        SyncDocumentsRepository::put_entity(&tx, "task", "moved", json!({}), true)
            .expect("tombstone before same-owner restore");
        let restored_fence = fence();
        SyncDocumentsRepository::put_entity(&tx, "task", "moved", json!({"agent":"b"}), false)
            .expect("restore same task owner");
        assert_eq!(
            fence(),
            restored_fence,
            "same-owner restore stays incremental"
        );
        SyncDocumentsRepository::put_entity(&tx, "task", "moved", json!({}), true)
            .expect("tombstone before physical prune");
        tx.execute(
            "DELETE FROM sync_entities WHERE collection='task' AND id='moved'",
            [],
        )
        .expect("physically prune retained tombstone");
        assert_eq!(
            provenance_count(),
            0,
            "physical tombstone pruning cleans metadata"
        );
    }

    #[test]
    fn scope_visibility_sql_keys_match_named_owner() {
        let schema = include_str!("../sync_schema.sql");
        assert_eq!(
            schema
                .matches(&format!("'{}'", super::ENTITY_SCOPE_TEAM_PREDICATE))
                .count(),
            3,
            "named team predicate key owner"
        );
        assert_eq!(
            schema
                .matches(super::ENTITY_SCOPE_TOMBSTONE_KEY_PREFIX)
                .count(),
            4,
            "named tombstone selector provenance owner"
        );
        assert_eq!(
            schema
                .matches(super::ENTITY_SCOPE_VISIBILITY_KEY_PREFIX)
                .count(),
            4,
            "sync_documents::ENTITY_SCOPE_VISIBILITY_KEY_PREFIX owns every bootstrap fence key"
        );
    }

    #[test]
    fn scoped_fingerprint_ignores_offscreen_rows_and_tracks_moves_and_deletions() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mut connection = Connection::open_in_memory().expect("memory SQLite");
        connection.execute_batch(
            "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
             CREATE INDEX sync_entities_non_transcript_seq ON sync_entities(seq) WHERE collection NOT LIKE 'transcript:%';
             CREATE INDEX sync_entities_live_root_scope ON sync_entities(json_extract(payload,'$.value.rootId'),collection,seq) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND json_valid(payload);
             CREATE INDEX sync_entities_live_agent_scope ON sync_entities(json_extract(payload,'$.value.agent'),collection,seq) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND json_valid(payload); CREATE INDEX sync_entities_invalid_payload ON sync_entities(collection,id) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND NOT json_valid(payload);
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2000)
             INSERT INTO sync_entities SELECT 'task',printf('other-%d',x),x,'{\"value\":{\"agent\":\"other\"}}',0 FROM n;
             INSERT INTO sync_entities VALUES('agentSummary','worker',2001,'{\"value\":{\"rootId\":\"lead\"}}',0),('task','visible',2002,'{\"value\":{\"agent\":\"worker\"}}',0);"
        ).expect("scope fixture");
        let transaction = connection.transaction().expect("snapshot");
        let fingerprint = || {
            SyncDocumentsRepository::entity_scope_fingerprint(
                &transaction,
                Some(r#"["task"]"#),
                None,
                Some("lead"),
                true,
            )
            .expect("fingerprint")
        };
        let before = fingerprint();
        transaction
            .execute("UPDATE sync_entities SET seq=2003 WHERE id='other-1'", [])
            .expect("offscreen update");
        let steps = Arc::new(AtomicUsize::new(0));
        let counter = steps.clone();
        transaction
            .progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .expect("count steps");
        assert_eq!(fingerprint(), before, "offscreen update is suppressed");
        transaction
            .progress_handler(0, None::<fn() -> bool>)
            .expect("stop counting");
        assert!(
            steps.load(Ordering::Relaxed) < 1000,
            "fingerprint read unrelated rows"
        );
        transaction.execute("UPDATE sync_entities SET payload='{\"value\":{\"rootId\":\"other\"}}',seq=2004 WHERE id='worker'", []).expect("move worker");
        let moved = fingerprint();
        assert_ne!(before, moved, "membership move removes old task");
        transaction.execute("UPDATE sync_entities SET payload='{\"value\":{\"rootId\":\"lead\"}}',seq=2005 WHERE id='worker'", []).expect("restore worker");
        assert_eq!(before, fingerprint());
        transaction
            .execute(
                "UPDATE sync_entities SET deleted=1,seq=2006 WHERE id='visible'",
                [],
            )
            .expect("delete task");
        assert_ne!(before, fingerprint(), "deletion removes task");
        transaction
            .execute(
                "INSERT INTO sync_entities VALUES('task','corrupt',2007,'broken',0)",
                [],
            )
            .expect("corrupt row remains insertable");
        assert!(
            SyncDocumentsRepository::entity_scope_fingerprint(
                &transaction,
                Some(r#"["task"]"#),
                None,
                Some("lead"),
                true
            )
            .is_err(),
            "corrupt scoped payload requires fallback"
        );
    }

    #[test]
    #[ignore = "manual isolated fingerprint benchmark"]
    fn benchmark_entity_scope_fingerprint() {
        use super::ENTITY_SCOPE_AGENT_PREDICATE;
        use rusqlite::named_params;
        use sha2::{Digest, Sha256};
        use std::time::Instant;
        let directory = tempfile::tempdir().expect("benchmark directory");
        let mut db =
            Connection::open(directory.path().join("canvas.sqlite3")).expect("benchmark database");
        db.execute_batch("CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
            CREATE INDEX sync_entities_collection_seq ON sync_entities(collection,seq);
            CREATE INDEX sync_entities_non_transcript_seq ON sync_entities(seq) WHERE collection NOT LIKE 'transcript:%';
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<233000)
            INSERT INTO sync_entities SELECT CASE WHEN x<=20000 THEN 'task' ELSE 'transcript:seed' END, printf('item-%d',x),x,json_object('value',json_object('agent',printf('agent-%d',x%1000))),0 FROM n;").expect("seed benchmark");
        let tx = db.transaction().expect("benchmark transaction");
        let plan = |sql: &str| {
            let mut statement = tx
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("plan");
            statement
                .query_map([], |row| row.get::<_, String>(3))
                .expect("plan rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("plan detail")
        };
        println!(
            "before_plan={:?}",
            plan(
                "SELECT collection,id,seq FROM sync_entities WHERE collection NOT LIKE 'transcript:%' AND collection IN (SELECT value FROM json_each('[\"task\"]')) AND deleted=0 AND (json_extract(payload,'$.value.agent')='agent-1' OR json_extract(payload,'$.value.agent') IN (SELECT id FROM sync_entities WHERE collection='agentSummary' AND deleted=0 AND json_extract(payload,'$.value.rootId')='agent-1') OR json_extract(payload,'$.value.rootId')='agent-1') ORDER BY seq"
            )
        );
        let started = Instant::now();
        for _ in 0..20 {
            let mut statement = tx.prepare(&format!("SELECT collection,id,seq FROM sync_entities WHERE collection NOT LIKE 'transcript:%' AND (:collections IS NULL OR collection IN (SELECT value FROM json_each(:collections))) AND (:id IS NULL OR id=:id) AND {ENTITY_SCOPE_AGENT_PREDICATE} AND deleted=0 ORDER BY seq")).expect("baseline query");
            let mut digest = Sha256::new();
            let mut rows = statement.query(named_params! { ":collections": r#"["task"]"#, ":id": Option::<&str>::None, ":agent": "agent-1", ":include_team_members": true }).expect("baseline rows");
            let mut count = 0;
            while let Some(row) = rows.next().expect("baseline row") {
                for column in [0, 1] {
                    let text: String = row.get(column).expect("identity");
                    digest.update((text.len() as u64).to_be_bytes());
                    digest.update(text.as_bytes());
                }
                digest.update(row.get::<_, i64>(2).expect("sequence").to_be_bytes());
                count += 1;
            }
            std::hint::black_box(digest.finalize());
            assert_eq!(count, 20);
        }
        println!(
            "baseline_ms_per_check={:.3} candidate_rows_per_check=20000",
            started.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
        let started = Instant::now();
        tx.execute_batch("CREATE INDEX sync_entities_live_root_scope ON sync_entities(json_extract(payload,'$.value.rootId'),collection,seq) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND json_valid(payload); CREATE INDEX sync_entities_live_agent_scope ON sync_entities(json_extract(payload,'$.value.agent'),collection,seq) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND json_valid(payload); CREATE INDEX sync_entities_invalid_payload ON sync_entities(collection,id) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND NOT json_valid(payload);").expect("build indexes");
        println!(
            "index_build_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        println!(
            "after_agent_plan={:?}",
            plan(
                "SELECT collection,id,seq FROM sync_entities INDEXED BY sync_entities_live_agent_scope WHERE collection NOT LIKE 'transcript:%' AND deleted=0 AND json_valid(payload) AND collection='task' AND json_extract(payload,'$.value.agent')='agent-1' ORDER BY seq"
            )
        );
        println!(
            "after_root_plan={:?}",
            plan(
                "SELECT id FROM sync_entities INDEXED BY sync_entities_live_root_scope WHERE collection NOT LIKE 'transcript:%' AND deleted=0 AND json_valid(payload) AND collection='agentSummary' AND json_extract(payload,'$.value.rootId')='agent-1'"
            )
        );
        println!(
            "invalid_payload_plan={:?}",
            plan(
                "SELECT id FROM sync_entities INDEXED BY sync_entities_invalid_payload WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND NOT json_valid(payload) AND collection='task'"
            )
        );
        println!(
            "unchanged_plan={:?}",
            plan(
                "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection NOT LIKE 'transcript:%'"
            )
        );
        let started = Instant::now();
        for _ in 0..20 {
            SyncDocumentsRepository::entity_scope_fingerprint(
                &tx,
                Some(r#"["task"]"#),
                None,
                Some("agent-1"),
                true,
            )
            .expect("indexed fingerprint");
        }
        println!(
            "indexed_ms_per_check={:.3} scoped_rows_per_check=20",
            started.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
        let started = Instant::now();
        for _ in 0..20 {
            SyncDocumentsRepository::entity_sequence(&tx).expect("unchanged sequence");
        }
        println!(
            "unchanged_ms_per_check={:.3} entity_rows_read=0 subscriptions=20",
            started.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
        tx.pragma_update(None, "cache_size", -8192)
            .expect("benchmark 8 MiB reader cache");
        let started = Instant::now();
        for _ in 0..20 {
            SyncDocumentsRepository::entity_scope_fingerprint(
                &tx,
                Some(r#"["task"]"#),
                None,
                Some("agent-1"),
                true,
            )
            .expect("indexed fingerprint with larger cache");
        }
        println!(
            "indexed_8m_cache_ms_per_check={:.3}",
            started.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
    }

    #[test]
    fn resume_entities_fingerprint_tracks_membership_swaps_with_equal_count_and_maximum() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite");
        connection.execute_batch(
            "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,payload TEXT,deleted INTEGER);
             CREATE INDEX sync_entities_live_root_scope ON sync_entities(json_extract(payload,'$.value.rootId'),collection,seq) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND json_valid(payload);
             CREATE INDEX sync_entities_live_agent_scope ON sync_entities(json_extract(payload,'$.value.agent'),collection,seq) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND json_valid(payload); CREATE INDEX sync_entities_invalid_payload ON sync_entities(collection,id) WHERE deleted=0 AND collection NOT LIKE 'transcript:%' AND NOT json_valid(payload);
             CREATE INDEX sync_entities_collection_seq ON sync_entities(collection,seq);
             CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT);
             INSERT INTO sync_entities VALUES
             ('agentSummary','a',20,'{\"value\":{\"rootId\":\"lead\"}}',0),
             ('agentSummary','b',21,'{\"value\":{\"rootId\":\"other\"}}',0),
             ('task','a-task',1,'{\"value\":{\"agent\":\"a\"}}',0),
             ('task','b-task',2,'{\"value\":{\"agent\":\"b\"}}',0),
             ('task','latest',10,'{\"value\":{\"agent\":\"lead\"}}',0);"
        ).expect("scope fixture");
        let transaction = connection.transaction().expect("snapshot");
        let fingerprint = |id, agent| {
            SyncDocumentsRepository::entity_scope_fingerprint(
                &transaction,
                Some(r#"["task"]"#),
                id,
                agent,
                true,
            )
            .expect("complete fingerprint")
        };
        let before = fingerprint(None, Some("lead"));
        let selected = |agent| {
            SyncDocumentsRepository::resume_entity_scope(
                &transaction,
                None,
                Some(r#"["task"]"#),
                None,
                Some(agent),
                true,
                100,
            )
            .expect("selected rows")
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>()
        };
        assert_eq!(selected("lead"), ["a-task", "latest"]);
        let exact = fingerprint(Some("latest"), Some("lead"));
        let empty = SyncDocumentsRepository::entity_scope_fingerprint(
            &transaction,
            Some("[]"),
            None,
            None,
            true,
        )
        .expect("empty scope");
        transaction.execute("UPDATE sync_entities SET payload=json_set(payload,'$.value.rootId',CASE id WHEN 'a' THEN 'other' ELSE 'lead' END),seq=seq+100 WHERE collection='agentSummary'", []).expect("swap team members");
        assert_eq!(selected("lead"), ["b-task", "latest"]);
        let swapped = fingerprint(None, Some("lead"));
        assert_ne!(
            before, swapped,
            "equal count and maximum do not prove equal membership"
        );
        assert_eq!(
            exact,
            fingerprint(Some("latest"), Some("lead")),
            "row identity filtering excludes the swap"
        );
        assert_eq!(
            empty,
            SyncDocumentsRepository::entity_scope_fingerprint(
                &transaction,
                Some("[]"),
                None,
                None,
                true
            )
            .expect("empty scope")
        );
        transaction
            .execute(
                "UPDATE sync_entities SET seq=200 WHERE collection='task' AND id='b-task'",
                [],
            )
            .expect("task status publication");
        let changed = fingerprint(None, Some("lead"));
        assert_ne!(
            swapped, changed,
            "an update with unchanged count must publish"
        );
        transaction.execute("UPDATE sync_entities SET deleted=1,seq=201 WHERE collection='task' AND id='latest'", []).expect("task removal");
        assert_ne!(
            changed,
            fingerprint(None, Some("lead")),
            "a removal must publish"
        );
        assert_eq!(selected("lead"), ["b-task"]);
    }

    #[test]
    fn resume_entities_current_old_and_expired_cursors() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite");
        connection.execute_batch(
            "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
             CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT);
             INSERT INTO sync_entities VALUES('agent','a',2,'a','{}',0),('agent','b',4,'b','{}',0),('agent','gone',5,'g','{}',1),('transcript:a','item:x',6,'x',NULL,0);
             INSERT INTO sync_entity_meta VALUES('entity_tombstone_floor','1');",
        ).expect("fixture");
        let transaction = connection.transaction().expect("snapshot");
        let current =
            SyncDocumentsRepository::resume_entities(&transaction, Some(5), None, None, 100)
                .expect("current");
        assert!(!current.reset);
        assert!(current.rows.is_empty());
        assert_eq!(current.checkpoint, 5);
        let old = SyncDocumentsRepository::resume_entities(&transaction, Some(2), None, None, 100)
            .expect("older");
        assert!(!old.reset);
        assert_eq!(
            old.rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "gone"]
        );
        for since in [None, Some(0), Some(6), Some(-1)] {
            let expired =
                SyncDocumentsRepository::resume_entities(&transaction, since, None, None, 100)
                    .expect("expired");
            assert!(expired.reset);
            assert_eq!(
                expired
                    .rows
                    .iter()
                    .map(|row| row.id.as_str())
                    .collect::<Vec<_>>(),
                ["a", "b"]
            );
            assert_eq!(expired.checkpoint, 5);
        }
        let scoped = SyncDocumentsRepository::resume_entities(
            &transaction,
            Some(2),
            Some("agent"),
            Some("a"),
            100,
        )
        .expect("scoped");
        assert!(
            scoped.rows.is_empty(),
            "an unrelated entity never enters this ref"
        );
    }

    #[test]
    fn entity_replay_and_patch_preserve_sequence_until_value_or_deletion_changes() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection.execute_batch(
            "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
             CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);",
        ).expect("sync fixture creates");
        let transaction = connection.transaction().expect("transaction begins");
        let value = serde_json::json!({"b":2,"a":1});
        assert!(
            SyncDocumentsRepository::put_entity(&transaction, "workspace", "current", value, false)
                .expect("initial entity")
        );
        let before =
            SyncDocumentsRepository::entity_sequence(&transaction).expect("initial sequence");
        assert!(
            !SyncDocumentsRepository::put_entity(
                &transaction,
                "workspace",
                "current",
                serde_json::json!({"a":1,"b":2}),
                false
            )
            .expect("normalized replay")
        );
        SyncDocumentsRepository::patch_entity(
            &transaction,
            "workspace",
            "current",
            serde_json::json!({"a":1}),
        )
        .expect("equal patch");
        assert_eq!(
            SyncDocumentsRepository::entity_sequence(&transaction).expect("unchanged sequence"),
            before
        );
        assert!(
            SyncDocumentsRepository::put_entity(
                &transaction,
                "workspace",
                "current",
                serde_json::json!({"a":2}),
                false
            )
            .expect("content change")
        );
        assert!(
            SyncDocumentsRepository::entity_sequence(&transaction).expect("changed sequence")
                > before
        );
        assert!(
            SyncDocumentsRepository::put_entity(
                &transaction,
                "workspace",
                "current",
                serde_json::json!({}),
                true
            )
            .expect("delete")
        );
        let deleted =
            SyncDocumentsRepository::entity_sequence(&transaction).expect("deleted sequence");
        assert!(
            !SyncDocumentsRepository::put_entity(
                &transaction,
                "workspace",
                "current",
                serde_json::json!({"ignored":true}),
                true
            )
            .expect("repeated delete")
        );
        assert_eq!(
            SyncDocumentsRepository::entity_sequence(&transaction).expect("delete replay sequence"),
            deleted
        );
        assert!(
            SyncDocumentsRepository::put_entity(
                &transaction,
                "workspace",
                "current",
                serde_json::json!({}),
                false
            )
            .expect("revive")
        );
    }

    #[test]
    fn hash_only_entity_uses_shared_sequence_and_replay_is_a_noop() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
                 CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);
                 INSERT INTO sync_documents VALUES(9,'drafts','d','{}',0);",
            )
            .expect("sync fixture creates");
        let transaction = connection.transaction().expect("transaction begins");
        assert!(
            SyncDocumentsRepository::put_entity_hash(
                &transaction,
                "transcript:agent",
                "item:one",
                "digest",
                false,
            )
            .expect("hash-only entity stores")
        );
        assert!(
            !SyncDocumentsRepository::put_entity_hash(
                &transaction,
                "transcript:agent",
                "item:one",
                "digest",
                false,
            )
            .expect("identical hash replays")
        );
        let row: (i64, Option<String>) = transaction
            .query_row(
                "SELECT seq,payload FROM sync_entities WHERE collection='transcript:agent' AND id='item:one'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("hash-only row reads");
        assert_eq!(row, (10, None));
        transaction.commit().expect("transaction commits");
    }
}
