//! Callback services plug into this typed dispatch boundary.
use serde_json::{Value, json};
use std::sync::Arc;
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
    monitor_dispatch_wake: std::sync::Arc<Notify>,
    managed_runtime: Option<std::sync::Arc<crate::provider_callbacks::ManagedToolRuntime>>,
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
        return Some(tool_call_failure(
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
        return Some(tool_call_failure(
            request,
            "Managed tool callback has no turn identity",
        ));
    };
    let body = match callback_context(store, account, generation, thread, turn) {
        Ok(Some(context)) => match crate::provider_callbacks::managed_tool_ports_with_bundle(
            store,
            &context,
            frame,
            wake,
            monitor_dispatch_wake,
            managed_runtime.as_deref(),
        )
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
            Ok(crate::provider_callbacks::CallbackDecision::Unsupported) => {
                tool_call_failure_body("Provider tool is not supported by this runtime")
            }
            Err(error) => tool_call_failure_body(&error.to_string()),
        },
        Ok(None) => tool_call_failure_body(
            "Managed tool callback has no unique active turn for this account generation",
        ),
        Err(error) => tool_call_failure_body(&format!(
            "Managed tool callback context is unavailable: {error}"
        )),
    };
    Some(Frame::Response {
        id: request.id.clone(),
        body,
        fields: Default::default(),
    })
}

pub(super) async fn wait_for_managed_catalog(
    store: &Arc<RuntimeStore>,
    account: &str,
    generation: u64,
    frame: &Frame,
    runtime: Option<&crate::provider_callbacks::ManagedToolRuntime>,
) -> Result<(), &'static str> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    let Frame::Request(request) = frame else {
        return Ok(());
    };
    if request.method.as_str() != "item/tool/call"
        || request
            .params
            .as_ref()
            .and_then(|params| params.get("tool"))
            .and_then(Value::as_str)
            != Some("orchestration_spawn")
    {
        return Ok(());
    }
    let Some(thread) = request
        .params
        .as_ref()
        .and_then(|params| params.get("threadId"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let Some(turn) = request
        .params
        .as_ref()
        .and_then(|params| params.get("turnId"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let context_store = Arc::clone(store);
    let account = account.to_owned();
    let thread = thread.to_owned();
    let turn = turn.to_owned();
    let Some(context) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_callback_context",
        move || callback_context(&context_store, &account, generation, &thread, &turn),
    )
    .await
    .map_err(|_| "Managed tool callback context is unavailable")?
    .map_err(|_| "Managed tool callback context is unavailable")?
    else {
        return Ok(());
    };
    runtime
        .wait_for_catalog(&context.account)
        .await
        .map_err(|_| "provider-session/model-list")
}

const CALLBACK_CONTEXT_QUERY: &str = "SELECT id,record FROM runtime_agents
WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1
  AND json_extract(record,'$.startAttempt.connectionGeneration')=?2
  AND json_extract(record,'$.threadId')=?3
  AND json_extract(record,'$.turnId')=?4
  AND json_extract(record,'$.status')='running'
  AND json_extract(record,'$.autoWake')=1
LIMIT 2";

fn callback_context(
    store: &RuntimeStore,
    account: &str,
    generation: u64,
    thread: &str,
    turn: &str,
) -> Result<Option<crate::provider_callbacks::CallbackContext>, studio_runtime_store::StoreError> {
    let connection = store.read()?;
    let Ok(generation_value) = i64::try_from(generation) else {
        return Ok(None);
    };
    let mut statement = connection.prepare_cached(CALLBACK_CONTEXT_QUERY)?;
    let candidates = statement
        .query_map(
            rusqlite::params![account, generation_value, thread, turn],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    if candidates.len() != 1 {
        return Ok(None);
    }
    let (agent_id, raw) = &candidates[0];
    let agent = system_reads::decode("runtime_agents", agent_id, raw)?;
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

fn unsupported() -> ResponseBody {
    ResponseBody::Error(
        json!({"code":-32601,"message":"Provider callback is not supported by this runtime","data":{"kind":"unsupported_callback"}}),
    )
}

pub(super) fn tool_call_failure(
    request: &studio_runtime_providers::RequestFrame,
    message: &str,
) -> Frame {
    Frame::Response {
        id: request.id.clone(),
        body: tool_call_failure_body(message),
        fields: Default::default(),
    }
}

fn tool_call_failure_body(message: &str) -> ResponseBody {
    ResponseBody::Result(json!({
        "success": false,
        "contentItems": [{"type": "inputText", "text": message}]
    }))
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
        CallbackDecision::Unsupported => unsupported(),
    };
    Some(Frame::Response {
        id,
        body,
        fields: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::{CALLBACK_CONTEXT_QUERY, callback_context};
    use rusqlite::params;
    use serde_json::json;
    use studio_runtime_store::RuntimeStore;

    #[test]
    fn callback_lookup_uses_its_composite_index_and_rejects_ambiguous_rows() {
        let directory = tempfile::tempdir().expect("temporary store");
        let store = RuntimeStore::open(Some(directory.path())).expect("runtime store");
        let connection = store.read().expect("read connection");
        let details = {
            let mut plan = connection
                .prepare(&format!("EXPLAIN QUERY PLAN {CALLBACK_CONTEXT_QUERY}"))
                .expect("explain query");
            plan.query_map(params!["account-a", 7_i64, "thread-a", "turn-a"], |row| {
                row.get::<_, String>(3)
            })
            .expect("query plan")
            .collect::<Result<Vec<_>, _>>()
            .expect("query plan rows")
        };
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("runtime_agent_callback_context")),
            "callback lookup must use its selective index: {details:?}"
        );
        drop(connection);

        let agent = |id: &str| {
            json!({
                "id": id, "rootId": id, "epoch": 1, "status": "running", "autoWake": true,
                "accountKey": "account-a", "threadId": "thread-a", "turnId": "turn-a",
                "startAttempt": {"connectionGeneration": 7, "connectionId": "connection-a"}
            })
        };
        store
            .transaction(|transaction| {
                for id in ["agent-a", "agent-b"] {
                    transaction.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                        params![id, agent(id).to_string()],
                    )?;
                }
                Ok(())
            })
            .expect("callback agents write");
        assert!(
            callback_context(&store, "account-a", 7, "thread-a", "turn-a")
                .expect("callback lookup")
                .is_none(),
            "LIMIT 2 must preserve ambiguous-context rejection"
        );
        store
            .transaction(|transaction| {
                transaction.execute("DELETE FROM runtime_agents WHERE id='agent-b'", [])?;
                Ok(())
            })
            .expect("remove ambiguous agent");
        let context = callback_context(&store, "account-a", 7, "thread-a", "turn-a")
            .expect("callback lookup")
            .expect("one matching context");
        assert_eq!(context.agent, "agent-a");
        assert!(
            callback_context(&store, "account-b", 7, "thread-a", "turn-a")
                .expect("account-scoped lookup")
                .is_none()
        );
        assert!(
            callback_context(&store, "account-a", 8, "thread-a", "turn-a")
                .expect("generation-scoped lookup")
                .is_none()
        );
    }
}
