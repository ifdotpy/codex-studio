//! Existing workspace profile records written by the Python runtime.

use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Deserializer, Serialize};

use crate::records::{JsonRecord, RecordKeyMode, RecordQueryShape, Table};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const PROFILE_TABLE: TableRequirement = TableRequirement {
    name: "runtime_profiles",
    columns: &["id", "record"],
};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[PROFILE_TABLE],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProfileRecord {
    pub id: String,
    pub name: String,
    pub role: ProfileRole,
    pub model: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub effort: Option<ReasoningEffort>,
    pub instructions: String,
}

impl JsonRecord for ProfileRecord {
    type Id = String;

    const TABLE: &'static str = "runtime_profiles";
    const SCHEMA: &'static str = "ProfileRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;

    fn record_key(&self) -> Option<&Self::Id> {
        Some(&self.id)
    }
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileRole {
    Reviewer,
    Implementer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProfileRepository;

impl ProfileRepository {
    pub fn list(connection: &Connection) -> StoreResult<Vec<ProfileRecord>> {
        Table::<ProfileRecord>::new(connection).list()
    }

    pub fn save(transaction: &Transaction<'_>, profile: &ProfileRecord) -> StoreResult<()> {
        let table = Table::<ProfileRecord>::new(transaction);
        if table.get(&profile.id)?.is_some() {
            table.update(profile)?;
        } else {
            table.put(profile)?;
        }
        Ok(())
    }

    pub fn delete(transaction: &Transaction<'_>, id: &str) -> StoreResult<()> {
        Table::<ProfileRecord>::new(transaction).delete(&id.to_owned())?;
        Ok(())
    }

    pub fn new_id(connection: &Connection) -> StoreResult<String> {
        let mut bytes =
            connection.query_row("SELECT randomblob(16)", [], |row| row.get::<_, Vec<u8>>(0))?;
        if bytes.len() != 16 {
            return Err(StoreError::SchemaMismatch(
                "SQLite returned invalid UUID bytes".to_owned(),
            ));
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(format_uuid(&bytes))
    }
}

fn format_uuid(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            value.push('-');
        }
        value.push(char::from(HEX[usize::from(*byte >> 4)]));
        value.push(char::from(HEX[usize::from(*byte & 0x0f)]));
    }
    value
}
