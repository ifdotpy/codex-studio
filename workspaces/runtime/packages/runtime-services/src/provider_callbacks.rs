//! Durable provider callbacks. Invoke once after journal admission, outside the
//! journal transaction. No provider I/O occurs here; the account worker owns it.
mod cards;
mod model;
mod projection;
mod recovery;
mod storage;
pub(crate) mod tools;
pub use cards::{
    AnswerAuthority, answer, answer_value, expire, prepare_delete, prepare_user_answer, resolve,
};
pub use model::{
    CallbackContext, CallbackDecision, CallbackError, CallbackSnapshot, Reply, ReplyIdentity,
};
pub use projection::project_tool_result;
pub use recovery::{recover_connection, unresolved_context_tool_receipts};
use serde_json::{Value, json};
pub use storage::{
    DeliveryOutcome, bind_journal, delivery, prepared_replies, retire_checkpointed,
    submission_ready,
};
use studio_runtime_providers::{Frame, ResponseBody};
use studio_runtime_store::RuntimeStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedCreationAccountError {
    Unknown,
    Deleted,
    Disconnected,
    SignInRequired,
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct ManagedCreationAccountCandidate {
    pub account_key: String,
    pub identity: crate::host_worker_create::AccountIdentity,
    pub ready: bool,
    pub can_attempt_native_proof: bool,
    pub disconnected: bool,
    pub deleted: bool,
    pub duplicate: bool,
}

#[derive(Clone, Debug)]
pub struct ManagedCreationAccountListing {
    pub default_account_key: Option<String>,
    pub accounts: Vec<ManagedCreationAccountCandidate>,
}

/// Narrow account read port used by managed child creation. The runtime server
/// adapts AccountDriver here; callback code does not receive its mutation
/// surface or credential material.
pub trait ManagedCreationAccountRead: Send + Sync {
    fn worker_accounts(&self)
    -> Result<ManagedCreationAccountListing, ManagedCreationAccountError>;

    fn resolve_identity(
        &self,
        account_key: &str,
        parent_account_key: &str,
    ) -> Result<crate::host_worker_create::AccountIdentity, ManagedCreationAccountError>;
}

/// Runtime-owned capabilities injected into managed tool callback services.
#[derive(Clone)]
pub(crate) struct ManagedToolRuntimePorts {
    pub(crate) monitor_dispatch_wake: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) monitor_runtime:
        Option<std::sync::Arc<crate::monitor_runtime_wiring::MonitorRuntime>>,
    pub(crate) account_session:
        Option<std::sync::Arc<dyn crate::account_session_runtime::AccountSessionPort>>,
    pub(crate) account_identity_read: Option<std::sync::Arc<dyn ManagedCreationAccountRead>>,
    pub(crate) lifecycle_ports:
        Option<std::sync::Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>>,
}

/// This is deliberately pure: journal bookkeeping may call it repeatedly.
pub fn is_callback(frame: &Frame) -> bool {
    matches!(frame, Frame::Request(_))
        || matches!(frame, Frame::UnknownMethod(request) if request.fields.contains_key("id"))
}

/// The caller runs this synchronous service on its blocking store bridge, and
/// submits Answer only after this call commits. Pending never waits for a human.
pub fn dispatch(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    now: f64,
) -> Result<CallbackDecision, CallbackError> {
    dispatch_with_managed_tools(
        store,
        context,
        frame,
        now,
        &crate::managed_host_tools::UnavailablePorts,
    )
}

/// Callback entry for a retained host worker that owns the local service
/// bundle. The account worker constructs this path after binding the callback
/// to its live connection, thread and turn context.
pub fn dispatch_with_managed_services(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    now: f64,
    runtime_ports: ManagedToolRuntimePorts,
) -> Result<CallbackDecision, CallbackError> {
    let Some(request) = model::Input::from_frame(frame)? else {
        return Ok(CallbackDecision::Unsupported);
    };
    if request.method == "item/tool/call" {
        return tools::dispatch_with_production_ports(store, context, &request, now, runtime_ports);
    }
    dispatch(store, context, frame, now)
}

/// Construct the real host bundle from the retained account callback path.
/// Filesystem snapshots are prepared before the callback's writer transaction.
#[cfg(test)]
pub(crate) fn managed_tool_ports(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    wake: std::sync::Arc<tokio::sync::Notify>,
) -> Result<impl crate::managed_host_tools::Ports, CallbackError> {
    managed_tool_ports_with_monitor_wake(
        store,
        context,
        frame,
        wake,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )
}

#[cfg(test)]
pub(crate) fn managed_tool_ports_with_monitor_wake(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    wake: std::sync::Arc<tokio::sync::Notify>,
    monitor_dispatch_wake: std::sync::Arc<tokio::sync::Notify>,
) -> Result<impl crate::managed_host_tools::Ports, CallbackError> {
    managed_tool_ports_with_runtime(
        store,
        context,
        frame,
        wake,
        ManagedToolRuntimePorts {
            monitor_dispatch_wake,
            monitor_runtime: None,
            account_session: None,
            account_identity_read: None,
            lifecycle_ports: None,
        },
    )
}

pub(crate) fn managed_tool_ports_with_runtime(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    wake: std::sync::Arc<tokio::sync::Notify>,
    runtime_ports: ManagedToolRuntimePorts,
) -> Result<impl crate::managed_host_tools::Ports, CallbackError> {
    let Some(request) = model::Input::from_frame(frame)? else {
        return Err(CallbackError::Invalid("Expected a managed tool callback"));
    };
    if request.method != "item/tool/call" {
        return Err(CallbackError::Invalid("Expected a managed tool callback"));
    }
    tools::production_ports(store, context, &request, wake, runtime_ports)
}

/// Account workers with a retained host runtime pass its explicit staged-effect
/// bundle here. The bundle is used inside the callback transaction only to
/// write durable intents; managed tool dispatch releases wake signals after
/// the final commit and before returning the decision.
pub(crate) fn dispatch_with_managed_tools<P: crate::managed_host_tools::Ports>(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    now: f64,
    ports: &P,
) -> Result<CallbackDecision, CallbackError> {
    let Some(request) = model::Input::from_frame(frame)? else {
        return Ok(CallbackDecision::Unsupported);
    };
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    if request.method == "item/tool/call" {
        return tools::dispatch_with_ports(store, context, &request, now, ports);
    }
    store.transaction_with(|tx| {
        let identity = ReplyIdentity::new(context, &request.id);
        if let Some(decision) = storage::saved_decision(tx, &identity, &request)? {
            return Ok(decision);
        }
        let result = match request.method.as_str() {
            "currentTime/read" => Some(ResponseBody::Result(json!({"currentTimeAt":now.trunc() as u64}))),
            "account/chatgptAuthTokens/refresh" => Some(ResponseBody::Error(json!({"code":-32601,"message":"Not implemented: requires linux-vm-auth/refresh; use the host account authentication flow"}))),
            method if cards::interactive(method) => {
                let snapshot = storage::snapshot(tx, context)?;
                if let Some(body) = cards::immediate(&snapshot, &request)? {
                    Some(body)
                } else {
                    return cards::admit(tx, context, &snapshot, &request, now);
                }
            }
            _ => None,
        };
        let Some(body) = result else {
            return Ok(CallbackDecision::Unsupported);
        };
        let reply = Reply::new(identity, body)?;
        storage::save_reply(tx, &request, &reply)?;
        Ok(CallbackDecision::Answer(reply))
    })
}

fn tool_result(value: Value, success: bool) -> Value {
    json!({"success":success,"contentItems":[{"type":"inputText","text":match value { Value::String(text)=>text, other=>other.to_string() }}]})
}
