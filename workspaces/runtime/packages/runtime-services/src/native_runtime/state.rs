use std::fs;
use std::path::Path;

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateReadError {
    #[error("native runtime status file could not be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("native runtime status file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("native runtime status record must be a JSON object")]
    NotObject,
}

/// Lossless view of Python's `native-runtime/status.json`. Unknown fields and
/// arbitrary account rotation receipts remain available in `record`.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeRuntimeState {
    record: Value,
}

impl NativeRuntimeState {
    #[must_use]
    pub fn record(&self) -> &Value {
        &self.record
    }

    #[must_use]
    pub fn selected(&self) -> Option<&Value> {
        self.record.get("selected").filter(|value| !value.is_null())
    }

    #[must_use]
    pub fn accounts(&self) -> Option<&serde_json::Map<String, Value>> {
        self.record.get("accounts").and_then(Value::as_object)
    }
}

/// Read a Python-written state record and preserve its complete JSON value.
pub fn read_state(path: &Path) -> Result<NativeRuntimeState, StateReadError> {
    let record: Value = serde_json::from_slice(&fs::read(path)?)?;
    if !record.is_object() {
        return Err(StateReadError::NotObject);
    }
    Ok(NativeRuntimeState { record })
}
