//! Open execution-attempt record for the shared typed table and read-only audit.
use crate::{JsonRecord, RecordKeyMode, RecordQueryShape};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[derive(Clone, Deserialize, Serialize)]
pub struct ClaudeAttemptRecord {
    id: String,
    #[serde(flatten)]
    fields: Map<String, Value>,
}
impl JsonRecord for ClaudeAttemptRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_execution_attempts";
    const SCHEMA: &'static str = "PersistedClaudeAttemptRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    fn record_key(&self) -> Option<&String> {
        Some(&self.id)
    }
}
