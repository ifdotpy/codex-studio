//! Frozen Claude start rows are loaded and saved under the caller's single transaction.
use crate::schema::SchemaRequirements;
use crate::{AgentId, AgentRepository, StoreError, StoreResult, Transaction};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub struct ClaudeRetryRecords {
    pub agent: Value,
    pub attempt: Value,
    pub events: Vec<Value>,
}
pub struct ClaudeRetryRepository;
impl ClaudeRetryRepository {
    pub fn load(
        tx: &Transaction<'_>,
        agent_id: &str,
        attempt_id: &str,
    ) -> StoreResult<Option<ClaudeRetryRecords>> {
        let Some(agent) = AgentRepository::get(tx, &AgentId(agent_id.into()))? else {
            return Ok(None);
        };
        let raw:Option<String>=tx.query_row("SELECT t.record FROM runtime_execution_attempts t JOIN runtime_execution_runs r ON r.id=t.run WHERE t.id=?1 AND r.agent=?2",params![attempt_id,agent_id],|r|r.get(0)).optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let attempt: Value = serde_json::from_str(&raw)?;
        validate_attempt(&attempt)?;
        if attempt.get("id").and_then(Value::as_str) != Some(attempt_id) {
            return Err(invalid(attempt_id));
        }
        let ids = attempt
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid(attempt_id))?;
        let mut events = Vec::new();
        for id in ids {
            let id = id.as_str().ok_or_else(|| invalid(attempt_id))?;
            let row:Option<Value>=tx.query_row("SELECT e.id,e.agent,e.epoch,e.kind,e.text,e.created,e.status,e.turn_id,e.error,COALESCE(m.record,'{}') FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?1",[id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"epoch":r.get::<_,i64>(2)?,"kind":r.get::<_,String>(3)?,"text":r.get::<_,String>(4)?,"created":r.get::<_,f64>(5)?,"status":r.get::<_,String>(6)?,"turn_id":r.get::<_,Option<String>>(7)?,"error":r.get::<_,Option<String>>(8)?,"metadata":r.get::<_,String>(9)?}))).optional()?;
            let Some(mut row) = row else {
                return Ok(None);
            };
            let encoded = row["metadata"]
                .as_str()
                .ok_or_else(|| invalid(attempt_id))?;
            row["metadata"] = serde_json::from_str(encoded)?;
            if !studio_runtime_schema::validate("RuntimeEventMetadataRecord", &row["metadata"])
                .map_err(|_| invalid(id))?
            {
                return Err(invalid(id));
            }
            events.push(row);
        }
        Ok(Some(ClaudeRetryRecords {
            agent: agent.value(),
            attempt,
            events,
        }))
    }
}
fn validate_attempt(record: &Value) -> StoreResult<()> {
    if !studio_runtime_schema::validate("PersistedClaudeAttemptRecord", record)
        .map_err(|_| invalid("attempt"))?
    {
        return Err(invalid("attempt"));
    }
    Ok(())
}
fn invalid(id: &str) -> StoreError {
    StoreError::InvalidStoredRecord {
        object: "claude retry",
        id: id.into(),
    }
}

impl ClaudeRetryRepository {
    pub fn replace_attempt(tx: &Transaction<'_>, agent: &str, attempt: &Value) -> StoreResult<()> {
        validate_attempt(attempt)?;
        let id = attempt
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("attempt"))?;
        let changed=tx.execute("UPDATE runtime_execution_attempts SET record=?1 WHERE id=?2 AND run IN (SELECT id FROM runtime_execution_runs WHERE agent=?3)",params![attempt.to_string(),id,agent])?;
        if changed != 1 {
            return Err(invalid(id));
        }
        Ok(())
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
            name: "runtime_execution_attempts",
            columns: &["id", "run", "record"],
        },
        crate::schema::TableRequirement {
            name: "runtime_execution_runs",
            columns: &["id", "agent"],
        },
        crate::schema::TableRequirement {
            name: "runtime_events",
            columns: &[
                "id", "agent", "epoch", "kind", "text", "created", "status", "turn_id", "error",
            ],
        },
        crate::schema::TableRequirement {
            name: "runtime_event_meta",
            columns: &["id", "record"],
        },
        crate::schema::TableRequirement {
            name: "runtime_items",
            columns: &["id", "agent", "created", "record"],
        },
        crate::schema::TableRequirement {
            name: "runtime_tasks",
            columns: &["id", "record"],
        },
        crate::schema::TableRequirement {
            name: "runtime_item_fulltext",
            columns: &["id", "body"],
        },
    ],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! { crate::repositories::SchemaRegistration { repository: "claude_retry", requirements: &REQUIREMENTS } }
