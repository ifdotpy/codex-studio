//! Narrow persistence for workspace completion receipts, lineage and recovery writes.
use crate::{AgentRecord, AgentRepository, CheckpointsRepository, StoreError, StoreResult};
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
    pub fn operation(connection: &Connection, id: &str) -> StoreResult<Option<Value>> {
        let text: Option<String> = connection
            .query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|s| serde_json::from_str(&s).map_err(StoreError::from))
            .transpose()
    }
    pub fn active_operations(connection: &Connection) -> StoreResult<Vec<Value>> {
        let mut query = connection.prepare("SELECT record FROM runtime_workspace_operations WHERE json_extract(record,'$.phase') IS NULL OR json_extract(record,'$.phase') NOT IN ('completed','failed') ORDER BY id")?;
        query
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|r| serde_json::from_str(&r?).map_err(StoreError::from))
            .collect()
    }
    pub fn history(
        connection: &Connection,
        agent: &str,
        checkpoint: &str,
    ) -> StoreResult<HistoryRows> {
        let mut query = connection.prepare(
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
            .prepare("SELECT id,created FROM runtime_items WHERE agent=?1 ORDER BY rowid")?;
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
    pub fn put_operation(tx: &Transaction<'_>, operation: &Value) -> StoreResult<()> {
        let record = crate::WorkspaceOperationRecord::from_value(operation.clone())?;
        tx.execute(
            "UPDATE runtime_workspace_operations SET record=?1 WHERE id=?2",
            params![record.value().to_string(), record.id()],
        )?;
        Ok(())
    }
    /// Read, decide and apply on this same writer transaction; no external effects.
    pub fn apply_recovery(
        tx: &Transaction<'_>,
        operation: &Value,
        agent: &Value,
    ) -> StoreResult<()> {
        Self::put_operation(tx, operation)?;
        AgentRepository::put(tx, &AgentRecord::from_value(agent.clone())?)
    }
}
