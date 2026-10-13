//! Shared identities and checked JSON boundary for relocation decisions.
use serde::de::DeserializeOwned;
use serde_json::Value;
use studio_runtime_store::StoreError;
use thiserror::Error;
use uuid::Uuid;

const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RelocationError {
    #[error("invalid stored {object} at {pointer}")]
    InvalidStored {
        object: &'static str,
        pointer: String,
    },
    #[error("{0}")]
    Refused(&'static str),
    #[error("Not implemented: requires {0}")]
    Requires(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("relocation database operation failed")]
    Sqlite(#[from] rusqlite::Error),
}

/// Same durable UUID namespace used by Python's paired channel.
pub fn identity(parts: &[&str]) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("studio-cross-server:{}", parts.join(":")).as_bytes(),
    )
    .to_string()
}

pub(crate) fn checked<T: DeserializeOwned>(
    schema: &str,
    object: &'static str,
    value: &Value,
) -> Result<T, RelocationError> {
    if value.to_string().len() > MAX_RECORD_BYTES {
        return Err(invalid(object, "record size"));
    }
    if let Some((pointer, _)) =
        studio_runtime_schema::first_error(schema, value).map_err(StoreError::SchemaValidation)?
    {
        return Err(invalid(object, &pointer));
    }
    serde_json::from_value(value.clone()).map_err(|_| invalid(object, "record"))
}

pub(crate) fn invalid(object: &'static str, pointer: &str) -> RelocationError {
    RelocationError::InvalidStored {
        object,
        pointer: pointer.to_owned(),
    }
}

pub(crate) fn timestamp(now: f64) -> Result<String, RelocationError> {
    if !now.is_finite() || !(0.0..=253_402_300_799.0).contains(&now) {
        return Err(RelocationError::Refused("Invalid relocation timestamp"));
    }
    chrono::DateTime::from_timestamp_millis((now * 1000.0).floor() as i64)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .ok_or(RelocationError::Refused("Invalid relocation timestamp"))
}
