//! Existing workspace profile records written by the Python runtime.

use rusqlite::{Connection, OptionalExtension, Transaction, params, types::Value as SqlValue};
use serde::{Deserialize, Deserializer, Serialize};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const PROFILE_TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_profiles",
    columns: &["id", "record"],
}];
const CONDITIONAL_TABLES: &[ConditionalTableRequirement] = &[ConditionalTableRequirement {
    table_exists: "runtime_profiles",
    tables: PROFILE_TABLES,
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: CONDITIONAL_TABLES,
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
        let mut statement =
            connection.prepare("SELECT id, record FROM runtime_profiles ORDER BY rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, SqlValue>(1)?))
        })?;
        let stored = rows.collect::<Result<Vec<_>, _>>()?;
        stored
            .into_iter()
            .map(|(id, record)| decode_record(&id, record))
            .collect()
    }

    pub fn save(transaction: &Transaction<'_>, profile: &ProfileRecord) -> StoreResult<()> {
        let existing = transaction
            .query_row(
                "SELECT record FROM runtime_profiles WHERE id=?1",
                [&profile.id],
                |row| row.get::<_, SqlValue>(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            decode_record(&profile.id, existing)?;
        }
        let record = serde_json::to_string(profile)?;
        transaction.execute(
            "INSERT INTO runtime_profiles(id, record) VALUES (?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![profile.id, record],
        )?;
        Ok(())
    }

    pub fn delete(transaction: &Transaction<'_>, id: &str) -> StoreResult<()> {
        transaction.execute("DELETE FROM runtime_profiles WHERE id=?1", [id])?;
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

fn decode_record(id: &str, stored: SqlValue) -> StoreResult<ProfileRecord> {
    let invalid = || StoreError::SchemaMismatch(format!("Invalid stored object profiles/{id}"));
    let SqlValue::Text(raw) = stored else {
        return Err(invalid());
    };
    let profile: ProfileRecord = serde_json::from_str(&raw).map_err(|_| invalid())?;
    if profile.id != id {
        return Err(invalid());
    }
    Ok(profile)
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
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "profiles",
        requirements: &REQUIREMENTS,
    }
}
