//! Bounded receipt snapshots and atomic positive delivery updates.
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{AgentId, AgentRecord, AgentRepository, StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[
        TableRequirement {
            name: "runtime_agents",
            columns: &["id", "record"],
        },
        TableRequirement {
            name: "runtime_events",
            columns: &[
                "id", "agent", "epoch", "kind", "text", "status", "created", "turn_id", "error",
            ],
        },
    ],
    conditional_tables: &[
        ConditionalTableRequirement {
            table_exists: "runtime_items",
            tables: &[TableRequirement {
                name: "runtime_items",
                columns: &["id", "agent", "created", "record"],
            }],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_item_fulltext",
            tables: &[TableRequirement {
                name: "runtime_item_fulltext",
                columns: &["id", "body"],
            }],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_event_meta",
            tables: &[TableRequirement {
                name: "runtime_event_meta",
                columns: &["id", "record"],
            }],
        },
        ConditionalTableRequirement {
            table_exists: "runtime_completed_turns",
            tables: &[TableRequirement {
                name: "runtime_completed_turns",
                columns: &["id"],
            }],
        },
    ],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

pub struct ContextRepairRepository;
impl ContextRepairRepository {
    pub fn agent(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Ok(AgentRepository::get(db, &AgentId::from(id))?.map(|a| a.value()))
    }
    pub fn capture(
        db: &Connection,
        id: &str,
        limit: usize,
    ) -> StoreResult<Option<(Value, Vec<Value>)>> {
        let Some(agent) = Self::agent(db, id)? else {
            return Ok(None);
        };
        let mut query = db.prepare("SELECT e.id,e.agent,e.epoch,e.kind,CASE WHEN length(CAST(e.text AS BLOB))<=65536 THEN e.text ELSE NULL END,e.status,e.created,e.turn_id,e.error,CASE WHEN length(CAST(m.record AS BLOB))<=65536 THEN m.record WHEN m.record IS NULL THEN NULL ELSE 'invalid-oversize' END FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id WHERE e.agent=?1 AND e.epoch=?2 AND e.status IN ('reserved','dispatching','uncertain') ORDER BY e.id LIMIT ?3")?;
        let rows = query.query_map(params![id, agent["epoch"].as_i64(), i64::try_from(limit).map_err(|_| StoreError::RecordContract("capture bound"))?], |row| {
            Ok((json!({"id": row.get::<_,String>(0)?, "agent": row.get::<_,String>(1)?, "epoch": row.get::<_,i64>(2)?, "kind": row.get::<_,String>(3)?, "text": row.get::<_,String>(4)?, "status": row.get::<_,String>(5)?, "created": row.get::<_,f64>(6)?, "turn_id": row.get::<_,Option<String>>(7)?, "error": row.get::<_,Option<String>>(8)?}), row.get::<_,Option<String>>(9)?))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (mut event, metadata) = row?;
            event["metadata"] = metadata
                .map(|text| serde_json::from_str(&text))
                .transpose()?
                .unwrap_or(Value::Null);
            events.push(event);
        }
        Ok(Some((agent, events)))
    }
    pub fn deliver(
        tx: &Transaction<'_>,
        id: &str,
        agent: &str,
        epoch: i64,
        turn: &str,
    ) -> StoreResult<()> {
        let changed = tx.execute("UPDATE runtime_events SET status='delivered',turn_id=?1,error=NULL WHERE id=?2 AND agent=?3 AND epoch=?4 AND status IN ('reserved','dispatching','uncertain')", params![turn,id,agent,epoch])?;
        if changed != 1 {
            return Err(StoreError::RecordContract("context input changed"));
        }
        Ok(())
    }
    pub fn requeue(tx: &Transaction<'_>, id: &str, agent: &str, epoch: i64) -> StoreResult<()> {
        tx.execute("UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL WHERE id=?1 AND agent=?2 AND epoch=?3 AND status IN ('reserved','dispatching','uncertain')", params![id,agent,epoch])?;
        Ok(())
    }
    pub fn pending(db: &Connection, agent: &Value) -> StoreResult<Option<Vec<Value>>> {
        let Some(ids) = agent["startAttempt"]["events"]
            .as_array()
            .filter(|ids| ids.len() <= 32)
        else {
            return Ok(None);
        };
        let mut result = Vec::new();
        for id in ids {
            let row: Option<String> = db.query_row("SELECT json_object('id',id,'agent',agent,'kind',kind,'text',text,'status',status,'created',created,'epoch',epoch,'turn_id',turn_id,'error',error) FROM runtime_events WHERE id=?1 AND agent=?2 AND epoch=?3 AND status='pending' AND turn_id IS NULL AND length(CAST(text AS BLOB))<=65536", params![id.as_str(),agent["id"].as_str(),agent["epoch"].as_i64()], |row| row.get(0)).optional()?;
            let Some(row) = row else {
                return Ok(None);
            };
            result.push(serde_json::from_str(&row)?);
        }
        Ok(Some(result))
    }
    pub fn batches(db: &Connection, agent: &Value, events: &[Value]) -> StoreResult<Vec<Value>> {
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for event in events {
            let Some(key) = event["metadata"]["transcriptItemId"].as_str() else {
                continue;
            };
            if !seen.insert(key) {
                continue;
            }
            let record: Option<(String,f64)> = db.query_row("SELECT record,created FROM runtime_items WHERE id=?1 AND agent=?2 AND length(CAST(record AS BLOB))<=65536", params![key,agent["id"].as_str()], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            let Some((record, created)) = record else {
                continue;
            };
            let item: Value = serde_json::from_str(&record)?;
            let Some(inputs) = item["inputs"]
                .as_array()
                .filter(|inputs| inputs.len() <= 32)
            else {
                continue;
            };
            let fulltext: Option<String> = db.query_row("SELECT body FROM runtime_item_fulltext WHERE id=?1 AND length(CAST(body AS BLOB))<=65536", [key], |row| row.get(0)).optional()?;
            let mut members = Vec::new();
            for input in inputs {
                let Some(id) = input["id"].as_str() else {
                    continue;
                };
                let member: Option<(String,String)> = db.query_row("SELECT json_object('id',e.id,'agent',e.agent,'epoch',e.epoch,'kind',e.kind,'text',e.text,'status',e.status,'created',e.created,'turn_id',e.turn_id,'error',e.error),m.record FROM runtime_events e JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?1 AND e.agent=?2 AND e.epoch=?3 AND length(CAST(e.text AS BLOB))<=65536 AND length(CAST(m.record AS BLOB))<=65536", params![id,agent["id"].as_str(),agent["epoch"].as_i64()], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
                if let Some((row, meta)) = member {
                    let mut row: Value = serde_json::from_str(&row)?;
                    row["metadata"] = serde_json::from_str(&meta)?;
                    members.push(row);
                }
            }
            result.push(json!({"item":item,"row":{"id":key,"agent":agent["id"],"created":created,"record":record},"fulltext":fulltext,"members":members}));
        }
        Ok(result)
    }
    pub fn completed(db: &Connection, agent: &str, turn: &str) -> StoreResult<bool> {
        Ok(db
            .query_row(
                "SELECT 1 FROM runtime_completed_turns WHERE id=?1",
                [format!("{agent}:{turn}")],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
    pub fn put_agent(db: &Connection, value: &Value) -> StoreResult<()> {
        let agent = AgentRecord::from_value(value.clone())?;
        AgentRepository::put(db, &agent)
    }
    pub fn has_candidates(db: &Connection, id: &str, epoch: i64) -> StoreResult<bool> {
        Ok(db.query_row("SELECT 1 FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id WHERE e.agent=?1 AND ((e.epoch=?2 AND e.status='uncertain') OR (e.status='delivered' AND e.turn_id IS NOT NULL AND e.kind IN ('monitor_exit','agent_message','work_review','work_decision') AND ((length(e.text)>3000 AND coalesce(json_extract(m.record,'$.modelEventProjection'),0)!=1) OR json_extract(m.record,'$.contextManifest.versions.roleSkill') IS NOT NULL))) LIMIT 1", params![id,epoch], |_| Ok(())).optional()?.is_some())
    }
}
inventory::submit! { crate::repositories::SchemaRegistration { repository: "context_repair", requirements: &REQUIREMENTS } }
