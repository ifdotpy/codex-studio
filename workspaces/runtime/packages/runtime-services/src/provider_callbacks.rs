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
pub use recovery::recover_connection;
use serde_json::{Value, json};
pub use storage::{
    DeliveryOutcome, bind_journal, delivery, prepared_replies, retire_checkpointed,
    submission_ready,
};
use studio_runtime_providers::{Frame, ResponseBody};
use studio_runtime_store::RuntimeStore;

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
) -> Result<CallbackDecision, CallbackError> {
    let Some(request) = model::Input::from_frame(frame)? else {
        return Ok(CallbackDecision::Unsupported);
    };
    if request.method == "item/tool/call" {
        return tools::dispatch_with_production_ports(store, context, &request, now);
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

pub(crate) fn managed_tool_ports_with_monitor_wake(
    store: &RuntimeStore,
    context: &CallbackContext,
    frame: &Frame,
    wake: std::sync::Arc<tokio::sync::Notify>,
    monitor_dispatch_wake: std::sync::Arc<tokio::sync::Notify>,
) -> Result<impl crate::managed_host_tools::Ports, CallbackError> {
    let Some(request) = model::Input::from_frame(frame)? else {
        return Err(CallbackError::Invalid("Expected a managed tool callback"));
    };
    if request.method != "item/tool/call" {
        return Err(CallbackError::Invalid("Expected a managed tool callback"));
    }
    tools::production_ports(store, context, &request, wake, monitor_dispatch_wake)
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
