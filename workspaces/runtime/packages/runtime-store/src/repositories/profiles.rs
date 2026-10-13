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
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[PROFILE_TABLE],
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
    const SCHEMA: &'static str = "PersistedProfileRecord";
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
        let bytes =
            connection.query_row("SELECT randomblob(16)", [], |row| row.get::<_, Vec<u8>>(0))?;
        let bytes: [u8; 16] = bytes.try_into().map_err(|_| {
            StoreError::SchemaMismatch("SQLite returned invalid UUID bytes".to_owned())
        })?;
        Ok(uuid::Builder::from_random_bytes(bytes)
            .into_uuid()
            .to_string())
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "profiles",
        requirements: &REQUIREMENTS,
    }
}
