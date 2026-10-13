//! Synchronous access to the durable sync document and entity tables.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::schema::{SchemaRequirements, TableRequirement, TriggerRequirement};
use crate::{StoreError, StoreResult};

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

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncDocumentsRepository;

impl SyncDocumentsRepository {
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
            .query_row(
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
        let mut statement = connection.prepare(
            "SELECT collection,id,seq,payload,deleted FROM sync_entities \
             WHERE seq>?1 AND collection NOT LIKE 'transcript:%' ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after, limit.clamp(1, 501)], |row| {
            Ok(EntityRow {
                collection: row.get(0)?,
                id: row.get(1)?,
                sequence: row.get(2)?,
                payload: row.get(3)?,
                deleted: row.get::<_, i64>(4)? != 0,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn entity_tombstone_floor(connection: &Connection) -> StoreResult<i64> {
        let value = connection
            .query_row(
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
            .query_row("SELECT id FROM sync_identity LIMIT 1", [], |row| row.get(0))
            .map_err(StoreError::from)
    }

    pub fn pull_drafts(
        connection: &Connection,
        after: i64,
        limit: i64,
    ) -> StoreResult<Vec<SyncDocument>> {
        let mut statement = connection.prepare(
            "SELECT seq,id,payload,deleted FROM sync_documents \
             WHERE scope='drafts' AND seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after.max(0), limit.clamp(1, 100)], |row| {
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
            .query_row(
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
        let current = transaction
            .query_row(
                "SELECT hash,deleted FROM sync_entities WHERE collection=?1 AND id=?2",
                params![collection, id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        if current
            .as_ref()
            .is_some_and(|(old_hash, old_deleted)| old_hash == &hash && *old_deleted == deleted)
        {
            return Ok(false);
        }
        transaction.execute(
            "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) \
             VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash, \
             payload=excluded.payload,deleted=excluded.deleted",
            params![
                collection,
                id,
                next_sequence(transaction)?,
                hash,
                payload,
                i64::from(deleted)
            ],
        )?;
        Ok(true)
    }
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
        .query_row(
            "SELECT MAX(entity_seq,document_seq,version_seq)+1 FROM ( \
             SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities), \
             (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents), \
             (SELECT COALESCE(MAX(seq),0) AS version_seq FROM sync_versions)",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}
