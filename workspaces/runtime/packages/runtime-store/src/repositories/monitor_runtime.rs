//! Monitor transaction primitives; reuse typed tables and existing projections.

use crate::{MonitorRecord, QueueEvent, RequestRecord, StoreResult, Table};
use rusqlite::{Connection, OptionalExtension, Transaction};

pub fn pending_approvals(
    connection: &Connection,
    monitor_id: &str,
) -> StoreResult<Vec<RequestRecord>> {
    // The unsettled-request partial index keeps this lookup off the request history.
    let mut query = connection.prepare_cached(&format!(
        "SELECT id FROM runtime_requests WHERE {} AND json_extract(record,'$.status')='pending' \
         AND json_extract(record,'$.method')='monitor/approve' \
         AND json_extract(record,'$.params.monitorId')=?1",
        crate::startup_recovery::unsettled_request_predicate()
    ))?;
    let ids = query
        .query_map([monitor_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .filter_map(|id| Table::<RequestRecord>::new(connection).get(&id).transpose())
        .collect()
}

pub fn put(transaction: &Transaction<'_>, value: serde_json::Value) -> StoreResult<()> {
    Table::<MonitorRecord>::new(transaction).put(&MonitorRecord::from_value(value)?)
}

pub fn running_ids(connection: &Connection) -> StoreResult<Vec<String>> {
    let mut query = connection.prepare_cached("SELECT id FROM runtime_monitors WHERE json_extract(record,'$.status')='running' ORDER BY id LIMIT 64")?;
    Ok(query
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn exit_event(connection: &Connection, id: &str) -> StoreResult<Option<QueueEvent>> {
    Ok(connection.query_row("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events WHERE id=?1", [id], |row| Ok(QueueEvent {
        id: crate::QueueEventId::from(row.get::<_, String>(0)?),
        agent: crate::AgentId::from(row.get::<_, String>(1)?),
        kind: row.get(2)?, text: row.get(3)?, status: row.get(4)?, created: row.get(5)?, epoch: row.get(6)?, turn_id: row.get(7)?, error: row.get(8)?,
    })).optional()?)
}

pub fn active_count(connection: &Connection) -> StoreResult<i64> {
    Ok(connection.query_row("SELECT COUNT(*) FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('running','starting','approval')", [], |row| row.get(0))?)
}
