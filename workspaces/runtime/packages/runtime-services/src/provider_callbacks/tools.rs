use super::{model::*, storage, tool_result};
use crate::tool_requests::{self, admission, lifecycle};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use studio_runtime_providers::{RequestId, ResponseBody};
use studio_runtime_store::{RuntimeStore, system_reads as records};

mod services;
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
pub(super) fn dispatch(
    store: &RuntimeStore,
    context: &CallbackContext,
    input: &Input,
    now: f64,
) -> Result<CallbackDecision, CallbackError> {
    let call = ToolCall::parse(input, context)?;
    if let Some(decision) = store.transaction_with(|tx| {
        storage::saved_decision(tx, &ReplyIdentity::new(context, &input.id), input)
    })? {
        return Ok(decision);
    }
    // Admission commits before any execution. All enabled services are local,
    // transactional writes. External effects need a durable admission adapter.
    store.transaction_with(|tx| {
        storage::snapshot(tx, context)?.active()?;
        let mut candidate = json!({
            "id":call.id, "agent":context.agent, "threadId":context.thread,
            "callId":call.call, "tool":call.name, "signature":call.signature,
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
    store.transaction_with(|tx| {
        let snapshot = storage::snapshot(tx, context)?;
        snapshot.active()?;
        let identity = ReplyIdentity::new(context, &input.id);
        if let Some(decision) = storage::saved_decision(tx, &identity, input)? {
            return Ok(decision);
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
        let result = if let Some(saved) = cached { saved }
        else {
            if record["connectionId"] != context.connection
                || record.pointer("/supervisor/generation").and_then(Value::as_u64) != Some(context.generation) {
                return Err(CallbackError::Stale);
            }
            if admission::claim(&mut record, now)? {
                execute_once(tx, &snapshot, &call, &mut record, now)?
            } else {
                tool_result(json!({"requestId":call.id,"outcome":record["outcome"],"stage":record["stage"],"recovery":"Read orchestration_request with this requestId. Do not repeat the mutation with a new id."}), true)
            }
        };
        let projected = super::project_tool_result(&result, &call.id, &record["outcome"])?;
        let reply = Reply::new(identity, ResponseBody::Result(projected))?;
        storage::save_reply(tx, input, &reply)?;
        let key = reply.identity.key();
        let mut outbox = storage::read(tx, &key)?.ok_or(CallbackError::Unknown)?;
        outbox["toolRequest"] = json!(call.id);
        storage::update(tx, &key, &outbox)?;
        Ok(CallbackDecision::Answer(reply))
    })
}

fn execute_once(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    call: &ToolCall,
    record: &mut Value,
    now: f64,
) -> Result<Value, CallbackError> {
    put(tx, record)?;
    // A refusal rolls back its effects before recording a not-applied receipt.
    // Store failures roll back claim, effect, result and reply together.
    tx.execute_batch("SAVEPOINT callback_service")?;
    let effect = services::execute(
        tx, snapshot, call.kind, &call.name, &call.args, &call.id, now,
    );
    let mut value = match effect {
        Ok(value) => {
            tx.execute_batch("RELEASE callback_service")?;
            tool_result(value, true)
        }
        Err(CallbackError::Invalid(message)) => {
            tx.execute_batch("ROLLBACK TO callback_service; RELEASE callback_service")?;
            tool_result(json!(message), false)
        }
        Err(CallbackError::Dependency(name)) => {
            tx.execute_batch("ROLLBACK TO callback_service; RELEASE callback_service")?;
            tool_result(
                json!(format!(
                    "{} is unavailable: requires {name}. Do not retry this tool until that service is available.",
                    call.name
                )),
                false,
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
    Ok(value)
}
