//! Multi-server audit projection over existing access records.

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

const TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_access_audit",
    columns: &["seq", "client", "actor", "action", "created"],
}];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct MultiServerRepository;

impl MultiServerRepository {
    pub fn audit(connection: &Connection) -> StoreResult<Value> {
        let mut statement = connection.prepare(
            "SELECT CAST(seq AS TEXT),client,actor,action,created FROM runtime_access_audit ORDER BY seq DESC LIMIT 100",
        )?;
        let records = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0),
                row.get::<_, String>(1),
                row.get::<_, String>(2),
                row.get::<_, String>(3),
                row.get::<_, f64>(4),
            ))
        })?;
        let mut values = Vec::new();
        for record in records {
            let (sequence, client_id, actor_id, action, created) = record?;
            let sequence = sequence.map_err(|_| invalid("sequence", "unknown"))?;
            let sequence_id = sequence.as_str();
            let sequence_number = sequence
                .parse::<i64>()
                .map_err(|_| invalid("sequence", sequence_id))?;
            let client_id = client_id.map_err(|_| invalid("client", sequence_id))?;
            let actor_id = actor_id.map_err(|_| invalid("actor", sequence_id))?;
            let action = action.map_err(|_| invalid("action", sequence_id))?;
            let created = created.map_err(|_| invalid("created", sequence_id))?;
            if sequence_number < 1 || action.is_empty() || !created.is_finite() {
                return Err(invalid("record", sequence_id));
            }
            values.push(json!({
                "sequence": sequence_number,
                "clientId": client_id,
                "actorId": actor_id,
                "action": action,
                "created": created,
            }));
        }
        Ok(json!({"records": values}))
    }
}

fn invalid(field: &str, sequence: &str) -> StoreError {
    StoreError::SchemaMismatch(format!(
        "invalid stored object runtime_access_audit/{sequence}: {field}"
    ))
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "multi_server",
        requirements: &REQUIREMENTS,
    }
}
