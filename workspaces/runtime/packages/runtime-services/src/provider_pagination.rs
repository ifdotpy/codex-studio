//! Strict cursor decoding shared by provider listing consumers.
use serde::Deserialize;

/// A listing's envelope; item shapes remain owned by each consumer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub data: Vec<T>,
    #[serde(default)]
    pub next_cursor: Cursor,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
pub enum Cursor {
    String(String),
    #[default]
    Null,
    Invalid(serde::de::IgnoredAny),
}

#[derive(Debug, thiserror::Error)]
#[error("invalid provider pagination cursor")]
pub struct InvalidCursor;

/// Only a missing or null cursor certifies the end of a listing.
pub fn next_cursor(cursor: &Cursor) -> Result<Option<&str>, InvalidCursor> {
    match cursor {
        Cursor::Null => Ok(None),
        Cursor::String(cursor) if !cursor.is_empty() => Ok(Some(cursor)),
        Cursor::String(_) | Cursor::Invalid(_) => Err(InvalidCursor),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn cursor_accepts_only_null_or_nonempty_strings() {
        for value in [
            json!(null),
            json!("next"),
            json!(""),
            json!(1),
            json!([]),
            json!({}),
        ] {
            let cursor: Cursor = serde_json::from_value(value.clone()).expect("cursor shape");
            match value {
                serde_json::Value::Null => assert!(matches!(next_cursor(&cursor), Ok(None))),
                serde_json::Value::String(ref s) if !s.is_empty() => {
                    assert_eq!(next_cursor(&cursor).expect("valid"), Some(s.as_str()));
                }
                Value::Bool(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Array(_)
                | Value::Object(_) => assert!(next_cursor(&cursor).is_err()),
            }
        }
    }
}
