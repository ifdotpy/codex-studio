//! Existing transfer rows; schema creation remains owned by store bootstrap.
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{
    AgentId, AgentRecord, JsonRecord, RecordKeyMode, RecordQueryShape, StoreError, StoreResult,
    Table,
};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Deserialize, Serialize)]
pub struct AccountTransferRecord {
    id: String,
    #[serde(flatten)]
    fields: Map<String, Value>,
}
impl JsonRecord for AccountTransferRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_account_transfers";
    const SCHEMA: &'static str = "PersistedAccountTransferRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    fn record_key(&self) -> Option<&String> {
        Some(&self.id)
    }
    // Remove this override once the record is closed and typed.
    fn encode_stored(&self, _: &rusqlite::Connection, row_id: &str) -> crate::StoreResult<String> {
        crate::records::encode_open_record(self, row_id)
    }
}
pub const MAX_TRANSFER_RECORD_BYTES: usize = 8 * 1024 * 1024;
/// Largest provider read reply kept inline in a transfer record. A larger reply
/// (a history page) lives in the payload blob store and the record keeps its
/// digest. Unit: bytes of the reply's JSON text.
pub const MAX_INLINE_READ_REPLY_BYTES: usize = 64 * 1024;
pub struct AccountTransferRepository;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_the_checked_value_without_a_second_decode() {
        let _: fn(&Connection, &str) -> StoreResult<Option<Value>> = AccountTransferRepository::get;
    }
}
impl AccountTransferRepository {
    /// Maximum combined operation and agent JSON size in one transfer snapshot, in bytes.
    pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

    pub fn available(db: &Connection) -> StoreResult<bool> {
        Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_account_transfers')", [], |row| row.get(0))?)
    }
    pub fn get(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Ok(Self::get_with_raw(db, id)?.map(|(value, _)| value))
    }

    /// Keep the original text for the ordered transfer snapshot decoder.
    pub fn get_with_raw(db: &Connection, id: &str) -> StoreResult<Option<(Value, String)>> {
        bounded_record(db, "runtime_account_transfers", id)?
            .map(|raw| {
                let record = Table::<AccountTransferRecord>::new(db).decode_stored(id, &raw)?;
                let mut fields = record.fields;
                fields.insert("id".to_owned(), Value::String(record.id));
                Ok((Value::Object(fields), raw))
            })
            .transpose()
    }
    /// Recover every nonterminal operation. Unknown statuses still reach the
    /// record validator; only known terminal history is skipped.
    pub fn recovery_ids(db: &Connection) -> StoreResult<Vec<String>> {
        let mut statement = db.prepare_cached(
            "SELECT id FROM runtime_account_transfers
             WHERE json_extract(record,'$.status') IS NOT 'completed'
             AND json_extract(record,'$.status') IS NOT 'cancelled' ORDER BY rowid",
        )?;
        Ok(statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    /// Request ids are global, including aliases enrolled into an active
    /// operation. The reducer checks the saved lead, destination and scope.
    pub fn request_operation(db: &Connection, request: &str) -> StoreResult<Option<String>> {
        Ok(db
            .query_row(
                "SELECT id FROM runtime_account_transfers
             WHERE id=?1 OR EXISTS(SELECT 1 FROM json_each(record,'$.requests') WHERE key=?1)
             ORDER BY rowid LIMIT 1",
                [request],
                |row| row.get(0),
            )
            .optional()?)
    }
    /// Live team records, including the lead independently of its root id.
    /// Scope controls enrollment in the service; the lead always owns the
    /// snapshot and summary, even for a subagent-only transfer. Unrelated rows
    /// never enter the snapshot's schema-owned team resource limits.
    pub fn team_agents(db: &Connection, lead: &AgentId) -> StoreResult<Vec<AgentRecord>> {
        let mut statement = db.prepare_cached(
            "SELECT id FROM runtime_agents WHERE id=?1 OR root_id=?1 ORDER BY rowid",
        )?;
        let mut rows = statement.query([lead.as_ref()])?;
        let mut agents = Vec::new();
        let mut bytes = 0_usize;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let Some(raw) = bounded_record(db, "runtime_agents", &id)? else {
                continue;
            };
            let agent = Table::<AgentRecord>::new(db).decode_stored(&id, &raw)?;
            if !agent.is_deleted() {
                bytes = bytes.saturating_add(serde_json::to_vec(&agent)?.len());
                if agents.len() >= studio_runtime_schema::agent_limits::MAX_TRANSFER_AGENT_RECORDS
                    || bytes > Self::MAX_SNAPSHOT_BYTES
                {
                    return Err(StoreError::SchemaValidation(
                        "transfer team exceeds snapshot resource limits".into(),
                    ));
                }
                agents.push(agent);
            }
        }
        Ok(agents)
    }
    pub fn put(db: &Transaction<'_>, id: &str, raw: &str) -> StoreResult<()> {
        if raw.len() > MAX_TRANSFER_RECORD_BYTES {
            return Err(StoreError::InvalidStoredObject {
                table: "runtime_account_transfers",
                id: id.into(),
            });
        }
        let value: Value = serde_json::from_str(raw)?;
        if value["id"] != id
            || !studio_runtime_schema::validate("PersistedAccountTransferRecord", &value)
                .map_err(StoreError::SchemaValidation)?
        {
            return Err(StoreError::InvalidStoredObject {
                table: "runtime_account_transfers",
                id: id.into(),
            });
        }
        // The caller retains member insertion order, which is observable in notices.
        db.execute("INSERT INTO runtime_account_transfers(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", (id, raw))?;
        Ok(())
    }
    /// The bounded stored agent record with its volatile fields.
    pub fn agent(db: &Connection, id: &str) -> StoreResult<Option<String>> {
        bounded_record(db, "runtime_agents", id)?
            .map(|raw| crate::agent_volatile::merged_text(db, id, raw))
            .transpose()
    }
}
const TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_account_transfers",
    columns: &["id", "record"],
}];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! { crate::repositories::SchemaRegistration { repository: "account_transfer", requirements: &REQUIREMENTS } }

fn bounded_record(db: &Connection, table: &'static str, id: &str) -> StoreResult<Option<String>> {
    let sql = format!("SELECT substr(CAST(record AS BLOB),1,?2) FROM {table} WHERE id=?1");
    let bytes: Option<Vec<u8>> = db
        .query_row(
            &sql,
            rusqlite::params![id, (MAX_TRANSFER_RECORD_BYTES + 1) as i64],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| {
            if bytes.len() > MAX_TRANSFER_RECORD_BYTES {
                return Err(StoreError::InvalidStoredObject {
                    table,
                    id: id.into(),
                });
            }
            String::from_utf8(bytes).map_err(|_| StoreError::InvalidStoredObject {
                table,
                id: id.into(),
            })
        })
        .transpose()
}
