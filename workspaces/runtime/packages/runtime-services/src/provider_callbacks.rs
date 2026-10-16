//! Durable provider callbacks. Invoke once after journal admission, outside the
//! journal transaction. No provider I/O occurs here; the account worker owns it.
//!
//! Managed monitor input uses the callback's stable tool request identity.
//! HTTP monitor input has no caller identity; its retry contract remains an
//! open owner decision and the HTTP entry point refuses it by name.
mod cards;
mod model;
mod projection;
mod recovery;
mod storage;
pub(crate) mod tools;
pub use crate::managed_child_create::{
    ManagedCreationEnvironment, ManagedCreationEnvironmentError,
};
pub(crate) use cards::answer_signature;
pub use cards::{
    AnswerAuthority, answer, answer_value, expire, pending_native_delete_context, prepare_delete,
    prepare_user_answer, resolve,
};
pub use model::{
    CallbackContext, CallbackDecision, CallbackError, CallbackSnapshot, Reply, ReplyIdentity,
};
pub use projection::project_tool_result;
pub use recovery::{
    ContextToolReceiptSnapshot, prepare_unresolved_context_tool_receipts, recover_connection,
    revalidate_unresolved_context_tool_receipts,
};
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

/// Minimal account identity needed for managed child planning. This deliberately
/// stays independent of the P02 host creation module; the composition adapter
/// converts it at the final integration boundary.
#[derive(Clone, Debug)]
pub struct ManagedAccountIdentity {
    pub provider: Option<String>,
    pub home: Option<String>,
    pub account_id: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ManagedCreationAccountCandidate {
    pub account_key: String,
    pub identity: ManagedAccountIdentity,
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
    ) -> Result<ManagedAccountIdentity, ManagedCreationAccountError>;
}

pub use tools::ManagedToolRuntime;
pub(crate) use tools::ManagedToolRuntimePorts;
#[cfg(test)]
pub(crate) use tools::MonitorInputRequest;
#[cfg(test)]
pub(crate) use tools::{MonitorCancelPort, MonitorInputPort};
pub use tools::{WorkerAccountSelection, select_worker_account, validate_worker_model_choice};

pub(crate) struct ManagedToolPortsConfig {
    pub(crate) wake: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) monitor_dispatch_wake: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) runtime_ports: ManagedToolRuntimePorts,
}

/// This is deliberately pure: journal bookkeeping may call it repeatedly.
pub fn is_callback(frame: &Frame) -> bool {
    matches!(frame, Frame::Request(_))
        || matches!(frame, Frame::UnknownMethod(request) if request.fields.contains_key("id"))
}

/// The caller runs this synchronous service on its blocking store bridge, and
/// submits Answer only after this call commits. Pending never waits for a human.
pub async fn dispatch(
    store: std::sync::Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &Frame,
    now: f64,
) -> Result<CallbackDecision, CallbackError> {
    dispatch_with_managed_tools(
        store,
        context,
        frame,
        now,
        std::sync::Arc::new(crate::managed_host_tools::UnavailablePorts),
    )
    .await
}

/// Construct the real host bundle from the retained account callback path.
/// Filesystem snapshots are prepared before the callback's writer transaction.
#[cfg(test)]
pub(crate) fn managed_tool_ports(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    wake: std::sync::Arc<tokio::sync::Notify>,
) -> Result<impl crate::managed_host_tools::Ports<Prepared = ()> + 'static, CallbackError> {
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
) -> Result<impl crate::managed_host_tools::Ports<Prepared = ()> + 'static, CallbackError> {
    managed_tool_ports_with_runtime(
        store,
        context,
        frame,
        ManagedToolPortsConfig {
            wake,
            monitor_dispatch_wake,
            runtime_ports: ManagedToolRuntimePorts {
                status_watch: None,
                child_creation: None,
                monitor_create: None,
                interrupt: None,
                monitor_input: None,
                monitor_cancel: None,
            },
        },
    )
}

pub(crate) fn managed_tool_ports_with_runtime(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    config: ManagedToolPortsConfig,
) -> Result<impl crate::managed_host_tools::Ports<Prepared = ()> + 'static, CallbackError> {
    let Some(request) = model::Input::from_frame(frame)? else {
        return Err(CallbackError::Invalid("Expected a managed tool callback"));
    };
    if request.method != "item/tool/call" {
        return Err(CallbackError::Invalid("Expected a managed tool callback"));
    }
    tools::production_ports(store, context, &request, config)
}

pub(crate) fn managed_tool_ports_with_bundle(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    wake: std::sync::Arc<tokio::sync::Notify>,
    monitor_dispatch_wake: std::sync::Arc<tokio::sync::Notify>,
    runtime: Option<&ManagedToolRuntime>,
) -> Result<impl crate::managed_host_tools::Ports<Prepared = ()> + 'static, CallbackError> {
    let runtime_ports =
        runtime
            .map(ManagedToolRuntime::ports)
            .unwrap_or_else(|| ManagedToolRuntimePorts {
                status_watch: None,
                child_creation: None,
                monitor_create: None,
                interrupt: None,
                monitor_input: None,
                monitor_cancel: None,
            });
    managed_tool_ports_with_runtime(
        store,
        context,
        frame,
        ManagedToolPortsConfig {
            wake,
            monitor_dispatch_wake,
            runtime_ports,
        },
    )
}

/// Account workers with a retained host runtime pass its explicit staged-effect
/// bundle here. The bundle is used inside the callback transaction only to
/// write durable intents; managed tool dispatch releases wake signals after
/// the final commit and before returning the decision.
pub(crate) async fn dispatch_with_managed_tools(
    store: std::sync::Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &Frame,
    now: f64,
    ports: std::sync::Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>>,
) -> Result<CallbackDecision, CallbackError> {
    let Some(request) = model::Input::from_frame(frame)? else {
        return Ok(CallbackDecision::Unsupported);
    };
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    if request.method == "item/tool/call" {
        return tools::dispatch_with_ports(store, context, &request, now, ports).await;
    }
    let context = context.clone();
    store.transaction_with(move |tx| {
        let identity = ReplyIdentity::new(&context, &request.id);
        if let Some(decision) = storage::saved_decision(tx, &identity, &request)? {
            return Ok(decision);
        }
        let result = match request.method.as_str() {
            "currentTime/read" => Some(ResponseBody::Result(json!({"currentTimeAt":now.trunc() as u64}))),
            "account/chatgptAuthTokens/refresh" => Some(ResponseBody::Error(json!({"code":-32601,"message":"Not implemented: requires linux-vm-auth/refresh; use the host account authentication flow"}))),
            method if cards::interactive(method) => {
                let snapshot = storage::snapshot(tx, &context)?;
                if let Some(body) = cards::immediate(&snapshot, &request)? {
                    Some(body)
                } else {
                    return cards::admit(tx, &context, &snapshot, &request, now);
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
    .await
}

fn tool_result(value: Value, success: bool) -> Value {
    json!({"success":success,"contentItems":[{"type":"inputText","text":match value { Value::String(text)=>text, other=>other.to_string() }}]})
}
