//! Durable provider callbacks. Invoke once after journal admission, outside the
//! journal transaction. No provider I/O occurs here; the account worker owns it.
mod cards;
mod model;
mod projection;
mod recovery;
mod storage;
mod tools;
pub use cards::{
    AnswerAuthority, answer, answer_value, expire, prepare_delete, prepare_user_answer, resolve,
};
pub use model::{
    CallbackContext, CallbackDecision, CallbackError, CallbackSnapshot, Reply, ReplyIdentity,
};
pub use projection::project_tool_result;
pub use recovery::recover_connection;
use serde_json::{Value, json};
pub use storage::{DeliveryOutcome, bind_journal, delivery, prepared_replies, retire_checkpointed};
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
    let Some(request) = model::Input::from_frame(frame)? else {
        return Ok(CallbackDecision::Unsupported);
    };
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    if request.method == "item/tool/call" {
        return tools::dispatch(store, context, &request, now);
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
