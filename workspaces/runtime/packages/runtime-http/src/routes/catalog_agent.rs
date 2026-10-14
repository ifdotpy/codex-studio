//! Skills, capabilities, finder and native command operations backed by saved agent state.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use studio_runtime_services::catalog_reads::{
    CapabilityAgentSnapshot, ProviderRequest, append_capability_error, apply_capability_result,
    capabilities_base, host_finder_view, skills_for_cwd,
};
use studio_runtime_store::WorkspaceViewsRepository;
use tokio::time::Instant;

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::{ApiJson, QueryParams};
use crate::routes::manifest::{
    Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post,
};

const CLAUDE_DISCOVERY_DEPENDENCIES: &[Dependency] =
    &[Dependency::new("provider-service/claude-discovery")];
const CAPABILITY_DEPENDENCIES: &[Dependency] = &[
    Dependency::new("provider-service/claude-discovery"),
    Dependency::new("thread-preparation/tool-catalog"),
];
const TOOL_CATALOG_DEPENDENCIES: &[Dependency] =
    &[Dependency::new("thread-preparation/tool-catalog")];
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const NATIVE_COMMAND_DEPENDENCIES: &[Dependency] =
    &[Dependency::new("turn-path/native-command-input")];
const VM_FINDER_DEPENDENCIES: &[Dependency] = &[Dependency::new("vm-agents/finder-view")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/agents/finder-view",
            OperationStatus::partial(VM_FINDER_DEPENDENCIES),
            finder_view,
        ),
        get(
            "/api/capabilities",
            OperationStatus::partial(CAPABILITY_DEPENDENCIES),
            capabilities,
        ),
        post(
            "/api/native-command",
            OperationStatus::partial(NATIVE_COMMAND_DEPENDENCIES),
            native_command,
        ),
        get(
            "/api/skills",
            OperationStatus::partial(CLAUDE_DISCOVERY_DEPENDENCIES),
            skills,
        ),
        post(
            "/api/sync/notify",
            OperationStatus::Implemented,
            sync_notify,
        ),
    ])
}

async fn finder_view(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = params
        .nonempty_str("agent")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Supply an agent ID"))?;
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let store = std::sync::Arc::clone(store);
    let agent_id = agent_id.to_owned();
    let agent = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        WorkspaceViewsRepository::agent(&connection, &agent_id)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent"))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    let record = agent.value();
    if let Some(view) = host_finder_view(&record) {
        return Ok(Json(super::generated_json::<
            crate::generated::FinderViewApiAgentsFinderViewGetResponse,
        >(view)?));
    }
    Err(dependency_error(
        "GET",
        "/api/agents/finder-view",
        VM_FINDER_DEPENDENCIES,
    )?)
}

async fn skills(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = required_agent(&params)?;
    let agent = read_agent(&state, agent_id).await?;
    ensure_live_agent(&agent)?;
    let cwd = agent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(invalid_stored_agent)?;
    let account = account_context(&state, &agent)?;
    ensure_codex(&account, "GET", "/api/skills")?;
    let responses = studio_runtime_services::catalog_reads::provider_exchange(
        state.native_path.as_deref().map(Path::new),
        &account.home,
        account.key != "default",
        vec![ProviderRequest {
            method: "skills/list".to_owned(),
            params: json!({"cwds":[cwd],"forceReload":false}),
        }],
        DISCOVERY_TIMEOUT,
    )
    .await;
    let result = match responses {
        Ok(mut responses) => match responses.pop().unwrap_or(Err(
            studio_runtime_services::catalog_reads::CatalogReadError::ProviderExchangeFailed,
        )) {
            Ok(response) => skills_for_cwd(cwd, &response)
                .unwrap_or_else(|message| json!({"skills":[],"errors":[message]})),
            Err(_) => json!({"skills":[],"errors":["Provider request failed"]}),
        },
        Err(_) => json!({"skills":[],"errors":["Provider request failed"]}),
    };
    Ok(Json(super::generated_json::<
        crate::generated::SkillsResponse,
    >(result)?))
}

async fn capabilities(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = required_agent(&params)?;
    let agent = read_agent(&state, agent_id).await?;
    ensure_live_agent(&agent)?;
    let catalog = match state.tool_catalog.as_ref() {
        Some(catalog) => catalog,
        None => {
            return Err(dependency_error(
                "GET",
                "/api/capabilities",
                TOOL_CATALOG_DEPENDENCIES,
            )?);
        }
    };
    let cwd = agent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(invalid_stored_agent)?;
    if let Some(cached) = state.capabilities_cache.get(agent_id, Instant::now()) {
        return Ok(Json(super::generated_json::<
            crate::generated::CapabilitiesResponse,
        >(cached)?));
    }
    let capability_agent =
        CapabilityAgentSnapshot::from_record(&agent).map_err(|_| invalid_stored_agent())?;
    let managed = studio_runtime_services::catalog_reads::managed_tool_catalog(catalog)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Invalid tool catalog"))?;
    let account = account_context(&state, &agent)?;
    ensure_codex(&account, "GET", "/api/capabilities")?;
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let store = std::sync::Arc::clone(store);
    let observed_agent_id = agent_id.to_owned();
    let observed = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        studio_runtime_store::CapabilityReadsRepository::observed_names(
            &connection,
            &observed_agent_id,
        )
        .map_err(|_| ApiError::internal())
    })
    .await
    .map_err(|_| ApiError::internal())??;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let mut result = capabilities_base(agent_id, &capability_agent, managed, observed, at);
    let cache = std::sync::Arc::clone(&state.capabilities_cache);
    let native_path = state.native_path.clone();
    let home = account.home;
    let isolated = account.key != "default";
    let cwd = cwd.to_owned();
    let thread_id = agent.get("threadId").cloned();
    let result = cache
        .get_or_fetch(agent_id, || async move {
            let responses = studio_runtime_services::catalog_reads::provider_exchange(
                native_path.as_deref().map(Path::new),
                &home,
                isolated,
                vec![
                    ProviderRequest {
                        method: "skills/list".to_owned(),
                        params: json!({"cwds":[cwd],"forceReload":true}),
                    },
                    ProviderRequest {
                        method: "mcpServerStatus/list".to_owned(),
                        params: json!({"threadId":thread_id,"limit":100}),
                    },
                ],
                DISCOVERY_TIMEOUT,
            )
            .await;
            match responses {
                Ok(responses) => {
                    for (index, label) in ["skills", "servers"].into_iter().enumerate() {
                        match responses.get(index) {
                            Some(Ok(response)) => {
                                apply_capability_result(&mut result, label, response)
                            }
                            _ => append_capability_error(
                                &mut result,
                                label,
                                "Provider request failed",
                            ),
                        }
                    }
                }
                Err(_) => {
                    append_capability_error(&mut result, "skills", "Provider request failed");
                    append_capability_error(&mut result, "servers", "Provider request failed");
                }
            }
            result
        })
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(super::generated_json::<
        crate::generated::CapabilitiesResponse,
    >(result)?))
}

async fn native_command(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<NativeCommandRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let store = std::sync::Arc::clone(store);
    let task_id = body.id.clone();
    let task = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        WorkspaceViewsRepository::task(&connection, &task_id)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown task"))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    if task.get("kind").and_then(Value::as_str) != Some("command")
        || task.get("status").and_then(Value::as_str) != Some("running")
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This native command has no active session",
        ));
    }
    task.get("processId")
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|n| n.to_string()))
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "This native command has no active session",
            )
        })?;
    let agent_id = task
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(invalid_stored_task)?
        .to_owned();
    let agent = read_agent(&state, &agent_id).await?;
    ensure_live_agent(&agent)?;
    match body.action {
        NativeCommandAction::Input => {
            body.text.ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "Command input must be text")
            })?;
            Err(dependency_error(
                "POST",
                "/api/native-command",
                NATIVE_COMMAND_DEPENDENCIES,
            )?)
        }
        NativeCommandAction::Cancel => {
            task.get("itemId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(invalid_stored_task)?;
            Err(dependency_error(
                "POST",
                "/api/native-command",
                NATIVE_COMMAND_DEPENDENCIES,
            )?)
        }
    }
}

async fn sync_notify(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    super::sync_stream::notify_resources(state, body).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCommandRequest {
    id: String,
    action: NativeCommandAction,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum NativeCommandAction {
    Input,
    Cancel,
}

fn dependency_error(
    method: &str,
    path: &str,
    dependencies: &[Dependency],
) -> Result<ApiError, ApiError> {
    dependency_unavailable(method, path, dependencies).map_err(|_| ApiError::internal())
}

fn invalid_stored_agent() -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Invalid stored agent")
}

fn invalid_stored_task() -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Invalid stored task")
}

fn required_agent(params: &QueryParams) -> Result<&str, ApiError> {
    params
        .nonempty_str("agent")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Select an agent"))
}

async fn read_agent(state: &HttpState, agent_id: &str) -> Result<Value, ApiError> {
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let store = std::sync::Arc::clone(store);
    let agent_id = agent_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        WorkspaceViewsRepository::agent(&connection, &agent_id)
            .map_err(|_| ApiError::internal())?
            .map(|agent| agent.value())
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent"))
    })
    .await
    .map_err(|_| ApiError::internal())?
}

fn ensure_live_agent(agent: &Value) -> Result<(), ApiError> {
    if agent
        .get("deletedAt")
        .is_some_and(|value| !value.is_null() && value != "")
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This agent was deleted",
        ));
    }
    Ok(())
}

struct AccountContext {
    key: String,
    home: String,
    provider: String,
}

fn account_context(state: &HttpState, agent: &Value) -> Result<AccountContext, ApiError> {
    let key = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let policy = state
        .account_policy
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let snapshot = match &state.account_driver {
        Some(driver) => driver.snapshot().map_err(|_| ApiError::internal())?,
        None => policy.accounts(),
    };
    let account = snapshot
        .accounts
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(key))
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown Codex account"))?;
    if matches!(
        account.get("status").and_then(Value::as_str),
        Some("changed" | "error")
    ) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Account is unavailable",
        ));
    }
    Ok(AccountContext {
        key: key.to_owned(),
        home: account
            .get("home")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        provider: account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_owned(),
    })
}

fn ensure_codex(account: &AccountContext, method: &str, path: &str) -> Result<(), ApiError> {
    if account.provider != "codex" {
        return Err(dependency_error(
            method,
            path,
            CLAUDE_DISCOVERY_DEPENDENCIES,
        )?);
    }
    Ok(())
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "catalog_agent",
        routes: Some(routes),
        dependencies: &[
            "provider-service/claude-discovery",
            "thread-preparation/tool-catalog",
            "native-command/request-identity",
            "vm-agents/finder-view",
        ],
    }
}
