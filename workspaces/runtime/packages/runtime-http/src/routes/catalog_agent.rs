//! Skills, capabilities, finder and native command operations backed by saved agent state.
use studio_runtime_schema::shared_values::{
    NATIVE_TERMINAL_ITEMS_PER_PAGE, NATIVE_TERMINAL_MAX_PAGES,
};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant as StdInstant};
use studio_runtime_services::catalog_reads::{
    CapabilityAgentSnapshot, ProviderRequest, append_capability_error, apply_capability_result,
    capabilities_base, host_finder_view, skills_for_cwd,
};
use studio_runtime_store::WorkspaceViewsRepository;
use tokio::time::Instant;

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::{ApiJson, QueryParams};
use crate::routes::manifest::{OperationStatus, RouteGroup, get, post};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/agents/finder-view",
            OperationStatus::Implemented,
            finder_view,
        ),
        get(
            "/api/capabilities",
            OperationStatus::Implemented,
            capabilities,
        ),
        post(
            "/api/native-command",
            OperationStatus::Implemented,
            native_command,
        ),
        get("/api/skills", OperationStatus::Implemented, skills),
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
) -> Result<Json<crate::generated::FinderViewApiAgentsFinderViewGetResponse>, ApiError> {
    let agent_id = params
        .nonempty_str("agent")
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Supply an agent ID"))?;
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let store = std::sync::Arc::clone(store);
    let agent_id = agent_id.to_owned();
    let agent = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/catalog_agent",
        move || {
            let connection = store.read().map_err(|_| ApiError::internal())?;
            WorkspaceViewsRepository::agent(&connection, &agent_id)
                .map_err(|_| ApiError::internal())?
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent"))
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let record = agent.value().clone();
    let state_dir = state.vm_state_dir.clone();
    let view = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/catalog_agent",
        move || {
            Ok::<_, ApiError>(host_finder_view(&record).unwrap_or_else(|| {
                studio_runtime_services::catalog_reads::vm_finder_view(&state_dir, &record)
            }))
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::FinderViewApiAgentsFinderViewGetResponse,
    >(view)?))
}

async fn account_provider_exchange(
    session: Option<
        std::sync::Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    >,
    account: AccountContext,
    requests: Vec<ProviderRequest>,
    timeout: Duration,
) -> Result<
    Vec<Result<Value, studio_runtime_services::catalog_reads::CatalogReadError>>,
    studio_runtime_services::catalog_reads::CatalogReadError,
> {
    use studio_runtime_services::catalog_reads::CatalogReadError;
    if !account.known || !account.available {
        return Err(CatalogReadError::ProviderExchangeFailed);
    }
    // Both providers answer catalog reads over the account's retained session;
    // no read launches a provider process of its own.
    let session = session.ok_or(CatalogReadError::ProviderExchangeFailed)?;
    session
        .ensure_session(&account.key)
        .await
        .map_err(|_| CatalogReadError::ProviderExchangeFailed)?;
    studio_runtime_services::catalog_reads::account_session_exchange(
        session.as_ref(),
        &account.key,
        requests,
        timeout,
    )
    .await
}

async fn skills(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<crate::generated::SkillsResponse>, ApiError> {
    let agent_id = required_agent(&params)?;
    let agent = read_agent(&state, agent_id).await?;
    ensure_live_agent(&agent)?;
    let cwd = agent
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(invalid_stored_agent)?;
    let account = account_context(&state, &agent)?;
    let responses = account_provider_exchange(
        state.account_session.clone(),
        account,
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
) -> Result<Json<crate::generated::CapabilitiesResponse>, ApiError> {
    let agent_id = required_agent(&params)?;
    let agent = read_agent(&state, agent_id).await?;
    ensure_live_agent(&agent)?;
    let catalog = match state.tool_catalog.as_ref() {
        Some(catalog) => catalog,
        None => return Err(ApiError::internal()),
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
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let store = std::sync::Arc::clone(store);
    let observed_agent_id = agent_id.to_owned();
    let observed = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/catalog_agent",
        move || {
            let connection = store.read().map_err(|_| ApiError::internal())?;
            studio_runtime_store::CapabilityReadsRepository::observed_names(
                &connection,
                &observed_agent_id,
            )
            .map_err(|_| ApiError::internal())
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let mut result = capabilities_base(agent_id, &capability_agent, managed, observed, at);
    let cache = std::sync::Arc::clone(&state.capabilities_cache);
    let session = state.account_session.clone();
    let cwd = cwd.to_owned();
    let thread_id = agent.get("threadId").cloned();
    let result = cache
        .get_or_fetch(agent_id, || async move {
            let responses = account_provider_exchange(
                session,
                account,
                vec![
                    ProviderRequest {
                        method: "skills/list".to_owned(),
                        params: json!({"cwds":[cwd],"forceReload":true}),
                    },
                    ProviderRequest {
                        method: "mcpServerStatus/list".to_owned(),
                        params: json!({"threadId":thread_id,"limit":NATIVE_TERMINAL_ITEMS_PER_PAGE}),
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
    // Python codex_workspace.py:2183-2217 uses per-invocation semantics and
    // does not bind this action to a caller request ID. This ticket is
    // internal to this attempt, so a later HTTP invocation is a new action.
    let request_id = uuid::Uuid::new_v4().to_string();
    let store = std::sync::Arc::clone(store);
    match body.action {
        NativeCommandAction::Input => {
            let text = body.text.ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "Command input must be text")
            })?;
            let admission_override = state.message_admission.clone();
            let admission_for_read = admission_override.clone();
            let store_for_read = std::sync::Arc::clone(&store);
            let task_id = body.id;
            let (agent_id, snapshot, prepared) =
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-http/routes/catalog_agent",
                    move || {
                        let connection = store_for_read.read().map_err(ApiError::stored)?;
                        let task = WorkspaceViewsRepository::task(&connection, &task_id)
                            .map_err(ApiError::stored)?
                            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown task"))?;
                        let agent_id = task
                            .get("agent")
                            .and_then(Value::as_str)
                            .ok_or_else(invalid_stored_task)?
                            .to_owned();
                        let agent = WorkspaceViewsRepository::agent(&connection, &agent_id)
                            .map_err(ApiError::stored)?
                            .ok_or_else(|| {
                                ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent")
                            })?;
                        let snapshot =
                            studio_runtime_services::native_actions::CommandSnapshot::from_records(
                                task,
                                agent.value(),
                            )
                            .map_err(native_command_error)?;
                        let prepared = admission_for_read
                            .is_none()
                            .then(|| {
                                studio_runtime_services::message_admission::PreparedAdmission::load(
                                    &connection,
                                    &agent_id,
                                )
                            })
                            .transpose()
                            .map_err(|_| ApiError::internal())?;
                        drop(connection);
                        Ok::<_, ApiError>((agent_id, snapshot, prepared))
                    },
                )
                .await
                .map_err(|_| ApiError::internal())??;
            let admission: std::sync::Arc<
                dyn studio_runtime_services::message_admission::MessageAdmission,
            > = match admission_override {
                Some(admission) => admission,
                None => std::sync::Arc::new(prepared.ok_or_else(ApiError::internal)?),
            };
            let posted = store
                .transaction_with_error(move |tx| {
                    let enqueue = NativeCommandEnqueue {
                        room: &agent_id,
                        admission: admission.as_ref(),
                    };
                    studio_runtime_services::native_actions::command_input(
                        tx,
                        &request_id,
                        &snapshot,
                        &text,
                        &enqueue,
                    )
                    .map_err(native_command_error)
                })
                .await
                .map_err(|_| ApiError::internal())?;
            state.event_wake_handle().notify_one();
            let result = posted;
            Ok(Json(result))
        }
        NativeCommandAction::Cancel => native_command_cancel(state, body.id, request_id)
            .await
            .map(Json),
    }
}

async fn native_command_cancel(
    state: HttpState,
    task_id: String,
    request_id: String,
) -> Result<Value, ApiError> {
    // Match codex_workspace.py:2194-2210: verify the exact item/process pair
    // in the provider's terminal list before sending terminate.
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (task, agent) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/catalog_agent",
        move || {
            let connection = store.read().map_err(ApiError::stored)?;
            let task = WorkspaceViewsRepository::task(&connection, &task_id)
                .map_err(ApiError::stored)?
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown task"))?;
            let agent_id = task
                .get("agent")
                .and_then(Value::as_str)
                .ok_or_else(invalid_stored_task)?;
            let agent = WorkspaceViewsRepository::agent(&connection, agent_id)
                .map_err(ApiError::stored)?
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?;
            Ok::<_, ApiError>((task, agent.value()))
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let snapshot = studio_runtime_services::native_actions::CommandSnapshot::from_records(
        task.clone(),
        agent.clone(),
    )
    .map_err(native_command_error)?;
    let account = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let port = state
        .account_session
        .clone()
        .ok_or_else(|| ApiError::dependency("provider-session/native-request"))?;
    let scope = port
        .ensure_session(&account)
        .await
        .map_err(|_| ApiError::dependency("provider-session/account-session"))?;
    if scope.account != account {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "The native account session changed",
        ));
    }
    let ticket = studio_runtime_services::workspace_operations::ProviderTicket {
        operation: task
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(invalid_stored_task)?
            .to_owned(),
        agent: agent
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(invalid_stored_agent)?
            .to_owned(),
        epoch: agent
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_stored_agent)?,
        account: account.clone(),
        thread: agent
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        connection: scope.connection.clone(),
        generation: scope.generation,
        request: request_id.clone(),
    };
    let mut params = json!({"threadId":ticket.thread,"limit":NATIVE_TERMINAL_ITEMS_PER_PAGE});
    let mut pages = Vec::new();
    let mut cursor_seen = std::collections::HashSet::new();
    let mut list_key = format!("{}:list", request_id);
    for page_number in 0..NATIVE_TERMINAL_MAX_PAGES {
        let page = provider_call(
            &port,
            &scope,
            &ticket,
            &list_key,
            "thread/backgroundTerminals/list",
            params.clone(),
        )
        .await?;
        let Some(entries) = page.get("data").and_then(Value::as_array) else {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Invalid native terminal list",
            ));
        };
        if entries.len() > NATIVE_TERMINAL_ITEMS_PER_PAGE {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Invalid native terminal list",
            ));
        }
        pages.extend(entries.iter().cloned());
        let Some(cursor) = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        else {
            break;
        };
        if page_number + 1 == NATIVE_TERMINAL_MAX_PAGES || !cursor_seen.insert(cursor.to_owned()) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Native terminal listing did not advance",
            ));
        }
        params["cursor"] = json!(cursor);
        list_key = format!(
            "{}:list:{}",
            request_id,
            studio_runtime_store::sha256_hex(cursor.as_bytes())
        );
    }
    let listed = json!({"data":pages});
    let mut list_ticket = ticket.clone();
    list_ticket.request = format!("{}:list", request_id);
    let list_receipt = studio_runtime_services::native_actions::TransportReceipt {
        ticket: list_ticket,
        delivery: studio_runtime_services::native_actions::Delivery::Written(listed),
    };
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let claim = store
        .transaction_with(move |tx| {
            studio_runtime_services::native_actions::command_cancel_claim(
                tx,
                &snapshot,
                &ticket,
                &list_receipt,
            )
            .map_err(native_command_error)
        })
        .await
        .map_err(|_| ApiError::internal())?;
    let request = match claim {
        studio_runtime_services::native_actions::CommandCancelClaim::Replay(result) => {
            return Ok(result);
        }
        studio_runtime_services::native_actions::CommandCancelClaim::Send(request) => request,
    };
    let result = provider_call(
        &port,
        &scope,
        &request.ticket,
        &request.ticket.request,
        &request.method,
        request.params.clone(),
    )
    .await?;
    if result.get("terminated") != Some(&Value::Bool(true)) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Codex did not confirm that the command stopped",
        ));
    }
    let receipt = studio_runtime_services::native_actions::TransportReceipt {
        ticket: request.ticket,
        delivery: studio_runtime_services::native_actions::Delivery::Written(result),
    };
    let store = state.store.ok_or_else(ApiError::internal)?;
    store
        .transaction_with(move |tx| {
            studio_runtime_services::native_actions::command_cancel_settle(tx, &receipt)
                .map_err(native_command_error)
        })
        .await
        .map_err(|_| ApiError::internal())
}

pub(super) async fn provider_call(
    port: &std::sync::Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    scope: &studio_runtime_services::account_session_runtime::ConnectionScope,
    ticket: &studio_runtime_services::workspace_operations::ProviderTicket,
    operation: &str,
    method: &str,
    params: Value,
) -> Result<Value, ApiError> {
    use studio_runtime_services::account_session_runtime::{
        ProviderRequest, ProviderResponse, RequestCertainty,
    };
    let method_value = studio_runtime_providers::Method::from_wire(method).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Unsupported native session operation",
        )
    })?;
    let actor = studio_runtime_services::account_worker::retained::RequestActor {
        agent: ticket.agent.clone(),
        epoch: i64::try_from(ticket.epoch).map_err(|_| ApiError::internal())?,
    };
    let port = std::sync::Arc::clone(port);
    let expected_scope = scope.clone();
    let account = ticket.account.clone();
    let operation = operation.to_owned();
    let expected_operation = operation.clone();
    let receipt = port
        .provider_request(ProviderRequest {
            account,
            operation: operation.clone(),
            actor: Some(actor),
            expected_scope: Some(expected_scope.clone()),
            deleted_actor_session: None,
            method: method_value,
            params,
            deadline: StdInstant::now() + Duration::from_secs(20),
        })
        .await
        .map_err(|error| match error {
            studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                name,
            ) => ApiError::dependency(name),
            studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                message,
            ) => ApiError::new(StatusCode::BAD_REQUEST, message),
            _ => ApiError::new(
                StatusCode::CONFLICT,
                "Native request outcome is unknown; inspect its receipt before retrying",
            ),
        })?;
    if receipt.operation != expected_operation
        || receipt.scope != expected_scope
        || receipt.ticket.as_ref().is_some_and(|issued| {
            issued.generation != expected_scope.generation || issued.method != method_value
        })
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Native provider receipt belongs to a different session",
        ));
    }
    match (receipt.certainty, receipt.result) {
        (RequestCertainty::Written, Some(ProviderResponse::Reply(value))) => Ok(value),
        (RequestCertainty::Written, Some(ProviderResponse::Rejected(_))) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Native provider rejected the request",
        )),
        (RequestCertainty::NotSent, _) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Native provider request was not sent",
        )),
        _ => Err(ApiError::new(
            StatusCode::CONFLICT,
            "Native request outcome is unknown; inspect its receipt before retrying",
        )),
    }
}

struct NativeCommandEnqueue<'a> {
    room: &'a str,
    admission: &'a dyn studio_runtime_services::message_admission::MessageAdmission,
}

impl studio_runtime_services::native_actions::CommandAdmission for NativeCommandEnqueue<'_> {
    fn enqueue(
        &self,
        tx: &rusqlite::Transaction<'_>,
        _agent: &Value,
        key: &str,
        instruction: &str,
    ) -> Result<Value, studio_runtime_services::native_actions::NativeError> {
        let result = studio_runtime_services::coordination::post_message_with_admission(
            tx,
            studio_runtime_services::coordination::PostManagedMessage {
                room: self.room,
                id: key,
                text: instruction,
                delivery: "queue",
                assets: &[],
            },
            self.admission,
        )
        .map_err(|_| {
            studio_runtime_services::native_actions::NativeError::Unavailable(
                "workspace-policy/message-admission",
            )
        })?;
        Ok(result.receipt)
    }
}

fn native_command_error(error: studio_runtime_services::native_actions::NativeError) -> ApiError {
    use studio_runtime_services::native_actions::NativeError;
    match error {
        NativeError::Invalid(_) | NativeError::Changed => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This native command has no active session",
        ),
        NativeError::Conflict => ApiError::new(
            StatusCode::CONFLICT,
            "This command request identity has different content",
        ),
        NativeError::Unknown => ApiError::new(
            StatusCode::CONFLICT,
            "The command action outcome is unknown",
        ),
        NativeError::Unavailable(dependency) => ApiError::dependency(dependency),
        NativeError::InvalidStored { .. }
        | NativeError::Store(_)
        | NativeError::Json(_)
        | NativeError::Sql(_) => ApiError::internal(),
    }
}

async fn sync_notify(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<crate::generated::ResourceNotifyAck>, ApiError> {
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

fn invalid_stored_agent() -> ApiError {
    ApiError::invalid_stored("runtime_agents", "<agent>")
}

fn invalid_stored_task() -> ApiError {
    ApiError::invalid_stored("runtime_tasks", "<task>")
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
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/catalog_agent",
        move || {
            let connection = store.read().map_err(|_| ApiError::internal())?;
            WorkspaceViewsRepository::agent(&connection, &agent_id)
                .map_err(|_| ApiError::internal())?
                .map(|agent| agent.value())
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent"))
        },
    )
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

#[derive(Clone)]
struct AccountContext {
    key: String,
    /// The registry has this account with a known provider.
    known: bool,
    available: bool,
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
        .find(|row| row.get("id").and_then(Value::as_str) == Some(key));
    Ok(AccountContext {
        key: key.to_owned(),
        known: account
            .is_some_and(|row| studio_runtime_providers::ProviderKind::of_record(row).is_ok()),
        available: account.is_some_and(|row| {
            row.get("disconnected") != Some(&Value::Bool(true))
                && row.get("deleted") != Some(&Value::Bool(true))
        }),
    })
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "catalog_agent",
        routes: Some(routes),
        dependencies: &[
            "turn-path/native-command-input",
        ],
    }
}
