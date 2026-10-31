//! Bounded continuation snapshots and atomic row writes on the runtime's existing writer.
use crate::{AgentId, AgentRecord, AgentRepository, StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
#[derive(Clone, Copy, Debug)]
pub enum ContinuationTable {
    Usage,
    Capacity,
}
impl ContinuationTable {
    pub fn name(self) -> &'static str {
        match self {
            Self::Usage => "runtime_usage_resumes",
            Self::Capacity => "runtime_capacity_retries",
        }
    }
}
pub struct ContinuationRepository;

fn raw(db: &Connection, table: &'static str, id: &str) -> StoreResult<Option<Value>> {
    let text = db
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    text.map(|s| {
        let mut value = serde_json::from_str(&s).map_err(|_| StoreError::InvalidStoredObject {
            table,
            id: id.into(),
        })?;
        crate::agent_volatile::merge_if_agent(db, table, id, &mut value)?;
        Ok(value)
    })
    .transpose()
}

impl ContinuationRepository {
    pub fn snapshot(
        db: &Connection,
        agent_id: &str,
        table: ContinuationTable,
        record_id: Option<&str>,
    ) -> StoreResult<Value> {
        let agent = raw(db, "runtime_agents", agent_id)?.ok_or_else(|| {
            StoreError::InvalidStoredObject {
                table: "runtime_agents",
                id: agent_id.into(),
            }
        })?;
        if agent.get("id").and_then(Value::as_str) != Some(agent_id) {
            return Err(StoreError::InvalidStoredObject {
                table: "runtime_agents",
                id: agent_id.into(),
            });
        }
        let mut ancestors = Vec::new();
        let mut parent = agent
            .get("parentId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        while let Some(id) = parent {
            if ancestors.len() >= 64 {
                return Err(StoreError::SchemaMismatch(
                    "continuation ancestor bound exceeded".into(),
                ));
            }
            let a =
                raw(db, "runtime_agents", &id)?.ok_or_else(|| StoreError::InvalidStoredObject {
                    table: "runtime_agents",
                    id: id.clone(),
                })?;
            if a.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(StoreError::InvalidStoredObject {
                    table: "runtime_agents",
                    id,
                });
            }
            parent = a.get("parentId").and_then(Value::as_str).map(str::to_owned);
            ancestors.push(a);
            if parent.as_ref().is_some_and(|id| {
                id == agent_id
                    || ancestors
                        .iter()
                        .any(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
            }) {
                break;
            }
        }
        let record = if let Some(id) = record_id {
            let row = db
                .query_row(
                    &format!(
                        "SELECT record FROM {} WHERE id=?1 AND agent=?2",
                        table.name()
                    ),
                    params![id, agent_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            let invalid = || StoreError::InvalidStoredObject {
                table: table.name(),
                id: id.into(),
            };
            Some(serde_json::from_str::<Value>(&row.ok_or_else(invalid)?).map_err(|_| invalid())?)
        } else {
            None
        };
        let mut statement=db.prepare_cached("SELECT id FROM runtime_work WHERE json_extract(record,'$.owner')=?1 AND json_extract(record,'$.rootId')=?2 AND json_extract(record,'$.status') NOT IN ('accepted','cancelled') ORDER BY id LIMIT 4097")?;
        let claims = statement
            .query_map(
                params![agent_id, agent.get("rootId").and_then(Value::as_str)],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(json!({"agent":agent,"ancestors":ancestors,"claims":claims,"record":record}))
    }
    pub fn put_record(
        db: &Connection,
        table: ContinuationTable,
        agent: &str,
        r: &Value,
    ) -> StoreResult<()> {
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .ok_or(StoreError::RecordContract("continuation id"))?;
        Self::put_serialized_record(db, table, agent, id, r)
    }

    /// Persist a continuation that has already passed the decision snapshot boundary.
    pub fn put_checked_record(
        db: &Connection,
        table: ContinuationTable,
        agent: &str,
        record: &studio_runtime_schema::continuation_snapshot::ContinuationRecord,
    ) -> StoreResult<()> {
        Self::put_serialized_record(db, table, agent, record.id(), record)
    }

    fn put_serialized_record(
        db: &Connection,
        table: ContinuationTable,
        agent: &str,
        id: &str,
        record: &impl serde::Serialize,
    ) -> StoreResult<()> {
        let owner: Option<String> = db
            .query_row(
                &format!("SELECT agent FROM {} WHERE id=?1", table.name()),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if owner.as_ref().is_some_and(|owner| owner != agent) {
            return Err(StoreError::InvalidStoredObject {
                table: table.name(),
                id: id.into(),
            });
        }
        db.execute(&format!("INSERT INTO {}(id,agent,record) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET record=excluded.record",table.name()),params![id,agent,serde_json::to_string(record)?])?;
        Ok(())
    }
    pub fn put_agent(db: &Connection, r: &Value) -> StoreResult<()> {
        let agent: AgentRecord = serde_json::from_value(r.clone())?;
        Self::put_checked_agent(db, &agent)
    }
    pub fn put_checked_agent(db: &Connection, agent: &AgentRecord) -> StoreResult<()> {
        if AgentRepository::get(db, &AgentId::from(agent.id.0.as_str()))?.is_none() {
            return Err(StoreError::RecordContract("missing continuation agent"));
        }
        AgentRepository::put(db, agent)
    }
}

/// State-check projection; the schema validates all fields used by the decision core.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CapacityContinuationRecord {
    id: String,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, Value>,
}
impl crate::JsonRecord for CapacityContinuationRecord {
    type Id = String;
    const TABLE: &'static str = "runtime_capacity_retries";
    const SCHEMA: &'static str = "continuation/CapacityContinuation";
    const KEY_MODE: crate::RecordKeyMode = crate::RecordKeyMode::PayloadId;
    const QUERY_SHAPE: crate::RecordQueryShape = crate::RecordQueryShape::PhysicalAgent;
    fn record_key(&self) -> Option<&String> {
        Some(&self.id)
    }
    // Remove this override once the record is closed and typed.
    fn encode_stored(&self, _: &rusqlite::Connection, row_id: &str) -> crate::StoreResult<String> {
        crate::records::encode_open_record(self, row_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_does_not_read_unrelated_queue_data() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(&crate::agent_volatile::TABLE_SQL).unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT,record TEXT); CREATE TABLE runtime_work(id TEXT,record TEXT);").unwrap();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [r#"{"id":"a","epoch":1,"rootId":"a"}"#],
        )
        .unwrap();
        let snapshot =
            ContinuationRepository::snapshot(&db, "a", ContinuationTable::Usage, None).unwrap();
        assert_eq!(
            snapshot,
            json!({"agent":{"id":"a","epoch":1,"rootId":"a"},"ancestors":[],"claims":[],"record":null})
        );
    }
}
