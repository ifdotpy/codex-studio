//! Multi-server audit projection over existing access records.

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_access_audit",
    columns: &["seq", "client", "actor", "action", "created"],
}];
const CONDITIONAL_TABLES: &[ConditionalTableRequirement] = &[ConditionalTableRequirement {
    table_exists: "runtime_access_clients",
    tables: TABLES,
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

#[derive(Clone, Copy, Debug, Default)]
pub struct MultiServerRepository;

impl MultiServerRepository {
    pub fn audit(connection: &Connection) -> StoreResult<Value> {
        let mut statement = connection.prepare(
            "SELECT seq,client,actor,action,created FROM runtime_access_audit ORDER BY seq DESC LIMIT 100",
        )?;
        let records = statement.query_map([], |row| {
            let sequence: i64 = row.get(0)?;
            let client_id: String = row.get(1)?;
            let actor_id: String = row.get(2)?;
            let action: String = row.get(3)?;
            let created: f64 = row.get(4)?;
            Ok((sequence, client_id, actor_id, action, created))
        })?;
        let mut values = Vec::new();
        for record in records {
            let (sequence, client_id, actor_id, action, created) = record?;
            if sequence < 1 || action.is_empty() || !created.is_finite() {
                return Err(StoreError::SchemaMismatch(
                    "invalid stored multi-server audit record".to_owned(),
                ));
            }
            values.push(json!({
                "sequence": sequence,
                "clientId": client_id,
                "actorId": actor_id,
                "action": action,
                "created": created,
            }));
        }
        Ok(json!({"records": values}))
    }
}
