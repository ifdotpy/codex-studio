//! Native Codex thread import routes.
//!
//! The committed `AgentResponse` contract omits Python's `importedFrom`
//! response field. The source thread remains persisted on the imported agent;
//! the web client does not read it from the import response.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use studio_runtime_providers::Method;
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionPort, NotSentReason, ProviderRequest, ProviderResponse,
    RequestCertainty,
};
use studio_runtime_store::{AgentId, AgentRepository};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/import", OperationStatus::Implemented, import_list),
        post("/api/import", OperationStatus::Implemented, import_thread),
    ])
}

async fn import_list(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<crate::generated::ImportListResponse>, ApiError> {
    let account = query
        .nonempty_str("account_key")
        .unwrap_or("default")
        .to_owned();
    let cursor = query.nonempty_str("cursor").map(str::to_owned);
    let port = state.account_session.clone().ok_or_else(unavailable)?;
    let scope = port
        .ensure_session(&account)
        .await
        .map_err(|_| unavailable())?;
    let result = provider_request(
        port.as_ref(),
        &account,
        &scope,
        format!("import-list:{}", operation_nonce()),
        "thread/list",
        json!({"limit":50,"cursor":cursor}),
    )
    .await?;
    Ok(Json(super::generated_json::<
        crate::generated::ImportListResponse,
    >(result)?))
}

async fn import_thread(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ImportRequest>,
) -> Result<Json<crate::generated::AgentResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let account = body.account_key.clone().unwrap_or_else(|| "default".into());
    let thread_id = body.thread_id.clone();
    if let Some(id) = body.id.as_ref().and_then(|id| id.as_ref()) {
        let id = id.clone();
        let existing = store
            .read_async("runtime-http/routes/imports", move |saved| {
                AgentRepository::get(saved, &AgentId::from(id.as_str()))
                    .map_err(ApiError::stored)
                    .map(|record| record.map(|record| record.value()))
            })
            .await?;
        if let Some(value) = existing {
            if value.get("importedFrom").and_then(Value::as_str) != Some(&thread_id)
                || value
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    != account
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "This import id belongs to another conversation",
                ));
            }
            return Ok(Json(super::generated_json::<
                crate::generated::AgentResponse,
            >(value)?));
        }
    }
    let port = state.account_session.clone().ok_or_else(unavailable)?;
    let operation_id = body.id.clone().flatten().unwrap_or_else(operation_nonce);
    let scope = port
        .ensure_session(&account)
        .await
        .map_err(|_| unavailable())?;
    let thread = provider_request(
        port.as_ref(),
        &account,
        &scope,
        format!("import:{operation_id}:read"),
        "thread/read",
        json!({"threadId":thread_id,"includeTurns":false}),
    )
    .await?;
    let turns = provider_request(
        port.as_ref(),
        &account,
        &scope,
        format!("import:{operation_id}:turns"),
        "thread/turns/list",
        json!({"threadId":thread_id,"limit":20,"sortDirection":"desc","itemsView":"full"}),
    )
    .await?;
    let thread = thread
        .get("thread")
        .cloned()
        .ok_or_else(ApiError::internal)?;
    let mut visible = Vec::new();
    for turn in turns
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
    {
        for item in turn
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match item.get("type").and_then(Value::as_str) {
                Some("agentMessage") => visible.push(format!(
                    "Assistant: {}",
                    item.get("text").and_then(Value::as_str).unwrap_or("")
                )),
                Some("userMessage") => {
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    visible.push(format!("User: {text}"));
                }
                _ => {}
            }
        }
    }
    if visible.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "No visible messages returned for this thread. Open it in Codex or start a new lead.",
        ));
    }
    let previous_text = visible.join("\n\n");
    let previous_text = previous_text
        .chars()
        .rev()
        .take(24_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let prior = format!(
        "Previous conversation, imported as context (last 20 turns, at most 24000 characters):\n{previous_text}"
    );
    let prompt = format!(
        "{prior}\n\nCurrent user task:\n{}",
        body.prompt
            .clone()
            .flatten()
            .as_deref()
            .unwrap_or("Continue this task.")
    );
    let create = json!({
        "account_key": account,
        "cwd": thread.get("cwd").cloned().unwrap_or(body.cwd.clone().flatten().unwrap_or_default().into()),
        "id": body.id.flatten(),
        "name": "Lead",
        "prompt": prompt,
        "role": "orchestrator",
        "yolo_mode": true
    });
    let mut create = create;
    if let Some(Some(model)) = body.model.clone() {
        create["model"] = json!(model);
    }
    // The source thread id is metadata, never an agent id.
    let Json(created) =
        super::agents_create::create_imported_agent(state, create, thread_id).await?;
    Ok(Json(created))
}

async fn provider_request(
    port: &dyn AccountSessionPort,
    account: &str,
    scope: &studio_runtime_services::account_session_runtime::ConnectionScope,
    operation: String,
    method: &str,
    params: Value,
) -> Result<Value, ApiError> {
    let request = ProviderRequest {
        account: account.to_owned(),
        operation,
        actor: None,
        expected_scope: Some(scope.clone()),
        deleted_actor_session: None,
        method: Method::from_wire(method).ok_or_else(ApiError::internal)?,
        params,
        deadline: Instant::now() + Duration::from_secs(30),
    };
    let receipt = port
        .provider_request(request)
        .await
        .map_err(|error| match error {
            AccountSessionError::Requires(name) => ApiError::dependency(name),
            AccountSessionError::Unknown => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Provider request outcome is unknown",
            ),
            _ => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
        })?;
    match (receipt.certainty, receipt.result) {
        (RequestCertainty::Written, Some(ProviderResponse::Reply(value))) => Ok(value),
        (RequestCertainty::Written, Some(ProviderResponse::Rejected(error))) => {
            Err(ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))
        }
        (RequestCertainty::NotSent, _)
            if receipt.not_sent_reason == Some(NotSentReason::NoLiveAccountWorker) =>
        {
            Err(unavailable())
        }
        (RequestCertainty::NotSent, _) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Provider request was not sent",
        )),
        _ => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Provider request outcome is unknown",
        )),
    }
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "No live provider session for the selected account",
    )
}

fn operation_nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "imports",
        routes: Some(routes),
        dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::FromRequestParts;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use studio_runtime_services::account_session_runtime::{
        AccountSessionFuture, ConnectionScope, SessionReceipt,
    };

    #[derive(Default)]
    struct DormantPort {
        live: AtomicBool,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    impl AccountSessionPort for DormantPort {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            self.live.load(Ordering::SeqCst).then(|| ConnectionScope {
                account: account.into(),
                connection: "fixture".into(),
                generation: 1,
            })
        }

        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> AccountSessionFuture<'a, ConnectionScope> {
            Box::pin(async move {
                self.live.store(true, Ordering::SeqCst);
                Ok(self.current_scope(account).expect("live scope"))
            })
        }

        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'_, SessionReceipt> {
            let scope = self
                .current_scope(&request.account)
                .expect("session must start before the request");
            assert_eq!(request.expected_scope.as_ref(), Some(&scope));
            assert_eq!(request.account, "selected");
            let result = match request.method {
                Method::ThreadList => json!({"data":[],"nextCursor":null}),
                Method::ThreadRead => json!({"thread":{"id":"thread","cwd":"/fixture"}}),
                Method::ThreadTurnsList => json!({"data":[],"nextCursor":null}),
                other => panic!("Unexpected request: {other:?}"),
            };
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            Box::pin(async move {
                Ok(SessionReceipt {
                    scope,
                    operation: request.operation,
                    ticket: None,
                    certainty: RequestCertainty::Written,
                    result: Some(ProviderResponse::Reply(result)),
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }

    async fn fixture() -> (tempfile::TempDir, HttpState, Arc<DormantPort>) {
        let root = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("store"),
        );
        let mut state = HttpState::new(
            "token".into(),
            "fixture".into(),
            root.path().display().to_string(),
            4000,
            None,
            Some(store),
            None,
        );
        let port = Arc::new(DormantPort::default());
        state.account_session = Some(port.clone());
        (root, state, port)
    }

    #[tokio::test]
    async fn import_list_starts_the_selected_dormant_account() {
        let (_root, state, port) = fixture().await;
        let (mut parts, _) = axum::http::Request::new(()).into_parts();
        parts
            .extensions
            .insert(crate::validation::ValidatedRequest {
                body: None,
                query: Arc::new(
                    serde_json::from_value(json!({"account_key":"selected"})).expect("query"),
                ),
                headers: Arc::default(),
            });
        let query = QueryParams::from_request_parts(&mut parts, &())
            .await
            .expect("query");
        let result = import_list(State(state), query).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(port.requests.lock().expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn import_reads_history_after_starting_the_selected_dormant_account() {
        let (_root, state, port) = fixture().await;
        let result = import_thread(
            State(state),
            ApiJson(
                serde_json::from_value(json!({
                    "id":"import-request", "threadId":"thread", "account_key":"selected"
                }))
                .expect("request"),
            ),
        )
        .await;
        let error = result.expect_err("empty native history refuses creation");
        let response = axum::response::IntoResponse::into_response(error);
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "native history validation must run"
        );
        let requests = port.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].operation, "import:import-request:read");
        assert_eq!(requests[1].operation, "import:import-request:turns");
    }
}
