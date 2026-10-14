use super::*;
pub(super) fn authorize(tx: &rusqlite::Transaction<'_>, call: &Call) -> Result<bool, WorkerError> {
    let Some(actor) = call.actor.as_ref() else {
        return Ok(match call.method.as_str() {
            "thread/list" => call.params.get("threadId").is_none(),
            "thread/loaded/list" | "model/list" | "claude/diagnostics" => true,
            "thread/read" => {
                call.params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
                    && call
                        .params
                        .get("includeTurns")
                        .is_none_or(|value| value == &json!(false))
            }
            "thread/turns/list" => {
                call.params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty())
                    && call
                        .params
                        .get("limit")
                        .and_then(Value::as_u64)
                        .is_some_and(|limit| limit <= 20)
                    && call.params.get("sortDirection").and_then(Value::as_str) == Some("desc")
                    && call.params.get("itemsView").and_then(Value::as_str) == Some("full")
            }
            "operationStatus" => call
                .params
                .get("operationId")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("monitor:") && id.len() > "monitor:".len()),
            _ => false,
        });
    };
    let Some(agent_record) = AgentRepository::get(tx, &AgentId::from(actor.agent.as_str()))? else {
        return Ok(false);
    };
    let common_identity = agent_record
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        == call.scope.account
        && !studio_runtime_store::truthy(agent_record.extra.get("deletedAt"))
        && agent_record.extra.get("deleted") != Some(&json!(true));
    let actor_identity = common_identity && agent_record.epoch == actor.epoch;
    // Stop commits the next epoch before asking the retained provider session
    // to interrupt the previous turn. Permit only that exact prior epoch for
    // turn/interrupt, while the agent is no longer eligible to wake.
    let stopped_turn_interrupt = common_identity
        && call.method == "turn/interrupt"
        && actor.epoch.checked_add(1) == Some(agent_record.epoch)
        && agent_record.extra.get("autoWake") == Some(&json!(false));
    let thread_matches = call.params["threadId"].as_str().is_some_and(|thread| {
        agent_record.extra.get("threadId").and_then(Value::as_str) == Some(thread)
    });
    let thread_start = call.method == "thread/start" && call.params.get("threadId").is_none();
    let ephemeral_turn = if call.method == "turn/start" {
        match call.params["threadId"].as_str() {
            Some(thread) => thread_started_for_actor(tx, call, thread)?,
            None => false,
        }
    } else {
        false
    };
    Ok((actor_identity || stopped_turn_interrupt)
        && (thread_matches || thread_start || ephemeral_turn))
}

fn thread_started_for_actor(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
    thread: &str,
) -> Result<bool, WorkerError> {
    let Some(actor) = call.actor.as_ref() else {
        return Ok(false);
    };
    let prefix = format!("account-worker:retained:{}:", call.scope.account);
    let exists: i64 = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts submitted JOIN runtime_operation_receipts result ON result.id=substr(submitted.id,1,length(submitted.id)-10)||':result' WHERE submitted.id>=?1 AND submitted.id<?2 AND substr(submitted.id,-10)=':submitted' AND json_extract(submitted.result,'$.call.method')='thread/start' AND json_extract(submitted.result,'$.call.actor.agent')=?3 AND json_extract(submitted.result,'$.call.actor.epoch')=?4 AND json_extract(submitted.result,'$.call.scope.generation')=?5 AND json_extract(result.result,'$.result.Reply.thread.id')=?6 AND json_extract(result.result,'$.result.Reply.thread.id') IS NOT NULL)",
        rusqlite::params![
            prefix,
            format!("{prefix}~"),
            actor.agent,
            actor.epoch,
            i64::try_from(call.scope.generation).map_err(|_| WorkerError::Capacity)?,
            thread,
        ],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
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
