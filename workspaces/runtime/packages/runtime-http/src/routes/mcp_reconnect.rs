//! Human-authorized, durable reconnect of one server on one retained session.
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use studio_runtime_services::account_session_runtime::ConnectionScope;
use studio_runtime_store::{EventWriteRepository, WorkspaceViewsRepository};

use crate::{error::ApiError, router::HttpState};
use super::{extractors::ApiJson, manifest::{OperationStatus, RouteGroup, post}};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post("/api/mcp/reconnect", OperationStatus::Implemented, reconnect)])
}

pub(super) fn binding(agent: &Value, scope: &ConnectionScope) -> Option<Value> {
    let thread = agent.get("threadId")?.as_str().filter(|v| !v.is_empty())?;
    Some(json!({"account":scope.account,"thread":thread,"epoch":agent.get("epoch")?.as_u64()?,
        "connection":scope.connection,"generation":scope.generation}))
}

fn changed() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "The agent session changed. Refresh the tools list.")
}
fn matches_agent(agent: &Value, expected: &Value) -> bool {
    !studio_runtime_store::truthy(agent.get("deletedAt"))
        && agent.get("deleted") != Some(&json!(true))
        && agent.get("threadId") == expected.get("thread")
        && agent.get("epoch") == expected.get("epoch")
        && agent.get("accountKey").and_then(Value::as_str).unwrap_or("default") == expected["account"].as_str().unwrap_or("")
}

async fn reconnect(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::McpReconnectRequest>,
) -> Result<Json<crate::generated::McpReconnectResponse>, ApiError> {
    // The HTTP boundary requires the user's canvas token and a trusted origin.
    // Detach settlement from HTTP cancellation. A crash after admission leaves
    // an unknown receipt, which cannot send the mutation again.
    let body = serde_json::to_value(body).map_err(|_| ApiError::internal())?;
    let result = tokio::spawn(execute(state, body)).await.map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json(result)?))
}

async fn execute(state: HttpState, body: Value) -> Result<Value, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let id = body["id"].as_str().ok_or_else(ApiError::internal)?.to_owned();
    let expected = body["session"].clone();
    let account = expected["account"].as_str().ok_or_else(ApiError::internal)?.to_owned();
    // Account-wide config/mcpServer/reload is not a per-server, per-thread
    // equivalent. Do not broaden a user's selected reconnect into that action.
    let policy = state.account_policy.as_ref().ok_or_else(ApiError::internal)?;
    let accounts = match &state.account_driver {
        Some(driver) => driver.snapshot().map_err(|_| ApiError::internal())?,
        None => policy.accounts(),
    };
    let provider = accounts.accounts.iter().find(|a| a["id"] == account)
        .and_then(|a| a["provider"].as_str()).unwrap_or("codex");
    if provider != "claude" {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "MCP server reconnect is not supported for Codex."));
    }
    let port = state.account_session.clone().ok_or_else(ApiError::internal)?;
    let scope = port.current_scope(&account).ok_or_else(changed)?;
    if expected["connection"] != scope.connection || expected["generation"] != scope.generation {
        return Err(changed());
    }
    let key = format!("mcp-reconnect:{}", body["request_id"].as_str().ok_or_else(ApiError::internal)?);
    let signature = body.to_string();
    let claim_key = key.clone();
    let claim_signature = signature.clone();
    let claim_id = id.clone();
    let claim_expected = expected.clone();
    let replay = store.transaction_with_error(move |tx| {
        let agent = WorkspaceViewsRepository::agent(tx, &claim_id).map_err(ApiError::stored)?
            .ok_or_else(changed)?.value();
        if !matches_agent(&agent, &claim_expected) { return Err(changed()); }
        if let Some(prior) = EventWriteRepository::receipt(tx, &claim_key).map_err(ApiError::stored)? {
            if prior.signature != claim_signature {
                return Err(ApiError::new(StatusCode::CONFLICT, "This reconnect request identity has different content."));
            }
            let saved = EventWriteRepository::receipt(tx, &format!("{claim_key}:result")).map_err(ApiError::stored)?;
            return Ok(Some(match saved {
                Some(saved) => serde_json::from_str(&saved.result).map_err(|_| ApiError::internal())?,
                None => json!({"status":"unknown","message":"Reconnect outcome is unconfirmed. Check this request again; do not start another reconnect."}),
            }));
        }
        EventWriteRepository::put_receipt(tx, &claim_key, &claim_signature, "{\"status\":\"submitted\"}").map_err(ApiError::stored)?;
        Ok(None)
    }).await?;
    if let Some(replay) = replay { return Ok(replay); }
    let ticket = studio_runtime_services::workspace_operations::ProviderTicket {
        operation: key.clone(), agent: id, epoch: expected["epoch"].as_u64().ok_or_else(ApiError::internal)?,
        account, thread: expected["thread"].as_str().map(str::to_owned),
        connection: scope.connection.clone(), generation: scope.generation, request: key.clone(),
    };
    let result = match super::catalog_agent::provider_call(&port, &scope, &ticket, &key,
        "mcpServer/reconnect", json!({"threadId":ticket.thread,"name":body["name"]})).await {
        Ok(_) => json!({"status":"completed","message":"Reconnect completed."}),
        Err(error) => {
            // Keep uncertain outcomes explicit and retain the same request ID.
            // The provider adapter reports all failures as typed HTTP errors.
            let (status, message) = (error.status(), error.message());
            json!({"status":if status == StatusCode::CONFLICT {"unknown"} else {"failed"},"message":message})
        }
    };
    let saved = result.clone();
    store.transaction_with_error(move |tx| {
        EventWriteRepository::put_receipt(tx, &format!("{key}:result"), &signature, &saved.to_string()).map_err(ApiError::stored)
    }).await?;
    Ok(result)
}

inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"mcp_reconnect",routes:Some(routes),dependencies:&[]}}
