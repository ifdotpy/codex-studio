//! Callback services plug into this typed dispatch boundary.
use serde_json::{Value, json};
use studio_runtime_providers::{Frame, ResponseBody};
use studio_runtime_store::{RuntimeStore, system_reads};
use tokio::sync::Notify;

pub enum CallbackDecision {
    Answer(ResponseBody),
    Unsupported,
}
pub fn dispatch(_request: &Frame) -> CallbackDecision {
    CallbackDecision::Unsupported
}

/// Resolve a tool callback only when this account worker has one active turn
/// bound to its current provider generation. Tool arguments never supply actor
/// identity. Ambiguous account sessions fail closed.
pub(super) fn managed_response(
    store: &RuntimeStore,
    account: &str,
    generation: u64,
    frame: &Frame,
    now: f64,
    wake: std::sync::Arc<Notify>,
) -> Option<Frame> {
    let Frame::Request(request) = frame else {
        return None;
    };
    if request.method.as_str() != "item/tool/call" {
        return None;
    }
    let Some(thread) = request
        .params
        .as_ref()
        .and_then(|params| params.get("threadId"))
        .and_then(Value::as_str)
    else {
        return Some(error_response(
            request,
            "Managed tool callback has no thread identity",
        ));
    };
    let Some(turn) = request
        .params
        .as_ref()
        .and_then(|params| params.get("turnId"))
        .and_then(Value::as_str)
    else {
        return Some(error_response(
            request,
            "Managed tool callback has no turn identity",
        ));
    };
    let body = match callback_context(store, account, generation, thread, turn) {
        Ok(Some(context)) => {
            // Integration obligation: runtime-server's MonitorRuntime owner
            // must supply its dispatch wake here as well as the account wake,
            // so committed monitor-driver cancellations wake their consumer.
            match crate::provider_callbacks::managed_tool_ports(store, &context, frame, wake)
                .and_then(|ports| {
                    crate::provider_callbacks::dispatch_with_managed_tools(
                        store, &context, frame, now, &ports,
                    )
                }) {
                Ok(crate::provider_callbacks::CallbackDecision::Answer(reply)) => reply.body,
                Ok(
                    crate::provider_callbacks::CallbackDecision::Pending(_)
                    | crate::provider_callbacks::CallbackDecision::Settled(_),
                ) => return None,
                Ok(crate::provider_callbacks::CallbackDecision::Unsupported) => unsupported(),
                Err(error) => {
                    ResponseBody::Error(json!({"code":-32603,"message":error.to_string()}))
                }
            }
        }
        Ok(None) => ResponseBody::Error(json!({
            "code":-32603,
            "message":"Managed tool callback has no unique active turn for this account generation"
        })),
        Err(_) => ResponseBody::Error(
            json!({"code":-32603,"message":"Managed tool callback context is unavailable"}),
        ),
    };
    Some(Frame::Response {
        id: request.id.clone(),
        body,
        fields: Default::default(),
    })
}

fn callback_context(
    store: &RuntimeStore,
    account: &str,
    generation: u64,
    thread: &str,
    turn: &str,
) -> Result<Option<crate::provider_callbacks::CallbackContext>, studio_runtime_store::StoreError> {
    let connection = store.read()?;
    let rows = system_reads::all(&connection, "runtime_agents")?;
    let mut candidates = rows.into_iter().filter(|agent| {
        agent["status"] == "running"
            && agent["autoWake"] == true
            && agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                == account
            && agent
                .pointer("/startAttempt/connectionGeneration")
                .and_then(Value::as_u64)
                == Some(generation)
            && agent["threadId"].as_str() == Some(thread)
            && agent["turnId"].as_str() == Some(turn)
    });
    let Some(agent) = candidates.next() else {
        return Ok(None);
    };
    if candidates.next().is_some() {
        return Ok(None);
    }
    let Some(agent_id) = agent["id"].as_str() else {
        return Ok(None);
    };
    let (Some(epoch), Some(thread), Some(turn), Some(connection)) = (
        agent["epoch"].as_u64(),
        agent["threadId"].as_str(),
        agent["turnId"].as_str(),
        agent
            .pointer("/startAttempt/connectionId")
            .and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    if thread.is_empty() || turn.is_empty() || connection.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::provider_callbacks::CallbackContext {
        agent: agent_id.to_owned(),
        account: account.to_owned(),
        connection: connection.to_owned(),
        generation,
        epoch,
        thread: thread.to_owned(),
        turn: turn.to_owned(),
    }))
}

fn error_response(request: &studio_runtime_providers::RequestFrame, message: &str) -> Frame {
    Frame::Response {
        id: request.id.clone(),
        body: ResponseBody::Error(json!({"code":-32603,"message":message})),
        fields: Default::default(),
    }
}

fn unsupported() -> ResponseBody {
    ResponseBody::Error(
        json!({"code":-32601,"message":"Provider callback is not supported by this runtime","data":{"kind":"unsupported_callback"}}),
    )
}
pub(super) fn response(frame: &Frame) -> Option<Frame> {
    let id = match frame {
        Frame::Request(request) => request.id.clone(),
        Frame::UnknownMethod(request) => {
            serde_json::from_value(request.fields.get("id")?.clone()).ok()?
        }
        _ => return None,
    };
    let body = match dispatch(frame) {
        CallbackDecision::Answer(body) => body,
        CallbackDecision::Unsupported => ResponseBody::Error(
            json!({"code":-32601,"message":"Provider callback is not supported by this runtime","data":{"kind":"unsupported_callback"}}),
        ),
    };
    Some(Frame::Response {
        id,
        body,
        fields: Default::default(),
    })
}
