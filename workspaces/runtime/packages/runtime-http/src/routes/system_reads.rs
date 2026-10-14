//! System reads that can be served from local state or the local filesystem.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::system_reads::{self, SystemReadError};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};
use crate::error::ApiError;
use crate::router::HttpState;

const REMOTE_BROWSE_DEPENDENCY: [Dependency; 2] = [
    Dependency::new("multi-server/discovery-pairing"),
    Dependency::new("multi-server/folder-browse"),
];
const DIAGNOSTICS_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("runtime-process/studio-loaded-threads"),
    Dependency::new("runtime-process/supervisor-status"),
    Dependency::new("runtime-process/process-tree"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/directories",
            OperationStatus::partial(&REMOTE_BROWSE_DEPENDENCY),
            directories,
        ),
        get(
            "/api/diagnostics",
            OperationStatus::partial(&DIAGNOSTICS_DEPENDENCIES),
            diagnostics,
        ),
        get("/api/ui-summary", OperationStatus::Implemented, ui_summary),
    ])
}

async fn directories(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let server = query
        .typed_value("server")
        .and_then(Value::as_str)
        .unwrap_or("");
    let local = server.is_empty()
        || server == "local"
        || local_server_id(&state.state_dir).as_deref() == Some(server);
    if !local {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let server = server.to_owned();
        let paired = tokio::task::spawn_blocking(move || {
            let mut connection = store.read().map_err(|_| ApiError::internal())?;
            let tx = connection.transaction().map_err(|_| ApiError::internal())?;
            use studio_runtime_store::system_reads as records;
            let client =
                records::get(&tx, "runtime_access_clients", &server).map_err(ApiError::stored)?;
            if let Some(client) = client {
                // The table schema has checked the complete access record;
                // directories consumes only this typed pairing projection.
                #[derive(serde::Deserialize)]
                struct AccessSelection {
                    kind: String,
                    status: String,
                    revoked: Option<f64>,
                }
                let client: AccessSelection = serde_json::from_value(client).map_err(|_| {
                    ApiError::stored(records::invalid("runtime_access_clients", &server))
                })?;
                return Ok::<bool, ApiError>(
                    client.kind == "server"
                        && client.status == "paired"
                        && client.revoked.unwrap_or(0.0) == 0.0,
                );
            }
            if records::get(&tx, "runtime_access_discovered", &server)
                .map_err(ApiError::stored)?
                .is_some()
            {
                return Err(ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Not implemented: requires multi-server/discovery-pairing",
                ));
            }
            Ok(false)
        })
        .await
        .map_err(|_| ApiError::internal())??;
        if !paired {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "The server is not paired or is revoked",
            ));
        }
        return Err(dependency_error(
            "GET",
            "/api/directories",
            &REMOTE_BROWSE_DEPENDENCY,
        ));
    }
    let path = query
        .typed_value("path")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let result = tokio::task::spawn_blocking(move || system_reads::directories(path.as_deref()))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(directory_error)?;
    super::generated_json::<crate::generated::DirectoriesResponse>(
        serde_json::to_value(result).map_err(|_| ApiError::internal())?,
    )
    .map(Json)
}

async fn diagnostics(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let mut missing_ports = Vec::new();
    if state.diagnostics_provider.is_none() {
        // Empty native-account maps are valid in the schema. The live loaded
        // thread count is required and cannot be inferred without the port.
        missing_ports.push(DIAGNOSTICS_DEPENDENCIES[0]);
    }
    if state.diagnostics_supervisor.is_none() {
        missing_ports.push(DIAGNOSTICS_DEPENDENCIES[1]);
    }
    if !missing_ports.is_empty() {
        return Err(dependency_error("GET", "/api/diagnostics", &missing_ports));
    }
    let (provider_port, supervisor_port) =
        match (state.diagnostics_provider, state.diagnostics_supervisor) {
            (Some(provider), Some(supervisor)) => (provider, supervisor),
            _ => {
                return Err(dependency_error("GET", "/api/diagnostics", &missing_ports));
            }
        };
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (
        provider,
        supervisor,
        process_tree,
        host_resources,
        durable_input_pending,
        executions,
        migrations,
    ) = tokio::task::spawn_blocking(move || {
        let provider = provider_port.snapshot();
        let supervisor = supervisor_port.snapshot();
        let (process_tree, host_resources) = studio_runtime_services::diagnostics::system_snapshot(
            studio_runtime_services::diagnostics::current_process_id(),
        );
        let Some(process_tree) = process_tree else {
            return Err(dependency_error(
                "GET",
                "/api/diagnostics",
                &DIAGNOSTICS_DEPENDENCIES[2..3],
            ));
        };
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let pending = studio_runtime_services::diagnostics::durable_input_pending(&connection)
            .map_err(|_| ApiError::internal())?;
        let ledger = studio_runtime_services::diagnostics::execution_ledger(&connection)
            .map_err(|_| ApiError::internal())?;
        let migrations = studio_runtime_services::diagnostics::migration_status(&connection)
            .map_err(|_| ApiError::internal())?;
        Ok::<_, ApiError>((
            provider,
            supervisor,
            process_tree,
            host_resources,
            pending,
            ledger,
            migrations,
        ))
    })
    .await
    .map_err(|_| ApiError::internal())??;

    let mut queues = provider.queues;
    queues.insert(
        "durableInputPending".to_owned(),
        serde_json::json!(durable_input_pending),
    );
    let mut attribution = vec![serde_json::json!({
        "component": "process",
        "operation": "ps",
        "count": process_tree["processes"].as_array().map_or(0, Vec::len),
    })];
    if let Some(kinds) = process_tree["kinds"].as_object() {
        for (kind, totals) in kinds {
            attribution.push(serde_json::json!({
                "component": kind,
                "operation": "resident",
                "count": totals["count"],
                "residentBytes": (totals["rssMiB"].as_f64().unwrap_or_default() * 1024.0 * 1024.0).round() as u64,
            }));
        }
    }
    let supervisor = serde_json::json!({
        "mode": supervisor.mode,
        "fallback": supervisor.fallback,
        "notice": supervisor.notice,
        "health": supervisor.health,
    });
    let value = serde_json::json!({
        "at": studio_runtime_services::diagnostics::now_rfc3339(),
        "processTree": process_tree,
        "hostResources": host_resources,
        "resourceAttribution": attribution,
        "nativeAccounts": provider.native_accounts,
        "studioLoadedThreads": provider.studio_loaded_threads,
        "queues": queues,
        "runtimeLockSamples": studio_runtime_services::diagnostics::runtime_lock_samples(),
        "executions": executions,
        "sqliteContention": studio_runtime_services::diagnostics::sqlite_contention(),
        "httpRequestMetrics": studio_runtime_services::diagnostics::http_request_metrics(),
        "analyticsCapture": {"available": false},
        "migrations": migrations,
        "searchMigrationError": null,
        "runtimeLockOperations": studio_runtime_services::diagnostics::runtime_lock_operations(),
        "supervisor": supervisor,
    });
    if !state
        .validators
        .validate_stored("DiagnosticsResponse", &value)
    {
        return Err(ApiError::internal());
    }
    super::generated_json::<crate::generated::DiagnosticsResponse>(value).map(Json)
}

async fn ui_summary(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let account_driver = state.account_driver;
    let account_policy = state.account_policy;
    let value = tokio::task::spawn_blocking(move || {
        let accounts = if let Some(driver) = account_driver {
            Some(
                driver
                    .listing_snapshot()
                    .map_err(|_| ApiError::internal())?,
            )
        } else {
            account_policy.map(|policy| policy.accounts())
        };
        let mut connection = store.read().map_err(|_| ApiError::internal())?;
        let transaction = connection.transaction().map_err(|_| ApiError::internal())?;
        let value = system_reads::ui_summary(&transaction, accounts).map_err(ApiError::stored)?;
        super::generated_json::<crate::generated::UiSummaryResponse>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

fn directory_error(error: SystemReadError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
}

fn local_server_id(state_dir: &str) -> Option<String> {
    let raw =
        std::fs::read_to_string(std::path::Path::new(state_dir).join("multi-server/identity.json"))
            .ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("serverId")?
        .as_str()
        .map(str::to_owned)
}

fn dependency_error(method: &str, path: &str, deps: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, deps).unwrap_or_else(|_| ApiError::internal())
}

#[cfg(test)]
mod diagnostics_http_tests {
    use super::DIAGNOSTICS_DEPENDENCIES;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use rusqlite::Connection;
    use serde_json::Value as JsonValue;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::SystemTime;
    use studio_runtime_services::diagnostics::{
        ProviderDiagnosticsPort, ProviderDiagnosticsSnapshot, SupervisorDiagnosticsPort,
        SupervisorDiagnosticsSnapshot,
    };
    use studio_runtime_store::RuntimeStore;
    use tower::ServiceExt;

    use crate::router::{HttpState, router};
    use crate::validation::Registry;

    #[derive(Default)]
    struct ProviderDouble(AtomicUsize);

    impl ProviderDiagnosticsPort for ProviderDouble {
        fn snapshot(&self) -> ProviderDiagnosticsSnapshot {
            self.0.fetch_add(1, Ordering::Relaxed);
            ProviderDiagnosticsSnapshot {
                native_accounts: json!({}),
                studio_loaded_threads: 0,
                queues: serde_json::Map::new(),
            }
        }
    }

    #[derive(Default)]
    struct SupervisorDouble(AtomicUsize);

    impl SupervisorDiagnosticsPort for SupervisorDouble {
        fn snapshot(&self) -> SupervisorDiagnosticsSnapshot {
            self.0.fetch_add(1, Ordering::Relaxed);
            SupervisorDiagnosticsSnapshot {
                mode: false,
                fallback: false,
                notice: None,
                health: None,
            }
        }
    }

    #[tokio::test]
    async fn diagnostics_route_validates_full_schema_with_both_ports() {
        let directory = tempfile::tempdir().expect("tmpfs test state");
        create_python_schema_fixture(directory.path());
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path())).expect("open isolated runtime store"),
        );
        let provider = Arc::new(ProviderDouble::default());
        let supervisor = Arc::new(SupervisorDouble::default());
        let proof_store = store.clone();
        let state = HttpState::new(
            "fixture-token".to_owned(),
            "fixture-workspace".to_owned(),
            None,
            directory.path().display().to_string(),
            4321,
            None,
            Some(store),
            None,
        )
        .with_diagnostics_provider(provider.clone())
        .with_diagnostics_supervisor(supervisor.clone());
        let app = router(state).expect("registered HTTP router");
        let proof_connection = proof_store.read().expect("read-only write proof");
        let version_before: i64 = proof_connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("initial database version");
        let files_before = state_directory_snapshot(directory.path());
        let mut request = Request::builder()
            .method("GET")
            .uri("/api/diagnostics")
            .header("host", "127.0.0.1:4321")
            .body(Body::empty())
            .expect("diagnostics request");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:4321"
                .parse::<std::net::SocketAddr>()
                .expect("loopback socket address"),
        ));
        let response = app.oneshot(request).await.expect("HTTP response");
        let status = response.status();
        let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .expect("bounded diagnostics response");
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let value: Value = serde_json::from_slice(&body).expect("JSON diagnostics response");
        let expected_fields = [
            "at",
            "processTree",
            "hostResources",
            "resourceAttribution",
            "nativeAccounts",
            "studioLoadedThreads",
            "queues",
            "runtimeLockSamples",
            "executions",
            "sqliteContention",
            "httpRequestMetrics",
            "analyticsCapture",
            "migrations",
            "searchMigrationError",
            "runtimeLockOperations",
            "supervisor",
        ];
        assert_eq!(
            value.as_object().map(|fields| fields.len()),
            Some(expected_fields.len())
        );
        for field in expected_fields {
            assert!(
                value.get(field).is_some(),
                "missing Python response field {field}"
            );
        }
        assert_eq!(value["analyticsCapture"], json!({"available": false}));
        assert!(
            Registry::compile(&[("GET".to_owned(), "/api/diagnostics".to_owned())])
                .expect("OpenAPI response schema")
                .validate_stored("DiagnosticsResponse", &value),
            "Rust diagnostics response must satisfy the Python-owned OpenAPI schema"
        );
        assert_eq!(provider.0.load(Ordering::Relaxed), 1);
        assert_eq!(supervisor.0.load(Ordering::Relaxed), 1);
        let version_after: i64 = proof_connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("final database version");
        assert_eq!(
            version_before, version_after,
            "diagnostics route writes no rows"
        );
        assert_eq!(
            files_before,
            state_directory_snapshot(directory.path()),
            "diagnostics route does not change state directory names, sizes, or mtimes"
        );
    }

    #[tokio::test]
    async fn partial_route_lists_dependencies_with_schema_valid_error_and_no_writes() {
        let directory = tempfile::tempdir().expect("tmpfs test state");
        create_python_schema_fixture(directory.path());
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path())).expect("open isolated runtime store"),
        );
        let proof_connection = store.read().expect("read-only write proof");
        let version_before: i64 = proof_connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("initial database version");
        let files_before = state_directory_snapshot(directory.path());
        let state = HttpState::new(
            "fixture-token".to_owned(),
            "fixture-workspace".to_owned(),
            None,
            directory.path().display().to_string(),
            4321,
            None,
            Some(store),
            None,
        );
        let response = router(state)
            .expect("registered HTTP router")
            .oneshot(diagnostics_request())
            .await
            .expect("HTTP response");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("bounded error response");
        let value: Value = serde_json::from_slice(&body).expect("JSON error response");
        assert!(
            Registry::compile(&[("GET".to_owned(), "/api/diagnostics".to_owned())])
                .expect("OpenAPI response schema")
                .validate_stored("ErrorResponse", &value),
            "named partial response must satisfy the common OpenAPI error schema"
        );
        let error = value["error"].as_str().expect("error string");
        assert!(error.contains("runtime-process/studio-loaded-threads"));
        assert!(error.contains("runtime-process/supervisor-status"));
        assert!(!error.contains("provider-session/native-accounts"));
        let operations =
            crate::routes::manifest::operation_manifest().expect("registered operation manifest");
        let operation = operations
            .iter()
            .find(|operation| operation.key() == ("GET".to_owned(), "/api/diagnostics".to_owned()))
            .expect("diagnostics manifest entry");
        assert!(matches!(
            operation.status(),
            crate::routes::manifest::OperationStatus::Partial { dependencies }
                if dependencies.contains(&DIAGNOSTICS_DEPENDENCIES[0])
                    && dependencies.contains(&DIAGNOSTICS_DEPENDENCIES[1])
        ));
        let version_after: i64 = proof_connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("final database version");
        assert_eq!(
            version_before, version_after,
            "501 route does not write rows"
        );
        assert_eq!(
            files_before,
            state_directory_snapshot(directory.path()),
            "501 route does not change state directory names, sizes, or mtimes"
        );
    }

    fn diagnostics_request() -> Request<Body> {
        let mut request = Request::builder()
            .method("GET")
            .uri("/api/diagnostics")
            .header("host", "127.0.0.1:4321")
            .body(Body::empty())
            .expect("diagnostics request");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:4321"
                .parse::<std::net::SocketAddr>()
                .expect("loopback socket address"),
        ));
        request
    }

    fn state_directory_snapshot(root: &Path) -> Vec<(String, u64, Option<SystemTime>)> {
        fn visit(
            root: &Path,
            current: &Path,
            entries: &mut Vec<(String, u64, Option<SystemTime>)>,
        ) {
            let mut children = std::fs::read_dir(current)
                .expect("list isolated state directory")
                .map(|entry| entry.expect("state directory entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for path in children {
                let metadata = std::fs::symlink_metadata(&path).expect("state entry metadata");
                let relative = path
                    .strip_prefix(root)
                    .expect("state entry is under root")
                    .to_string_lossy()
                    .into_owned();
                entries.push((relative, metadata.len(), metadata.modified().ok()));
                if metadata.is_dir() {
                    visit(root, &path, entries);
                }
            }
        }
        let mut entries = Vec::new();
        visit(root, root, &mut entries);
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    fn create_python_schema_fixture(root: &Path) {
        let schema: JsonValue = serde_json::from_str(include_str!(
            "../../../runtime-store/tests/fixtures/live-state-schema.json"
        ))
        .expect("Python-written live schema snapshot");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let db = Connection::open(root.join(name)).expect("fixture database");
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; BEGIN;")
                .expect("fixture durability");
            let mut objects = schema[name]["sqlite_master"]
                .as_array()
                .expect("Python schema objects")
                .iter()
                .filter(|object| {
                    ![
                        "runtime_search_next_config",
                        "runtime_search_next_data",
                        "runtime_search_next_docsize",
                        "runtime_search_next_idx",
                    ]
                    .contains(&object["name"].as_str().expect("schema object name"))
                })
                .collect::<Vec<_>>();
            objects.sort_by_key(|object| {
                (
                    match object["type"].as_str() {
                        Some("table") => 0,
                        Some("index") => 1,
                        Some("view") => 2,
                        _ => 3,
                    },
                    object["name"].as_str(),
                )
            });
            for object in objects {
                db.execute_batch(object["sql"].as_str().expect("Python schema DDL"))
                    .expect("create Python schema object");
            }
            db.execute_batch("COMMIT").expect("commit fixture schema");
        }
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name:"system_reads", routes:Some(routes), dependencies:&[
            "multi-server/discovery-pairing","multi-server/folder-browse",
            "runtime-process/process-tree","provider-session/native-accounts","runtime-process/studio-loaded-threads","runtime-process/supervisor-status",
        ],
    }
}
