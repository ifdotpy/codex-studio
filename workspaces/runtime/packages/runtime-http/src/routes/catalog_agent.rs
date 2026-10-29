//! Skills, capabilities, finder and native command operations backed by saved agent state.

use studio_runtime_schema::shared_values::{
    NATIVE_TERMINAL_ITEMS_PER_PAGE, NATIVE_TERMINAL_MAX_PAGES,
};
use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;

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
    let agent = store
        .read_async("runtime-http/routes/catalog_agent", move |connection| {
            WorkspaceViewsRepository::agent(connection, &agent_id)
                .map_err(|_| ApiError::internal())?
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent"))
        })
        .await?;
    let record = agent.value();
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
    let cursor = params.nonempty_str("serversCursor").map(str::to_owned);
    let fresh =
        cursor.is_some() || params.typed_value("fresh").and_then(Value::as_bool) == Some(true);
    if !fresh && let Some(cached) = state.capabilities_cache.get(agent_id, Instant::now()) {
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
    let observed = store
        .read_async("runtime-http/routes/catalog_agent", move |connection| {
            studio_runtime_store::CapabilityReadsRepository::observed_names(
                connection,
                &observed_agent_id,
            )
            .map_err(|_| ApiError::internal())
        })
        .await?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let mut result = capabilities_base(agent_id, &capability_agent, managed, observed, at);
    let cache = std::sync::Arc::clone(&state.capabilities_cache);
    let session = state.account_session.clone();
    let cwd = cwd.to_owned();
    let thread_id = agent.get("threadId").cloned();
    let fetch = || async move {
        let scope = match &session {
            Some(port) => port.ensure_session(&account.key).await.ok(),
            None => None,
        };
        result["mcpSession"] = scope
            .as_ref()
            .and_then(|scope| super::mcp_reconnect::binding(&agent, scope))
            .unwrap_or(Value::Null);
        let mut server_params =
            json!({"threadId":thread_id,"limit":NATIVE_TERMINAL_ITEMS_PER_PAGE});
        if let Some(cursor) = cursor {
            server_params["cursor"] = json!(cursor);
        }
        let responses = account_provider_exchange(
            session.clone(),
            account.clone(),
            vec![
                ProviderRequest {
                    method: "skills/list".to_owned(),
                    params: json!({"cwds":[cwd],"forceReload":true}),
                },
                ProviderRequest {
                    method: "mcpServerStatus/list".to_owned(),
                    params: server_params,
                },
            ],
            DISCOVERY_TIMEOUT,
        )
        .await;
        match responses {
            Ok(responses) => {
                for (index, label) in ["skills", "servers"].into_iter().enumerate() {
                    match responses.get(index) {
                        Some(Ok(response)) => apply_capability_result(&mut result, label, response),
                        _ => append_capability_error(&mut result, label, "Provider request failed"),
                    }
                }
            }
            Err(_) => {
                append_capability_error(&mut result, "skills", "Provider request failed");
                append_capability_error(&mut result, "servers", "Provider request failed");
            }
        }
        if scope.as_ref()
            != session
                .as_ref()
                .and_then(|port| port.current_scope(&account.key))
                .as_ref()
        {
            result["mcpSession"] = Value::Null;
        }
        result
    };
    let result = if fresh {
        fetch().await
    } else {
        cache
            .get_or_fetch(agent_id, fetch)
            .await
            .map_err(|_| ApiError::internal())?
    };
    Ok(Json(super::generated_json::<
        crate::generated::CapabilitiesResponse,
    >(result)?))
}

async fn native_command(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<NativeCommandRequest>,
) -> Result<Json<crate::generated::NativeCommandResponse>, ApiError> {
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let request_id = body.request_id.clone();
    let store = std::sync::Arc::clone(store);
    let lookup_root = request_id.clone();
    let lookup_task = body.id.clone();
    let lookup_action = body.action;
    let lookup_text = body.text.clone();
    let replay = store
        .read_async("runtime-http/native-command-replay", move |connection| {
            let task = WorkspaceViewsRepository::task(connection, &lookup_task)
                .map_err(ApiError::stored)?
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown task"))?;
            let agent_id = task
                .get("agent")
                .and_then(Value::as_str)
                .ok_or_else(invalid_stored_task)?;
            let action = match lookup_action {
                NativeCommandAction::Input => {
                    studio_runtime_services::native_actions::CommandAction::Input(
                        lookup_text.as_deref().ok_or_else(|| {
                            ApiError::new(StatusCode::BAD_REQUEST, "Command input must be text")
                        })?,
                    )
                }
                NativeCommandAction::Cancel => {
                    studio_runtime_services::native_actions::CommandAction::Cancel
                }
            };
            studio_runtime_services::native_actions::replay_command(
                connection,
                agent_id,
                &lookup_task,
                &lookup_root,
                action,
            )
            .map_err(native_command_error)
        })
        .await?;
    if let Some(reply) = replay {
        return serde_json::from_str(&reply.response_json)
            .map(Json)
            .map_err(ApiError::json);
    }
    match body.action {
        NativeCommandAction::Input => {
            let text = body.text.ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "Command input must be text")
            })?;
            let admission_override = state.message_admission.clone();
            let admission_for_read = admission_override.clone();
            let store_for_read = std::sync::Arc::clone(&store);
            let task_id = body.id;
            let (agent_id, snapshot, prepared) = store_for_read
                .read_async("runtime-http/routes/catalog_agent", move |connection| {
                    let task = WorkspaceViewsRepository::task(connection, &task_id)
                        .map_err(ApiError::stored)?
                        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown task"))?;
                    let agent_id = task
                        .get("agent")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid_stored_task)?
                        .to_owned();
                    let agent = WorkspaceViewsRepository::agent(connection, &agent_id)
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
                            studio_runtime_services::message_admission::PreparedAdmission::read(
                                connection, &agent_id,
                            )
                        })
                        .transpose()
                        .map_err(|_| ApiError::internal())?;
                    Ok::<_, ApiError>((agent_id, snapshot, prepared))
                })
                .await?;
            let admission: std::sync::Arc<
                dyn studio_runtime_services::message_admission::MessageAdmission,
            > = match admission_override {
                Some(admission) => admission,
                None => std::sync::Arc::new(
                    prepared
                        .ok_or_else(ApiError::internal)?
                        .prepare()
                        .await
                        .map_err(|_| ApiError::internal())?,
                ),
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
            serde_json::from_str::<crate::generated::NativeCommandResponse>(posted.get())
                .map(Json)
                .map_err(ApiError::json)
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
) -> Result<crate::generated::NativeCommandResponse, ApiError> {
    // Match codex_workspace.py:2194-2210: verify the exact item/process pair
    // in the provider's terminal list before sending terminate.
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (task, agent) = store
        .read_async("runtime-http/routes/catalog_agent", move |connection| {
            let task = WorkspaceViewsRepository::task(connection, &task_id)
                .map_err(ApiError::stored)?
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown task"))?;
            let agent_id = task
                .get("agent")
                .and_then(Value::as_str)
                .ok_or_else(invalid_stored_task)?;
            let agent = WorkspaceViewsRepository::agent(connection, agent_id)
                .map_err(ApiError::stored)?
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?;
            Ok::<_, ApiError>((task, agent.value()))
        })
        .await?;
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
            ("", None),
        )
        .await?;
        let studio_runtime_schema::json_object::JsonObject(page):
            studio_runtime_schema::json_object::JsonObject<
                studio_runtime_services::provider_pagination::Page<Value>,
            > = serde::Deserialize::deserialize(page).map_err(|_| {
            ApiError::new(StatusCode::BAD_REQUEST, "Invalid native terminal list")
        })?;
        if page.data.len() > NATIVE_TERMINAL_ITEMS_PER_PAGE {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Invalid native terminal list",
            ));
        }
        pages.extend(page.data);
        let studio_runtime_services::provider_pagination::Cursor::String(cursor) = page.next_cursor
        else {
            break;
        };
        if cursor.is_empty() {
            break;
        }
        if page_number + 1 == NATIVE_TERMINAL_MAX_PAGES || !cursor_seen.insert(cursor.clone()) {
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
            return super::generated_json(result);
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
        (&request.root_request_id, request.admitted_request.as_ref()),
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
    let result = store
        .transaction_with(move |tx| {
            studio_runtime_services::native_actions::command_cancel_settle(tx, &receipt)
                .map_err(native_command_error)
        })
        .await
        .map_err(|_| ApiError::internal())?;
    super::generated_json(result)
}

pub(super) async fn provider_call(
    port: &std::sync::Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    scope: &studio_runtime_services::account_session_runtime::ConnectionScope,
    ticket: &studio_runtime_services::workspace_operations::ProviderTicket,
    operation: &str,
    method: &str,
    params: Value,
    request_identity: (
        &str,
        Option<&studio_runtime_store::request_results::AdmittedRequest>,
    ),
) -> Result<Value, ApiError> {
    use studio_runtime_services::account_session_runtime::{ProviderRequest, ProviderResponse};
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
            root_request_id: request_identity.0.to_owned(),
            admitted_request: request_identity.1.cloned(),
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
            studio_runtime_services::account_session_runtime::AccountSessionError::Expired => ApiError::new(axum::http::StatusCode::GONE, error.to_string()),
            studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                name,
            ) => ApiError::dependency(name),
            studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                message,
            ) => ApiError::new(StatusCode::BAD_REQUEST, message),
            studio_runtime_services::account_session_runtime::AccountSessionError::InvalidResponse(_) | studio_runtime_services::account_session_runtime::AccountSessionError::Persistence | studio_runtime_services::account_session_runtime::AccountSessionError::Unknown => ApiError::new(
                StatusCode::CONFLICT,
                "Native request outcome is unknown; inspect its receipt before retrying",
            ),
        })?;
    if receipt.operation != expected_operation
        || receipt.scope != expected_scope
        || receipt.ticket().is_some_and(|issued| {
            issued.generation != expected_scope.generation || issued.method != method_value
        })
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Native provider receipt belongs to a different session",
        ));
    }
    match receipt.outcome {
        ReceiptOutcome::Written {
            response: ProviderResponse::Reply(value),
            ..
        } => Ok(value),
        ReceiptOutcome::Written {
            response: ProviderResponse::Rejected(_),
            ..
        } => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Native provider rejected the request",
        )),
        ReceiptOutcome::WorkerNotSent { .. }
        | ReceiptOutcome::ExpectedScopeMismatch
        | ReceiptOutcome::DeadlineExpired
        | ReceiptOutcome::NotHandedOff
        | ReceiptOutcome::NoLiveAccountWorker => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Native provider request was not sent",
        )),
        ReceiptOutcome::Unknown { .. } => Err(ApiError::new(
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
    ) -> Result<
        Box<serde_json::value::RawValue>,
        studio_runtime_services::native_actions::NativeError,
    > {
        let result = studio_runtime_services::coordination::post_message_with_admission(
            tx,
            studio_runtime_services::coordination::PostManagedMessage {
                room: self.room,
                id: key,
                text: instruction,
                delivery: studio_runtime_schema::DeliveryMoment::CurrentTurn,
                assets: &[],
            },
            self.admission,
        )
        .map_err(|_| {
            studio_runtime_services::native_actions::NativeError::Unavailable(
                "workspace-policy/message-admission",
            )
        })?;
        serde_json::value::to_raw_value(&result.receipt).map_err(|error| {
            studio_runtime_services::native_actions::NativeError::Store(error.into())
        })
    }
}

pub(super) fn native_command_error(
    error: studio_runtime_services::native_actions::NativeError,
) -> ApiError {
    use studio_runtime_services::native_actions::NativeError;
    match error {
        NativeError::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
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
        NativeError::Store(error) => ApiError::stored(error),
        NativeError::Json(error) => ApiError::json(error),
        NativeError::Sql(error) => ApiError::stored(error.into()),
        NativeError::InvalidStored { table, id, .. } => ApiError::invalid_stored(table, &id),
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
    request_id: String,
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
    let agent_id = agent_id.to_owned();
    store
        .read_async("runtime-http/routes/catalog_agent", move |connection| {
            WorkspaceViewsRepository::agent(connection, &agent_id)
                .map_err(|_| ApiError::internal())?
                .map(|agent| agent.value())
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Unknown managed agent"))
        })
        .await
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
        .find(|row| row.id.as_str() == Some(key));
    Ok(AccountContext {
        key: key.to_owned(),
        known: account.is_some_and(|row| row.provider_kind().is_some()),
        available: account.is_some_and(|row| {
            row.disconnected.as_bool() != Some(true) && row.deleted.as_bool() != Some(true)
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

#[cfg(test)]
mod native_command_tests {
    use super::*;

    struct PagePort {
        scope: studio_runtime_services::account_session_runtime::ConnectionScope,
        pages: std::sync::Mutex<std::collections::VecDeque<Value>>,
        calls: std::sync::Mutex<
            Vec<studio_runtime_services::account_session_runtime::ProviderRequest>,
        >,
    }

    impl studio_runtime_services::account_session_runtime::AccountSessionPort for PagePort {
        fn ensure_session<'a>(
            &'a self,
            _: &'a str,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            studio_runtime_services::account_session_runtime::ConnectionScope,
        > {
            Box::pin(async move { Ok(self.scope.clone()) })
        }
        fn provider_request<'a>(
            &'a self,
            request: studio_runtime_services::account_session_runtime::ProviderRequest,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            studio_runtime_services::account_session_runtime::SessionReceipt,
        > {
            Box::pin(async move {
                use studio_runtime_services::account_session_runtime::{
                    ProviderResponse, SessionOutcome, SessionReceipt,
                };
                let value = if request.method.as_str() == "thread/backgroundTerminals/list" {
                    self.pages.lock().unwrap().pop_front().expect("page")
                } else {
                    assert_eq!(
                        request.method.as_str(),
                        "thread/backgroundTerminals/terminate"
                    );
                    json!({"terminated":true})
                };
                let ticket = studio_runtime_providers::RequestTicket {
                    generation: self.scope.generation,
                    id: studio_runtime_providers::RequestId::String(request.operation.clone()),
                    method: request.method,
                };
                let operation = request.operation.clone();
                self.calls.lock().unwrap().push(request);
                Ok(SessionReceipt {
                    scope: self.scope.clone(),
                    operation,
                    replayed: false,
                    outcome: SessionOutcome::Written {
                        ticket,
                        response: ProviderResponse::Reply(value),
                    },
                })
            })
        }
    }

    async fn page_fixture(
        pages: Vec<Value>,
    ) -> (tempfile::TempDir, HttpState, std::sync::Arc<PagePort>) {
        let directory = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(directory.path()))
                .await
                .unwrap(),
        );
        store.transaction_with(|tx| {
            let owner = json!({"id":"owner","rootId":"owner","epoch":1,"status":"running","accountKey":"default","provider":"codex","threadId":"thread","turnId":"turn","inFlight":true,"autoWake":true});
            let task = json!({"id":"owner:command","agent":"owner","kind":"command","itemId":"command","processId":"42","status":"running","ownerEpoch":1,"sourceAccountKey":"default","sourceConnectionId":"actual-uuid","sourceThreadId":"thread","turnId":"turn","created":1});
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('owner',?)", [owner.to_string()])?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES('owner:command',?)", [task.to_string()])?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.unwrap();
        let port = std::sync::Arc::new(PagePort {
            scope: studio_runtime_services::account_session_runtime::ConnectionScope {
                account: "default".into(),
                connection: "actual-uuid".into(),
                generation: 7,
            },
            pages: std::sync::Mutex::new(pages.into()),
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let mut state = HttpState::new(
            "token".into(),
            "workspace".into(),
            directory.path().display().to_string(),
            4621,
            None,
            Some(store),
            None,
        );
        state.account_session = Some(port.clone());
        (directory, state, port)
    }

    #[tokio::test]
    async fn native_terminal_pagination_preserves_provider_calls_and_permissive_cursor() {
        for cursor in [
            json!(null),
            json!(""),
            json!(false),
            json!(3),
            json!({}),
            json!([]),
        ] {
            let (_dir, state, port) = page_fixture(vec![
                json!({"data":[null,{"other":true}],"nextCursor":"next"}),
                json!({"data":[{"itemId":"command","processId":"42"}],"nextCursor":cursor}),
            ])
            .await;
            let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
            native_command_cancel(state, "owner:command".into(), root.clone())
                .await
                .expect("cancel");
            let calls = port.calls.lock().unwrap();
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[0].operation, format!("{root}:list"));
            assert_eq!(
                calls[1].operation,
                format!("{root}:list:{}", studio_runtime_store::sha256_hex(b"next"))
            );
            assert_eq!(
                calls[0].params,
                json!({"threadId":"thread","limit":NATIVE_TERMINAL_ITEMS_PER_PAGE})
            );
            assert_eq!(
                calls[1].params,
                json!({"threadId":"thread","limit":NATIVE_TERMINAL_ITEMS_PER_PAGE,"cursor":"next"})
            );
            for call in calls.iter() {
                assert_eq!(call.expected_scope.as_ref(), Some(&port.scope));
                assert_eq!(call.actor.as_ref().unwrap().agent, "owner");
                assert_eq!(call.actor.as_ref().unwrap().epoch, 1);
            }
            assert_eq!(
                calls[2].params,
                json!({"threadId":"thread","processId":"42"})
            );
        }
    }

    #[tokio::test]
    async fn native_terminal_pagination_refuses_invalid_and_repeated_pages_before_effect() {
        use axum::response::IntoResponse;
        for (pages, status, count) in [
            (vec![json!({})], 400, 1),
            (vec![json!({"data":null})], 400, 1),
            (vec![json!([[]])], 400, 1),
            (
                vec![json!({"data":vec![Value::Null; NATIVE_TERMINAL_ITEMS_PER_PAGE + 1]})],
                400,
                1,
            ),
            (
                vec![
                    json!({"data":[],"nextCursor":"again"}),
                    json!({"data":[],"nextCursor":"again"}),
                ],
                409,
                2,
            ),
        ] {
            let (_dir, state, port) = page_fixture(pages).await;
            let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
            let error = match native_command_cancel(state, "owner:command".into(), root).await {
                Ok(_) => panic!("expected refusal"),
                Err(error) => error,
            };
            assert_eq!(error.into_response().status().as_u16(), status);
            assert_eq!(port.calls.lock().unwrap().len(), count);
        }
    }

    #[test]
    fn native_command_requires_the_origin_request_identity_for_both_actions() {
        for action in ["input", "cancel"] {
            let missing = json!({"id":"task","action":action,"text":"input"});
            assert!(serde_json::from_value::<NativeCommandRequest>(missing).is_err());
            let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
            let parsed: NativeCommandRequest = serde_json::from_str(&format!(
                r#"{{"id":"task","action":"{action}","text":"input","request_id":"{root}"}}"#
            ))
            .expect("caller identity");
            assert_eq!(parsed.request_id, root);
        }
    }

    #[tokio::test]
    async fn native_command_http_replays_origin_input_and_cancel_after_task_completion() {
        use axum::{
            body::{Body, to_bytes},
            extract::ConnectInfo,
            http::Request,
        };
        use tower::ServiceExt;
        let directory = tempfile::tempdir().expect("state");
        let store = std::sync::Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let request_root = root.clone();
        let cancel_root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let stored_cancel_root = cancel_root.clone();
        store.transaction(move |tx| {
            studio_runtime_store::receipt_schema::create(tx)?;
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('agent',?1)", [json!({"id":"agent","rootId":"agent","provider":"codex","epoch":1,"status":"idle"}).to_string()])?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES('task',?1)", [json!({"id":"task","agent":"agent","kind":"command","status":"completed","processId":"17","itemId":"item","created":1.0}).to_string()])?;
            let signature = json!({"task":"task","action":"input","text":"one"}).to_string();
            let envelope = json!({"actor":"agent","reply":{"id":"original-input","status":"pending"},"references":[]});
            let identity = studio_runtime_schema::persistence::RequestIdentity::new("native-command", &[], &request_root, &[]).expect("identity");
            tx.execute("INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)", rusqlite::params![identity.id(),signature,envelope.to_string(),identity.issued_ms()])?;
            let request = studio_runtime_services::native_actions::NativeRequest {
                root_request_id: stored_cancel_root.clone(), admitted_request: None,
                ticket: studio_runtime_services::workspace_operations::ProviderTicket {
                    operation:"task".into(), request:stored_cancel_root.clone(),agent:"agent".into(),epoch:1,
                    account:"default".into(),thread:Some("thread".into()),connection:"connection".into(),generation:1,
                }, method:studio_runtime_providers::Method::CommandExecTerminate.as_str().to_owned(),params:json!({}),
            };
            let signature = json!({"task":"task","action":"cancel"}).to_string();
            let envelope = json!({"actor":"agent","reply":{"request":request,"result":{"terminated":true}},"references":[]});
            let identity = studio_runtime_schema::persistence::RequestIdentity::new("native-command", &[], &stored_cancel_root, &[]).expect("identity");
            tx.execute("INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)", rusqlite::params![identity.id(),signature,envelope.to_string(),identity.issued_ms()])?;
            Ok(())
        }).await.expect("new-table fixture");
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            directory.path().display().to_string(),
            4621,
            None,
            Some(store.clone()),
            None,
        );
        let app = crate::router::router(state).expect("router");
        for (action, text, token, request_root, expected) in [
            ("input", "one", true, root.clone(), 200),
            ("cancel", "", true, cancel_root, 200),
            ("input", "different", true, root.clone(), 409),
            ("input", "one", false, root.clone(), 403),
            (
                "input",
                "one",
                true,
                "f43c1885-f4fc-485b-bf99-8b4f31e7e7c4".to_owned(),
                410,
            ),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/native-command")
                .header("host", "127.0.0.1:4621")
                .header("origin", "http://127.0.0.1:4621")
                .header("content-type", "application/json")
                .header(
                    crate::schema::API_SCHEMA_HASH_HEADER,
                    crate::schema::api_schema_hash(),
                )
                .extension(ConnectInfo(
                    "127.0.0.1:12345"
                        .parse::<std::net::SocketAddr>()
                        .expect("peer"),
                ));
            if token {
                request = request.header("x-canvas-token", "token");
            }
            let response = app
                .clone()
                .oneshot(
                    request
                        .body(Body::from(
                            json!({"id":"task","action":action,"text":text,"request_id":request_root})
                                .to_string(),
                        ))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status().as_u16(), expected);
            if expected == 200 {
                let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
                let reply: crate::generated::NativeCommandResponse =
                    serde_json::from_slice(&bytes).expect("typed original reply");
                if action == "input" {
                    assert_eq!(reply.id.flatten().as_deref(), Some("original-input"));
                } else {
                    assert_eq!(reply.terminated.flatten(), Some(true));
                }
            }
        }
        let events: i64 = store
            .test_reader()
            .expect("reader")
            .query_row("SELECT count(*) FROM runtime_events", [], |row| row.get(0))
            .expect("event count");
        assert_eq!(events, 0, "replay never enqueues a new native input");
    }
}
