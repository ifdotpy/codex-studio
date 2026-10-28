//! Shared process panel with exact-owner checks and retained stop receipts.
use super::{
    extractors::{ApiRawJson, QueryParams},
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{Extension, Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::process_controls::{
    ProcessActionResult, ProcessActionStatus, ProcessControlRequest, ProcessOutcome,
    ProcessReceipt, ProcessResponse, Request, ResponseEntry, ResponseOutcome,
};
use studio_runtime_schema::shared_values::{
    NATIVE_TERMINAL_ITEMS_PER_PAGE, NATIVE_TERMINAL_MAX_PAGES,
};
use studio_runtime_services::account_session_runtime::{AccountSessionPort, ConnectionScope};
use studio_runtime_services::native_actions::NativeRequest;
use studio_runtime_services::workspace_operations::ProviderTicket;
use studio_runtime_services::{
    monitor_runtime_wiring::MonitorRuntime, process_controls as controls,
};
use studio_runtime_store::request_results::AdmittedRequest;
use studio_runtime_store::{AgentRecord, RuntimeStore};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/processes", OperationStatus::Implemented, listing),
        get(
            "/api/processes/action",
            OperationStatus::Implemented,
            receipt,
        ),
        post(
            "/api/processes/action",
            OperationStatus::Implemented,
            action,
        ),
    ])
}
fn error(error: controls::Error) -> ApiError {
    match error {
        controls::Error::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
        controls::Error::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        controls::Error::Store(error) => ApiError::stored(error),
    }
}
fn invalid(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}
fn store(state: &HttpState) -> Result<Arc<RuntimeStore>, ApiError> {
    state.store.clone().ok_or_else(ApiError::internal)
}
fn provider(state: &HttpState, owner: &Value) -> Result<Arc<dyn AccountSessionPort>, ApiError> {
    let agent: AgentRecord =
        serde_json::from_value(owner.clone()).map_err(|_| ApiError::internal())?;
    Ok(super::agent_management::native_action_provider(
        store(state)?.as_ref(),
        &agent,
        state.account_session.clone(),
        state.browser_answer_guest.clone(),
    )?
    .0)
}
fn connections(
    state: &HttpState,
    tx: &studio_runtime_store::Transaction<'_>,
    owners: &[Value],
    rows: &[(bool, &Value, Value)],
) -> Result<controls::Connections, controls::Error> {
    let mut connections = controls::Connections::default();
    for owner in owners {
        let account = owner["accountKey"].as_str().unwrap_or("default");
        if let Some(scope) = provider(state, owner)
            .ok()
            .and_then(|port| port.current_scope(account))
        {
            connections.observe_owner(text(owner, "id").to_owned(), scope);
        }
    }
    if let Some(source) = &state.account_session {
        for original in controls::monitor_sources(tx, rows)? {
            if let Some(scope) = source.current_scope(&original.account) {
                connections.observe_source(scope);
            }
        }
    }
    Ok(connections)
}
pub(super) async fn read_listing(state: HttpState, scope: String) -> Result<Value, ApiError> {
    let store = store(&state)?;
    let proof = held_response_proof(&state, &scope).await;
    store
        .read_async("runtime-http/routes/process_controls", move |tx| {
            let owners = controls::owners(tx, &scope).map_err(error)?;
            let rows = controls::process_rows(tx, &owners).map_err(error)?;
            let mut connections = connections(&state, tx, &owners, &rows).map_err(error)?;
            if let Some(proof) = proof {
                connections.observe_response(proof);
            }
            controls::listing(tx, &owners, &rows, &connections).map_err(error)
        })
        .await
}
/// Observe only the root held response. An unavailable observation suppresses
/// Stop without hiding process rows. No lifecycle state changes here.
async fn held_response_proof(
    state: &HttpState,
    agent: &str,
) -> Option<controls::HeldResponseProof> {
    let agent = agent.to_owned();
    let owner = store(state)
        .ok()?
        .read_async("runtime-http/routes/process_controls", move |tx| {
            controls::owner(tx, &agent).map_err(error)
        })
        .await
        .ok()?;
    if !controls::held_response_candidate(&owner) {
        return None;
    }
    let port = provider(state, &owner).ok()?;
    let scope = port.current_scope(owner["accountKey"].as_str().unwrap_or("default"))?;
    let operation = format!("process-response-observation:{}", uuid::Uuid::new_v4());
    let ticket = ProviderTicket {
        operation: operation.clone(),
        request: operation.clone(),
        agent: text(&owner, "id").to_owned(),
        epoch: owner["epoch"].as_u64()?,
        account: scope.account.clone(),
        thread: Some(text(&owner, "threadId").to_owned()),
        connection: scope.connection.clone(),
        generation: scope.generation,
    };
    let reply = super::catalog_agent::provider_call(
        &port,
        &scope,
        &ticket,
        &operation,
        "thread/read",
        json!({"threadId":owner["threadId"],"includeTurns":false}),
        ("", None),
    )
    .await
    .ok()?;
    controls::HeldResponseProof::observe(owner, scope, &reply)
}
async fn listing(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let scope = params
        .nonempty_str("agent")
        .ok_or_else(|| invalid("Supply an agent ID"))?
        .to_owned();
    Ok(Json(read_listing(state, scope).await?))
}
async fn receipt(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<ProcessReceipt>, ApiError> {
    let query = Value::Object((*params.into_typed()).clone());
    let store = store(&state)?;
    let value = store
        .read_async("runtime-http/routes/process_controls", move |connection| {
            controls::receipt(connection, &query).map_err(error)
        })
        .await?;
    Ok(Json(value))
}
async fn action(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    ApiRawJson(request): ApiRawJson<Request>,
) -> Result<Json<ProcessActionResult>, ApiError> {
    execute(state, runtime.map(|Extension(r)| r), request)
        .await
        .map(Json)
}

async fn execute(
    state: HttpState,
    runtime: Option<Arc<MonitorRuntime>>,
    request: Request,
) -> Result<ProcessActionResult, ApiError> {
    let store = store(&state)?;
    // A lost response must reuse its saved action without another native read.
    let replay_request = request.clone();
    if let Some(result) = store
        .read_async("runtime-http/routes/process_controls", move |tx| {
            controls::replay(tx, replay_request.raw()).map_err(error)
        })
        .await?
    {
        return Ok(result);
    }
    let proof = if matches!(
        request.command(),
        ProcessControlRequest::StopResponse { .. }
    ) {
        held_response_proof(&state, &request.command().scope().agent).await
    } else {
        None
    };
    let claim_state = state.clone();
    let claim_request = request.clone();
    let claim_proof = proof.clone();
    let now =
        studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())?;
    let claim = store
        .transaction_with(move |tx| {
            let owners = controls::owners(tx, &claim_request.command().scope().agent)?;
            let rows = controls::process_rows(tx, &owners)?;
            let mut connections = connections(&claim_state, tx, &owners, &rows)?;
            if let Some(proof) = claim_proof {
                connections.observe_response(proof);
            }
            controls::claim(tx, &claim_request, &connections, now)
        })
        .await
        .map_err(error)?;
    let (mut record, targets, mut result) = match claim {
        controls::Claim::Replay(value) => return Ok(value),
        controls::Claim::New {
            record,
            targets,
            result,
        } => (*record, targets, result),
    };
    let root_request_id = record.root_request_id.clone();
    let admitted = record.admitted_request.clone();
    let scope = request.command().scope();
    match request.command() {
        ProcessControlRequest::BackgroundCommand { item_id, .. } => {
            let outcome = background_command(
                &state,
                &record.response_target,
                item_id,
                &scope.request_id,
                (&root_request_id, &admitted),
            )
            .await
            .unwrap_or_else(|_| ResponseOutcome::Uncertain {
                error: Some(
                    "The background move outcome is unknown. Inspect its receipt before retrying"
                        .into(),
                ),
            });
            result.status = response_status(&outcome);
            result.response = Some(ProcessResponse::Response(ResponseEntry {
                agent: scope.agent.clone(),
                item_id: Some(item_id.clone()),
                outcome,
            }));
        }
        ProcessControlRequest::StopResponse { .. } => {
            let outcome = stop_response(
                &state,
                &record.response_target,
                &scope.request_id,
                proof.as_ref(),
                (&root_request_id, &admitted),
            )
            .await
            .unwrap_or_else(|_| ResponseOutcome::Uncertain {
                error: Some(
                    "The response stop outcome is unknown. Inspect its receipt before retrying"
                        .into(),
                ),
            });
            result.status = response_status(&outcome);
            result.response = Some(ProcessResponse::Response(ResponseEntry {
                agent: scope.agent.clone(),
                item_id: None,
                outcome,
            }));
        }
        ProcessControlRequest::StopSelected { .. } | ProcessControlRequest::StopOwned { .. } => {
            for target in targets {
                let outcome = stop_process(
                    &state,
                    runtime.as_ref(),
                    &target,
                    &scope.request_id,
                    (&root_request_id, &admitted),
                )
                .await
                .unwrap_or_else(|_| ProcessOutcome::Uncertain {
                    error: Some(
                        "The process stop outcome is unknown. Inspect its receipt before retrying"
                            .into(),
                    ),
                });
                if let Some(entry) = result
                    .processes
                    .iter_mut()
                    .flatten()
                    .find(|p| p.key == text(&target, "key"))
                {
                    entry.outcome = outcome;
                }
                update_record(&mut record, &result);
                save(&store, record.clone()).await?;
            }
            result.status = if result
                .processes
                .iter()
                .flatten()
                .any(|p| matches!(p.outcome, ProcessOutcome::Uncertain { .. }))
            {
                ProcessActionStatus::Uncertain
            } else if result.processes.iter().flatten().any(|p| {
                matches!(
                    p.outcome,
                    ProcessOutcome::Failed { .. } | ProcessOutcome::Stale { .. }
                )
            }) {
                ProcessActionStatus::Partial
            } else {
                ProcessActionStatus::Completed
            };
        }
    }
    record.phase = if result.status == ProcessActionStatus::Uncertain {
        studio_runtime_store::ProcessActionPhase::Uncertain
    } else {
        studio_runtime_store::ProcessActionPhase::Completed
    };
    update_record(&mut record, &result);
    save(&store, record).await?;
    Ok(result)
}
fn response_status(outcome: &ResponseOutcome) -> ProcessActionStatus {
    match outcome {
        ResponseOutcome::Uncertain { .. } => ProcessActionStatus::Uncertain,
        ResponseOutcome::Requested
        | ResponseOutcome::Backgrounded
        | ResponseOutcome::NotRunning
        | ResponseOutcome::Stale { .. }
        | ResponseOutcome::Failed { .. } => ProcessActionStatus::Completed,
    }
}
fn update_record(
    record: &mut studio_runtime_store::ProcessActionRecord,
    result: &ProcessActionResult,
) {
    record.processes = result.processes.clone().unwrap_or_default();
    record.result = result.clone();
}
async fn save(
    store: &RuntimeStore,
    record: studio_runtime_store::ProcessActionRecord,
) -> Result<(), ApiError> {
    store
        .transaction_with(move |tx| controls::save(tx, &record))
        .await
        .map_err(error)
}

async fn bind(
    state: &HttpState,
    process: &Value,
) -> Result<(Arc<dyn AccountSessionPort>, ConnectionScope, ProviderTicket), ApiError> {
    let expected = process.clone();
    let (owners, processes) = {
        let state = state.clone();
        let agent = text(&expected, "agent").to_owned();
        store(&state)?
            .read_async("runtime-http/routes/process_controls", move |connection| {
                let owners = controls::owners(connection, &agent).map_err(error)?;
                let rows = controls::process_rows(connection, &owners).map_err(error)?;
                let connections = connections(&state, connection, &owners, &rows).map_err(error)?;
                let processes =
                    controls::processes(connection, &rows, &connections).map_err(error)?;
                Ok::<_, ApiError>((owners, processes))
            })
            .await?
    };
    if !processes
        .iter()
        .any(|p| controls::matches(p, &expected) && p["capabilities"]["stop"] == true)
    {
        return Err(invalid("The process changed. Refresh the process list"));
    }
    let owner = &owners[0];
    let port = provider(state, owner)?;
    let account = text(&expected, "_sourceAccountKey");
    let scope = port
        .current_scope(account)
        .ok_or_else(|| invalid("The native process connection is unavailable"))?;
    if scope.connection != text(&expected, "_sourceConnectionId")
        || owner["threadId"] != expected["_sourceThreadId"]
    {
        return Err(invalid("The native process connection changed"));
    }
    let ticket = ProviderTicket {
        operation: text(&expected, "key").to_owned(),
        agent: text(&expected, "agent").to_owned(),
        epoch: expected["epoch"].as_u64().ok_or_else(ApiError::internal)?,
        account: account.to_owned(),
        thread: Some(text(&expected, "_sourceThreadId").to_owned()),
        connection: scope.connection.clone(),
        generation: scope.generation,
        request: String::new(),
    };
    Ok((port, scope, ticket))
}
// Reserve the exact provider suboperation from its persisted process owner.
async fn dispatch(
    state: &HttpState,
    port: &Arc<dyn AccountSessionPort>,
    scope: &ConnectionScope,
    owner: &AdmittedRequest,
    request: NativeRequest,
) -> Result<Value, ApiError> {
    let operation = request.ticket.request.clone();
    let reserved_operation = operation.clone();
    let owner = owner.clone();
    let request = store(state)?
        .transaction_with_error(move |tx| {
            studio_runtime_services::native_actions::reserve_provider_intent(
                tx,
                &owner,
                &reserved_operation,
                request,
            )
            .map_err(super::catalog_agent::native_command_error)
        })
        .await?;
    super::catalog_agent::provider_call(
        port,
        scope,
        &request.ticket,
        &operation,
        &request.method,
        request.params,
        (&request.root_request_id, request.admitted_request.as_ref()),
    )
    .await
}

async fn stop_process(
    state: &HttpState,
    runtime: Option<&Arc<MonitorRuntime>>,
    process: &Value,
    request_id: &str,
    request_identity: (&str, &AdmittedRequest),
) -> Result<ProcessOutcome, ApiError> {
    if process["source"] == "monitor" {
        let Some(runtime) = runtime else {
            return Ok(ProcessOutcome::Failed {
                error: "Monitor runtime is unavailable".into(),
            });
        };
        let now =
            studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())?;
        return match runtime.cancel_process(process.clone(), now).await {
            Ok(result) => Ok(if result["status"] == "cancelled" {
                ProcessOutcome::Stopped
            } else if matches!(result["status"].as_str(), Some("completed" | "failed")) {
                ProcessOutcome::AlreadyFinished
            } else if result["cancelRequested"] == true && result["error"].is_null() {
                ProcessOutcome::Requested
            } else {
                ProcessOutcome::Uncertain {
                    error: Some(result["error"].clone()),
                }
            }),
            Err(studio_runtime_services::monitor_policy::MonitorError::Refused(message)) => {
                Ok(ProcessOutcome::Stale {
                    error: message.into(),
                })
            }
            Err(_) => Ok(ProcessOutcome::Uncertain {
                error: Some("The monitor stop outcome is unknown".into()),
            }),
        };
    }
    let (port, scope, mut ticket) = match bind(state, process).await {
        Ok(binding) => binding,
        Err(_) => {
            return Ok(ProcessOutcome::Stale {
                error: "The native process owner changed. Refresh the process list".into(),
            });
        }
    };
    let operation = format!(
        "process-action:{request_id}:{}",
        controls::digest(&process["key"])
    );
    ticket.request = operation.clone();
    let claude = process["source"] == "claude_task";
    let item = text(&process["_raw"], "itemId");
    let task = item.strip_prefix("task:").unwrap_or(item);
    let pid = &process["_raw"]["processId"];
    let mut params = if claude {
        json!({"threadId":ticket.thread})
    } else {
        json!({"threadId":ticket.thread,"limit":NATIVE_TERMINAL_ITEMS_PER_PAGE})
    };
    let mut seen = std::collections::HashSet::new();
    let mut found = false;
    for page_index in 0..NATIVE_TERMINAL_MAX_PAGES {
        let result = super::catalog_agent::provider_call(
            &port,
            &scope,
            &ticket,
            &format!("{operation}:list:{page_index}"),
            if claude {
                "claude/state"
            } else {
                "thread/backgroundTerminals/list"
            },
            params.clone(),
            ("", None),
        )
        .await?;
        let items = result[if claude { "tasks" } else { "data" }]
            .as_array()
            .ok_or_else(|| invalid("Invalid native process list"))?;
        if items.len() > NATIVE_TERMINAL_ITEMS_PER_PAGE && !claude {
            return Err(invalid("Invalid native terminal list"));
        }
        found = items.iter().any(|entry| {
            if claude {
                entry["task_id"] == task
            } else {
                entry["itemId"] == item && entry["processId"] == *pid
            }
        });
        if found || claude {
            break;
        }
        let Some(cursor) = studio_runtime_services::provider_pagination::next_cursor(&result)
            .map_err(|_| invalid("Invalid native terminal list cursor"))?
        else {
            break;
        };
        if page_index + 1 == NATIVE_TERMINAL_MAX_PAGES || !seen.insert(cursor.to_owned()) {
            return Err(invalid("Native terminal listing did not advance"));
        }
        params["cursor"] = json!(cursor);
    }
    if !found {
        return Ok(ProcessOutcome::AlreadyFinished);
    }
    // Recheck the stored process after native discovery, before any stop request.
    let (_, current_scope, _) = bind(state, process).await?;
    if current_scope != scope {
        return Ok(ProcessOutcome::Stale {
            error: "The native process connection changed before stop".into(),
        });
    }
    let result = dispatch(
        state,
        &port,
        &scope,
        request_identity.1,
        NativeRequest {
            root_request_id: request_identity.0.to_owned(),
            admitted_request: None,
            ticket: ticket.clone(),
            method: if claude {
                "claude/stopTask"
            } else {
                "thread/backgroundTerminals/terminate"
            }
            .to_owned(),
            params: if claude {
                json!({"threadId":ticket.thread,"taskId":task})
            } else {
                json!({"threadId":ticket.thread,"processId":pid})
            },
        },
    )
    .await?;
    if !claude {
        return Ok(if result["terminated"] == true {
            ProcessOutcome::Stopped
        } else {
            ProcessOutcome::Uncertain {
                error: Some("The provider did not confirm that the process stopped".into()),
            }
        });
    }
    let after = super::catalog_agent::provider_call(
        &port,
        &scope,
        &ticket,
        &format!("{operation}:after"),
        "claude/state",
        json!({"threadId":ticket.thread}),
        ("", None),
    )
    .await?;
    let tasks = after["tasks"]
        .as_array()
        .ok_or_else(|| invalid("Invalid native task state"))?;
    Ok(if tasks.iter().any(|t| t["task_id"] == task) {
        ProcessOutcome::Requested
    } else {
        ProcessOutcome::Stopped
    })
}
async fn stop_response(
    state: &HttpState,
    target: &Value,
    request_id: &str,
    proof: Option<&controls::HeldResponseProof>,
    request_identity: (&str, &AdmittedRequest),
) -> Result<ResponseOutcome, ApiError> {
    let expected = target.clone();
    let request_id = request_id.to_owned();
    let binding = async {
        let agent = text(&expected, "agent").to_owned();
        let owners = store(state)?
            .read_async("runtime-http/routes/process_controls", move |connection| {
                controls::owners(connection, &agent).map_err(error)
            })
            .await?;
        let owner = &owners[0];
        let port = provider(state, owner)?;
        let scope = port
            .current_scope(text(&expected, "accountKey"))
            .ok_or_else(|| invalid("The response connection is unavailable"))?;
        if !controls::validate_stop_response(owner, &expected, &scope, proof) {
            return Err(invalid("The response changed. Refresh the process list"));
        }
        let ticket = ProviderTicket {
            operation: request_id.clone(),
            request: format!("process-action:{request_id}:response"),
            agent: text(&expected, "agent").to_owned(),
            epoch: expected["epoch"].as_u64().ok_or_else(ApiError::internal)?,
            account: scope.account.clone(),
            thread: Some(text(&expected, "threadId").to_owned()),
            connection: scope.connection.clone(),
            generation: scope.generation,
        };
        Ok::<_, ApiError>((
            port,
            scope,
            ticket,
            owner["provider"] == ProviderKind::Claude.key(),
        ))
    }
    .await;
    let (port, scope, ticket, claude) = match binding {
        Ok(binding) => binding,
        Err(_) => {
            return Ok(ResponseOutcome::Stale {
                error: "The response changed. Refresh the process list".into(),
            });
        }
    };
    let mut params = json!({"threadId":target["threadId"],"turnId":target["turnId"]});
    if claude {
        params["stopBackgroundTasks"] = json!(false);
    }
    dispatch(
        state,
        &port,
        &scope,
        request_identity.1,
        NativeRequest {
            root_request_id: request_identity.0.to_owned(),
            admitted_request: None,
            ticket,
            method: "turn/interrupt".to_owned(),
            params,
        },
    )
    .await?;
    Ok(ResponseOutcome::Requested)
}
async fn background_command(
    state: &HttpState,
    target: &Value,
    item_id: &str,
    request_id: &str,
    request_identity: (&str, &AdmittedRequest),
) -> Result<ResponseOutcome, ApiError> {
    let expected = target.clone();
    let request_id = request_id.to_owned();
    let binding = async {
        let agent = text(&expected, "agent").to_owned();
        let owners = store(state)?
            .read_async("runtime-http/routes/process_controls", move |connection| {
                controls::owners(connection, &agent).map_err(error)
            })
            .await?;
        let owner = &owners[0];
        // Only the Claude adapter implements `thread/commandExecution/background`.
        let supported = matches!(ProviderKind::of_record(owner), Ok(ProviderKind::Claude));
        let port = provider(state, owner)?;
        let scope = port
            .current_scope(text(&expected, "accountKey"))
            .ok_or_else(|| invalid("The response connection is unavailable"))?;
        if !controls::validate_response(owner, &expected, Some(&scope.connection)) {
            return Err(invalid("The response changed. Refresh the conversation"));
        }
        let ticket = ProviderTicket {
            operation: request_id.clone(),
            request: format!("process-action:{request_id}:background"),
            agent: text(&expected, "agent").to_owned(),
            epoch: expected["epoch"].as_u64().ok_or_else(ApiError::internal)?,
            account: scope.account.clone(),
            thread: Some(text(&expected, "threadId").to_owned()),
            connection: scope.connection.clone(),
            generation: scope.generation,
        };
        Ok::<_, ApiError>((port, scope, ticket, supported))
    }
    .await;
    let (port, scope, ticket, supported) = match binding {
        Ok(binding) => binding,
        Err(_) => {
            return Ok(ResponseOutcome::Stale {
                error: "The response changed. Refresh the conversation".into(),
            });
        }
    };
    if !supported {
        return Ok(ResponseOutcome::Failed {
            error: "This provider cannot move a running command to the background".into(),
        });
    }
    let result = match dispatch(state, &port, &scope, request_identity.1, NativeRequest {
        root_request_id: request_identity.0.to_owned(), admitted_request: None,
        ticket, method: "thread/commandExecution/background".to_owned(),
        params: json!({"threadId":target["threadId"],"turnId":target["turnId"],"itemId":item_id}),
    }).await
    {
        Ok(result) => result,
        // A rejected or unsent request did not move the command.
        Err(error)
            if matches!(
                error.status(),
                StatusCode::BAD_REQUEST | StatusCode::SERVICE_UNAVAILABLE
            ) =>
        {
            return Ok(ResponseOutcome::Failed {
                error: error.message().into(),
            });
        }
        Err(error) => return Err(error),
    };
    Ok(match result["status"].as_str() {
        Some("backgrounded") => ResponseOutcome::Backgrounded,
        Some("not_running") => ResponseOutcome::NotRunning,
        _ => ResponseOutcome::Uncertain {
            error: Some("The provider returned an unknown background move result".into()),
        },
    })
}
inventory::submit! {crate::routes::manifest::RouteModuleRegistration {name:"process_controls",routes:Some(routes),dependencies:&["provider-session/native-request","provider-session/monitor-commands"]}}
