use super::*;
use rusqlite::OptionalExtension;

/// SQL for the account that owns a monitor's provider command. A rule monitor
/// stores `accountKey`; a submitted command stores `operation.accountKey`. A tool
/// monitor that is not submitted yet stores neither, so its owner's current
/// account applies, as Python `run_monitor` uses `agent.accountKey` when it
/// starts the command. `?{agent_param}` binds that owner account.
fn monitor_account_sql(agent_param: usize) -> String {
    format!(
        "COALESCE(json_extract(record,'$.accountKey'),json_extract(record,'$.operation.accountKey'),?{agent_param})"
    )
}

/// Whether `agent` runs its provider in a VM. Only such an actor has its own
/// session and journal lane. Every other authorized actor's request is sent
/// and answered on the host session.
pub fn vm_actor(connection: &rusqlite::Connection, agent: &str) -> Result<bool, WorkerError> {
    let record = AgentRepository::get(connection, &AgentId::from(agent))?;
    Ok(record.is_some_and(|record| {
        record.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
    }))
}

pub(super) fn authorize(tx: &rusqlite::Transaction<'_>, call: &Call) -> Result<bool, WorkerError> {
    let Some(actor) = call.actor.as_ref() else {
        if let Some(transfer) = call.transfer.as_ref() {
            let Some(raw) =
                studio_runtime_store::AccountTransferRepository::get(tx, &transfer.transfer_id)?
            else {
                return Ok(false);
            };
            let operation: Value = match serde_json::from_str(&raw) {
                Ok(operation) => operation,
                Err(_) => return Ok(false),
            };
            let member = &operation["members"][&transfer.member];
            return Ok(operation["status"] == "pending"
                && operation["id"] == transfer.transfer_id
                && member.is_object()
                && member["sourceAccountKey"] == transfer.source_account
                && member["sourceThreadId"] == transfer.thread_id
                && call.scope.account == transfer.source_account
                && call.params.get("threadId").and_then(Value::as_str)
                    == Some(transfer.thread_id.as_str())
                && call.operation.starts_with(&format!(
                    "transfer:{}:{}:",
                    transfer.transfer_id, transfer.member
                ))
                && matches!(
                    call.method.as_str(),
                    "thread/queue/list" | "thread/backgroundTerminals/list" | "thread/unsubscribe"
                ));
        }
        return Ok(Method::from_wire(&call.method).is_some_and(|method| {
            super::valid_actorless_request_shape(method, &call.params, &call.operation)
        }));
    };
    let Some(agent_record) = AgentRepository::get(tx, &AgentId::from(actor.agent.as_str()))? else {
        return Ok(false);
    };
    let agent_account = agent_record
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let common_identity = agent_account == call.scope.account
        && !studio_runtime_store::truthy(agent_record.extra.get("deletedAt"))
        && agent_record.extra.get("deleted") != Some(&json!(true));
    let actor_identity = common_identity && agent_record.epoch == actor.epoch;
    let deleted_voice_stop = authorize_deleted_voice_stop(tx, call, actor, &agent_record.extra)?;
    let repair_fork = if call.method == "thread/fork" {
        let repair = agent_record
            .extra
            .get("contextRepair")
            .unwrap_or(&Value::Null);
        let source = &repair["source"];
        let snapshot = &repair["snapshot"];
        let fork_params = &repair["forkParams"];
        let path_is_absolute = |value: &Value| {
            value
                .as_str()
                .is_some_and(|path| std::path::Path::new(path).is_absolute())
        };
        repair["id"].as_str().is_some_and(|id| {
            !id.is_empty()
                && repair["agent"] == actor.agent
                && repair["phase"] == "submitted"
                && repair["rpcMethod"] == "thread/fork"
                && repair["forkOperation"] == call.operation
                && source["id"] == actor.agent
                && source["epoch"].as_i64() == Some(actor.epoch)
                && source["accountKey"].as_str() == Some(call.scope.account.as_str())
                && source["threadId"].as_str()
                    == agent_record.extra.get("threadId").and_then(Value::as_str)
                && path_is_absolute(&snapshot["sourcePath"])
                && path_is_absolute(&snapshot["copyPath"])
                && snapshot["sourceSha256"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
                && snapshot["sourceBytes"].as_u64().is_some()
                && snapshot["sourceFileIdentity"].is_object()
                && snapshot["copySha256"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
                && snapshot["copyBytes"].as_u64().is_some()
                && snapshot["eventIds"].is_array()
                && snapshot["importThreadId"].as_str()
                    == call.params.get("threadId").and_then(Value::as_str)
                && snapshot["copyPath"].as_str() == call.params.get("path").and_then(Value::as_str)
                && fork_params == &call.params
                && call.params.get("excludeTurns") == Some(&json!(true))
                && call.params.get("deferGoalContinuation") == Some(&json!(true))
        })
    } else {
        false
    };
    // Stop and delete commit the next epoch before asking the retained provider
    // session to end the previous turn's work: turn/interrupt, then the
    // thread's command processes that outlive an interrupt. Permit only these
    // requests from that exact prior epoch, while the agent can no longer wake.
    // A delete tombstone keeps this authority, so deletion cannot leave the
    // turn and its commands running; it grants nothing else to a deleted actor.
    let stopped_turn_cleanup = agent_account == call.scope.account
        && matches!(
            call.method.as_str(),
            "turn/interrupt" | "thread/backgroundTerminals/clean"
        )
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
    let config_operation = call.operation.strip_prefix("monitor-config:");
    let config_monitor_id = config_operation.and_then(|rest| rest.split(':').next());
    let config_read = call.method == "config/read"
        && config_operation.is_some_and(|rest| rest.split(':').count() == 2)
        && config_monitor_id.is_some_and(|id| !id.is_empty())
        && call.params.get("includeLayers") == Some(&json!(false))
        && call.params.as_object().is_some_and(|params| {
            params
                .keys()
                .all(|key| matches!(key.as_str(), "cwd" | "includeLayers"))
        })
        && call.params.get("cwd").is_some_and(|cwd| {
            cwd.as_str().is_some_and(|cwd| {
                agent_record.extra.get("cwd").and_then(Value::as_str) == Some(cwd)
            })
        })
        && if let Some(monitor_id) = config_monitor_id {
            tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM runtime_monitors WHERE id=?1 AND json_extract(record,'$.agent')=?2 AND json_extract(record,'$.epoch')=?3 AND {}=?4 AND json_extract(record,'$.cwd')=?5 AND json_extract(record,'$.status') IN ('starting','running'))", monitor_account_sql(6)),
                rusqlite::params![monitor_id, actor.agent, actor.epoch, call.scope.account, call.params["cwd"].as_str(), agent_account],
                |row| row.get::<_, i64>(0),
            )? != 0
        } else {
            false
        };
    let monitor_method = matches!(
        call.method.as_str(),
        "command/exec" | "command/exec/write" | "command/exec/resize" | "command/exec/terminate"
    );
    let process_id = call.params.get("processId").and_then(Value::as_str);
    let (monitor_id, liveness_generation) = match call.operation.strip_prefix("liveness:") {
        Some(rest) => rest
            .split_once(':')
            .map_or((process_id, None), |(id, generation)| {
                (Some(id), Some(generation))
            }),
        None => (process_id, None),
    };
    let monitor_matches = if monitor_method {
        if let Some(monitor_id) = monitor_id {
            let monitor = tx
                .query_row(
                    &format!(
                        "SELECT CASE WHEN json_valid(record) THEN json_extract(record,'$.agent') END, \
                            CASE WHEN json_valid(record) THEN json_extract(record,'$.epoch') END, \
                            CASE WHEN json_valid(record) THEN {} END, \
                            CASE WHEN json_valid(record) THEN json_extract(record,'$.cwd') END, \
                            CASE WHEN json_valid(record) THEN json_extract(record,'$.status') END \
                     FROM runtime_monitors WHERE id=?1",
                        monitor_account_sql(2)
                    ),
                    rusqlite::params![monitor_id, agent_account],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()?;
            monitor.is_some_and(|(agent, epoch, account, cwd, status)| {
                agent.as_deref() == Some(actor.agent.as_str())
                    && epoch == Some(actor.epoch)
                    && account.as_deref().unwrap_or("default") == call.scope.account
                    && match call.method.as_str() {
                        "command/exec" => {
                            let regular = call.operation == format!("monitor:{monitor_id}")
                                && process_id == Some(monitor_id)
                                && matches!(status.as_deref(), Some("starting" | "running"));
                            let liveness = liveness_generation.is_some_and(|generation| {
                                !generation.is_empty()
                                    && process_id == Some(call.operation.as_str())
                                    && status.as_deref() == Some("running")
                            });
                            (regular || liveness)
                                && call.params.get("cwd").and_then(Value::as_str) == cwd.as_deref()
                        }
                        "command/exec/write" | "command/exec/resize" => {
                            call.operation
                                .starts_with(&format!("monitor-input:{monitor_id}:"))
                                && call.operation.len()
                                    > format!("monitor-input:{monitor_id}:").len()
                                && status.as_deref() == Some("running")
                        }
                        "command/exec/terminate" => {
                            call.operation == format!("monitor-cancel:{monitor_id}")
                                && status.as_deref() == Some("running")
                        }
                        _ => false,
                    }
            })
        } else {
            false
        }
    } else {
        false
    };
    Ok(authorize_recovered_monitor_cancel(tx, call, actor)?
        || (actor_identity || stopped_turn_cleanup || deleted_voice_stop)
            && (thread_matches
                || (actor_identity && repair_fork)
                || thread_start
                || ephemeral_turn
                || config_read
                || monitor_matches
                || deleted_voice_stop))
}

/// No orphan processes after owner deletion across restart: only a latched
/// cancellation of the exact proved monitor handle may cross the inactive-owner boundary.
fn authorize_recovered_monitor_cancel(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
    actor: &RequestActor,
) -> Result<bool, WorkerError> {
    if call.method != "command/exec/terminate" {
        return Ok(false);
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct CancelParams {
        process_id: String,
    }
    let Ok(params) = serde_json::from_value::<CancelParams>(call.params.clone()) else {
        return Ok(false);
    };
    if call.operation != format!("monitor-cancel:{}", params.process_id) {
        return Ok(false);
    }
    let Some(record) = studio_runtime_store::MonitorsRepository::get(tx, &params.process_id)?
    else {
        return Ok(false);
    };
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RecoveredCancelMonitor {
        agent: String,
        epoch: i64,
        operation: crate::monitor_policy::Operation,
        status: crate::monitor_policy::MonitorStatus,
        cancel_requested: bool,
        reattached_connection_id: String,
    }
    let Ok(monitor) = serde_json::from_value::<RecoveredCancelMonitor>(record.value().clone())
    else {
        return Ok(false);
    };
    Ok(monitor.agent == actor.agent
        && monitor.epoch == actor.epoch
        && monitor.operation.agent == actor.agent
        && monitor.operation.epoch == actor.epoch
        && monitor.operation.account_key == call.scope.account
        && monitor.status == crate::monitor_policy::MonitorStatus::Running
        && monitor.cancel_requested
        && monitor.reattached_connection_id == call.scope.connection)
}

fn authorize_deleted_voice_stop(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
    actor: &RequestActor,
    agent_extra: &std::collections::BTreeMap<String, Value>,
) -> Result<bool, WorkerError> {
    let Some(session_id) = call.deleted_actor_session.as_deref() else {
        return Ok(false);
    };
    let Some(thread_id) = call.params.get("threadId").and_then(Value::as_str) else {
        return Ok(false);
    };
    let deleted = studio_runtime_store::truthy(agent_extra.get("deletedAt"))
        || agent_extra.get("deleted") == Some(&json!(true));
    if session_id.is_empty()
        || call.method != "thread/realtime/stop"
        || call.operation != format!("voice:delete-stop:{session_id}")
        || call
            .params
            .as_object()
            .is_none_or(|params| params.len() != 1)
        || !deleted
        || actor.epoch < 0
        || agent_extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != call.scope.account
    {
        return Ok(false);
    }
    let receipt_id = format!("agent-delete-voice:{}:{}", actor.agent, actor.epoch);
    let raw: Option<String> = tx
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [&receipt_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let Ok(receipt) = serde_json::from_str::<Value>(&raw) else {
        return Ok(false);
    };
    if receipt.get("agent").and_then(Value::as_str) != Some(actor.agent.as_str())
        || receipt.get("epoch").and_then(Value::as_i64) != Some(actor.epoch)
    {
        return Ok(false);
    }
    let Some(sessions) = receipt.get("voiceEndSessions").and_then(Value::as_array) else {
        return Ok(false);
    };
    Ok(sessions.iter().any(|session| {
        session.get("sessionId").and_then(Value::as_str) == Some(session_id)
            && session.get("agentId").and_then(Value::as_str) == Some(actor.agent.as_str())
            && session.get("epoch").and_then(Value::as_i64) == Some(actor.epoch)
            && session.get("accountKey").and_then(Value::as_str)
                == Some(call.scope.account.as_str())
            && session.get("threadId").and_then(Value::as_str) == Some(thread_id)
            && session.get("connectionId").and_then(Value::as_str)
                == Some(call.scope.connection.as_str())
            && session.get("generation").and_then(Value::as_u64) == Some(call.scope.generation)
    }))
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
/// Apply the same authorization as durable requests without a receipt write.
/// Recheck after provider preparation, as `submitted` does for durable calls.
pub(super) async fn authorize_volatile_read(
    s: &RuntimeStore,
    call: &Call,
) -> Result<Result<(), PortError>, WorkerError> {
    let call = call.clone();
    s.read_async("runtime-services/retained_read_authorize", move |tx| {
        Ok(if authorize(tx, &call)? {
            Ok(())
        } else {
            Err(PortError::Identity)
        })
    })
    .await
}

pub(super) async fn replay(
    s: &RuntimeStore,
    call: &Call,
) -> Result<Result<Replay, PortError>, WorkerError> {
    let call = call.clone();
    s.read_async("runtime-services/retained_replay", move |tx| {
        replay_in_transaction(tx, &call)
    })
    .await
}

/// Resolve a replay from a reader snapshot: it only reads receipts, so it never
/// takes the writer lane. Kept synchronous so actor callbacks never await.
pub(super) fn replay_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
) -> Result<Result<Replay, PortError>, WorkerError> {
    if !authorize(tx, call)? {
        return Ok(Err(PortError::Identity));
    }
    replay_recorded(tx, call)
}

/// Resolve a replay from `call`'s own receipts without the live authorization.
/// Only for reading the answer of a request that was authorized and submitted
/// earlier: it never queues a provider write.
pub(super) fn replay_recorded(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
) -> Result<Result<Replay, PortError>, WorkerError> {
    let Some(prior) = store::receipt(tx, &call.key(), "submitted")? else {
        return Ok(Ok(Replay::Miss));
    };
    let prior_call: Call =
        serde_json::from_value(prior["call"].clone()).map_err(|_| WorkerError::Capacity)?;
    if !same_replay_identity(&prior_call, call) {
        return Ok(Err(PortError::Identity));
    }
    let request = store::decode(&prior["request"])?;
    let saved = store::receipt(tx, &call.key(), "result")?;
    let result = if let Some(saved) = saved {
        // The result receipt is committed before its journal event advances
        // the checkpoint. Only expose it after that event is known committed
        // in the generation that recorded the result. Looking up the prior
        // generation is important: reconnecting must not hide a settled
        // result merely because the new generation has a different cursor.
        let checkpoint = crate::account_worker::store::checkpoint(
            tx,
            &prior_call.scope.account,
            prior_call.scope.generation,
        )?;
        if saved["cursor"]
            .as_u64()
            .is_some_and(|cursor| cursor <= checkpoint)
        {
            serde_json::from_value(saved["result"].clone()).map_err(|_| WorkerError::Capacity)?
        } else {
            RequestResult::Unknown
        }
    } else {
        RequestResult::Unknown
    };
    if result == RequestResult::NotSent {
        return Ok(Ok(Replay::Retry(request.ticket)));
    }
    // A committed operation may be inspected after the provider reconnects.
    // Return its original ticket and scope; this path only reads receipts and
    // never queues another provider write.
    let mut receipt = prior_call.receipt(request.ticket, result);
    receipt.replayed = true;
    Ok(Ok(Replay::Return(receipt)))
}

pub(super) enum Replay {
    Miss,
    Return(SessionReceipt),
    Retry(studio_runtime_providers::RequestTicket),
}

fn same_replay_identity(prior: &Call, current: &Call) -> bool {
    if prior.scope.account != current.scope.account {
        return false;
    }
    let mut prior = prior.clone();
    prior.scope.connection = current.scope.connection.clone();
    prior.scope.generation = current.scope.generation;
    prior == *current
}

pub(super) async fn submitted(
    s: &RuntimeStore,
    call: &Call,
    request: &RecoveredRequest,
    retry_ticket: Option<&studio_runtime_providers::RequestTicket>,
) -> Result<Result<(), PortError>, WorkerError> {
    let call = call.clone();
    let request = request.clone();
    let retry_ticket = retry_ticket.cloned();
    scheduler::lifecycle_transaction_with(s, move |tx| {
        if !authorize(tx, &call)? {
            return Ok(Err(PortError::Identity));
        }
        if let Some(expected_ticket) = retry_ticket {
            let Some(prior) = store::receipt(tx, &call.key(), "submitted")? else {
                return Ok(Err(PortError::Identity));
            };
            let prior_call: Call =
                serde_json::from_value(prior["call"].clone()).map_err(|_| WorkerError::Capacity)?;
            if !same_replay_identity(&prior_call, &call) {
                return Ok(Err(PortError::Identity));
            }
            let prior_request = store::decode(&prior["request"])?;
            let checkpoint =
                store::checkpoint(tx, &prior_call.scope.account, prior_call.scope.generation)?;
            let proven_not_sent = store::receipt(tx, &call.key(), "result")?.is_some_and(|saved| {
                saved["result"] == json!(RequestResult::NotSent)
                    && saved["cursor"]
                        .as_u64()
                        .is_some_and(|cursor| cursor <= checkpoint)
            });
            if prior_request.ticket != expected_ticket
                || request.ticket != expected_ticket
                || !proven_not_sent
            {
                return Ok(Err(PortError::Identity));
            }
            store::delete_receipt(tx, &call.key(), "result")?;
            store::replace(
                tx,
                &call.key(),
                "submitted",
                json!({"call":call,"request":store::record(&request)}),
            )?;
        } else {
            // The replay read and this admission are separate transactions. Two
            // concurrent callers can both observe a miss, then race to persist
            // the same operation key. Validate the winner's durable intent,
            // then resolve a changed identity as a caller refusal and an exact
            // identity as uncertain. Neither race may overwrite the intent or
            // submit a second provider request.
            if let Some(prior) = store::receipt(tx, &call.key(), "submitted")? {
                let prior_call: Call = serde_json::from_value(prior["call"].clone())
                    .map_err(|_| WorkerError::Capacity)?;
                let prior_request = store::decode(&prior["request"])?;
                let key = call.key();
                if prior_call.key() != key
                    || prior_request.ticket.id
                        != studio_runtime_providers::RequestId::String(key.clone())
                    || prior_request.ticket.generation != prior_call.scope.generation
                    || prior_request.ticket.method.as_str() != prior_call.method
                    || prior_request.operation_id.0 != key
                {
                    return Err(WorkerError::Capacity);
                }
                return Ok(Err(if same_replay_identity(&prior_call, &call) {
                    PortError::Unknown
                } else {
                    PortError::Identity
                }));
            }
            store::save(
                tx,
                &call.key(),
                "submitted",
                json!({"call":call,"request":store::record(&request)}),
            )?;
        }
        Ok(Ok(()))
    })
    .await
}
pub(in crate::account_worker) async fn recover(
    s: &RuntimeStore,
    account: &str,
    generation: u64,
) -> Result<Vec<(PendingRequest, RecoveredRequest)>, WorkerError> {
    let account = account.to_owned();
    s.transaction_with(move |tx| {
        let prefix = format!("account-worker:retained:{account}:");
        let checkpoint = store::checkpoint(tx, &account, generation)?;
        let mut statement = tx.prepare("SELECT r.result FROM runtime_operation_receipts r WHERE r.id>=?1 AND r.id<?2 AND substr(r.id,-10)=':submitted' AND json_extract(r.result,'$.call.scope.generation')=?3 AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':result' AND json_extract(done.result,'$.result') != 'Unknown' AND json_extract(done.result,'$.cursor') <=?4) ORDER BY r.id")?;
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
            // The answer comes back on the lane the request was sent on, as
            // in `retained_request`: a host request keeps no actor lane.
            let actor_id = match call.actor.as_ref() {
                Some(actor) if vm_actor(tx, &actor.agent)? => Some(actor.agent.clone()),
                _ => None,
            };
            result.push((PendingRequest {
                call,
                ticket: request.ticket.clone(),
                actor_id,
                reply: None,
                result: None,
                result_cursor: 0,
                volatile: false,
            }, request));
        }
        Ok(result)
    }).await
}

#[cfg(test)]
mod replay_identity_tests {
    use super::*;

    fn call() -> Call {
        Call {
            scope: ConnectionScope {
                account: "default".into(),
                connection: "old-connection".into(),
                generation: 4,
            },
            operation: "operation-1".into(),
            actor: Some(RequestActor {
                agent: "agent-1".into(),
                epoch: 7,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: "thread/read".into(),
            params: json!({"threadId":"thread-1","includeTurns":false}),
        }
    }

    #[tokio::test]
    async fn stopped_and_deleted_prior_epoch_may_only_end_its_turn_work() {
        let root = tempfile::tempdir().expect("root");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        store
            .transaction(|tx| {
                for record in [
                    json!({"id":"stopped","rootId":"stopped","epoch":8,"status":"paused","autoWake":false,"threadId":"thread-1"}),
                    json!({"id":"deleted","rootId":"deleted","epoch":8,"deletedAt":1,"status":"paused","autoWake":false,"threadId":"thread-1"}),
                    json!({"id":"woken","rootId":"woken","epoch":8,"status":"queued","autoWake":true,"threadId":"thread-1"}),
                ] {
                    AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(record)?,
                    )?;
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed stopped, deleted and woken owners");
        store
            .read_async("test/stopped-turn-authority", |tx| {
                let work = [
                    (
                        "turn/interrupt",
                        json!({"threadId":"thread-1","turnId":"turn-1"}),
                    ),
                    (
                        "thread/backgroundTerminals/clean",
                        json!({"threadId":"thread-1"}),
                    ),
                ];
                for agent in ["stopped", "deleted"] {
                    for (method, params) in &work {
                        let mut exact = call();
                        exact.actor = Some(RequestActor {
                            agent: agent.into(),
                            epoch: 7,
                        });
                        exact.method = (*method).into();
                        exact.params = params.clone();
                        assert!(authorize(tx, &exact)?, "{agent} ends {method}");
                        let mut invalid = exact.clone();
                        invalid.params["threadId"] = json!("thread-2");
                        assert!(!authorize(tx, &invalid)?, "{agent} {method} other thread");
                        let mut invalid = exact.clone();
                        invalid.actor.as_mut().expect("actor").epoch = 6;
                        assert!(!authorize(tx, &invalid)?, "{agent} {method} older epoch");
                        let mut invalid = exact.clone();
                        invalid.scope.account = "other".into();
                        assert!(!authorize(tx, &invalid)?, "{agent} {method} other account");
                    }
                    let mut read = call();
                    read.actor = Some(RequestActor {
                        agent: agent.into(),
                        epoch: 7,
                    });
                    assert!(!authorize(tx, &read)?, "{agent} gets no other request");
                }
                let mut deleted_current = call();
                deleted_current.actor = Some(RequestActor {
                    agent: "deleted".into(),
                    epoch: 8,
                });
                deleted_current.method = "turn/interrupt".into();
                deleted_current.params = json!({"threadId":"thread-1","turnId":"turn-1"});
                assert!(!authorize(tx, &deleted_current)?);
                for (method, params) in &work {
                    let mut woken = call();
                    woken.actor = Some(RequestActor {
                        agent: "woken".into(),
                        epoch: 7,
                    });
                    woken.method = (*method).into();
                    woken.params = params.clone();
                    assert!(
                        !authorize(tx, &woken)?,
                        "a woken agent keeps no stop authority"
                    );
                }
                Ok::<_, WorkerError>(())
            })
            .await
            .expect("stop authority witnesses");
    }

    #[tokio::test]
    async fn monitor_completion_recovered_cancel_authority_is_exact_and_cannot_resume() {
        let root = tempfile::tempdir().expect("root");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        store.transaction(|tx| {
            AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(json!({"id":"agent-1","rootId":"agent-1","epoch":9,"deletedAt":1,"status":"interrupted","autoWake":false}))?)?;
            studio_runtime_store::Table::new(tx).put(&studio_runtime_store::MonitorRecord::from_value(json!({"id":"watch","agent":"agent-1","epoch":7,"status":"running","cancelRequested":true,"reattachedConnectionId":"new-connection","operation":{"agent":"agent-1","epoch":7,"accountKey":"default","connectionId":"old"}}))?)?;
            Ok::<_,studio_runtime_store::StoreError>(())
        }).await.expect("seed deleted owner and proved handle");
        store
            .read_async("test/cancel-authority", |tx| {
                let mut exact = call();
                exact.scope.connection = "new-connection".into();
                exact.operation = "monitor-cancel:watch".into();
                exact.method = "command/exec/terminate".into();
                exact.params = json!({"processId":"watch"});
                assert!(authorize(tx, &exact)?);
                for method in [
                    "command/exec",
                    "command/exec/write",
                    "command/exec/resize",
                    "thread/read",
                ] {
                    let mut invalid = exact.clone();
                    invalid.method = method.into();
                    assert!(
                        !authorize(tx, &invalid)?,
                        "deleted actor must not resume {method}"
                    );
                }
                let mut invalid = exact.clone();
                invalid.operation = "monitor-cancel:other".into();
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.params = json!({"processId":"other"});
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.actor.as_mut().expect("actor").epoch = 9;
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.scope.connection = "unproved".into();
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.scope.account = "other".into();
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact;
                invalid.params = json!({"processId":"watch","command":["true"]});
                assert!(!authorize(tx, &invalid)?);
                Ok::<_, WorkerError>(())
            })
            .await
            .expect("authority witnesses");
    }

    #[test]
    fn replay_identity_normalizes_only_connection_generation() {
        let prior = call();
        let mut reconnected = prior.clone();
        reconnected.scope.connection = "new-connection".into();
        reconnected.scope.generation = 5;
        assert!(same_replay_identity(&prior, &reconnected));

        let mut changed = prior.clone();
        changed.scope.account = "other-account".into();
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.operation.push_str(":other");
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.actor = Some(RequestActor {
            agent: "agent-1".into(),
            epoch: 8,
        });
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.transfer = Some(TransferAuthorization {
            transfer_id: "transfer-1".into(),
            member: "member-1".into(),
            source_account: "default".into(),
            thread_id: "thread-1".into(),
        });
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.deleted_actor_session = Some("voice-session-1".into());
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.method = "thread/turns/list".into();
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior;
        changed.params["threadId"] = json!("thread-2");
        assert!(!same_replay_identity(&call(), &changed));
    }

    #[tokio::test]
    async fn concurrent_submissions_with_different_call_identity_are_refused_without_replacing_intent()
     {
        let state = tempfile::tempdir().expect("retained receipt test state directory");
        let store = RuntimeStore::open(Some(state.path()))
            .await
            .expect("open retained receipt test store");
        let first = Call {
            scope: ConnectionScope {
                account: "default".into(),
                connection: "connection-1".into(),
                generation: 1,
            },
            operation: "import:fixture:read".into(),
            actor: None,
            transfer: None,
            deleted_actor_session: None,
            method: "thread/read".into(),
            params: json!({"threadId":"source-thread","includeTurns":false}),
        };
        let conflicting = Call {
            params: json!({"threadId":"other-source-thread","includeTurns":false}),
            ..first.clone()
        };
        let recovered = |call: &Call| {
            let id = call.key();
            RecoveredRequest {
                ticket: RequestTicket {
                    id: studio_runtime_providers::RequestId::String(id.clone()),
                    generation: call.scope.generation,
                    method: Method::ThreadRead,
                },
                operation_id: studio_runtime_process::OperationId(id),
                thread_id: call.params["threadId"].as_str().map(str::to_owned),
                lightweight: true,
                deadline_ms: 60_000,
                unknown: true,
                remote_id: None,
            }
        };

        // Model the valid interleaving: both reads finish before either
        // transaction commits the submitted intent.
        assert!(matches!(
            replay(&store, &first)
                .await
                .expect("read first replay state")
                .expect("first replay should miss"),
            Replay::Miss
        ));
        assert!(matches!(
            replay(&store, &conflicting)
                .await
                .expect("read conflicting replay state")
                .expect("conflicting replay should miss before admission"),
            Replay::Miss
        ));
        assert_eq!(
            submitted(&store, &first, &recovered(&first), None)
                .await
                .expect("persist first submitted intent"),
            Ok(())
        );
        assert_eq!(
            submitted(&store, &conflicting, &recovered(&conflicting), None)
                .await
                .expect("inspect conflicting submitted intent"),
            Err(PortError::Identity)
        );

        // The first durable identity remains replayable; the losing identity
        // cannot replace it or advance into a provider submission.
        assert!(matches!(
            replay(&store, &first)
                .await
                .expect("read first durable intent")
                .expect("first durable intent should be replayable"),
            Replay::Return(_)
        ));
        assert!(matches!(
            replay(&store, &conflicting)
                .await
                .expect("read changed identity outcome"),
            Err(PortError::Identity)
        ));

        let exact = Call {
            operation: "import:fixture:exact-read".into(),
            ..first.clone()
        };
        let exact_retry = exact.clone();
        assert!(matches!(
            replay(&store, &exact)
                .await
                .expect("read exact operation replay state")
                .expect("first exact operation replay should miss"),
            Replay::Miss
        ));
        assert!(matches!(
            replay(&store, &exact_retry)
                .await
                .expect("read exact retry replay state")
                .expect("concurrent exact retry should miss before admission"),
            Replay::Miss
        ));
        assert_eq!(
            submitted(&store, &exact, &recovered(&exact), None)
                .await
                .expect("persist exact submitted intent"),
            Ok(())
        );
        assert_eq!(
            submitted(&store, &exact_retry, &recovered(&exact_retry), None)
                .await
                .expect("inspect exact concurrent submitted intent"),
            Err(PortError::Unknown)
        );
        assert!(matches!(
            replay(&store, &exact_retry)
                .await
                .expect("read durable exact retry outcome")
                .expect("durable exact retry should replay"),
            Replay::Return(SessionReceipt {
                result: RequestResult::Unknown,
                replayed: true,
                ..
            })
        ));
    }
}
