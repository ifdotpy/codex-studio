//! Human-authorized, durable reconnect of one server on one retained session.
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use studio_runtime_providers::ProviderKind;
use studio_runtime_services::account_session_runtime::ConnectionScope;
use studio_runtime_store::{EventWriteRepository, WorkspaceViewsRepository};

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

pub(super) fn binding(agent: &Value, scope: &ConnectionScope) -> Option<Value> {
    let thread = agent.get("threadId")?.as_str().filter(|v| !v.is_empty())?;
    Some(
        json!({"account":scope.account,"thread":thread,"epoch":agent.get("epoch")?.as_u64()?,
        "connection":scope.connection,"generation":scope.generation}),
    )
}

fn changed() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "The agent session changed. Refresh the tools list.",
    )
}
fn matches_agent(agent: &Value, expected: &crate::generated::McpSessionBinding) -> bool {
    !studio_runtime_store::truthy(agent.get("deletedAt"))
        && agent.get("deleted") != Some(&json!(true))
        && agent.get("threadId").and_then(Value::as_str) == Some(expected.thread.as_str())
        && agent.get("epoch").and_then(Value::as_i64) == Some(expected.epoch)
        && agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            == expected.account
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
    Ok(Json(super::generated_json(result)?))
}

async fn execute(
    state: HttpState,
    body: crate::generated::McpReconnectRequest,
) -> Result<Value, ApiError> {
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
        .find(|a| a["id"] == account)
        .and_then(|account| ProviderKind::of_record(account).ok());
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
    if let Some(replay) = replay {
        return Ok(replay);
    }
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
    )
    .await
    {
        Ok(_) => json!({"status":"completed","message":"Reconnect completed."}),
        Err(error) => {
            // Keep uncertain outcomes explicit and retain the same request ID.
            // The provider adapter reports all failures as typed HTTP errors.
            let (status, message) = (error.status(), error.message());
            json!({"status":if status == StatusCode::CONFLICT {"unknown"} else {"failed"},"message":message})
        }
    };
    let saved = result.clone();
    store
        .transaction_with_error(move |tx| {
            EventWriteRepository::put_receipt(
                tx,
                &format!("{key}:result"),
                &signature,
                &saved.to_string(),
            )
            .map_err(ApiError::stored)
        })
        .await?;
    Ok(result)
}

inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"mcp_reconnect",routes:Some(routes),dependencies:&[]}}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_helpers_preserve_request_types() {
        fn typed_execute<F: Future<Output = Result<Value, ApiError>>>(
            _: fn(HttpState, crate::generated::McpReconnectRequest) -> F,
        ) {
        }
        typed_execute(execute);
        const _: fn(&Value, &crate::generated::McpSessionBinding) -> bool = matches_agent;
    }
}
