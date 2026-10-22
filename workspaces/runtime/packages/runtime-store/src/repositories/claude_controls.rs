//! Transactional Claude control records; other workspace operation kinds are untouched.
use crate::schema::SchemaRequirements;
use crate::{AgentId, AgentRecord, AgentRepository, StoreError, StoreResult, Transaction};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
pub struct ClaudeControlRepository;
impl ClaudeControlRepository {
    pub fn load(
        tx: &Transaction<'_>,
        agent: &str,
        id: &str,
    ) -> StoreResult<Option<(Value, Option<Value>)>> {
        let Some(agent) = AgentRepository::get(tx, &AgentId(agent.into()))? else {
            return Ok(None);
        };
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let operation = raw
            .map(|raw| serde_json::from_str::<Value>(&raw))
            .transpose()?;
        if let Some(op) = &operation {
            validate(op)?;
            if op.get("id").and_then(Value::as_str) != Some(id) {
                return Err(invalid(id));
            }
        }
        Ok(Some((agent.value(), operation)))
    }
    /// Only settled file rewinds in this exact account, thread and workspace may release backups.
    pub fn settled_file_rewinds(tx: &Transaction<'_>, agent: &Value) -> StoreResult<Vec<String>> {
        let id = agent
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("agent"))?;
        let source = crate::CheckpointsRepository::workspace_source(agent);
        let mut query = tx.prepare_cached("SELECT record FROM runtime_workspace_operations WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.kind')='claude_rollback' AND json_extract(record,'$.phase')='completed' AND json_extract(record,'$.restoreFiles')=1")?;
        let mut ids = Vec::new();
        for raw in query.query_map([id], |r| r.get::<_, String>(0))? {
            let op: Value = serde_json::from_str(&raw?)?;
            validate(&op)?;
            if op.get("source") == Some(&source) && op["result"]["filesRestored"] == true {
                ids.push(
                    op["requestId"]
                        .as_str()
                        .ok_or_else(|| invalid("requestId"))?
                        .to_owned(),
                );
            }
        }
        ids.sort();
        Ok(ids)
    }
    pub fn pending(tx: &Transaction<'_>, agent: &str) -> StoreResult<bool> {
        Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status IN ('pending','reserved','dispatching','uncertain'))",[agent],|r|r.get(0))?)
    }
    pub fn apply(
        tx: &Transaction<'_>,
        agent: &Value,
        operation: &Value,
        removed: &[String],
    ) -> StoreResult<()> {
        validate(operation)?;
        let actor = AgentRecord::from_value(agent.clone())?;
        let id = operation
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("operation"))?;
        if operation.get("agent") != agent.get("id") {
            return Err(invalid(id));
        }
        if !removed.is_empty() {
            use crate::{RecordOrder, RecordPredicate, Table, TranscriptItemRecord, truthy};
            let turns: std::collections::BTreeSet<_> = removed.iter().map(String::as_str).collect();
            for item in Table::<TranscriptItemRecord>::new(tx).list_matching(
                Some(RecordPredicate::PhysicalAgentEquals(&actor.id.0)),
                RecordOrder::RowId,
            )? {
                let mut value = item.value().clone();
                if value
                    .get("turnId")
                    .and_then(Value::as_str)
                    .is_some_and(|turn| turns.contains(turn))
                    && !truthy(value.get("afterRestore"))
                {
                    value["afterRestore"] = serde_json::json!(id);
                    if !Table::<TranscriptItemRecord>::new(tx)
                        .update(&TranscriptItemRecord::from_value(value)?)?
                    {
                        return Err(invalid("transcript"));
                    }
                }
            }
        }
        AgentRepository::put(tx, &actor)?;
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,operation.to_string()])?;
        Ok(())
    }
}
fn validate(record: &Value) -> StoreResult<()> {
    if !studio_runtime_schema::validate("PersistedClaudeControlRecord", record)
        .map_err(|_| invalid("operation"))?
    {
        return Err(invalid("operation"));
    }
    Ok(())
}
fn invalid(id: &str) -> StoreError {
    StoreError::InvalidStoredRecord {
        object: "claude control",
        id: id.into(),
    }
}

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[
        crate::schema::TableRequirement {
            name: "runtime_agents",
            columns: &["id", "record"],
        },
        crate::schema::TableRequirement {
            name: "runtime_workspace_operations",
            columns: &["id", "record"],
        },
        crate::schema::TableRequirement {
            name: "runtime_events",
            columns: &["agent", "status"],
        },
        crate::schema::TableRequirement {
            name: "runtime_items",
            columns: &["id", "agent", "record"],
        },
    ],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! { crate::repositories::SchemaRegistration { repository: "claude_controls", requirements: &REQUIREMENTS } }
