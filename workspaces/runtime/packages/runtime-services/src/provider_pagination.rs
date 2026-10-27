//! Strict cursor decoding shared by provider listing consumers.
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("invalid provider pagination cursor")]
pub struct InvalidCursor;

/// Only a missing or null cursor certifies the end of a listing.
pub fn next_cursor(page: &Value) -> Result<Option<&str>, InvalidCursor> {
    match page.get("nextCursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if !cursor.is_empty() => Ok(Some(cursor)),
        Some(_) => Err(InvalidCursor),
    }
}
