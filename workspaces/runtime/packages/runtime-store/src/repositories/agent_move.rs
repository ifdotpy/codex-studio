//! Existing agent-move records, conditionally present after paired-channel initialization.
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{JsonRecord, RecordKeyMode, RecordQueryShape, StoreResult, Table};
use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_agent_moves",
        tables: &[TableRequirement {
            name: "runtime_agent_moves",
            columns: &["id", "record"],
        }],
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
#[derive(Clone, Serialize, Deserialize)]
pub struct MoveRecord {
    id: String,
    #[serde(flatten)]
    fields: Map<String, Value>,
}
impl JsonRecord for MoveRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_agent_moves";
    const SCHEMA: &'static str = "relocation/MoveRecord";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::PayloadId;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    fn record_key(&self) -> Option<&String> {
        Some(&self.id)
    }
}
pub struct AgentMoveRepository;
impl AgentMoveRepository {
    pub fn get(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Table::<MoveRecord>::new(db)
            .get(&id.to_owned())?
            .map(|row| serde_json::to_value(row).map_err(Into::into))
            .transpose()
    }
    pub fn save(tx: &Transaction<'_>, row: &Value) -> StoreResult<()> {
        let row: MoveRecord = serde_json::from_value(row.clone())?;
        Table::<MoveRecord>::new(tx).put(&row)
    }
    pub fn pending_events(db: &Connection, agent: &str) -> StoreResult<bool> {
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status='pending')",
            [agent],
            |row| row.get(0),
        )?)
    }
}
inventory::submit! {crate::repositories::SchemaRegistration {repository:"agent_move",requirements:&REQUIREMENTS}}
