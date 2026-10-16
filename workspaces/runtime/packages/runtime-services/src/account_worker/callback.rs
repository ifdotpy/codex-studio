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
pub(super) struct ManagedResponseDependencies {
    pub wake: Arc<Notify>,
    pub monitor_dispatch_wake: Arc<Notify>,
    pub managed_runtime: Option<Arc<crate::provider_callbacks::ManagedToolRuntime>>,
}
pub fn dispatch(_request: &Frame) -> CallbackDecision {
    CallbackDecision::Unsupported
}

/// Resolve a Codex VM token refresh only for the actor whose guest emitted it.
/// The resolver owns host account validation and the stable retained refresh ID.
pub(super) fn is_vm_codex_refresh_request(frame: &Frame) -> bool {
    match frame {
        Frame::Request(request) => request.method.as_str() == "account/chatgptAuthTokens/refresh",
        Frame::UnknownMethod(request) => {
            request.method == "account/chatgptAuthTokens/refresh"
                && request.fields.contains_key("id")
        }
        _ => false,
    }
}

pub(super) async fn vm_codex_refresh_response(
    account: &str,
    emitting_actor: Option<&str>,
    actor_epoch: Option<i64>,
    generation: u64,
    frame: &Frame,
    resolver: Option<
        std::sync::Arc<dyn crate::account_worker::preparation::VmProviderLaunchResolver>,
    >,
) -> Option<Frame> {
    if !is_vm_codex_refresh_request(frame) {
        return None;
    }
    let (id, params) = match frame {
        Frame::Request(request) => (request.id.clone(), request.params.as_ref()),
        Frame::UnknownMethod(request) => (
            serde_json::from_value(request.fields.get("id")?.clone()).ok()?,
            request.fields.get("params"),
        ),
        _ => return None,
    };
    let failed = || Frame::Response {
        id: id.clone(),
        body: ResponseBody::Error(json!({
            "code": -32000,
            "message": "Host access token refresh failed. Check the host sign-in. The guest has no refresh token."
        })),
        fields: Default::default(),
    };
    let Some(emitting_actor) = emitting_actor.filter(|actor| !actor.is_empty()) else {
        return Some(failed());
    };
    let Some(actor_epoch) = actor_epoch.filter(|epoch| *epoch >= 0) else {
        return Some(failed());
    };
    let Some(previous_account_id) = params
        .and_then(|params| params.get("previousAccountId"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Some(failed());
    };
    let Some(resolver) = resolver else {
        return Some(failed());
    };
    let result = resolver
        .refresh_codex(
            account.to_owned(),
            previous_account_id.to_owned(),
            emitting_actor.to_owned(),
            actor_epoch,
            generation,
        )
        .await;
    let Ok(payload) = result else {
        return Some(failed());
    };
    if payload.get("chatgptAccountId").and_then(Value::as_str) != Some(previous_account_id)
        || payload
            .get("accessToken")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Some(failed());
    }
    let result = json!({
        "accessToken": payload["accessToken"],
        "chatgptAccountId": payload["chatgptAccountId"],
    });
    Some(Frame::Response {
        id,
        body: ResponseBody::Result(result),
        fields: Default::default(),
    })
}

/// Resolve a tool callback only when this account worker has one active turn
/// bound to its current provider generation. Tool arguments never supply actor
/// identity. Ambiguous account sessions fail closed.
pub(super) async fn managed_response(
    store: &Arc<RuntimeStore>,
    account: &str,
    generation: u64,
    emitting_actor: Option<&str>,
    frame: &Frame,
    now: f64,
    dependencies: ManagedResponseDependencies,
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
    let store_for_context = Arc::clone(store);
    let account_for_context = account.to_owned();
    let actor_for_context = emitting_actor.map(str::to_owned);
    let thread_for_context = thread.to_owned();
    let turn_for_context = turn.to_owned();
    let context = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_callback_context",
        move || {
            callback_context(
                &store_for_context,
                &account_for_context,
                generation,
                actor_for_context.as_deref(),
                &thread_for_context,
                &turn_for_context,
            )
        },
    )
    .await
    .ok();
    let body = match context {
        Some(Ok(Some(context))) => match crate::provider_callbacks::managed_tool_ports_with_bundle(
            store,
            &context,
            frame,
            dependencies.wake,
            dependencies.monitor_dispatch_wake,
            dependencies.managed_runtime.as_deref(),
        ) {
            Ok(ports) => match crate::provider_callbacks::dispatch_with_managed_tools(
                Arc::clone(store),
                &context,
                frame,
                now,
                Arc::new(ports),
            )
            .await
            {
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
            Err(error) => tool_call_failure_body(&error.to_string()),
        },
        Some(Ok(None)) => tool_call_failure_body(
            "Managed tool callback has no unique active turn for this account generation",
        ),
        Some(Err(error)) => tool_call_failure_body(&format!(
            "Managed tool callback context is unavailable: {error}"
        )),
        None => tool_call_failure_body("Managed tool callback context read failed"),
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
    emitting_actor: Option<&str>,
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
    let emitting_actor = emitting_actor.map(str::to_owned);
    let thread = thread.to_owned();
    let turn = turn.to_owned();
    let Some(context) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_callback_context",
        move || {
            callback_context(
                &context_store,
                &account,
                generation,
                emitting_actor.as_deref(),
                &thread,
                &turn,
            )
        },
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
  AND (?5 IS NULL OR id=?5)
LIMIT 2";

fn callback_context(
    store: &RuntimeStore,
    account: &str,
    generation: u64,
    emitting_actor: Option<&str>,
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
            rusqlite::params![account, generation_value, thread, turn, emitting_actor],
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
            plan.query_map(
                params![
                    "account-a",
                    7_i64,
                    "thread-a",
                    "turn-a",
                    Option::<&str>::None
                ],
                |row| row.get::<_, String>(3),
            )
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
            callback_context(&store, "account-a", 7, None, "thread-a", "turn-a")
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
        let context = callback_context(&store, "account-a", 7, None, "thread-a", "turn-a")
            .expect("callback lookup")
            .expect("one matching context");
        assert_eq!(context.agent, "agent-a");
        assert!(
            callback_context(&store, "account-b", 7, None, "thread-a", "turn-a")
                .expect("account-scoped lookup")
                .is_none()
        );
        assert!(
            callback_context(&store, "account-a", 8, None, "thread-a", "turn-a")
                .expect("generation-scoped lookup")
                .is_none()
        );
    }
}

#[cfg(test)]
mod vm_auth_tests {
    use super::*;
    use crate::account_worker::preparation::{
        VmProviderAuthFuture, VmProviderLaunchFuture, VmProviderLaunchRequest,
        VmProviderLaunchResolver,
    };
    use serde_json::Map;
    use std::sync::{Arc, Mutex};
    use studio_runtime_providers::{RequestId, UnknownMethodFrame};

    struct Resolver {
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl VmProviderLaunchResolver for Resolver {
        fn resolve(&self, _request: VmProviderLaunchRequest) -> VmProviderLaunchFuture<'_> {
            Box::pin(async { Err(crate::account_worker::WorkerError::TaskFailed) })
        }

        fn refresh_codex(
            &self,
            account: String,
            previous_account_id: String,
            actor_id: String,
            actor_epoch: i64,
            generation: u64,
        ) -> VmProviderAuthFuture<'_> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .map_err(|_| crate::account_worker::WorkerError::TaskFailed)?
                    .push((
                        format!("{account}:{actor_id}:{actor_epoch}:{generation}"),
                        previous_account_id.clone(),
                    ));
                Ok(json!({
                    "accessToken":"fresh-fixture-token",
                    "chatgptAccountId":previous_account_id,
                    "refreshToken":"test-only-secret"
                }))
            })
        }
    }

    fn refresh_request() -> Frame {
        let fields = serde_json::from_value::<Map<String, Value>>(json!({
            "id": 41,
            "params": {"previousAccountId":"fixture-account"}
        }))
        .expect("valid provider request fields");
        Frame::UnknownMethod(UnknownMethodFrame {
            method: "account/chatgptAuthTokens/refresh".into(),
            fields,
        })
    }

    #[tokio::test]
    async fn vm_codex_refresh_uses_the_emitting_actor_account_scope() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let resolver: Arc<dyn VmProviderLaunchResolver> = Arc::new(Resolver {
            calls: Arc::clone(&calls),
        });
        let response = vm_codex_refresh_response(
            "account-key",
            Some("vm-agent"),
            Some(3),
            7,
            &refresh_request(),
            Some(resolver),
        )
        .await
        .expect("refresh response");
        let Frame::Response { id, body, .. } = response else {
            panic!("expected response frame");
        };
        assert_eq!(id, RequestId::Number(41));
        assert_eq!(
            body,
            ResponseBody::Result(json!({
                "accessToken":"fresh-fixture-token",
                "chatgptAccountId":"fixture-account"
            }))
        );
        assert_eq!(
            *calls.lock().expect("read refresh calls"),
            vec![("account-key:vm-agent:3:7".into(), "fixture-account".into())]
        );
    }

    #[tokio::test]
    async fn vm_codex_refresh_without_an_emitting_actor_fails_closed() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let resolver: Arc<dyn VmProviderLaunchResolver> = Arc::new(Resolver {
            calls: Arc::clone(&calls),
        });
        let response = vm_codex_refresh_response(
            "account-key",
            None,
            None,
            7,
            &refresh_request(),
            Some(resolver),
        )
        .await
        .expect("failure response");
        let Frame::Response {
            id,
            body: ResponseBody::Error(error),
            ..
        } = response
        else {
            panic!("expected refresh error response");
        };
        assert_eq!(id, RequestId::Number(41));
        assert_eq!(error["code"], -32000);
        assert!(calls.lock().expect("read refresh calls").is_empty());
    }

    #[tokio::test]
    async fn vm_codex_refresh_without_a_captured_actor_epoch_fails_closed() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let resolver: Arc<dyn VmProviderLaunchResolver> = Arc::new(Resolver {
            calls: Arc::clone(&calls),
        });
        let response = vm_codex_refresh_response(
            "account-key",
            Some("vm-agent"),
            None,
            7,
            &refresh_request(),
            Some(resolver),
        )
        .await
        .expect("failed response for stale actor");
        assert!(matches!(
            response,
            Frame::Response {
                body: ResponseBody::Error(_),
                ..
            }
        ));
        assert!(calls.lock().expect("read refresh calls").is_empty());
    }
}
