//! Narrow persistence for workspace completion receipts, lineage and recovery writes.
use crate::{
    AgentRecord, AgentRepository, CheckpointsRepository, StoreError, StoreResult, Table,
    WorkspaceOperationRecord,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

pub const MAX_CHECKPOINT_LINEAGE: usize = 16_384;

pub struct HistoryRows {
    pub checkpoints: Vec<Value>,
    pub items: Vec<(String, f64)>,
}

pub struct WorkspaceOperationsRepository;
impl WorkspaceOperationsRepository {
    pub fn completed(
        connection: &Connection,
        agent: &str,
        turn: Option<&str>,
    ) -> StoreResult<bool> {
        let key = format!("{agent}:{}", turn.unwrap_or("None"));
        Ok(connection
            .query_row(
                "SELECT 1 FROM runtime_completed_turns WHERE id=?1",
                [key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
    pub fn operation(
        connection: &Connection,
        id: &str,
    ) -> StoreResult<Option<WorkspaceOperationRecord>> {
        Table::<WorkspaceOperationRecord>::new(connection).get(&id.to_owned())
    }
    pub fn active_operations(
        connection: &Connection,
    ) -> StoreResult<Vec<WorkspaceOperationRecord>> {
        let table = Table::<WorkspaceOperationRecord>::new(connection);
        let mut query = connection.prepare_cached("SELECT id,record FROM runtime_workspace_operations WHERE json_extract(record,'$.phase') IS NULL OR json_extract(record,'$.phase') NOT IN ('completed','failed') ORDER BY id")?;
        query
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .map(|r| {
                let (id, raw) = r?;
                table.decode_stored(&id, &raw)
            })
            .collect()
    }
    pub fn history(
        connection: &Connection,
        agent: &str,
        checkpoint: &str,
    ) -> StoreResult<HistoryRows> {
        let mut query = connection.prepare_cached(
            "WITH RECURSIVE lineage(id,record) AS (
               SELECT id,record FROM runtime_checkpoints WHERE id=?1
               UNION SELECT c.id,c.record FROM runtime_checkpoints c JOIN lineage p
                 ON c.id=json_extract(p.record,'$.historyParent')
             ) SELECT record FROM lineage LIMIT ?2",
        )?;
        let checkpoints = query
            .query_map(
                params![checkpoint, (MAX_CHECKPOINT_LINEAGE + 1) as i64],
                |r| r.get::<_, String>(0),
            )?
            .map(|r| serde_json::from_str(&r?).map_err(StoreError::from))
            .collect::<StoreResult<Vec<_>>>()?;
        let mut query = connection
            .prepare_cached("SELECT id,created FROM runtime_items WHERE agent=?1 ORDER BY rowid")?;
        let items = query
            .query_map([agent], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(HistoryRows { checkpoints, items })
    }
    pub fn checkpoint_error(tx: &Transaction<'_>, agent: &str, error: &str) -> StoreResult<()> {
        let Some(mut a) = CheckpointsRepository::agent(tx, agent)? else {
            return Ok(());
        };
        a.record["checkpointError"] = Value::String(error.into());
        AgentRepository::put(tx, &AgentRecord::from_value(a.record)?)
    }
    pub fn put_operation(
        tx: &Transaction<'_>,
        operation: &WorkspaceOperationRecord,
    ) -> StoreResult<()> {
        let changed = tx.execute(
            "UPDATE runtime_workspace_operations SET record=?1 WHERE id=?2",
            params![serde_json::to_string(operation)?, operation.id],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidStoredObject {
                table: "runtime_workspace_operations",
                id: operation.id.clone(),
            });
        }
        Ok(())
    }
    /// Read, decide and apply on this same writer transaction; no external effects.
    pub fn apply_recovery(
        tx: &Transaction<'_>,
        operation: &WorkspaceOperationRecord,
        agent: &Value,
    ) -> StoreResult<()> {
        Self::put_operation(tx, operation)?;
        AgentRepository::put(tx, &AgentRecord::from_value(agent.clone())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stored_operation_roundtrips_nullable_and_opaque_evidence() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .unwrap();
        let raw = json!({"id":"op","agent":"agent","kind":"branch","phase":"provider_pending","source":null,"provider":{"vendor":[null,3]},"result":null,"signature":null,"restartHold":null,"vendor":{"opaque":true}});
        db.execute(
            "INSERT INTO runtime_workspace_operations VALUES('op',?1)",
            [raw.to_string()],
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        let record = WorkspaceOperationsRepository::operation(&tx, "op")
            .unwrap()
            .unwrap();
        WorkspaceOperationsRepository::put_operation(&tx, &record).unwrap();
        let saved: String = tx
            .query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id='op'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved, raw);
        assert!(saved.get("unknownField").is_none());
    }

    #[test]
    fn operation_identity_and_writes_are_checked() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )
        .unwrap();
        let operation =
            json!({"id":"b","agent":"agent","kind":"capture","phase":"capture_pending"});
        db.execute(
            "INSERT INTO runtime_workspace_operations VALUES (?1,?2)",
            params!["a", operation.to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO runtime_workspace_operations VALUES (?1,?2)",
            params!["b", operation.to_string()],
        )
        .unwrap();
        db.execute_batch("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_agents VALUES ('agent', 'unchanged')").unwrap();
        assert!(WorkspaceOperationsRepository::operation(&db, "a").is_err());
        assert!(WorkspaceOperationsRepository::active_operations(&db).is_err());
        let b: String = db
            .query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id='b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let agent: String = db
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b, operation.to_string());
        assert_eq!(agent, "unchanged");
        db.execute("DELETE FROM runtime_workspace_operations WHERE id='b'", [])
            .unwrap();
        let tx = db.transaction().unwrap();
        let typed: WorkspaceOperationRecord = serde::Deserialize::deserialize(&operation).unwrap();
        assert!(
            WorkspaceOperationsRepository::apply_recovery(&tx, &typed, &json!({"id":"agent"}))
                .is_err()
        );
        assert!(
            <WorkspaceOperationRecord as serde::Deserialize>::deserialize(
                json!({"id":"a","phase":17})
            )
            .is_err()
        );
        let valid = json!({"id":"a","agent":"agent","kind":"capture","phase":"completed"});
        let typed: WorkspaceOperationRecord = serde::Deserialize::deserialize(&valid).unwrap();
        WorkspaceOperationsRepository::put_operation(&tx, &typed).unwrap();
        assert_eq!(
            json!(
                WorkspaceOperationsRepository::operation(&tx, "a")
                    .unwrap()
                    .unwrap()
            ),
            valid
        );
    }
}
