use super::{model::*, storage, tool_result};
use crate::tool_requests::{self, admission, lifecycle};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use studio_runtime_providers::{RequestId, ResponseBody};
use studio_runtime_store::{RuntimeStore, system_reads as records};

fn dispatch_gate(key: String) -> Result<Arc<Mutex<()>>, CallbackError> {
    static GATES: OnceLock<Mutex<std::collections::HashMap<String, Weak<Mutex<()>>>>> =
        OnceLock::new();
    let gates = GATES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut gates = gates.lock().map_err(|_| CallbackError::Unknown)?;
    gates.retain(|_, gate| gate.strong_count() > 0);
    Ok(gates.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
        let gate = Arc::new(Mutex::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }))
}

mod runtime_dispatch;
mod runtime_ports;
pub(crate) mod services;
pub(crate) use runtime_dispatch::ManagedToolRuntimePorts;
#[cfg(test)]
pub(crate) use runtime_dispatch::MonitorCreatePort;
#[cfg(test)]
pub(crate) use runtime_dispatch::MonitorInputRequest;
#[cfg(test)]
pub(crate) use runtime_dispatch::{MonitorCancelPort, MonitorInputPort};
#[cfg(test)]
pub(crate) use runtime_dispatch::{RulesActionResult, StatusWatchPort};
pub use runtime_dispatch::{
    WorkerAccountSelection, select_worker_account, validate_worker_model_choice,
};
pub use runtime_ports::ManagedToolRuntime;
impl From<tool_requests::ToolRequestError> for CallbackError {
    fn from(error: tool_requests::ToolRequestError) -> Self {
        match error {
            tool_requests::ToolRequestError::Store(error) => Self::Store(error),
            tool_requests::ToolRequestError::RequiresDependency(name) => Self::Dependency(name),
            tool_requests::ToolRequestError::InvalidRequest(message) => Self::Invalid(message),
        }
    }
}
#[derive(Clone, Copy, serde::Deserialize)]
enum ToolKind {
    #[serde(rename = "orchestration_request")]
    Request,
    #[serde(rename = "orchestration_task")]
    Task,
    #[serde(rename = "orchestration_message")]
    Message,
    #[serde(rename = "orchestration_chat_read")]
    ChatRead,
    #[serde(rename = "orchestration_context")]
    Context,
    #[serde(rename = "orchestration_peers")]
    Peers,
    #[serde(rename = "orchestration_status")]
    Status,
    #[serde(rename = "orchestration_read")]
    Read,
    #[serde(rename = "orchestration_search")]
    Search,
    #[serde(rename = "orchestration_send")]
    Send,
    #[serde(rename = "orchestration_spawn")]
    Spawn,
    #[serde(rename = "orchestration_review")]
    Review,
    #[serde(rename = "orchestration_monitor")]
    Monitor,
    #[serde(rename = "orchestration_monitor_input")]
    MonitorInput,
    #[serde(rename = "orchestration_cancel_monitor")]
    CancelMonitor,
    #[serde(rename = "orchestration_move")]
    Move,
    #[serde(rename = "orchestration_teleport")]
    Teleport,
    #[serde(rename = "orchestration_servers")]
    Servers,
    #[serde(rename = "orchestration_interrupt")]
    Interrupt,
    #[serde(rename = "orchestration_agent_manage")]
    AgentManage,
    #[serde(rename = "orchestration_watch")]
    Watch,
    #[serde(rename = "orchestration_complaint")]
    Complaint,
    #[serde(rename = "orchestration_speak")]
    Speak,
    #[serde(rename = "orchestration_title")]
    Title,
    #[serde(rename = "host_exec")]
    HostExec,
    #[serde(other)]
    Unknown,
}
#[derive(Clone)]
struct ToolCall {
    name: String,
    kind: ToolKind,
    args: Value,
    id: String,
    call: String,
    prefix: String,
    signature: String,
    alias: Option<String>,
}
impl ToolCall {
    fn parse(input: &Input, context: &CallbackContext) -> Result<Self, CallbackError> {
        input.check_context(context)?;
        let name = input.params["tool"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or(CallbackError::Invalid("Supply tool name"))?
            .to_owned();
        let args = match input.params.get("arguments") {
            Some(Value::String(raw)) => serde_json::from_str(raw)?,
            None => json!({}),
            Some(v) => v.clone(),
        };
        if !args.is_object() {
            return Err(CallbackError::Invalid("Tool arguments must be an object"));
        }
        let call = match input.params.get("callId") {
            Some(Value::String(value)) => value.clone(),
            Some(Value::Number(value)) => value.to_string(),
            None => match &input.id {
                RequestId::String(value) => value.clone(),
                RequestId::Number(value) => value.to_string(),
            },
            _ => return Err(CallbackError::Invalid("Invalid tool call identity")),
        };
        if call.is_empty() || call.len() > 1000 {
            return Err(CallbackError::Invalid("Invalid tool call identity"));
        }
        let alias_prefix = match name.as_str() {
            "orchestration_spawn" => Some("spawn:"),
            "orchestration_send" => Some("send:"),
            "orchestration_review" => Some("review:"),
            "orchestration_servers" if args["action"] != "receipt" => Some("server:"),
            "orchestration_move" | "orchestration_teleport" => Some("move:"),
            _ => None,
        };
        let alias = if alias_prefix.is_some() && args.get("request_id").is_some() {
            let value = args["request_id"].as_str().ok_or(CallbackError::Invalid(
                "Supply request_id with 1 to 200 characters and no surrounding whitespace",
            ))?;
            if !(1..=200).contains(&value.chars().count())
                || value.trim() != value
                || value.chars().any(|c| c < ' ')
            {
                return Err(CallbackError::Invalid(
                    "Supply request_id with 1 to 200 characters and no surrounding whitespace",
                ));
            }
            Some(value.to_owned())
        } else {
            None
        };
        let prefix = if context.account == "default" {
            format!("{}:", context.thread)
        } else {
            format!("{}:{}:", context.account, context.thread)
        };
        let id = format!(
            "{prefix}{}",
            alias
                .as_ref()
                .zip(alias_prefix)
                .map(|(a, p)| format!("{p}{a}"))
                .unwrap_or(call.clone())
        );
        let signature = studio_runtime_store::sha256_hex(
            studio_runtime_store::python_json_compact(&json!({"tool":name,"arguments":args}))
                .as_bytes(),
        );
        let kind = serde_json::from_value(json!(name))?;
        Ok(Self {
            name,
            kind,
            args,
            id,
            call,
            prefix,
            signature,
            alias,
        })
    }
}
pub(super) fn put(tx: &Transaction<'_>, record: &Value) -> Result<(), CallbackError> {
    let id = record["id"]
        .as_str()
        .ok_or(CallbackError::Invalid("Invalid tool receipt"))?;
    records::validate("runtime_tool_requests", id, record.clone())?;
    tx.execute(
        "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
        params![record.to_string(), id],
    )?;
    Ok(())
}
pub(super) fn dispatch_with_ports<P: crate::managed_host_tools::Ports>(
    store: &RuntimeStore,
    context: &CallbackContext,
    input: &Input,
    now: f64,
    ports: &P,
) -> Result<CallbackDecision, CallbackError> {
    let call = ToolCall::parse(input, context)?;
    // A durable claim prevents duplicate execution across restart. Within one
    // process, serialize the same request so a concurrent replay can observe
    // the first request's committed result instead of racing its service call.
    let gate = dispatch_gate(format!("{}:{}", store.state_dir().display(), call.id))?;
    let _gate = gate.lock().map_err(|_| CallbackError::Unknown)?;
    if matches!(call.kind, ToolKind::Unknown) {
        return Ok(CallbackDecision::Unsupported);
    }
    if let Some(decision) = store.transaction_with(|tx| {
        storage::saved_decision(tx, &ReplyIdentity::new(context, &input.id), input)
    })? {
        return Ok(decision);
    }
    // The complete call identity and arguments commit before a service can act.
    store.transaction_with(|tx| {
        let snapshot = storage::snapshot(tx, context)?;
        snapshot.active()?;
        if call.name == "orchestration_monitor_input" {
            let actor = crate::managed_host_tools::Actor::from_record(
                snapshot.actor(),
                &context.agent,
                context.epoch,
            )
            .map_err(services::managed_error)?;
            runtime_dispatch::authorize_monitor_input_in_transaction(
                tx,
                &actor,
                call.args
                    .get("monitor_id")
                    .and_then(Value::as_str)
                    .ok_or(CallbackError::Invalid("Missing monitor_id"))?,
            )
            .map_err(services::managed_error)?;
        }
        let mut candidate = json!({
            "id":call.id, "agent":context.agent, "threadId":context.thread,
            "callId":call.call, "tool":call.name, "signature":call.signature,
            "arguments":call.args,
            "rpcId":input.id, "turnId":context.turn, "epoch":context.epoch,
            "accountKey":context.account, "connectionId":context.connection,
            "supervisor":{"generation":context.generation},
            "created":now, "updated":now,
            "readOnly":services::read_only(&call.name,&call.args),
            "stage":"queued", "outcome":"pending", "cancelRequested":false
        });
        if let Some(alias) = &call.alias {
            candidate["request_id"] = json!(alias);
        }
        admission::reserve(tx, candidate, &call.call, &call.prefix, call.alias.clone())?;
        Ok::<_, CallbackError>(())
    })?;
    enum DispatchState {
        Existing(CallbackDecision),
        Result(Value),
        Claimed,
        Retry,
    }
    // The callback transaction commits only the execution claim. It is the
    // durable intent boundary: any later dispatcher work is associated with
    // this exact tool request and its complete argument signature.
    let state = store.transaction_with(|tx| {
        let snapshot = storage::snapshot(tx, context)?;
        snapshot.active()?;
        let identity = ReplyIdentity::new(context, &input.id);
        if let Some(decision) = storage::saved_decision(tx, &identity, input)? {
            return Ok(DispatchState::Existing(decision));
        }
        let uncertain: i64 = tx.query_row(
            "SELECT count(*) FROM runtime_operation_receipts WHERE json_extract(result,'$.toolRequest')=?1 AND json_extract(result,'$.delivery') IN ('submitted','unknown')",
            [&call.id], |row| row.get(0))?;
        if uncertain != 0 { return Err(CallbackError::Unknown); }
        let mut record = records::get(tx, "runtime_tool_requests", &call.id)?
            .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
        if record["agent"] != context.agent
            || record["epoch"].as_u64() != Some(context.epoch)
            || record["accountKey"] != context.account
            || record["threadId"] != context.thread
            || record["signature"] != call.signature {
            return Err(CallbackError::Stale);
        }
        let cached = records::get(tx, "runtime_tool_results", &call.id)?
            .or_else(|| record.get("result").filter(|v| v.is_object()).cloned());
        if let Some(saved) = cached {
            return Ok(DispatchState::Result(saved));
        }
        if record["connectionId"] != context.connection
            || record.pointer("/supervisor/generation").and_then(Value::as_u64) != Some(context.generation) {
            return Err(CallbackError::Stale);
        }
        if admission::claim(&mut record, now)? {
            put(tx, &record)?;
            Ok(DispatchState::Claimed)
        } else {
            Ok(DispatchState::Retry)
        }
    })?;
    let state = match state {
        DispatchState::Existing(decision) => return Ok(decision),
        other => other,
    };
    // Dispatch starts only after the claim transaction above has committed. The
    // service transaction below either records the local result and its durable
    // effects together, or leaves the claim as an inspectable unknown receipt.
    let (result, signals) = match state {
        DispatchState::Result(result) => (result, Vec::new()),
        state @ (DispatchState::Claimed | DispatchState::Retry) => {
            if matches!(state, DispatchState::Retry) {
                // The running stage is a durable claim without a settled
                // result. Python's callback protocol must acknowledge that
                // state with recovery information and must never repeat the
                // operation merely because its response was lost.
                let pending = store.transaction_with(|tx| {
                    records::get(tx, "runtime_tool_requests", &call.id)?
                        .ok_or(CallbackError::Invalid("Unknown tool receipt"))
                })?;
                let result = tool_result(
                    json!({
                        "requestId": call.id,
                        "stage": pending.get("stage").and_then(Value::as_str).unwrap_or("running"),
                        "outcome": pending.get("outcome").and_then(Value::as_str).unwrap_or("pending"),
                        "recovery": "Read orchestration_request with this requestId. Do not repeat the mutation with a new id."
                    }),
                    true,
                );
                (result, Vec::new())
            } else {
                let actor = store.transaction_with(|tx| {
                    let snapshot = storage::snapshot(tx, context)?;
                    snapshot.active()?;
                    let actor = crate::managed_host_tools::Actor::from_record(
                        snapshot.actor(),
                        &context.agent,
                        context.epoch,
                    )
                    .map_err(services::managed_error)?;
                    Ok::<_, CallbackError>(actor)
                })?;
                let dispatched = {
                    ports
                        .dispatch_managed(store, &actor, &call.name, &call.args, &call.id, now)
                        .map_err(services::managed_error)?
                };
                if let Some(dispatched) = dispatched {
                    store.transaction_with(|tx| {
                        let snapshot = storage::snapshot(tx, context)?;
                        snapshot.active()?;
                        let mut record = records::get(tx, "runtime_tool_requests", &call.id)?
                            .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
                        if record["agent"] != context.agent
                            || record["epoch"].as_u64() != Some(context.epoch)
                            || record["connectionId"] != context.connection
                            || record
                                .pointer("/supervisor/generation")
                                .and_then(Value::as_u64)
                                != Some(context.generation)
                            || record["signature"] != call.signature
                        {
                            return Err(CallbackError::Stale);
                        }
                        if let Some(saved) = records::get(tx, "runtime_tool_results", &call.id)? {
                            return Ok((saved, Vec::new()));
                        }
                        finalize_dispatched(tx, &call, &mut record, now, dispatched)
                    })?
                } else {
                    store.transaction_with(|tx| {
                        let snapshot = storage::snapshot(tx, context)?;
                        snapshot.active()?;
                        let mut record = records::get(tx, "runtime_tool_requests", &call.id)?
                            .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
                        if record["agent"] != context.agent
                            || record["epoch"].as_u64() != Some(context.epoch)
                            || record["connectionId"] != context.connection
                            || record
                                .pointer("/supervisor/generation")
                                .and_then(Value::as_u64)
                                != Some(context.generation)
                            || record["signature"] != call.signature
                        {
                            return Err(CallbackError::Stale);
                        }
                        if let Some(saved) = records::get(tx, "runtime_tool_results", &call.id)? {
                            return Ok((saved, Vec::new()));
                        }
                        execute_once(tx, &snapshot, &call, &mut record, now, ports)
                    })?
                }
            }
        }
        DispatchState::Existing(_) => unreachable!("handled above"),
    };
    let (decision, signals) = store.transaction_with(|tx| {
        let identity = ReplyIdentity::new(context, &input.id);
        if let Some(decision) = storage::saved_decision(tx, &identity, input)? {
            return Ok((decision, Vec::new()));
        }
        let record = records::get(tx, "runtime_tool_requests", &call.id)?
            .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
        let projected = super::project_tool_result(&result, &call.id, &record["outcome"])?;
        let reply = Reply::new(identity, ResponseBody::Result(projected))?;
        Ok::<_, CallbackError>((CallbackDecision::Answer(reply), signals))
    })?;
    if !signals.is_empty() {
        ports
            .release(&signals)
            .map_err(|_| CallbackError::Unknown)?;
    }
    // The callback reply is committed only after the post-commit dispatcher has
    // observed the durable intents and completed its wake/reconciliation step.
    // If the process exits in this gap, the tool result and request receipt above
    // remain authoritative; replay reconstructs the same reply without executing
    // the operation again.
    if let CallbackDecision::Answer(reply) = &decision {
        store.transaction_with(|tx| {
            storage::save_reply(tx, input, reply)?;
            let key = reply.identity.key();
            let mut outbox = storage::read(tx, &key)?.ok_or(CallbackError::Unknown)?;
            outbox["toolRequest"] = json!(call.id);
            storage::update(tx, &key, &outbox)?;
            Ok::<_, CallbackError>(())
        })?;
    }
    Ok(decision)
}

pub(super) fn production_ports(
    store: &RuntimeStore,
    context: &CallbackContext,
    input: &Input,
    config: crate::provider_callbacks::ManagedToolPortsConfig,
) -> Result<services::ProductionPorts, CallbackError> {
    let reader = store.read()?;
    let admission = crate::message_admission::PreparedAdmission::load(&reader, &context.agent)
        .map_err(CallbackError::from)?;
    let room_directory = if input.params["tool"] == "orchestration_search" {
        Some(crate::sync_entity_context::RoomDirectory::prepare(&reader)?)
    } else {
        None
    };
    Ok(services::ProductionPorts::new(
        services::ProductionPortsConfig {
            context: context.clone(),
            room_directory,
            admission,
            event_wake: config.wake,
            monitor_dispatch_wake: config.monitor_dispatch_wake,
            runtime_ports: config.runtime_ports,
        },
    ))
}

fn execute_once(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    call: &ToolCall,
    record: &mut Value,
    now: f64,
    ports: &impl crate::managed_host_tools::Ports,
) -> Result<(Value, Vec<crate::managed_host_tools::WakeSignal>), CallbackError> {
    put(tx, record)?;
    // A refusal rolls back its effects before recording a not-applied receipt.
    // Store failures roll back claim, effect, result and reply together.
    tx.execute_batch("SAVEPOINT callback_service")?;
    let effect = services::execute(
        tx, snapshot, call.kind, &call.name, &call.args, &call.id, now, ports,
    );
    let (mut value, signals) = match effect {
        Ok(staged) => {
            tx.execute_batch("RELEASE callback_service")?;
            (tool_result(staged.value, true), staged.signals)
        }
        Err(CallbackError::Invalid(message)) => {
            tx.execute_batch("ROLLBACK TO callback_service; RELEASE callback_service")?;
            (tool_result(json!(message), false), Vec::new())
        }
        Err(CallbackError::Dependency(name)) => {
            tx.execute_batch("ROLLBACK TO callback_service; RELEASE callback_service")?;
            (
                tool_result(
                    json!(format!(
                        "{} is unavailable: requires {name}. Do not retry this tool until that service is available.",
                        call.name
                    )),
                    false,
                ),
                Vec::new(),
            )
        }
        Err(error) => return Err(error),
    };
    let finalized = chrono::DateTime::from_timestamp_micros((now * 1_000_000.0).round() as i64)
        .ok_or(CallbackError::Invalid("Invalid callback clock"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    value["contentItems"].as_array_mut().ok_or(CallbackError::Invalid("Invalid tool result"))?
        .push(json!({"type":"inputText","text":format!("[Time awareness] Tool result finalized at {finalized}")}));
    if value["success"] == false {
        admission::reject(record)?;
    } else {
        lifecycle::settle(record, &value, true)?;
    }
    record["result"] = value.clone();
    record["updated"] = json!(now);
    record["finished"] = json!(now);
    put(tx, record)?;
    records::validate("runtime_tool_results", &call.id, value.clone())?;
    tx.execute(
        "INSERT INTO runtime_tool_results(id,result) VALUES(?1,?2)",
        params![call.id, value.to_string()],
    )?;
    Ok((value, signals))
}

fn finalize_dispatched(
    tx: &Transaction<'_>,
    call: &ToolCall,
    record: &mut Value,
    now: f64,
    dispatched: crate::managed_host_tools::DispatchedOperation,
) -> Result<(Value, Vec<crate::managed_host_tools::WakeSignal>), CallbackError> {
    use crate::managed_host_tools::DeliveryCertainty;
    let certainty = match dispatched.certainty {
        DeliveryCertainty::Written => "written",
        DeliveryCertainty::NotSent => "not_sent",
        DeliveryCertainty::Unknown => "unknown",
    };
    record["deliveryCertainty"] = json!(certainty);
    let success = dispatched
        .applied
        .unwrap_or(matches!(dispatched.certainty, DeliveryCertainty::Written));
    let mut value = tool_result(dispatched.result.value, success);
    let finalized = chrono::DateTime::from_timestamp_micros((now * 1_000_000.0).round() as i64)
        .ok_or(CallbackError::Invalid("Invalid callback clock"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    value["contentItems"]
        .as_array_mut()
        .ok_or(CallbackError::Invalid("Invalid tool result"))?
        .push(json!({"type":"inputText","text":format!("[Time awareness] Tool result finalized at {finalized}")}));
    match dispatched.certainty {
        DeliveryCertainty::NotSent => admission::reject(record)?,
        DeliveryCertainty::Unknown => {
            lifecycle::settle(record, &value, true)?;
        }
        DeliveryCertainty::Written if dispatched.applied == Some(false) => {
            admission::reject(record)?
        }
        DeliveryCertainty::Written => {
            lifecycle::settle(record, &value, true)?;
        }
    }
    record["result"] = value.clone();
    record["updated"] = json!(now);
    record["finished"] = json!(now);
    put(tx, record)?;
    records::validate("runtime_tool_results", &call.id, value.clone())?;
    tx.execute(
        "INSERT INTO runtime_tool_results(id,result) VALUES(?1,?2)",
        params![call.id, value.to_string()],
    )?;
    Ok((value, dispatched.result.signals))
}
