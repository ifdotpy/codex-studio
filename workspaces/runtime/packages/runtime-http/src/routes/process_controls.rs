//! Shared process panel with exact-owner checks and retained stop receipts.
use super::{
    extractors::{ApiJson, QueryParams},
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{Extension, Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_schema::shared_values::{
    NATIVE_TERMINAL_ITEMS_PER_PAGE, NATIVE_TERMINAL_MAX_PAGES,
};
use studio_runtime_services::account_session_runtime::{AccountSessionPort, ConnectionScope};
use studio_runtime_services::workspace_operations::ProviderTicket;
use studio_runtime_services::{
    monitor_runtime_wiring::MonitorRuntime, process_controls as controls,
};
use studio_runtime_store::{AgentRecord, RuntimeStore};

struct HttpError(ApiError);
impl From<studio_runtime_store::StoreError> for HttpError {
    fn from(error: studio_runtime_store::StoreError) -> Self {
        Self(ApiError::stored(error))
    }
}
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
        for original in controls::monitor_sources(tx, owners)? {
            if let Some(scope) = source.current_scope(&original.account) {
                connections.observe_source(scope);
            }
        }
    }
    Ok(connections)
}
async fn read_listing(state: HttpState, scope: String) -> Result<Value, ApiError> {
    let store = store(&state)?;
    let proof = held_response_proof(&state, &scope).await;
    store
        .read_async("runtime-http/routes/process_controls", move |tx| {
            let owners = controls::owners(tx, &scope).map_err(error)?;
            let mut connections = connections(&state, tx, &owners).map_err(error)?;
            if let Some(proof) = proof {
                connections.observe_response(proof);
            }
            controls::listing(tx, &scope, &connections).map_err(error)
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
) -> Result<Json<Value>, ApiError> {
    let query = Value::Object((*params.into_typed()).clone());
    let store = store(&state)?;
    let value = store
        .read_async("runtime-http/routes/process_controls", move |connection| {
            controls::receipt(connection, &query).map_err(error)
        })
        .await?;
    Ok(Json(value))
}
fn validate(request: &Value) -> Result<(), ApiError> {
    let processes = request["processes"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    let process = !request["process"].is_null();
    let response = !request["responseTarget"].is_null();
    let item = !request["itemId"].is_null();
    if (text(request, "action") == "background_command")
        != request["itemId"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && id.len() <= 200)
        || (item && !request["itemId"].is_string())
    {
        return Err(invalid("Background move requires exactly one command item"));
    }
    match text(request, "action") {
        "stop_selected" if !process || processes => {
            return Err(invalid("Select exactly one process to stop"));
        }
        "stop_owned" if process => {
            return Err(invalid("Owned process stop accepts a process snapshot"));
        }
        "stop_owned" if !processes => {
            return Err(invalid("No owned process identities were supplied"));
        }
        "stop_response" | "background_command" => {
            let name = if text(request, "action") == "stop_response" {
                "Response stop"
            } else {
                "Background move"
            };
            if process || processes {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("{name} does not accept process identities"),
                ));
            }
            if !response {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("{name} requires the current response identity"),
                ));
            }
            if ["agent", "accountKey", "epoch"]
                .iter()
                .any(|k| request[k] != request["responseTarget"][k])
            {
                return Err(invalid(
                    "Response identity does not match the process owner",
                ));
            }
            if request["includeDescendants"] == true {
                return Err(invalid("Descendant scope applies only to process stop"));
            }
        }
        _ => {}
    }
    if !matches!(
        text(request, "action"),
        "stop_response" | "background_command"
    ) && response
    {
        return Err(invalid(
            "Response identity applies only to response stop and background move",
        ));
    }
    Ok(())
}
async fn action(
    State(state): State<HttpState>,
    runtime: Option<Extension<Arc<MonitorRuntime>>>,
    ApiJson(request): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    validate(&request)?;
    let store = store(&state)?;
    super::sync_delta::write_external_async(
        store,
        move || async move {
            execute(state, runtime.map(|Extension(r)| r), request)
                .await
                .map_err(HttpError)
        },
        |e| e.0,
    )
    .await
    .map(Json)
}
async fn execute(
    state: HttpState,
    runtime: Option<Arc<MonitorRuntime>>,
    request: Value,
) -> Result<Value, ApiError> {
    let store = store(&state)?;
    // A lost response must reuse its saved action without another native read.
    let replay_request = request.clone();
    if let Some(result) = store
        .read_async("runtime-http/routes/process_controls", move |tx| {
            controls::replay(tx, &replay_request).map_err(error)
        })
        .await?
    {
        return Ok(result);
    }
    let proof = if request["action"] == "stop_response" {
        held_response_proof(&state, text(&request, "agent")).await
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
            let owners = controls::owners(tx, text(&claim_request, "agent"))?;
            let mut connections = connections(&claim_state, tx, &owners)?;
            if let Some(proof) = claim_proof {
                connections.observe_response(proof);
            }
            controls::claim(tx, &claim_request, &connections, now)
        })
        .await
        .map_err(error)?;
    let (mut record, targets) = match claim {
        controls::Claim::Replay(value) => return Ok(value),
        controls::Claim::New { record, targets } => (record, targets),
    };
    if request["action"] == "background_command" {
        let outcome = match background_command(
            &state,
            &record["responseTarget"],
            text(&request, "itemId"),
            text(&request, "requestId"),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                json!({"status":"uncertain","error":"The background move outcome is unknown. Inspect its receipt before retrying"})
            }
        };
        record["phase"] = json!(if outcome["status"] == "uncertain" {
            "uncertain"
        } else {
            "completed"
        });
        let mut response = outcome;
        response["agent"] = request["agent"].clone();
        response["itemId"] = request["itemId"].clone();
        record["result"] = json!({"requestId":request["requestId"],"status":record["phase"],"processes":[],"response":response});
    } else if request["action"] == "stop_response" {
        let outcome = match stop_response(
            &state,
            &record["responseTarget"],
            text(&request, "requestId"),
            proof.as_ref(),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                json!({"status":"uncertain","error":"The response stop outcome is unknown. Inspect its receipt before retrying"})
            }
        };
        record["phase"] = json!(if outcome["status"] == "uncertain" {
            "uncertain"
        } else {
            "completed"
        });
        let mut response = outcome;
        response["agent"] = request["agent"].clone();
        record["result"] = json!({"requestId":request["requestId"],"status":record["phase"],"processes":[],"response":response});
    } else {
        for target in targets {
            let outcome = match stop_process(
                &state,
                runtime.as_ref(),
                &target,
                text(&request, "requestId"),
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(_) => {
                    json!({"status":"uncertain","error":"The process stop outcome is unknown. Inspect its receipt before retrying"})
                }
            };
            if let Some(entry) = record["processes"]
                .as_array_mut()
                .expect("processes")
                .iter_mut()
                .find(|p| p["key"] == target["key"])
            {
                entry
                    .as_object_mut()
                    .expect("process")
                    .extend(outcome.as_object().expect("outcome").clone());
            }
            record["result"]["processes"] = record["processes"].clone();
            save(&store, record.clone()).await?;
        }
        let entries = record["processes"].as_array().expect("processes");
        let status = if entries.iter().any(|e| e["status"] == "uncertain") {
            "uncertain"
        } else if entries
            .iter()
            .any(|e| matches!(e["status"].as_str(), Some("failed" | "stale")))
        {
            "partial"
        } else {
            "completed"
        };
        record["phase"] = json!(if status == "uncertain" {
            "uncertain"
        } else {
            "completed"
        });
        record["result"] = json!({"requestId":request["requestId"],"status":status,"processes":record["processes"]});
    }
    save(&store, record.clone()).await?;
    Ok(record["result"].clone())
}
async fn save(store: &RuntimeStore, record: Value) -> Result<(), ApiError> {
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
                controls::processes(
                    connection,
                    &agent,
                    &connections(
                        &state,
                        connection,
                        &controls::owners(connection, &agent).map_err(error)?,
                    )
                    .map_err(error)?,
                )
                .map_err(error)
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
async fn stop_process(
    state: &HttpState,
    runtime: Option<&Arc<MonitorRuntime>>,
    process: &Value,
    request_id: &str,
) -> Result<Value, ApiError> {
    if process["source"] == "monitor" {
        let Some(runtime) = runtime else {
            return Ok(json!({"status":"failed","error":"Monitor runtime is unavailable"}));
        };
        let now =
            studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())?;
        return match runtime.cancel_process(process.clone(), now).await {
            Ok(result) => Ok(if result["status"] == "cancelled" {
                json!({"status":"stopped"})
            } else if matches!(result["status"].as_str(), Some("completed" | "failed")) {
                json!({"status":"already_finished"})
            } else if result["cancelRequested"] == true && result["error"].is_null() {
                json!({"status":"requested"})
            } else {
                json!({"status":"uncertain","error":result["error"]})
            }),
            Err(studio_runtime_services::monitor_policy::MonitorError::Refused(message)) => {
                Ok(json!({"status":"stale","error":message}))
            }
            Err(_) => {
                Ok(json!({"status":"uncertain","error":"The monitor stop outcome is unknown"}))
            }
        };
    }
    let (port, scope, mut ticket) = match bind(state, process).await {
        Ok(binding) => binding,
        Err(_) => {
            return Ok(
                json!({"status":"stale","error":"The native process owner changed. Refresh the process list"}),
            );
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
        let Some(cursor) = result["nextCursor"].as_str().filter(|v| !v.is_empty()) else {
            break;
        };
        if page_index + 1 == NATIVE_TERMINAL_MAX_PAGES || !seen.insert(cursor.to_owned()) {
            return Err(invalid("Native terminal listing did not advance"));
        }
        params["cursor"] = json!(cursor);
    }
    if !found {
        return Ok(json!({"status":"already_finished"}));
    }
    // Recheck the stored process after native discovery, before any stop request.
    let (_, current_scope, _) = bind(state, process).await?;
    if current_scope != scope {
        return Ok(
            json!({"status":"stale","error":"The native process connection changed before stop"}),
        );
    }
    let result = super::catalog_agent::provider_call(
        &port,
        &scope,
        &ticket,
        &operation,
        if claude {
            "claude/stopTask"
        } else {
            "thread/backgroundTerminals/terminate"
        },
        if claude {
            json!({"threadId":ticket.thread,"taskId":task})
        } else {
            json!({"threadId":ticket.thread,"processId":pid})
        },
    )
    .await?;
    if !claude {
        return Ok(if result["terminated"] == true {
            json!({"status":"stopped"})
        } else {
            json!({"status":"uncertain","error":"The provider did not confirm that the process stopped"})
        });
    }
    let after = super::catalog_agent::provider_call(
        &port,
        &scope,
        &ticket,
        &format!("{operation}:after"),
        "claude/state",
        json!({"threadId":ticket.thread}),
    )
    .await?;
    let tasks = after["tasks"]
        .as_array()
        .ok_or_else(|| invalid("Invalid native task state"))?;
    Ok(json!({"status":if tasks.iter().any(|t|t["task_id"]==task) {"requested"} else {"stopped"}}))
}
async fn stop_response(
    state: &HttpState,
    target: &Value,
    request_id: &str,
    proof: Option<&controls::HeldResponseProof>,
) -> Result<Value, ApiError> {
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
        Ok::<_, ApiError>((port, scope, ticket, owner["provider"] == "claude"))
    }
    .await;
    let (port, scope, ticket, claude) = match binding {
        Ok(binding) => binding,
        Err(_) => {
            return Ok(
                json!({"status":"stale","error":"The response changed. Refresh the process list"}),
            );
        }
    };
    let mut params = json!({"threadId":target["threadId"],"turnId":target["turnId"]});
    if claude {
        params["stopBackgroundTasks"] = json!(false);
    }
    super::catalog_agent::provider_call(
        &port,
        &scope,
        &ticket,
        &ticket.request,
        "turn/interrupt",
        params,
    )
    .await?;
    Ok(json!({"status":"requested"}))
}
/// Providers whose adapter implements `thread/commandExecution/background`.
/// Codex app-server has no request that ends a running exec wait early.
const BACKGROUND_COMMAND_PROVIDERS: &[&str] = &["claude"];

async fn background_command(
    state: &HttpState,
    target: &Value,
    item_id: &str,
    request_id: &str,
) -> Result<Value, ApiError> {
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
        let supported = BACKGROUND_COMMAND_PROVIDERS.contains(&text(owner, "provider"));
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
            return Ok(
                json!({"status":"stale","error":"The response changed. Refresh the conversation"}),
            );
        }
    };
    if !supported {
        return Ok(
            json!({"status":"failed","error":"This provider cannot move a running command to the background"}),
        );
    }
    let result = match super::catalog_agent::provider_call(
        &port,
        &scope,
        &ticket,
        &ticket.request,
        "thread/commandExecution/background",
        json!({"threadId":target["threadId"],"turnId":target["turnId"],"itemId":item_id}),
    )
    .await
    {
        Ok(result) => result,
        // A rejected or unsent request did not move the command.
        Err(error)
            if matches!(
                error.status(),
                StatusCode::BAD_REQUEST | StatusCode::SERVICE_UNAVAILABLE
            ) =>
        {
            return Ok(json!({"status":"failed","error":error.message()}));
        }
        Err(error) => return Err(error),
    };
    Ok(match result["status"].as_str() {
        Some(status @ ("backgrounded" | "not_running")) => json!({"status":status}),
        _ => {
            json!({"status":"uncertain","error":"The provider returned an unknown background move result"})
        }
    })
}
inventory::submit! {crate::routes::manifest::RouteModuleRegistration {name:"process_controls",routes:Some(routes),dependencies:&["provider-session/native-request","provider-session/monitor-commands"]}}
