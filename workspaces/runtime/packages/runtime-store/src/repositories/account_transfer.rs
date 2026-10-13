//! Existing transfer rows; schema creation remains owned by store bootstrap.
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{JsonRecord, RecordKeyMode, RecordQueryShape, StoreError, StoreResult, Table};
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
}
pub const MAX_TRANSFER_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub struct AccountTransferRepository;
impl AccountTransferRepository {
    pub fn available(db: &Connection) -> StoreResult<bool> {
        Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_account_transfers')", [], |row| row.get(0))?)
    }
    pub fn get(db: &Connection, id: &str) -> StoreResult<Option<String>> {
        bounded_record(db, "runtime_account_transfers", id)?
            .map(|raw| {
                Table::<AccountTransferRecord>::new(db).decode_stored(id, &raw)?;
                Ok(raw)
            })
            .transpose()
    }
    pub fn ids(db: &Connection) -> StoreResult<Vec<String>> {
        let mut statement =
            db.prepare("SELECT id FROM runtime_account_transfers ORDER BY rowid LIMIT 4097")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > 4096 {
            return Err(StoreError::SchemaValidation(
                "transfer operation scan exceeds resource limit".into(),
            ));
        }
        Ok(ids)
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
    pub fn agent(db: &Connection, id: &str) -> StoreResult<Option<String>> {
        bounded_record(db, "runtime_agents", id)
    }
}
const TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_account_transfers",
    columns: &["id", "record"],
}];
const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_account_transfers",
        tables: TABLES,
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
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
