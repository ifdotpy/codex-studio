//! Persisted terminal reads and session-dependent terminal operations.

use super::extractors::{ApiJson, QueryParams};
#[cfg(not(unix))]
use super::manifest::Dependency;
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::{MatchedPath, State};
use axum::http::StatusCode;
use serde_json::Value;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use studio_runtime_services::terminal_driver::TerminalCreate;
use studio_runtime_services::terminal_driver::{self, TerminalError};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};

use crate::error::ApiError;
use crate::router::HttpState;

#[cfg(not(unix))]
const PLATFORM_DEPENDENCIES: [Dependency; 1] = [Dependency::new("runtime-process/local-pty-unix")];
#[cfg(unix)]
const ROUTE_DEPENDENCIES: &[&str] = &[];
#[cfg(not(unix))]
const ROUTE_DEPENDENCIES: &[&str] = &["runtime-process/local-pty-unix"];

fn live_operation_status() -> OperationStatus {
    #[cfg(unix)]
    {
        OperationStatus::Implemented
    }
    #[cfg(not(unix))]
    {
        OperationStatus::partial(&PLATFORM_DEPENDENCIES)
    }
}

pub(crate) fn routes() -> RouteGroup {
    let mut routes = vec![
        get("/api/terminals", OperationStatus::Implemented, listing),
        get(
            "/api/terminals/output",
            OperationStatus::Implemented,
            output,
        ),
    ];
    routes.extend(
        [
            "/api/terminals/input",
            "/api/terminals/resize",
            "/api/terminals/rename",
            "/api/terminals/close",
        ]
        .into_iter()
        .map(|path| post(path, live_operation_status(), action)),
    );
    routes.push(post(
        "/api/terminals/create",
        live_operation_status(),
        create,
    ));
    RouteGroup::new(routes)
}

async fn listing(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || terminal_driver::listing(&store))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(terminal_error)?;
    Ok(Json(
        super::generated_json::<crate::generated::TerminalList>(result)?,
    ))
}

async fn output(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let offset = query
        .typed_value("offset")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let history = query.typed_value("history").and_then(Value::as_str) == Some("1");
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_i64)
        .unwrap_or(65_536);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        if history {
            terminal_driver::history_output(&store, &id, offset, limit)
        } else {
            terminal_driver::output(&store, &id, offset)
        }
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(terminal_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::TerminalOutput,
    >(result)?))
}

async fn create(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    #[cfg(unix)]
    {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let agent_id = body
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::validation(vec![Value::String("body".into())]))?
            .to_owned();
        let agent_store = store.clone();
        let (agent, agent_cwd, cwd) =
            tokio::task::spawn_blocking(move || checked_agent_cwd(&agent_store, &agent_id))
                .await
                .map_err(|_| ApiError::internal())?
                .map_err(terminal_body_error)?;
        let dimension = |name: &str, default| match body.get(name) {
            None => Ok(default),
            Some(value) => value.as_u64().ok_or(TerminalError::InvalidRequest),
        };
        let columns = dimension("cols", 100).map_err(terminal_body_error)?;
        let rows = dimension("rows", 28).map_err(terminal_body_error)?;
        let request_id = body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let shell = terminal_shell();
        let environment = serde_json::json!({
            "TERM":"xterm-256color",
            "COLORTERM":"truecolor"
        });
        let driver = state
            .terminal_driver_handle()
            .ok_or_else(ApiError::internal)?;
        let result = tokio::task::spawn_blocking(move || {
            driver.create(TerminalCreate {
                request_id: &request_id,
                agent: &agent,
                agent_cwd: &agent_cwd,
                cwd: &cwd,
                columns,
                rows,
                shell: &shell,
                environment,
                submitted_request: &body,
            })
        })
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(terminal_body_error)?;
        Ok(Json(super::generated_json::<
            crate::generated::TerminalRecord,
        >(result)?))
    }
    #[cfg(not(unix))]
    {
        let _ = (state, body);
        Err(super::manifest::dependency_unavailable(
            "POST",
            "/api/terminals/create",
            &PLATFORM_DEPENDENCIES,
        )
        .unwrap_or_else(|_| ApiError::internal()))
    }
}

async fn action(
    path: MatchedPath,
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    #[cfg(unix)]
    {
        let driver = state
            .terminal_driver_handle()
            .ok_or_else(ApiError::internal)?;
        let action = path
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let is_input = action == "input";
        let result = tokio::task::spawn_blocking(move || driver.action(&action, &body))
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(terminal_body_error)?;
        if is_input {
            Ok(Json(super::generated_json::<
                crate::generated::TerminalInputResult,
            >(result)?))
        } else {
            Ok(Json(super::generated_json::<
                crate::generated::TerminalRecord,
            >(result)?))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (state, body);
        Err(
            super::manifest::dependency_unavailable("POST", path.as_str(), &PLATFORM_DEPENDENCIES)
                .unwrap_or_else(|_| ApiError::internal()),
        )
    }
}

fn checked_agent_cwd(
    store: &RuntimeStore,
    id: &str,
) -> Result<(String, String, PathBuf), TerminalError> {
    let connection = store.read()?;
    let agent = AgentRepository::get(&connection, &AgentId::from(id))?
        .ok_or(TerminalError::InvalidRequest)?;
    if agent.extra.get("deletedAt").is_some_and(json_truthy) {
        return Err(TerminalError::InvalidRequest);
    }
    let agent_cwd = agent
        .extra
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(TerminalError::InvalidRequest)?
        .to_owned();
    let canonical = Path::new(&agent_cwd)
        .canonicalize()
        .map_err(|_| TerminalError::InvalidRequest)?;
    if !canonical.is_dir() {
        return Err(TerminalError::InvalidRequest);
    }
    Ok((agent.id.0, agent_cwd, canonical))
}

#[cfg(unix)]
fn terminal_shell() -> String {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/bin/sh".to_owned())
}

fn terminal_body_error(error: TerminalError) -> ApiError {
    match error {
        TerminalError::InvalidRequest
        | TerminalError::UnknownTerminal
        | TerminalError::NotRunning
        | TerminalError::ReceiptConflict => {
            ApiError::validation(vec![Value::String("body".to_owned())])
        }
        other => terminal_error(other),
    }
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn terminal_error(error: TerminalError) -> ApiError {
    match error {
        TerminalError::InvalidRequest
        | TerminalError::InvalidCursor
        | TerminalError::InvalidHistoryRange
        | TerminalError::NotRunning => {
            ApiError::validation(vec![Value::String("query".to_owned())])
        }
        TerminalError::UnknownTerminal => ApiError::validation(vec![
            Value::String("query".to_owned()),
            Value::String("id".to_owned()),
        ]),
        TerminalError::InvalidStoredObject(id) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object terminals/{id}"),
        ),
        TerminalError::Store(studio_runtime_store::StoreError::InvalidStoredObject {
            table,
            id,
        }) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        _ => ApiError::internal(),
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "terminals",
        routes: Some(routes),
        dependencies: ROUTE_DEPENDENCIES,
    }
}
