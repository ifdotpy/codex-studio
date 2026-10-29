//! Bounded continuation snapshots and atomic row writes on the runtime's existing writer.
use crate::{AgentId, AgentRecord, AgentRepository, StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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
const REFERENCE_BATCH_SIZE: usize = 256;

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

fn objects_by_ids(
    db: &Connection,
    table: &'static str,
    ids: &BTreeSet<String>,
) -> StoreResult<BTreeMap<String, Value>> {
    let sql =
        format!("SELECT id,record FROM {table} WHERE id IN (SELECT value FROM json_each(?1))");
    let mut objects = BTreeMap::new();
    for ids in ids.iter().collect::<Vec<_>>().chunks(REFERENCE_BATCH_SIZE) {
        let encoded_ids = serde_json::to_string(ids)?;
        let mut statement = db.prepare_cached(&sql)?;
        let rows = statement.query_map([encoded_ids], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, record) = row?;
            let record =
                serde_json::from_str(&record).map_err(|_| StoreError::InvalidStoredObject {
                    table,
                    id: id.clone(),
                })?;
            objects.insert(id, record);
        }
    }
    Ok(objects)
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
        let mut statement=db.prepare_cached("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status IN ('pending','reserved','dispatching','uncertain') ORDER BY created,rowid LIMIT 4097")?;
        let events=statement.query_map(params![agent_id,agent.get("epoch").and_then(Value::as_i64)],|r|Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"text":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"created":r.get::<_,f64>(5)?,"epoch":r.get::<_,i64>(6)?,"turn_id":r.get::<_,Option<String>>(7)?,"error":r.get::<_,Option<String>>(8)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let mut metadata = serde_json::Map::new();
        let event_ids = events
            .iter()
            .filter_map(|event| event.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let event_metadata = objects_by_ids(db, "runtime_event_meta", &event_ids)?;
        let mut asset_ids = BTreeSet::new();
        let mut complaint_ids = BTreeSet::new();
        let mut work_ids = BTreeSet::new();
        for event in &events {
            let id = event["id"]
                .as_str()
                .ok_or(StoreError::RecordContract("event id"))?;
            let meta = event_metadata.get(id);
            if let Some(ids) = meta
                .as_ref()
                .and_then(|m| m.get("assets"))
                .and_then(Value::as_array)
            {
                asset_ids.extend(ids.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            metadata.insert(id.into(), meta.cloned().unwrap_or(Value::Null));
            if let Some(references) =
                studio_runtime_schema::notification_text::continuation_references(
                    event["text"].as_str().unwrap_or(""),
                )
            {
                complaint_ids.extend(references.complaints);
                if let Some(id) = references.task {
                    work_ids.insert(id);
                }
            }
        }
        let assets = objects_by_ids(db, "runtime_assets", &asset_ids)?;
        let complaints = objects_by_ids(db, "runtime_complaints", &complaint_ids)?;
        let work = objects_by_ids(db, "runtime_work", &work_ids)?;
        let mut statement=db.prepare_cached("SELECT id FROM runtime_work WHERE json_extract(record,'$.owner')=?1 AND json_extract(record,'$.rootId')=?2 AND json_extract(record,'$.status') NOT IN ('accepted','cancelled') ORDER BY id LIMIT 4097")?;
        let claims = statement
            .query_map(
                params![agent_id, agent.get("rootId").and_then(Value::as_str)],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(
            json!({"agent":agent,"ancestors":ancestors,"claims":claims,"record":record,"events":events,"metadata":metadata,"assets":assets,"complaints":complaints,"work":work}),
        )
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
    pub fn put_event(db: &Connection, r: &Value) -> StoreResult<()> {
        let count = db.execute(
            "UPDATE runtime_events SET status=?1,error=?2 WHERE id=?3 AND agent=?4 AND epoch=?5",
            params![
                r["status"].as_str(),
                r["error"].as_str(),
                r["id"].as_str(),
                r["agent"].as_str(),
                r["epoch"].as_i64()
            ],
        )?;
        if count != 1 {
            return Err(StoreError::RecordContract("missing continuation event"));
        }
        Ok(())
    }
    pub fn put_metadata(db: &Connection, id: &str, r: &Value) -> StoreResult<()> {
        db.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,serde_json::to_string(r)?])?;
        Ok(())
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
}

#[cfg(test)]
mod reference_tests {
    use super::*;

    #[test]
    fn snapshot_prefetches_independent_partial_notification_references() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("canvas.sqlite3")).unwrap();
        db.execute_batch(&crate::agent_volatile::TABLE_SQL).unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT,record TEXT);\
            CREATE TABLE runtime_events(id TEXT,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);\
            CREATE TABLE runtime_event_meta(id TEXT,record TEXT);\
            CREATE TABLE runtime_assets(id TEXT,record TEXT);\
            CREATE TABLE runtime_complaints(id TEXT,record TEXT);\
            CREATE TABLE runtime_work(id TEXT,record TEXT);").unwrap();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [r#"{"id":"a","epoch":1,"rootId":"root"}"#],
        )
        .unwrap();
        for (id, text) in [
            (
                "one",
                r#"{"task":"old","task":"t","complaints":[{"complaint_id":"c"},null,{}, {"complaint_id":""},{"complaint_id":4}]}"#,
            ),
            (
                "two",
                r#"{"task":null,"complaints":[{"complaint_id":"c"},{"complaint_id":"other","complaint_id":"d"}]}"#,
            ),
            (
                "three",
                r#"{"task":"ignored","complaints":[{"complaint_id":"bad"}],"extra":1e999}"#,
            ),
            ("four", "[1,2]"),
            ("five", "{"),
        ] {
            db.execute(
                "INSERT INTO runtime_events VALUES(?1,'a','message',?2,'pending',1,1,NULL,NULL)",
                params![id, text],
            )
            .unwrap();
        }
        for (table, ids) in [
            ("runtime_complaints", vec!["", "c", "d", "bad"]),
            ("runtime_work", vec!["t", "ignored"]),
        ] {
            for id in ids {
                db.execute(
                    &format!("INSERT INTO {table} VALUES(?1,?2)"),
                    params![id, json!({"id":id}).to_string()],
                )
                .unwrap();
            }
        }
        let result =
            ContinuationRepository::snapshot(&db, "a", ContinuationTable::Usage, None).unwrap();
        assert_eq!(
            result["complaints"],
            json!({"":{"id":""},"c":{"id":"c"},"d":{"id":"d"}})
        );
        assert_eq!(result["work"], json!({"t":{"id":"t"}}));
        assert_eq!(
            result["events"][0]["text"],
            r#"{"task":"old","task":"t","complaints":[{"complaint_id":"c"},null,{}, {"complaint_id":""},{"complaint_id":4}]}"#
        );
        assert_eq!(result["events"].as_array().unwrap().len(), 5);
        db.execute(
            "INSERT INTO runtime_event_meta VALUES('one',?1)",
            [r#"{"assets":["asset"]}"#],
        )
        .unwrap();
        db.execute("INSERT INTO runtime_assets VALUES('asset','{')", [])
            .unwrap();
        db.execute("UPDATE runtime_complaints SET record='{' WHERE id='c'", [])
            .unwrap();
        db.execute("UPDATE runtime_work SET record='{' WHERE id='t'", [])
            .unwrap();
        for (table, id, expected) in [
            ("runtime_assets", "asset", "runtime_assets"),
            ("runtime_complaints", "c", "runtime_complaints"),
            ("runtime_work", "t", "runtime_work"),
        ] {
            assert!(
                matches!(ContinuationRepository::snapshot(&db, "a", ContinuationTable::Usage, None), Err(StoreError::InvalidStoredObject { table, .. }) if table == expected)
            );
            db.execute(
                &format!("UPDATE {table} SET record='{{}}' WHERE id=?1"),
                [id],
            )
            .unwrap();
        }
    }
}
