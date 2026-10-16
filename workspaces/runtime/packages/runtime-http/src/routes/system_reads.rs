//! System reads that can be served from local state or the local filesystem.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::system_reads::{self, SystemReadError};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, get};
use crate::error::ApiError;
use crate::router::HttpState;

const DIAGNOSTICS_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("runtime-process/studio-loaded-threads"),
    Dependency::new("runtime-process/supervisor-status"),
    Dependency::new("runtime-process/process-tree"),
];

const DIAGNOSTICS_RESPONSE_FIELDS: &[&str] = &[
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
    "blockingPoolMetrics",
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/directories",
            OperationStatus::Implemented,
            directories,
        ),
        get(
            "/api/diagnostics",
            OperationStatus::Implemented,
            diagnostics,
        ),
        get("/api/ui-summary", OperationStatus::Implemented, ui_summary),
    ])
}

async fn directories(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<crate::generated::DirectoriesResponse>, ApiError> {
    let server = query
        .typed_value("server")
        .and_then(Value::as_str)
        .unwrap_or("");
    let local = if server.is_empty() || server == "local" {
        true
    } else {
        let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
        store
            .local_server_id()
            .map_err(ApiError::stored)?
            .as_deref()
            == Some(server)
    };
    if !local {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let server_id = server.to_owned();
        let paired = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/system_reads",
            move || {
                let mut connection = store.read().map_err(|_| ApiError::internal())?;
                let tx = connection.transaction().map_err(|_| ApiError::internal())?;
                use studio_runtime_store::system_reads as records;
                let client = records::get(&tx, "runtime_access_clients", &server_id)
                    .map_err(ApiError::stored)?;
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
                        ApiError::stored(records::invalid("runtime_access_clients", &server_id))
                    })?;
                    return Ok::<bool, ApiError>(
                        client.kind == "server"
                            && client.status == "paired"
                            && client.revoked.unwrap_or(0.0) == 0.0,
                    );
                }
                if records::get(&tx, "runtime_access_discovered", &server_id)
                    .map_err(ApiError::stored)?
                    .is_some()
                {
                    return Err(ApiError::new(
                        StatusCode::FORBIDDEN,
                        "The server is not paired or is revoked",
                    ));
                }
                Ok(false)
            },
        )
        .await
        .map_err(|_| ApiError::internal())??;
        if !paired {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "The server is not paired or is revoked",
            ));
        }
        let driver = state.orchestration_driver.ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The remote server transport is unavailable",
            )
        })?;
        let path = query
            .typed_value("path")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let request_id = match query.typed_value("request_id").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => studio_runtime_services::remote_transport::new_nonce()
                .map_err(|_| ApiError::internal())?,
        };
        let server = server.to_owned();
        let now = studio_runtime_services::event_write::unix_time()
            .map_err(|_| ApiError::internal())? as i64;
        let result = driver
            .exchange(
                &server,
                "folders",
                &serde_json::json!({"cwd":path}),
                &request_id,
                now,
            )
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::from_u16(error.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                    error.to_string(),
                )
            })?;
        let value = match result {
            studio_runtime_services::multi_server_orchestration::ExchangeOutcome::Applied(
                value,
            ) => value,
            studio_runtime_services::multi_server_orchestration::ExchangeOutcome::NotApplied(
                message,
            ) => return Err(ApiError::new(StatusCode::BAD_REQUEST, message)),
            studio_runtime_services::multi_server_orchestration::ExchangeOutcome::Unknown
            | studio_runtime_services::multi_server_orchestration::ExchangeOutcome::PreEffectWaiting => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The remote folder browse outcome is unknown",
                ));
            }
        };
        return super::generated_json::<crate::generated::DirectoriesResponse>(value).map(Json);
    }
    let path = query
        .typed_value("path")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/system_reads",
        move || system_reads::directories(path.as_deref()),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(directory_error)?;
    super::generated_json::<crate::generated::DirectoriesResponse>(
        serde_json::to_value(result).map_err(|_| ApiError::internal())?,
    )
    .map(Json)
}

async fn diagnostics(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::DiagnosticsResponse>, ApiError> {
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
        return Err(named_dependency_error(&missing_ports));
    }
    let (provider_port, supervisor_port) =
        match (state.diagnostics_provider, state.diagnostics_supervisor) {
            (Some(provider), Some(supervisor)) => (provider, supervisor),
            _ => {
                return Err(named_dependency_error(&missing_ports));
            }
        };
    let store = state.store.ok_or_else(ApiError::internal)?;
    let provider = provider_port.snapshot().await;
    let (
        provider,
        supervisor,
        process_tree,
        host_resources,
        durable_input_pending,
        executions,
        migrations,
    ) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/system_reads",
        move || {
            let supervisor = supervisor_port.snapshot();
            let (process_tree, host_resources) =
                studio_runtime_services::diagnostics::system_snapshot(
                    studio_runtime_services::diagnostics::current_process_id(),
                );
            let Some(process_tree) = process_tree else {
                return Err(named_dependency_error(&DIAGNOSTICS_DEPENDENCIES[2..3]));
            };
            studio_runtime_services::diagnostics::log_on_demand_memory_snapshot();
            let connection = store.read().map_err(|_| ApiError::internal())?;
            let pending = studio_runtime_services::diagnostics::durable_input_pending(&connection)
                .map_err(|_| {
                    diagnostics_collection_error(DiagnosticsCollectionFailure::DurableInputPending)
                })?;
            let ledger = studio_runtime_services::diagnostics::execution_ledger(&connection)
                .map_err(|_| {
                    diagnostics_collection_error(DiagnosticsCollectionFailure::ExecutionLedger)
                })?;
            let migrations = studio_runtime_services::diagnostics::migration_status(&connection)
                .map_err(|_| {
                    diagnostics_collection_error(DiagnosticsCollectionFailure::MigrationStatus)
                })?;
            Ok::<_, ApiError>((
                provider,
                supervisor,
                process_tree,
                host_resources,
                pending,
                ledger,
                migrations,
            ))
        },
    )
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
    let blocking_pool_metrics = {
        let snapshot = studio_runtime_process::blocking_pool::snapshot();
        serde_json::json!({
            "enabled": snapshot.enabled,
            "queued": snapshot.queued,
            "running": snapshot.running,
            "peakOccupancy": snapshot.peak_occupancy,
            "groups": snapshot.groups.into_iter().map(|group| serde_json::json!({
                "group": group.group,
                "count": group.count,
                "queueWaitAvgMs": group.queue_wait_avg_ms,
                "queueWaitMaxMs": group.queue_wait_max_ms,
                "queueWaitP95Ms": group.queue_wait_p95_ms,
                "queueWaitP99Ms": group.queue_wait_p99_ms,
                "closureAvgMs": group.closure_avg_ms,
                "closureMaxMs": group.closure_max_ms,
                "closureP95Ms": group.closure_p95_ms,
                "closureP99Ms": group.closure_p99_ms,
            })).collect::<Vec<_>>(),
        })
    };
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
        "blockingPoolMetrics": blocking_pool_metrics,
    });
    let mut value = value;
    redact_untrusted_diagnostic_text(&mut value);
    strip_diagnostics_execution_extras(&state.validators, &mut value);
    if !state
        .validators
        .validate_stored("DiagnosticsResponse", &value)
    {
        let (field, keyword) = diagnostics_schema_rejection(&state.validators, &value);
        eprintln!(
            "diagnostics response schema rejection: schema=DiagnosticsResponse field={field} keyword={keyword}"
        );
        return Err(ApiError::internal());
    }
    super::generated_json::<crate::generated::DiagnosticsResponse>(value).map(Json)
}

fn diagnostics_schema_rejection(
    registry: &crate::validation::Registry,
    value: &Value,
) -> (&'static str, &'static str) {
    let rejection = registry.first_stored_error("DiagnosticsResponse", value);
    let Some((pointer, keyword)) = rejection else {
        return ("unlocated", "unavailable");
    };
    let field = pointer
        .strip_prefix('/')
        .and_then(|path| path.split('/').next())
        .and_then(|field| {
            DIAGNOSTICS_RESPONSE_FIELDS
                .iter()
                .copied()
                .find(|known| *known == field)
        })
        .unwrap_or("unlocated");
    let keyword = match keyword.as_str() {
        "additionalProperties" => "additionalProperties",
        "anyOf" => "anyOf",
        "const" => "const",
        "enum" => "enum",
        "format" => "format",
        "maximum" => "maximum",
        "minimum" => "minimum",
        "required" => "required",
        "type" => "type",
        "unevaluatedProperties" => "unevaluatedProperties",
        "uniqueItems" => "uniqueItems",
        _ => "other",
    };
    (field, keyword)
}

fn strip_diagnostics_execution_extras(
    registry: &crate::validation::Registry,
    response: &mut Value,
) {
    if let Some(executions) = response.get_mut("executions") {
        registry.strip_stored_extra_fields("ExecutionLedger", executions);
    }
}

#[derive(Clone, Copy)]
enum DiagnosticsCollectionFailure {
    DurableInputPending,
    ExecutionLedger,
    MigrationStatus,
}

impl DiagnosticsCollectionFailure {
    const fn label(self) -> &'static str {
        match self {
            Self::DurableInputPending => "durable_input_pending",
            Self::ExecutionLedger => "execution_ledger",
            Self::MigrationStatus => "migration_status",
        }
    }
}

fn diagnostics_collection_error(failure: DiagnosticsCollectionFailure) -> ApiError {
    eprintln!(
        "diagnostics collection failed: operation={}",
        failure.label()
    );
    ApiError::internal()
}

async fn ui_summary(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::UiSummaryResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let account_driver = state.account_driver;
    let account_policy = state.account_policy;
    let value = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/system_reads",
        move || {
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
            let value =
                system_reads::ui_summary(&transaction, accounts).map_err(ApiError::stored)?;
            super::generated_json::<crate::generated::UiSummaryResponse>(value)
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

fn directory_error(error: SystemReadError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
}

fn named_dependency_error(dependencies: &[Dependency]) -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        format!(
            "Not implemented: requires {}",
            dependencies
                .iter()
                .map(|dependency| dependency.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}

fn redact_untrusted_diagnostic_text(value: &mut Value) {
    fn redact_fields(value: &mut Value, fields: &[&str]) {
        match value {
            Value::Object(object) => {
                for field in fields {
                    if *field == "outcome"
                        && object.get(*field).and_then(Value::as_str) == Some("not_applied")
                    {
                        continue;
                    }
                    if *field == "searchMigrationError" && object.contains_key(*field) {
                        object.insert((*field).to_owned(), Value::Null);
                    } else if *field == "error" && object.contains_key(*field) {
                        object.insert(
                            (*field).to_owned(),
                            Value::String("Details redacted".to_owned()),
                        );
                    } else {
                        object.remove(*field);
                    }
                }
                for child in object.values_mut() {
                    redact_fields(child, fields);
                }
            }
            Value::Array(values) => {
                for child in values {
                    redact_fields(child, fields);
                }
            }
            _ => {}
        }
    }

    redact_fields(
        value,
        &[
            "error",
            "previousError",
            "detail",
            "reason",
            "result",
            "outcome",
            "notSubmittedReason",
            "prepareError",
            "responseError",
            "stdoutReaderError",
            "persistenceErrors",
            "stateReadError",
            "lastError",
            "claudeInputRequest",
            "input",
            "source",
            "configuration",
            "reviewTarget",
            "modelSettings",
            "searchMigrationError",
        ],
    );
}

#[cfg(test)]
mod diagnostics_http_tests {
    use super::{
        DIAGNOSTICS_RESPONSE_FIELDS, redact_untrusted_diagnostic_text,
        strip_diagnostics_execution_extras,
    };
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

    #[test]
    fn diagnostics_schema_rejection_logs_only_known_fields_and_keywords() {
        let registry = Registry::compile(&[]).expect("stored diagnostics schema");
        for value in [
            json!({
                "executions": {"runs": [{"unexpected-private-name": "secret-value"}]}
            }),
            json!({"private-root-name": "secret-value"}),
        ] {
            let (field, keyword) = super::diagnostics_schema_rejection(&registry, &value);
            assert!(
                field == "unlocated" || DIAGNOSTICS_RESPONSE_FIELDS.contains(&field),
                "the log field must come from the known response DTO"
            );
            assert!(matches!(
                keyword,
                "additionalProperties"
                    | "anyOf"
                    | "const"
                    | "enum"
                    | "format"
                    | "maximum"
                    | "minimum"
                    | "required"
                    | "type"
                    | "unevaluatedProperties"
                    | "uniqueItems"
                    | "other"
                    | "unavailable"
            ));
            assert!(!format!("{field}{keyword}").contains("secret"));
        }
    }

    #[test]
    fn diagnostics_projects_legacy_execution_extras_without_changing_stored_rows() {
        let connection = Connection::open_in_memory().expect("in-memory read fixture");
        connection
            .execute_batch(
                "CREATE TABLE runtime_execution_runs (
                    id TEXT PRIMARY KEY, agent TEXT, account TEXT, epoch INTEGER,
                    thread TEXT, turn TEXT, created REAL, record TEXT NOT NULL
                );
                CREATE TABLE runtime_execution_attempts (
                    id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL
                );",
            )
            .expect("fixture tables");
        let run = json!({
            "id": "run-1", "agent": "agent-1", "accountKey": "default",
            "epoch": 1, "threadId": null, "turnId": null, "created": 1.0,
            "status": "running", "firstAttemptId": null, "rootAttemptId": null,
            "latestAttemptId": null,
            "restartRecovery": {"startAttempt": {"id": "restart-attempt", "legacyField": true}},
            "disconnectRecovery": {"startAttempt": {"id": "disconnect-attempt", "legacyField": true}},
            "attempts": [], "nodes": [], "effects": [], "attemptIds": [],
            "requestIds": [], "inputEventIds": []
        });
        let attempt = json!({"id": "attempt-1", "legacyField": true});
        let raw_run = run.to_string();
        let raw_attempt = attempt.to_string();
        connection
            .execute(
                "INSERT INTO runtime_execution_runs(id, created, record) VALUES (?1, ?2, ?3)",
                rusqlite::params!["run-1", 1.0, raw_run],
            )
            .expect("stored run fixture");
        connection
            .execute(
                "INSERT INTO runtime_execution_attempts(id, run, record) VALUES (?1, ?2, ?3)",
                rusqlite::params!["attempt-1", "run-1", raw_attempt],
            )
            .expect("stored attempt fixture");

        let registry = Registry::compile(&[]).expect("stored diagnostics schema");
        let mut response = json!({
            "executions": studio_runtime_services::diagnostics::execution_ledger(&connection)
                .expect("read-only execution projection")
        });
        strip_diagnostics_execution_extras(&registry, &mut response);
        assert!(registry.validate_stored("ExecutionLedger", &response["executions"]));
        assert!(
            response["executions"]["runs"][0]["attempts"][0]
                .get("legacyField")
                .is_none()
        );
        assert!(
            response["executions"]["runs"][0]["restartRecovery"]["startAttempt"]
                .get("legacyField")
                .is_none()
        );
        assert!(
            response["executions"]["runs"][0]["disconnectRecovery"]["startAttempt"]
                .get("legacyField")
                .is_none()
        );

        let mut malformed_known_field = response["executions"].clone();
        malformed_known_field["runs"][0]["attempts"][0]["epoch"] = json!("invalid");
        assert!(!registry.validate_stored("ExecutionLedger", &malformed_known_field));

        let stored_run: String = connection
            .query_row(
                "SELECT record FROM runtime_execution_runs WHERE id='run-1'",
                [],
                |row| row.get(0),
            )
            .expect("read persisted run after projection");
        let stored_attempt: String = connection
            .query_row(
                "SELECT record FROM runtime_execution_attempts WHERE id='attempt-1'",
                [],
                |row| row.get(0),
            )
            .expect("read persisted attempt after projection");
        assert_eq!(stored_run, run.to_string());
        assert_eq!(stored_attempt, attempt.to_string());
    }

    #[test]
    fn diagnostics_redacts_free_form_errors_and_results_without_dropping_ids() {
        let secret = "diagnostics-private-request-marker";
        let mut value = json!({
            "executions": {"runs": [{"id": "run-1", "error": secret,
                "attempts": [{"claudeInputRequest": {"input": secret}}],
                "claudeInputRejection": {"outcome": "not_applied"},
                "nodes": [{"id": "node-1", "result": secret}]}]},
            "migrations": {"payloads": {"tasks": {"error": secret}}},
            "analyticsCapture": {"lastError": secret},
            "searchMigrationError": secret,
            "supervisor": {"health": {"handles": [{"id": "handle-1",
                "stdoutReaderError": secret, "persistenceErrors": {"write": secret}}]}}
        });
        redact_untrusted_diagnostic_text(&mut value);
        assert!(!value.to_string().contains(secret));
        assert_eq!(value["executions"]["runs"][0]["id"], "run-1");
        assert_eq!(value["executions"]["runs"][0]["error"], "Details redacted");
        assert_eq!(value["searchMigrationError"], Value::Null);
        assert_eq!(value["executions"]["runs"][0]["nodes"][0]["id"], "node-1");
        assert_eq!(
            value["executions"]["runs"][0]["claudeInputRejection"]["outcome"],
            "not_applied"
        );
        assert_eq!(
            value["supervisor"]["health"]["handles"][0]["id"],
            "handle-1"
        );
    }

    #[derive(Default)]
    struct ProviderDouble(AtomicUsize);

    impl ProviderDiagnosticsPort for ProviderDouble {
        fn snapshot<'a>(
            &'a self,
        ) -> futures_util::future::BoxFuture<'a, ProviderDiagnosticsSnapshot> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::Relaxed);
                ProviderDiagnosticsSnapshot {
                    native_accounts: json!({}),
                    studio_loaded_threads: 0,
                    queues: serde_json::Map::new(),
                }
            })
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

    #[test]
    fn app_server_log_opens_at_server_construction_not_http_state_creation() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let log_path = directory.path().join("app-server.log");
        let state = HttpState::new(
            "fixture-token".to_owned(),
            "fixture-workspace".to_owned(),
            None,
            directory.path().display().to_string(),
            4321,
            None,
            None,
            None,
        );
        assert!(!log_path.exists());
        let _state = state.open_app_server_log();
        assert!(log_path.exists());
    }

    #[tokio::test]
    async fn diagnostics_route_validates_full_schema_with_both_ports() {
        let directory = tempfile::tempdir().expect("tmpfs test state");
        create_python_schema_fixture(directory.path());
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open isolated runtime store"),
        );
        let provider = Arc::new(ProviderDouble::default());
        let supervisor = Arc::new(SupervisorDouble::default());
        let proof_store = store.clone();
        let files_before = state_directory_snapshot(directory.path());
        let state = HttpState::new(
            "fixture-token".to_owned(),
            "fixture-workspace".to_owned(),
            None,
            directory.path().display().to_string(),
            4321,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .with_diagnostics_provider(provider.clone())
        .with_diagnostics_supervisor(supervisor.clone());
        let app = router(state).expect("registered HTTP router");
        let proof_connection = proof_store.read().expect("read-only write proof");
        let version_before: i64 = proof_connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("initial database version");
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
        assert_eq!(
            value.as_object().map(|fields| fields.len()),
            Some(DIAGNOSTICS_RESPONSE_FIELDS.len())
        );
        for field in DIAGNOSTICS_RESPONSE_FIELDS {
            assert!(
                value.get(*field).is_some(),
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
    async fn missing_diagnostics_ports_are_named_and_leave_no_writes() {
        let directory = tempfile::tempdir().expect("tmpfs test state");
        create_python_schema_fixture(directory.path());
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open isolated runtime store"),
        );
        let store_lifetime_guard = Arc::clone(&store);
        let proof_connection = store.read().expect("read-only write proof");
        let version_before: i64 = proof_connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("initial database version");
        let state = HttpState::new(
            "fixture-token".to_owned(),
            "fixture-workspace".to_owned(),
            None,
            directory.path().display().to_string(),
            4321,
            None,
            Some(Arc::clone(&store)),
            None,
        );
        // HttpState creates the operational log as part of setup; take the
        // no-write baseline after setup and immediately before the route call.
        let files_before = state_directory_snapshot(directory.path());
        let response = router(state)
            .expect("registered HTTP router")
            .oneshot(diagnostics_request())
            .await
            .expect("HTTP response");
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("bounded error response");
        assert_eq!(
            status,
            StatusCode::NOT_IMPLEMENTED,
            "unexpected diagnostics response: {}",
            String::from_utf8_lossy(&body)
        );
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
        assert_eq!(
            operation.status(),
            crate::routes::manifest::OperationStatus::Implemented
        );
        let version_after: i64 = proof_connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("final database version");
        assert_eq!(
            version_before, version_after,
            "diagnostics refusal does not write rows"
        );
        assert_eq!(
            files_before,
            state_directory_snapshot(directory.path()),
            "diagnostics refusal does not change state directory names, sizes, or mtimes"
        );
        drop(store_lifetime_guard);
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
                if path
                    .file_name()
                    .is_some_and(|name| name == "app-server.log")
                {
                    continue;
                }
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
            "runtime-process/process-tree","provider-session/native-accounts","runtime-process/studio-loaded-threads","runtime-process/supervisor-status",
        ],
    }
}
