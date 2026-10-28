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

fn capture_bytes(used: &mut usize, value: &Value, maximum_bytes: usize) -> StoreResult<()> {
    *used = used
        .checked_add(serde_json::to_vec(value)?.len())
        .filter(|total| *total <= maximum_bytes)
        .ok_or(StoreError::RecordContract("context capture byte bound"))?;
    Ok(())
}

impl ContextRepairRepository {
    /// Check the original attempt and native turn before dispatching the fork.
    pub fn validate_terminal_owner(tx: &Transaction<'_>, repair: &Value) -> StoreResult<bool> {
        if repair["nativeTerminalReceipt"]["turnId"].is_null() {
            return Ok(true);
        }
        Ok(tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_execution_attempts a JOIN runtime_execution_runs r ON r.id=a.run WHERE a.id=?1 AND json_extract(a.record,'$.epoch')=?2 AND r.epoch=?2 AND r.agent=?3 AND r.account=?4 AND r.thread=?5 AND r.turn=?6 AND json_type(a.record,'$.terminalRepair') IS NULL)",
            params![repair["startAttemptSnapshot"]["id"].as_str(), repair["startAttemptSnapshot"]["epoch"].as_i64(), repair["source"]["id"].as_str(), repair["source"]["accountKey"].as_str(), repair["source"]["threadId"].as_str(), repair["nativeTerminalReceipt"]["turnId"].as_str()],
            |row| row.get(0),
        )?)
    }

    /// Long-lived callback authority belongs to the original execution attempt.
    pub fn save_terminal_provenance(
        tx: &Transaction<'_>,
        attempt: &str,
        record: &Value,
    ) -> StoreResult<()> {
        let changed = tx.execute(
            "UPDATE runtime_execution_attempts SET record=json_set(record,'$.terminalRepair',json(?1)) WHERE id=?2 AND json_extract(record,'$.epoch')=?3 AND run IN (SELECT id FROM runtime_execution_runs WHERE agent=?4 AND account=?5 AND thread=?6 AND epoch=?3 AND turn=?7) AND (json_type(record,'$.terminalRepair') IS NULL OR json_extract(record,'$.terminalRepair')=json(?1))",
            params![record.to_string(), attempt, record["startAttemptSnapshot"]["epoch"].as_i64(),record["source"]["id"].as_str(),record["source"]["accountKey"].as_str(),record["source"]["threadId"].as_str(),record["proof"]["turnId"].as_str()],
        )?;
        if changed != 1 {
            return Err(StoreError::RecordContract(
                "context terminal provenance changed",
            ));
        }
        Ok(())
    }

    pub fn terminal_provenance(
        db: &Connection,
        account: &str,
        thread: &str,
        turn: &str,
    ) -> StoreResult<Option<Value>> {
        db.query_row(
            "SELECT json_extract(record,'$.terminalRepair') FROM runtime_execution_attempts WHERE json_type(record,'$.terminalRepair')='object' AND json_extract(record,'$.terminalRepair.source.accountKey')=?1 AND json_extract(record,'$.terminalRepair.source.threadId')=?2 AND json_extract(record,'$.terminalRepair.proof.turnId')=?3",
            params![account, thread, turn], |row| row.get::<_,String>(0),
        ).optional()?.map(|raw| serde_json::from_str(&raw).map_err(StoreError::from)).transpose()
    }

    pub fn agent(db: &Connection, id: &str) -> StoreResult<Option<Value>> {
        Ok(AgentRepository::get(db, &AgentId::from(id))?.map(|a| a.value()))
    }
    pub fn capture(
        db: &Connection,
        id: &str,
        maximum_bytes: usize,
    ) -> StoreResult<Option<(Value, Vec<Value>)>> {
        let Some(agent) = Self::agent(db, id)? else {
            return Ok(None);
        };
        let mut used_bytes = 0;
        capture_bytes(&mut used_bytes, &agent, maximum_bytes)?;
        let mut query = db.prepare_cached("SELECT e.id,e.agent,e.epoch,e.kind,CASE WHEN length(CAST(e.text AS BLOB))<=65536 THEN e.text ELSE NULL END,e.status,e.created,e.turn_id,e.error,CASE WHEN length(CAST(m.record AS BLOB))<=65536 THEN m.record WHEN m.record IS NULL THEN NULL ELSE 'invalid-oversize' END FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id WHERE e.agent=?1 AND e.epoch=?2 AND e.status IN ('reserved','dispatching','uncertain') ORDER BY e.id")?;
        let rows = query.query_map(params![id, agent["epoch"].as_i64()], |row| {
            Ok((json!({"id": row.get::<_,String>(0)?, "agent": row.get::<_,String>(1)?, "epoch": row.get::<_,i64>(2)?, "kind": row.get::<_,String>(3)?, "text": row.get::<_,String>(4)?, "status": row.get::<_,String>(5)?, "created": row.get::<_,f64>(6)?, "turn_id": row.get::<_,Option<String>>(7)?, "error": row.get::<_,Option<String>>(8)?}), row.get::<_,Option<String>>(9)?))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (mut event, metadata) = row?;
            event["metadata"] = metadata
                .map(|text| serde_json::from_str(&text))
                .transpose()?
                .unwrap_or(Value::Null);
            capture_bytes(&mut used_bytes, &event, maximum_bytes)?;
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
    pub fn pending(
        db: &Connection,
        agent: &Value,
        maximum_bytes: usize,
    ) -> StoreResult<Option<Vec<Value>>> {
        let Some(ids) = agent["startAttempt"]["events"].as_array() else {
            return Ok(None);
        };
        let mut result = Vec::new();
        let mut used_bytes = 0;
        for id in ids {
            let row: Option<String> = db.query_row("SELECT json_object('id',id,'agent',agent,'kind',kind,'text',text,'status',status,'created',created,'epoch',epoch,'turn_id',turn_id,'error',error) FROM runtime_events WHERE id=?1 AND agent=?2 AND epoch=?3 AND status='pending' AND turn_id IS NULL AND length(CAST(text AS BLOB))<=65536", params![id.as_str(),agent["id"].as_str(),agent["epoch"].as_i64()], |row| row.get(0)).optional()?;
            let Some(row) = row else {
                return Ok(None);
            };
            let row = serde_json::from_str(&row)?;
            capture_bytes(&mut used_bytes, &row, maximum_bytes)?;
            result.push(row);
        }
        Ok(Some(result))
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_provenance_remains_on_its_exact_execution_owner() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,thread TEXT,epoch INTEGER,turn TEXT);
            INSERT INTO runtime_execution_runs VALUES('run','agent','account','source',2,'turn');
            CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
            INSERT INTO runtime_execution_attempts VALUES('attempt','run','{\"id\":\"attempt\",\"epoch\":2}');").expect("owners");
        let tx = db.transaction().expect("transaction");
        let proof = json!({"operationId":"repair","source":{"id":"agent","accountKey":"account","threadId":"source","epoch":2},"proof":{"turnId":"turn","threadId":"source","status":"completed"},"startAttemptSnapshot":{"id":"attempt","epoch":2},"connectionId":"old-connection","connectionGeneration":7,"newThreadId":"destination","phase":"completed"});
        ContextRepairRepository::save_terminal_provenance(&tx, "attempt", &proof)
            .expect("save authority");
        ContextRepairRepository::save_terminal_provenance(&tx, "attempt", &proof)
            .expect("same authority");
        assert_eq!(
            ContextRepairRepository::terminal_provenance(&tx, "account", "source", "turn")
                .expect("lookup"),
            Some(proof.clone())
        );
        assert!(
            ContextRepairRepository::terminal_provenance(&tx, "other-account", "source", "turn")
                .expect("other scope")
                .is_none()
        );
        let mut changed = proof.clone();
        changed["newThreadId"] = json!("another-destination");
        assert!(
            ContextRepairRepository::save_terminal_provenance(&tx, "attempt", &changed).is_err()
        );
        let mut changed = proof;
        changed["source"]["id"] = json!("another-agent");
        assert!(
            ContextRepairRepository::save_terminal_provenance(&tx, "attempt", &changed).is_err()
        );
    }
}
