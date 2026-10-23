use super::{model::*, storage, tool_result};
use crate::tool_requests::{self, admission, lifecycle};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use studio_runtime_providers::{RequestId, ResponseBody};
use studio_runtime_store::{RuntimeStore, system_reads as records};

fn dispatch_gate_key(state_dir: &std::path::Path, id: &str) -> String {
    format!("{}:{id}", state_dir.display())
}

/// Hold the in-process dispatch gate of request `id` if no dispatch owns it.
/// `None` means a dispatch in this process still owns the request and will
/// settle it itself; a recovery must leave the row to that owner so the real
/// outcome and its wake signals are recorded exactly once.
pub(super) fn unowned_dispatch(
    state_dir: &std::path::Path,
    id: &str,
) -> Result<Option<tokio::sync::OwnedMutexGuard<()>>, CallbackError> {
    Ok(dispatch_gate(dispatch_gate_key(state_dir, id))?
        .try_lock_owned()
        .ok())
}

fn dispatch_gate(key: String) -> Result<Arc<tokio::sync::Mutex<()>>, CallbackError> {
    static GATES: OnceLock<Mutex<std::collections::HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let gates = GATES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut gates = gates.lock().map_err(|_| CallbackError::Unknown)?;
    gates.retain(|_, gate| gate.strong_count() > 0);
    Ok(gates.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }))
}

mod runtime_dispatch;
mod runtime_ports;
pub(crate) mod services;
#[cfg(test)]
pub(crate) use runtime_dispatch::LatePorts;
pub(crate) use runtime_dispatch::ManagedToolRuntimePorts;
#[cfg(test)]
pub(crate) use runtime_dispatch::MonitorCreatePort;
#[cfg(test)]
pub(crate) use runtime_dispatch::MonitorInputRequest;
#[cfg(test)]
pub(crate) use runtime_dispatch::{ChildCreationPort, InterruptPort};
#[cfg(test)]
pub(crate) use runtime_dispatch::{MonitorCancelPort, MonitorInputPort};
#[cfg(test)]
pub(crate) use runtime_dispatch::{RulesActionResult, StatusWatchPort};
pub use runtime_dispatch::{
    WorkActionOutcome, WorkActionPort, WorkerAccountRequest, WorkerAccountSelection,
    select_worker_account, validate_worker_model_choice,
};
pub use runtime_ports::ManagedToolRuntime;
impl From<tool_requests::ToolRequestError> for CallbackError {
    fn from(error: tool_requests::ToolRequestError) -> Self {
        match error {
            tool_requests::ToolRequestError::Store(error) => Self::Store(error),
            tool_requests::ToolRequestError::Payload(_) => {
                Self::Stored("runtime_tool_requests payload".into())
            }
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
    #[serde(rename = "orchestration_sign_in")]
    SignIn,
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
pub(crate) fn put(tx: &Transaction<'_>, record: &Value) -> Result<(), CallbackError> {
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
pub(super) async fn dispatch_with_ports(
    store: Arc<RuntimeStore>,
    context: &CallbackContext,
    input: &Input,
    now: f64,
    ports: Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>>,
) -> Result<CallbackDecision, CallbackError> {
    let call = ToolCall::parse(input, context)?;
    // A durable claim prevents duplicate execution across restart. Within one
    // process, serialize the same request so a concurrent replay can observe
    // the first request's committed result instead of racing its service call.
    let gate = dispatch_gate(dispatch_gate_key(store.state_dir(), &call.id))?;
    let _gate = gate.lock().await;
    if matches!(call.kind, ToolKind::Unknown) {
        return Ok(CallbackDecision::Unsupported);
    }
    // The complete call identity and arguments commit before a service can act.
    let context_for_admission = context.clone();
    let call_for_admission = call.clone();
    let input_for_admission = input.clone();
    store
        .transaction_with(move |tx| {
            let context = &context_for_admission;
            let call = &call_for_admission;
            let input = &input_for_admission;
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
        })
        .await?;
    enum DispatchState {
        Result(Value),
        Claimed,
        Retry,
    }
    // The callback transaction commits only the execution claim. It is the
    // durable intent boundary: any later dispatcher work is associated with
    // this exact tool request and its complete argument signature.
    let context_for_claim = context.clone();
    let call_for_claim = call.clone();
    let state = store
        .transaction_with(move |tx| {
            let context = &context_for_claim;
            let call = &call_for_claim;
            let snapshot = storage::snapshot(tx, context)?;
            snapshot.active()?;
            let mut record = records::get(tx, "runtime_tool_requests", &call.id)?
                .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
            if record["agent"] != context.agent
                || record["epoch"].as_u64() != Some(context.epoch)
                || record["accountKey"] != context.account
                || record["threadId"] != context.thread
                || record["signature"] != call.signature
            {
                return Err(CallbackError::Stale);
            }
            let cached = records::get(tx, "runtime_tool_results", &call.id)?
                .or_else(|| record.get("result").filter(|v| v.is_object()).cloned());
            if let Some(saved) = cached {
                return Ok(DispatchState::Result(saved));
            }
            if record["connectionId"] != context.connection
                || record
                    .pointer("/supervisor/generation")
                    .and_then(Value::as_u64)
                    != Some(context.generation)
            {
                return Err(CallbackError::Stale);
            }
            if admission::claim(&mut record, now)? {
                put(tx, &record)?;
                Ok(DispatchState::Claimed)
            } else {
                Ok(DispatchState::Retry)
            }
        })
        .await?;
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
                let pending_id = call.id.clone();
                let pending = store
                    .transaction_with(move |tx| {
                        records::get(tx, "runtime_tool_requests", &pending_id)?
                            .ok_or(CallbackError::Invalid("Unknown tool receipt"))
                    })
                    .await?;
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
                // From here the claim is durable. Every exit settles it: a
                // completed dispatch records its result, a failed one records a
                // terminal refusal or uncertainty, and a dropped future (lost
                // connection or retired worker) records uncertainty.
                let guard = ClaimGuard::arm(&store, &call, now);
                let outcome = run_claimed(&store, context, &call, now, &ports).await;
                let settled = match outcome {
                    Ok(done) => Ok(done),
                    Err(failure) => {
                        // The caller still receives the original error; the
                        // settled receipt answers every replay of this request.
                        // A real operation that ran keeps its wake signals.
                        let ClaimFailure { error, operation } = *failure;
                        let signals =
                            record_settlement(&store, &call.id, &call.signature, now, operation)
                                .await;
                        if !signals.is_empty() && ports.release(&signals).is_err() {
                            eprintln!("Could not wake owners of managed tool request {}", call.id);
                        }
                        Err(error)
                    }
                };
                guard.disarm();
                settled?
            }
        }
    };
    // The tool's effect has committed. From here nothing may report failure:
    // the model would retry a command that already ran. A lost wake only
    // delays its owners; their own recovery observes the durable intents.
    if !signals.is_empty() && ports.release(&signals).is_err() {
        eprintln!("Could not wake owners of managed tool request {}", call.id);
    }
    // The durable tool result is the only copy of the answer: a replay of this
    // request rebuilds the same reply from it, so no reply row is kept.
    let call_id = call.id.clone();
    let outcome = store
        .transaction_with(move |tx| {
            records::get(tx, "runtime_tool_requests", &call_id)?
                .map(|record| record["outcome"].clone())
                .ok_or(CallbackError::Invalid("Unknown tool receipt"))
        })
        .await?;
    let projected = super::project_tool_result(&result, &call.id, &outcome)?;
    // Every replay rebuilds the identical reply: its attempt is the tool request.
    let mut identity = ReplyIdentity::new(context, &input.id);
    identity.attempt = call.id;
    Ok(CallbackDecision::Answer(Reply {
        identity,
        body: ResponseBody::Result(projected),
    }))
}

pub(super) async fn production_ports(
    store: &RuntimeStore,
    context: &CallbackContext,
    input: &Input,
    config: crate::provider_callbacks::ManagedToolPortsConfig,
) -> Result<services::ProductionPorts, CallbackError> {
    let agent = context.agent.clone();
    let search = input.params["tool"] == "orchestration_search";
    let admission = store
        .read_async("runtime-services/provider_callbacks/tools", move |reader| {
            crate::message_admission::PreparedAdmission::read(reader, &agent)
                .map_err(CallbackError::from)
        })
        .await?;
    let admission = admission.prepare().await.map_err(CallbackError::from)?;
    let room_directory = if search {
        Some(crate::sync_entity_context::RoomDirectory::read_and_prepare(store).await?)
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
            preparation: config.preparation,
        },
    ))
}

/// A claimed dispatch that ended without its result. `operation` is the
/// terminal evidence to record: proven non-execution or preserved uncertainty.
struct ClaimFailure {
    error: CallbackError,
    operation: crate::managed_host_tools::DispatchedOperation,
}

fn claim_settlement(
    error: CallbackError,
    certainty: crate::managed_host_tools::DeliveryCertainty,
    call: &ToolCall,
) -> Box<ClaimFailure> {
    use crate::managed_host_tools::{DeliveryCertainty, DispatchedOperation, OperationResult};
    let (value, applied) = match certainty {
        DeliveryCertainty::NotSent => (json!(error.to_string()), Some(false)),
        _ => (json!(unknown_outcome(&call.name, &call.id, &error)), None),
    };
    Box::new(ClaimFailure {
        error,
        operation: DispatchedOperation {
            result: OperationResult {
                value,
                signals: Vec::new(),
            },
            certainty,
            applied,
        },
    })
}

fn unknown_outcome(tool: &str, id: &str, cause: &dyn std::fmt::Display) -> String {
    format!(
        "{tool} outcome is unknown: {cause}. Read orchestration_request with requestId {id}. Do not repeat the mutation with a new id."
    )
}

/// A store failure surfaced by the writer can follow a successful commit; a
/// failure returned by the transaction body itself always rolls back.
enum LocalFailure {
    Body(CallbackError),
    Writer(studio_runtime_store::StoreError),
}
impl From<studio_runtime_store::StoreError> for LocalFailure {
    fn from(error: studio_runtime_store::StoreError) -> Self {
        Self::Writer(error)
    }
}

fn check_claim_owner(
    record: &Value,
    context: &CallbackContext,
    call: &ToolCall,
) -> Result<(), CallbackError> {
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
    Ok(())
}

/// Run one claimed request. Ok carries its recorded result; Err carries the
/// terminal evidence the caller records instead.
async fn run_claimed(
    store: &Arc<RuntimeStore>,
    context: &CallbackContext,
    call: &ToolCall,
    now: f64,
    ports: &Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>>,
) -> Result<(Value, Vec<crate::managed_host_tools::WakeSignal>), Box<ClaimFailure>> {
    use crate::managed_host_tools::DeliveryCertainty::{NotSent, Unknown};
    let context_for_actor = context.clone();
    // This transaction only reads; no service has been invoked yet.
    let actor = store
        .transaction_with(move |tx| {
            let snapshot = storage::snapshot(tx, &context_for_actor)?;
            snapshot.active()?;
            crate::managed_host_tools::Actor::from_record(
                snapshot.actor(),
                &context_for_actor.agent,
                context_for_actor.epoch,
            )
            .map_err(services::managed_error)
        })
        .await
        .map_err(|error| claim_settlement(error, NotSent, call))?;
    // Ports classify their own proven pre-write refusals as `NotSent`; any
    // error they still return may follow a write and stays uncertain.
    let dispatched = ports
        .dispatch_managed(
            Arc::clone(store),
            &actor,
            &call.name,
            &call.args,
            &call.id,
            now,
        )
        .await
        .map_err(|error| claim_settlement(services::managed_error(error), Unknown, call))?;
    if let Some(dispatched) = dispatched {
        // The operation already ran, so its receipt is recorded even if the
        // turn ended meanwhile; the record identity still binds this caller.
        let context_for_finish = context.clone();
        let call_for_finish = call.clone();
        let operation = dispatched.clone();
        return store
            .transaction_with(move |tx| {
                let call = &call_for_finish;
                let mut record = records::get(tx, "runtime_tool_requests", &call.id)?
                    .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
                check_claim_owner(&record, &context_for_finish, call)?;
                if let Some(saved) = records::get(tx, "runtime_tool_results", &call.id)? {
                    return Ok((saved, Vec::new()));
                }
                finalize_dispatched(tx, &call.id, &mut record, now, dispatched)
            })
            .await
            .map_err(|error| Box::new(ClaimFailure { error, operation }));
    }
    let payloads = async {
        if matches!(call.kind, ToolKind::Request | ToolKind::Read) {
            let read_context = context.clone();
            let read_call = call.clone();
            let rows = store
                .read_async("runtime-services/callback-payload-rows", move |tx| {
                    let snapshot = storage::snapshot(tx, &read_context)?;
                    snapshot.active()?;
                    let selection = services::payload_selection(
                        tx,
                        &snapshot,
                        read_call.kind,
                        &read_call.args,
                    )?
                    .ok_or(CallbackError::Dependency("tool-request/prepared-payloads"))?;
                    tool_requests::prepared::Rows::read(tx, &read_context.agent, selection)
                        .map_err(CallbackError::from)
                })
                .await?;
            let root = store.state_dir().to_path_buf();
            Ok(Some(
                studio_runtime_store::blocking_pool::spawn_blocking(
                    "runtime-services/callback-payload-io",
                    move || rows.prepare(&root, now),
                )
                .await
                .map_err(|_| CallbackError::Unknown)?
                .map_err(CallbackError::from)?,
            ))
        } else {
            Ok(None)
        }
    }
    .await;
    let context_for_local = context.clone();
    let call_for_local = call.clone();
    let ports_for_local = Arc::clone(ports);
    store
        .transaction_with(move |tx| {
            let context = &context_for_local;
            let call = &call_for_local;
            let body = || {
                let snapshot = storage::snapshot(tx, context)?;
                snapshot.active()?;
                let mut record = records::get(tx, "runtime_tool_requests", &call.id)?
                    .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
                check_claim_owner(&record, context, call)?;
                if let Some(saved) = records::get(tx, "runtime_tool_results", &call.id)? {
                    return Ok((saved, Vec::new()));
                }
                if let Ok(Some(payloads)) = &payloads {
                    let selection =
                        services::payload_selection(tx, &snapshot, call.kind, &call.args)?
                            .ok_or(CallbackError::Dependency("tool-request/prepared-payloads"))?;
                    payloads.revalidate(tx, &context.agent, selection)?;
                }
                execute_once(
                    tx,
                    &snapshot,
                    call,
                    &mut record,
                    now,
                    payloads,
                    ports_for_local.as_ref(),
                )
            };
            body().map_err(LocalFailure::Body)
        })
        .await
        .map_err(|failure| failure.settlement(call))
}

impl LocalFailure {
    /// A rolled-back body proves nothing was applied; a writer failure may
    /// follow a commit, so its outcome stays unknown.
    fn settlement(self, call: &ToolCall) -> Box<ClaimFailure> {
        use crate::managed_host_tools::DeliveryCertainty::{NotSent, Unknown};
        match self {
            Self::Body(error) => claim_settlement(error, NotSent, call),
            Self::Writer(error) => claim_settlement(CallbackError::Store(error), Unknown, call),
        }
    }
}

/// Record terminal evidence for a claim that is still running without a
/// result. A result recorded first wins, so this never overwrites an outcome.
fn settle_claim(
    tx: &Transaction<'_>,
    id: &str,
    signature: &str,
    now: f64,
    operation: crate::managed_host_tools::DispatchedOperation,
) -> Result<(Value, Vec<crate::managed_host_tools::WakeSignal>), CallbackError> {
    if let Some(saved) = records::get(tx, "runtime_tool_results", id)? {
        return Ok((saved, Vec::new()));
    }
    let mut record = records::get(tx, "runtime_tool_requests", id)?
        .ok_or(CallbackError::Invalid("Unknown tool receipt"))?;
    if record["signature"] != signature {
        return Err(CallbackError::Stale);
    }
    if record["stage"] != "running" || record["outcome"] != "pending" {
        return record
            .get("result")
            .filter(|value| value.is_object())
            .cloned()
            .map(|value| (value, Vec::new()))
            .ok_or(CallbackError::Unknown);
    }
    finalize_dispatched(tx, id, &mut record, now, operation)
}

/// Returns the wake signals of the recorded operation, if it recorded one.
/// A failed settlement leaves the claim running; startup recovery owns it then.
async fn record_settlement(
    store: &RuntimeStore,
    id: &str,
    signature: &str,
    now: f64,
    operation: crate::managed_host_tools::DispatchedOperation,
) -> Vec<crate::managed_host_tools::WakeSignal> {
    let (id_for_tx, signature) = (id.to_owned(), signature.to_owned());
    match store
        .transaction_with(move |tx| settle_claim(tx, &id_for_tx, &signature, now, operation))
        .await
    {
        Ok((_, signals)) => signals,
        Err(error) => {
            eprintln!("Could not settle managed tool request {id}: {error}");
            Vec::new()
        }
    }
}

/// Settles an abandoned claim as uncertain when the dispatch future is dropped
/// before it records an outcome, for example when its worker is retired after
/// a lost connection. Startup recovery covers a process that exits instead.
struct ClaimGuard {
    armed: Option<(Arc<RuntimeStore>, String, String, String, f64)>,
}
impl ClaimGuard {
    fn arm(store: &Arc<RuntimeStore>, call: &ToolCall, now: f64) -> Self {
        Self {
            armed: Some((
                Arc::clone(store),
                call.id.clone(),
                call.signature.clone(),
                call.name.clone(),
                now,
            )),
        }
    }
    fn disarm(mut self) {
        self.armed = None;
    }
}
impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let Some((store, id, signature, tool, now)) = self.armed.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let operation = crate::managed_host_tools::DispatchedOperation {
                result: crate::managed_host_tools::OperationResult {
                    value: json!(unknown_outcome(
                        &tool,
                        &id,
                        &"the callback ended before its result was recorded"
                    )),
                    signals: Vec::new(),
                },
                certainty: crate::managed_host_tools::DeliveryCertainty::Unknown,
                applied: None,
            };
            record_settlement(&store, &id, &signature, now, operation).await;
        });
    }
}

fn execute_once(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    call: &ToolCall,
    record: &mut Value,
    now: f64,
    payloads: Result<Option<tool_requests::prepared::Payloads>, CallbackError>,
    ports: &(impl crate::managed_host_tools::Ports<Prepared = ()> + ?Sized),
) -> Result<(Value, Vec<crate::managed_host_tools::WakeSignal>), CallbackError> {
    put(tx, record)?;
    // A refusal rolls back its effects before recording a not-applied receipt.
    // Store failures roll back claim, effect, result and reply together.
    tx.execute_batch("SAVEPOINT callback_service")?;
    let effect = payloads.and_then(|payloads| {
        services::execute(
            tx,
            snapshot,
            call.kind,
            &call.name,
            &call.args,
            &call.id,
            now,
            payloads.as_ref(),
            ports,
        )
    });
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

/// Python `codex_server_exec.output_expiry`: the command expiry in a server
/// receipt or an output-reference page.
fn output_expiry(value: &Value) -> Option<f64> {
    if let Some(expires) = value.get("outputExpiresAt").filter(|v| v.is_number())
        && (value.get("handle").is_some() || value.get("outputRef").is_some())
    {
        return expires.as_f64();
    }
    ["value", "result"].into_iter().find_map(|field| {
        value
            .get(field)
            .filter(|nested| nested.is_object())
            .and_then(output_expiry)
    })
}

fn finalize_dispatched(
    tx: &Transaction<'_>,
    id: &str,
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
    // Python `dynamic`: a command page keeps its own expiry on the tool result.
    let output_expires = matches!(
        record["tool"].as_str(),
        Some("orchestration_servers" | "orchestration_read")
    )
    .then(|| output_expiry(&dispatched.result.value))
    .flatten();
    let mut value = tool_result(dispatched.result.value, success);
    if let Some(expires) = output_expires {
        value["serverOutputExpiresAt"] = json!(expires);
        tx.execute(
            "INSERT OR IGNORE INTO runtime_server_output_tools VALUES (?1,?2)",
            params![id, expires],
        )?;
    }
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
    records::validate("runtime_tool_results", id, value.clone())?;
    tx.execute(
        "INSERT INTO runtime_tool_results(id,result) VALUES(?1,?2)",
        params![id, value.to_string()],
    )?;
    Ok((value, dispatched.result.signals))
}

#[cfg(test)]
mod local_failure_tests {
    use super::{CallbackError, LocalFailure, ToolCall, ToolKind};
    use crate::managed_host_tools::DeliveryCertainty;
    use serde_json::json;

    fn call() -> ToolCall {
        ToolCall {
            name: "orchestration_search".into(),
            kind: ToolKind::Search,
            args: json!({}),
            id: "thread:call".into(),
            call: "call".into(),
            prefix: "thread:".into(),
            signature: "signature".into(),
            alias: None,
        }
    }

    #[test]
    fn writer_failure_that_may_hide_a_commit_settles_unknown() {
        let failure = LocalFailure::Writer(studio_runtime_store::StoreError::RecordContract(
            "commit outcome unknown",
        ))
        .settlement(&call());
        assert_eq!(failure.operation.certainty, DeliveryCertainty::Unknown);
        assert_eq!(failure.operation.applied, None);
        let text = failure
            .operation
            .result
            .value
            .as_str()
            .expect("unknown outcome text");
        assert!(text.contains("outcome is unknown"), "{text}");
    }

    #[test]
    fn rolled_back_body_settles_not_applied() {
        let failure = LocalFailure::Body(CallbackError::Invalid("refused")).settlement(&call());
        assert_eq!(failure.operation.certainty, DeliveryCertainty::NotSent);
        assert_eq!(failure.operation.applied, Some(false));
    }
}
