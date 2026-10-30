//! Human-authorized, durable reconnect of one server on one retained session.
use crate::generated::{McpReconnectResponse, McpReconnectResponseStatus};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, OwnerKind};
use studio_runtime_services::account_session_runtime::ConnectionScope;
use studio_runtime_services::event_write::{
    CallerAdmission, RequestKey, begin_request, replay_request, settle_request,
};
use studio_runtime_store::WorkspaceViewsRepository;
use studio_runtime_store::outbox::{Outbox, OutboxRow};
use studio_runtime_store::request_results::AdmittedRequest;

#[derive(serde::Serialize, serde::Deserialize)]
struct ReconnectAttempt {
    request: AdmittedRequest,
}
fn unknown() -> McpReconnectResponse {
    McpReconnectResponse { status: McpReconnectResponseStatus::Unknown, message: "Reconnect outcome is unconfirmed. Check this request again; do not start another reconnect.".to_owned() }
}

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/mcp/reconnect",
        OperationStatus::Implemented,
        reconnect,
    )])
}

pub(super) fn binding(agent: &studio_runtime_store::AgentRecord, scope: &ConnectionScope) -> Option<Value> {
    let thread = agent.thread_id.as_deref().filter(|v| !v.is_empty())?;
    Some(
        json!({"account":scope.account,"thread":thread,"epoch":u64::try_from(agent.epoch).ok()?,
        "connection":scope.connection,"generation":scope.generation}),
    )
}

fn changed() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "The agent session changed. Refresh the tools list.",
    )
}
fn matches_agent(agent: &studio_runtime_store::AgentRecord, expected: &crate::generated::McpSessionBinding) -> bool {
    !agent.is_deleted()
        && agent.extra.get("deleted") != Some(&json!(true))
        && agent.thread_id.as_deref() == Some(expected.thread.as_str())
        && agent.epoch == expected.epoch
        && agent.account_key == expected.account
}

async fn reconnect(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::McpReconnectRequest>,
) -> Result<Json<crate::generated::McpReconnectResponse>, ApiError> {
    // The HTTP boundary requires the user's canvas token and a trusted origin.
    // Detach settlement from HTTP cancellation. A crash after admission leaves
    // an unknown receipt, which cannot send the mutation again.
    let result = tokio::spawn(execute(state, body))
        .await
        .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

async fn execute(
    state: HttpState,
    body: crate::generated::McpReconnectRequest,
) -> Result<McpReconnectResponse, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    // Retained receipts use the canonical JSON object encoding, including its
    // key ordering and normalized UUID. Keep those bytes across typed adapters.
    let signature = serde_json::to_value(&body)
        .map_err(|_| ApiError::internal())?
        .to_string();
    let id = body.id;
    let expected = body.session;
    let account = expected.account.clone();
    // Account-wide config/mcpServer/reload is not a per-server, per-thread
    // equivalent. Do not broaden a user's selected reconnect into that action.
    let policy = state
        .account_policy
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let accounts = match &state.account_driver {
        Some(driver) => driver.snapshot().map_err(|_| ApiError::internal())?,
        None => policy.accounts(),
    };
    let provider = accounts
        .accounts
        .iter()
        .find(|a| a.id.as_str() == Some(account.as_str()))
        .and_then(|account| account.provider_kind());
    match provider {
        Some(ProviderKind::Claude) => {}
        Some(ProviderKind::Codex) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "MCP server reconnect is not supported for Codex.",
            ));
        }
        None => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "MCP server reconnect requires a supported account provider.",
            ));
        }
    }
    let port = state
        .account_session
        .clone()
        .ok_or_else(ApiError::internal)?;
    let scope = port.current_scope(&account).ok_or_else(changed)?;
    if scope.account != account
        || expected.connection != scope.connection
        || u64::try_from(expected.generation).ok() != Some(scope.generation)
    {
        return Err(changed());
    }
    let key = format!("mcp-reconnect:{}", body.request_id);
    let claim_key = key.clone();
    let claim_signature = signature.clone();
    let claim_id = id.clone();
    let claim_expected = expected.clone();
    let root = body.request_id.to_string();
    let claim_root = root.clone();
    let now_ms = (studio_runtime_services::event_write::unix_time()
        .map_err(|_| ApiError::internal())?
        * 1000.0) as i64;
    let replay = store
        .transaction_with_error(move |tx| {
            let agent = WorkspaceViewsRepository::agent(tx, &claim_id)
                .map_err(ApiError::stored)?
                .ok_or_else(changed)?;
            if !matches_agent(&agent, &claim_expected) {
                return Err(changed());
            }
            claim_request(
                tx,
                &claim_key,
                &claim_signature,
                &claim_root,
                &claim_id,
                now_ms,
            )
        })
        .await?;
    let admitted = match replay {
        CallerAdmission::Replay(reply) => return Ok(reply),
        CallerAdmission::Fresh(request) => request,
    };
    let ticket = studio_runtime_services::workspace_operations::ProviderTicket {
        operation: key.clone(),
        agent: id,
        epoch: expected
            .epoch
            .try_into()
            .map_err(|_| ApiError::internal())?,
        account,
        thread: Some(expected.thread),
        connection: scope.connection.clone(),
        generation: scope.generation,
        request: key.clone(),
    };
    let result = match super::catalog_agent::provider_call(
        &port,
        &scope,
        &ticket,
        &key,
        "mcpServer/reconnect",
        json!({"threadId":ticket.thread,"name":body.name}),
        (&root, None),
    )
    .await
    {
        Ok(_) => McpReconnectResponse {
            status: McpReconnectResponseStatus::Completed,
            message: "Reconnect completed.".to_owned(),
        },
        Err(error) => {
            // Keep uncertain outcomes explicit and retain the same request ID.
            // The provider adapter reports all failures as typed HTTP errors.
            let (status, message) = (error.status(), error.message());
            McpReconnectResponse {
                status: if status == StatusCode::CONFLICT {
                    McpReconnectResponseStatus::Unknown
                } else {
                    McpReconnectResponseStatus::Failed
                },
                message: message.to_owned(),
            }
        }
    };
    store
        .transaction_with_error(move |tx| {
            settle_request(tx, &admitted, Some(&ticket.agent), &result)?;
            // An unconfirmed provider call remains an obligation for its owner.
            if result.status != McpReconnectResponseStatus::Unknown {
                Outbox::delete_settled(tx, &key, &signature)?;
            }
            Ok::<_, ApiError>(result)
        })
        .await
}

fn claim_request(
    tx: &studio_runtime_store::Transaction<'_>,
    key: &str,
    signature: &str,
    root: &str,
    agent: &str,
    now_ms: i64,
) -> Result<CallerAdmission<McpReconnectResponse>, ApiError> {
    if let Some(saved) = replay_request::<McpReconnectResponse>(tx, key, signature, now_ms)? {
        return Ok(CallerAdmission::Replay(saved.reply));
    }
    if let Some(attempt) = Outbox::get::<ReconnectAttempt>(tx, key)? {
        if attempt.signature != signature || attempt.owner_id != agent {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "This request ID has different content",
            ));
        }
        return Ok(CallerAdmission::Replay(
            replay_request::<McpReconnectResponse>(
                tx,
                attempt.payload.request.id(),
                signature,
                now_ms,
            )?
            .map_or_else(unknown, |saved| saved.reply),
        ));
    }
    let request = match begin_request(
        tx,
        RequestKey::new("mcp-reconnect", &[agent], root)
            .raw(key)
            .operation(&["reconnect"]),
        signature,
        now_ms,
    )? {
        CallerAdmission::Replay(reply) => return Ok(CallerAdmission::Replay(reply)),
        CallerAdmission::Fresh(request) => request,
    };
    Outbox::put(
        tx,
        &OutboxRow {
            id: key.to_owned(),
            kind: OutboxKind::McpReconnect,
            owner_kind: OwnerKind::Agent,
            owner_id: agent.to_owned(),
            request_id: Some(request.id().to_owned()),
            signature: signature.to_owned(),
            status: OutboxStatus::Unknown,
            account: None,
            stream_id: None,
            generation: None,
            ordinal: None,
            next_at: None,
            payload: ReconnectAttempt {
                request: request.clone(),
            },
            created: request.created(),
        },
    )?;
    Ok(CallerAdmission::Fresh(request))
}

inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"mcp_reconnect",routes:Some(routes),dependencies:&[]}}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_non_v7_miss_is_expired() {
        let mut db = studio_runtime_store::Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("schema");
        db.execute_batch(
            "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("owner");
        let tx = db.transaction().expect("transaction");
        let error = match claim_request(
            &tx,
            "mcp-reconnect:old-id",
            "signature",
            "old-id",
            "agent",
            1000,
        ) {
            Err(error) => error,
            Ok(_) => panic!("opaque miss must expire"),
        };
        assert_eq!(error.status(), StatusCode::GONE);
        assert!(error.message().contains("expired"));
    }

    #[test]
    fn reconnect_helpers_preserve_request_types() {
        fn typed_execute<F: Future<Output = Result<McpReconnectResponse, ApiError>>>(
            _: fn(HttpState, crate::generated::McpReconnectRequest) -> F,
        ) {
        }
        typed_execute(execute);
        const _: fn(&studio_runtime_store::AgentRecord, &crate::generated::McpSessionBinding) -> bool = matches_agent;
    }
}
