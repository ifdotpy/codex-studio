use super::*;
pub(super) fn authorize(tx: &rusqlite::Transaction<'_>, call: &Call) -> Result<bool, WorkerError> {
    let Some(agent) = AgentRepository::get(tx, &AgentId::from(call.actor.agent.as_str()))? else {
        return Ok(false);
    };
    Ok(agent.epoch == call.actor.epoch
        && agent
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            == call.scope.account
        && agent.extra.get("deleted") != Some(&json!(true))
        && call.params["threadId"].as_str().is_some_and(|thread| {
            agent.extra.get("threadId").and_then(Value::as_str) == Some(thread)
        }))
}
pub(super) fn replay(
    s: &RuntimeStore,
    call: &Call,
) -> Result<Result<Option<SessionReceipt>, PortError>, WorkerError> {
    s.transaction_with(|tx| {
        if !authorize(tx, call)? {
            return Ok(Err(PortError::Identity));
        }
        let Some(prior) = store::receipt(tx, &call.key(), "submitted")? else {
            return Ok(Ok(None));
        };
        if prior["call"] != serde_json::to_value(call).map_err(|_| WorkerError::Capacity)? {
            return Ok(Err(PortError::Identity));
        }
        let request = store::decode(&prior["request"])?;
        let saved = store::receipt(tx, &call.key(), "result")?;
        let result = match saved {
            Some(saved)
                if saved["cursor"].as_u64().unwrap_or(u64::MAX)
                    <= store::checkpoint(tx, &call.scope.account, call.scope.generation)? =>
            {
                serde_json::from_value(saved["result"].clone())
                    .map_err(|_| WorkerError::Capacity)?
            }
            _ => RequestResult::Unknown,
        };
        Ok(Ok(Some(call.receipt(request.ticket, result))))
    })
}
pub(super) fn submitted(
    s: &RuntimeStore,
    call: &Call,
    request: &RecoveredRequest,
) -> Result<Result<(), PortError>, WorkerError> {
    scheduler::lifecycle_transaction_with(s, |tx| {
        if !authorize(tx, call)? {
            return Ok(Err(PortError::Identity));
        }
        store::save(
            tx,
            &call.key(),
            "submitted",
            json!({"call":call,"request":store::record(request)}),
        )?;
        Ok(Ok(()))
    })
}
pub(in crate::account_worker) fn recover(
    s: &RuntimeStore,
    account: &str,
    generation: u64,
) -> Result<Vec<(PendingRequest, RecoveredRequest)>, WorkerError> {
    s.transaction_with(|tx| {
        let prefix = format!("account-worker:retained:{account}:");
        let checkpoint = store::checkpoint(tx, account, generation)?;
        let mut statement = tx.prepare("SELECT r.result FROM runtime_operation_receipts r WHERE r.id>=?1 AND r.id<?2 AND substr(r.id,-10)=':submitted' AND json_extract(r.result,'$.call.scope.generation')=?3 AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':result' AND json_extract(done.result,'$.result') != 'Unknown' AND json_extract(done.result,'$.cursor') <=?4) ORDER BY r.id LIMIT 33")?;
        let rows = statement.query_map(rusqlite::params![prefix, format!("{prefix}~"), i64::try_from(generation).map_err(|_|WorkerError::Capacity)?, i64::try_from(checkpoint).map_err(|_|WorkerError::Capacity)?], |row|row.get::<_,String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            let value: Value = serde_json::from_str(&row?).map_err(|_|WorkerError::Capacity)?;
            let call: Call = serde_json::from_value(value["call"].clone()).map_err(|_|WorkerError::Capacity)?;
            let checkpoint = store::checkpoint(tx, &call.scope.account, generation)?;
            if let Some(value) = store::receipt(tx, &call.key(), "result")?
                && value["result"] != json!("Unknown")
                && value["cursor"].as_u64().is_some_and(|c| c <= checkpoint) { continue; }
            let request = store::decode(&store::receipt(tx, &call.key(), "written")?.unwrap_or(value["request"].clone()))?;
            result.push((PendingRequest { call, ticket: request.ticket.clone(), reply: None, result: None, result_cursor: 0 }, request));
            if result.len() > MAX_PENDING_RESPONSES { return Err(WorkerError::Capacity); }
        }
        Ok(result)
    })
}
