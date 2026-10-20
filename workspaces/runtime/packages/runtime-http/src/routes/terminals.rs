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
use std::sync::Arc;
#[cfg(unix)]
use studio_runtime_services::terminal_driver::TerminalCreate;
use studio_runtime_services::terminal_driver::{self, TerminalError};
use studio_runtime_store::{AgentId, AgentRepository};

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

#[cfg(unix)]
async fn require_reconciled_driver(state: &HttpState) -> Result<(), ApiError> {
    let Some(gate) = state.terminal_write_gate_handle() else {
        return Ok(());
    };
    let reconciled = gate().await;
    if reconciled {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Terminal restart cleanup is still pending",
        ))
    }
}

async fn listing(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::TerminalList>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = terminal_driver::listing(&store)
        .await
        .map_err(terminal_error)?;
    Ok(Json(
        super::generated_json::<crate::generated::TerminalList>(result)?,
    ))
}

async fn output(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::TerminalOutput>, ApiError> {
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
    let result = if history {
        terminal_driver::history_output(Arc::clone(&store), &id, offset, limit)
            .await
            .map_err(terminal_error)?
    } else {
        terminal_driver::output(&store, &id, offset)
            .await
            .map_err(terminal_error)?
    };
    Ok(Json(super::generated_json::<
        crate::generated::TerminalOutput,
    >(result)?))
}

async fn create(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<crate::generated::TerminalRecord>, ApiError> {
    #[cfg(unix)]
    {
        require_reconciled_driver(&state).await?;
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let agent_id = body
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::validation(vec![Value::String("body".into())]))?
            .to_owned();
        let stored = store
            .read_async("runtime-http/routes/terminals", move |connection| {
                AgentRepository::get(connection, &AgentId::from(agent_id.as_str()))
                    .map_err(TerminalError::from)
            })
            .await
            .map_err(terminal_body_error)?;
        let (agent, agent_cwd, cwd) = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/terminals",
            move || checked_agent_cwd(stored),
        )
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
        let result = driver
            .create(TerminalCreate {
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
            .await
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
) -> Result<Json<TerminalActionResponse>, ApiError> {
    #[cfg(unix)]
    {
        require_reconciled_driver(&state).await?;
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
        let result = driver
            .action(&action, &body)
            .await
            .map_err(terminal_body_error)?;
        if is_input {
            Ok(Json(TerminalActionResponse::Input(
                super::generated_json::<crate::generated::TerminalInputResult>(result)?,
            )))
        } else {
            Ok(Json(TerminalActionResponse::Record(
                super::generated_json::<crate::generated::TerminalRecord>(result)?,
            )))
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

#[derive(serde::Serialize)]
#[serde(untagged)]
enum TerminalActionResponse {
    Input(crate::generated::TerminalInputResult),
    Record(crate::generated::TerminalRecord),
}

/// Check a stored agent and canonicalize its working directory (filesystem work).
fn checked_agent_cwd(
    agent: Option<studio_runtime_store::AgentRecord>,
) -> Result<(String, String, PathBuf), TerminalError> {
    let agent = agent.ok_or(TerminalError::InvalidRequest)?;
    if agent
        .extra
        .get("deletedAt")
        .is_some_and(|value| studio_runtime_store::truthy(Some(value)))
    {
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
        TerminalError::InvalidStoredObject(id) => ApiError::invalid_stored("terminals", &id),
        TerminalError::Store(studio_runtime_store::StoreError::InvalidStoredObject {
            table,
            id,
        }) => ApiError::invalid_stored(table, &id),
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

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use studio_runtime_store::{RuntimeStore, TerminalsRepository};
    use tower::ServiceExt;

    #[tokio::test]
    async fn pending_restart_cleanup_returns_retryable_error_without_terminal_writes() {
        let tmpfs = std::env::var_os("CS_TERMINAL_TEST_STATE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("cs-terminals-test-state"));
        std::fs::create_dir_all(&tmpfs).expect("tmpfs state parent");
        let root = tempfile::Builder::new()
            .prefix("terminal-http-gate-")
            .tempdir_in(tmpfs)
            .expect("isolated terminal HTTP state");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store"),
        );
        let before = TerminalsRepository::list(&store.test_reader().expect("read before request"))
            .expect("read terminal rows before request");
        let state = crate::router::HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().to_string_lossy().into_owned(),
            12345,
            None,
            Some(store.clone()),
            None,
        )
        .with_terminal_write_gate(Arc::new(|| Box::pin(async { false })));
        let app = crate::router::router(state).expect("HTTP router composes the terminal route");
        let body = json!({"id":"pending-recovery-create","agent":"agent"});
        let request = Request::builder()
            .method("POST")
            .uri("/api/terminals/create")
            .header("host", "127.0.0.1:12345")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header("x-canvas-workspace", "workspace")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .body(Body::from(body.to_string()))
            .expect("terminal request builds");
        let mut request = request;
        request
            .extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))));
        let response = app.oneshot(request).await.expect("terminal response");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(response.into_body(), 4096)
            .await
            .expect("response body reads");
        let body: Value = serde_json::from_slice(&bytes).expect("error envelope parses");
        assert_eq!(
            body,
            json!({"error":"Terminal restart cleanup is still pending"})
        );
        let after = TerminalsRepository::list(&store.test_reader().expect("read after request"))
            .expect("read terminal rows after request");
        assert_eq!(after.len(), before.len());
        assert_eq!(after, before);
    }
}
