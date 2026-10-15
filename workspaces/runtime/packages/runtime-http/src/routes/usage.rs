//! Local cost snapshots and persisted usage controls.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_services::usage::{self, UsageError};
use studio_runtime_store::{UsageRepositoryError, disable_usage_resume};

use crate::error::ApiError;
use crate::router::HttpState;

const CAPACITY_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("event-driver/queue-mutations"),
    Dependency::new("store-bootstrap/capacity-retry-schema"),
    Dependency::new("turn-driver/capacity-retry"),
];
const USAGE_RESUME_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("store-bootstrap/usage-resume-schema"),
    Dependency::new("turn-driver/usage-resume"),
];
const COSTS_DEPENDENCIES: [Dependency; 1] = [Dependency::new("costs/local-scanner")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/costs", OperationStatus::Implemented, costs),
        post(
            "/api/limits/reset",
            OperationStatus::Implemented,
            reset_limits,
        ),
        post(
            "/api/capacity-retry",
            OperationStatus::Implemented,
            capacity_retry,
        ),
        post(
            "/api/usage-resume",
            OperationStatus::Implemented,
            usage_resume,
        ),
    ])
}

async fn costs(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<crate::generated::AccountCostResponse>, ApiError> {
    let account_key = params
        .typed_value("account_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("default")
        .to_owned();
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let account = account_policy
        .accounts()
        .accounts
        .into_iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(&account_key))
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown Codex account"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let refresh_cancelled = state.cost_scan_cancellation_handle();
    let build_script = state.cost_scanner_build_script.clone();
    let state_dir = std::path::PathBuf::from(state.state_dir.clone());
    let refresh_state_dir = state_dir.clone();
    let refresh_account = account.clone();
    let refresh_key = account_key.clone();
    let snapshot_key = account_key.clone();
    let notification_key = refresh_key.clone();
    let notifications = state.resource_notifications.clone();
    let mut result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/usage",
        move || usage::account_cost_snapshot(&state_dir, &snapshot_key, &account, now),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(cost_error)?;
    let refresh_due = result.get("_refreshDue").and_then(Value::as_bool) == Some(true);
    if let Some(object) = result.as_object_mut() {
        object.remove("_refreshDue");
    }
    let refresh_claimed = refresh_due
        && result.get("refreshing").and_then(Value::as_bool) != Some(true)
        && build_script.is_some()
        && usage::claim_cost_refresh_attempt(&account_key);
    if refresh_claimed {
        let build_script = build_script.expect("the refresh requires the resolved scanner builder");
        let refresh_worker = state.track_cost_refresh_worker();
        if let Some(object) = result.as_object_mut() {
            object.insert("refreshing".to_owned(), Value::Bool(true));
        }
        tokio::spawn(async move {
            let _refresh_worker = refresh_worker;
            let outcome = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/usage",
                move || {
                    usage::refresh_account_costs_report(
                        &refresh_state_dir,
                        &refresh_key,
                        &refresh_account,
                        &refresh_cancelled,
                        &build_script,
                    )
                },
            )
            .await;
            let cache_saved = matches!(&outcome, Ok((true, _)));
            match outcome {
                Ok((_, Ok(()))) => {}
                Ok((_, Err(UsageError::ScannerCancelled))) => {}
                Ok((_, Err(error))) => eprintln!("Local cost refresh failed: {error}"),
                Err(error) => eprintln!("Local cost refresh task failed: {error}"),
            }
            if cache_saved {
                let request = serde_json::json!({
                    "requestId": format!("local-costs:{}:{now}", notification_key),
                    "resources": [{"kind":"costs"}],
                });
                if let Err(error) = notifications.publish_once(&request) {
                    eprintln!("Could not publish local cost refresh: {error}");
                }
            }
        });
    }
    Ok(Json(super::generated_json::<
        crate::generated::AccountCostResponse,
    >(result)?))
}

async fn reset_limits(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let session = state
        .account_session
        .clone()
        .ok_or_else(ApiError::internal)?;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/usage",
        move || {
            studio_runtime_services::limit_reset::consume_reset(&store, session.as_ref(), &body)
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(reset_error)?;
    Ok(Json(result))
}

fn reset_error(error: studio_runtime_services::limit_reset::ResetError) -> ApiError {
    use studio_runtime_services::limit_reset::ResetError;
    match error {
        ResetError::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        ResetError::Provider(message) => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message),
        ResetError::Store(_) | ResetError::Sql(_) | ResetError::Json(_) => ApiError::internal(),
    }
}

async fn capacity_retry(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CapacityRetryRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.action == crate::generated::RetryAction::Retry {
        let agent_id = body.id;
        let retry_id = body.retry_id;
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let now = current_time()?;
        let retry_for_read = retry_id.clone();
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/usage",
            move || {
                store.transaction_with(
                    |tx| -> Result<_, studio_runtime_services::continuation::ContinuationError> {
                        let staged =
                            studio_runtime_services::continuation_runtime::stage_capacity_retry(
                                tx, &agent_id, &retry_id, now, false,
                            )?;
                        let raw: String = tx
                        .query_row(
                            "SELECT record FROM runtime_capacity_retries WHERE id=?1 AND agent=?2",
                            rusqlite::params![retry_for_read, agent_id],
                            |row| row.get(0),
                        )
                        .map_err(studio_runtime_store::StoreError::from)?;
                        let value: Value = serde_json::from_str(&raw).map_err(|_| {
                            studio_runtime_services::continuation::ContinuationError::Invalid(
                                "/capacity/record".to_owned(),
                            )
                        })?;
                        let response = serde_json::from_value::<crate::generated::RetryResponse>(
                            value,
                        )
                        .map_err(|_| {
                            studio_runtime_services::continuation::ContinuationError::Invalid(
                                "/capacity/response".to_owned(),
                            )
                        })?;
                        Ok::<_, studio_runtime_services::continuation::ContinuationError>((
                            serde_json::to_value(response).map_err(|_| {
                                studio_runtime_services::continuation::ContinuationError::Invalid(
                                    "/capacity/response".to_owned(),
                                )
                            })?,
                            staged.changed(),
                        ))
                    },
                )
            },
        )
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(continuation_error)?;
        if result.1 {
            state.continuation_wake.notify_one();
            state.event_wake.notify_one();
        }
        return Ok(Json(result.0));
    }
    let agent_id = body.id;
    let retry_id = body.retry_id;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = current_time()?;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/usage",
        move || {
            let value = store.transaction_with(|tx| {
                studio_runtime_services::continuation_runtime::stage_capacity_cancel(
                    tx, &agent_id, &retry_id, now,
                )
            })?;
            let response = serde_json::from_value::<crate::generated::RetryResponse>(value)
                .map_err(|_| {
                    studio_runtime_services::continuation::ContinuationError::Invalid(
                        "/capacity/response".to_owned(),
                    )
                })?;
            serde_json::to_value(response).map_err(|_| {
                studio_runtime_services::continuation::ContinuationError::Invalid(
                    "/capacity/response".to_owned(),
                )
            })
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(continuation_error)?;
    state.continuation_wake.notify_one();
    state.event_wake.notify_one();
    Ok(Json(result))
}

async fn usage_resume(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::UsageResumeRequest>,
) -> Result<Json<Option<Value>>, ApiError> {
    let agent_id = body.id;
    let resume_id = body.resume_id;
    let enabled = body.enabled;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = current_time()?;
    if enabled {
        let agent = agent_id.clone();
        let record = resume_id.clone();
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/usage",
            move || {
                studio_runtime_services::continuation_driver::drive(
                    &store,
                    &agent,
                    studio_runtime_store::ContinuationTable::Usage,
                    Some(&record),
                    now,
                    |snapshot| {
                        studio_runtime_services::usage_resume::set_enabled(snapshot, true, now)
                    },
                )
            },
        )
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(continuation_error)?;
        let value = result.record;
        state.continuation_wake.notify_one();
        state.event_wake.notify_one();
        let Some(value) = value else {
            return Ok(Json(None));
        };
        let response = serde_json::from_value::<crate::generated::UsageResumeResponse>(value)
            .map_err(|_| ApiError::internal())?;
        return Ok(Json(Some(
            serde_json::to_value(response).map_err(|_| ApiError::internal())?,
        )));
    }
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/usage",
        move || {
            store.transaction_with(
                |transaction| -> Result<Option<Value>, UsageRepositoryError> {
                    let value = disable_usage_resume(transaction, &agent_id, &resume_id, now)?;
                    if value.is_null() {
                        return Ok(None);
                    }
                    let response =
                        serde_json::from_value::<crate::generated::UsageResumeResponse>(value)
                            .map_err(|_| UsageRepositoryError::InvalidStoredObject {
                                table: "runtime_usage_resumes",
                                id: resume_id.clone(),
                            })?;
                    serde_json::to_value(response).map(Some).map_err(|_| {
                        UsageRepositoryError::InvalidStoredObject {
                            table: "runtime_usage_resumes",
                            id: resume_id.clone(),
                        }
                    })
                },
            )
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|error| usage_error(error, "/api/usage-resume"))?;
    state.continuation_wake.notify_one();
    state.event_wake.notify_one();
    Ok(Json(result))
}

fn current_time() -> Result<f64, ApiError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64())
}

fn cost_error(error: UsageError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
}

fn continuation_error(error: studio_runtime_services::continuation::ContinuationError) -> ApiError {
    match error {
        studio_runtime_services::continuation::ContinuationError::Invalid(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        studio_runtime_services::continuation::ContinuationError::Unavailable(_) => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
        studio_runtime_services::continuation::ContinuationError::Store(error) => {
            eprintln!("usage continuation store operation failed: {error}");
            ApiError::internal()
        }
        studio_runtime_services::continuation::ContinuationError::WakeFailures(_) => {
            ApiError::internal()
        }
    }
}

fn usage_error(error: UsageRepositoryError, path: &'static str) -> ApiError {
    match error {
        UsageRepositoryError::MissingFamily => ApiError::internal(),
        UsageRepositoryError::EventDriverRequired => ApiError::internal(),
        UsageRepositoryError::UnknownAgent => ApiError::new(StatusCode::NOT_FOUND, "Unknown agent"),
        UsageRepositoryError::UnknownContinuation => {
            let message = if path == "/api/capacity-retry" {
                "Unknown capacity retry."
            } else {
                "Unknown automatic resume"
            };
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        UsageRepositoryError::InvalidStoredObject { table, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        UsageRepositoryError::Sqlite(_) | UsageRepositoryError::Store(_) => ApiError::internal(),
    }
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

#[cfg(test)]
mod tests {
    use super::current_time;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method as HttpMethod, Request, StatusCode};
    use serde_json::{Value, json};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex};
    use studio_runtime_providers::{Method, RequestId, RequestTicket};
    use studio_runtime_services::account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    };
    use tower::ServiceExt;

    struct FakeResetSession {
        calls: Mutex<Vec<(String, Value)>>,
        reads: Mutex<usize>,
        scope: ConnectionScope,
    }

    impl AccountSessionPort for FakeResetSession {
        fn ensure_session(&self, account: &str) -> Result<ConnectionScope, AccountSessionError> {
            assert_eq!(account, "default");
            Ok(self.scope.clone())
        }

        fn read_limits(&self, account: &str) -> Result<Value, AccountSessionError> {
            assert_eq!(account, "default");
            *self.reads.lock().unwrap() += 1;
            Ok(
                json!({"accountKey":"default","data":{"accountId":"account-A","rateLimitResetCredits":{"credits":[{"id":"credit-A","status":"available","resetType":"codexRateLimits"}]}}}),
            )
        }

        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> Result<SessionReceipt, AccountSessionError> {
            assert_eq!(request.method, Method::AccountRateLimitResetCreditConsume);
            self.calls
                .lock()
                .unwrap()
                .push((request.operation.clone(), request.params.clone()));
            let (certainty, result, ticket) = if self.calls.lock().unwrap().len() == 1 {
                (RequestCertainty::NotSent, None, None)
            } else {
                (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Reply(json!({"outcome":"reset"}))),
                    Some(RequestTicket {
                        generation: self.scope.generation,
                        id: RequestId::String(request.operation.clone()),
                        method: request.method,
                    }),
                )
            };
            Ok(SessionReceipt {
                scope: self.scope.clone(),
                operation: request.operation,
                ticket,
                certainty,
                result,
                not_sent_reason: None,
                replayed: false,
            })
        }
    }

    fn reset_request(body: Value) -> Request<Body> {
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/api/limits/reset")
            .header("host", "127.0.0.1:49172")
            .header("content-type", "application/json")
            .header("x-canvas-token", "reset-test-token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                12345,
            )))
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[test]
    fn runtime_clock_is_a_finite_epoch_timestamp() {
        let now = current_time().expect("system epoch clock");
        assert!(now.is_finite() && now > 0.0);
    }

    #[tokio::test]
    async fn reset_http_route_retries_only_a_proven_not_sent_attempt() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(studio_runtime_store::RuntimeStore::open(Some(root.path())).unwrap());
        let session = Arc::new(FakeResetSession {
            calls: Mutex::new(Vec::new()),
            reads: Mutex::new(0),
            scope: ConnectionScope {
                account: "default".into(),
                connection: "fake-account-session".into(),
                generation: 1,
            },
        });
        let mut state = crate::router::HttpState::new(
            "reset-test-token".into(),
            "reset-test-workspace".into(),
            None,
            root.path().to_string_lossy().into_owned(),
            49_172,
            None,
            Some(store),
            None,
        );
        state.account_session = Some(session.clone());
        let app = crate::router::router(state).unwrap();
        let body = json!({
            "account_key":"default",
            "account_id":"account-A",
            "credit_id":"credit-A",
            "request_id":"11000000-0000-4000-8000-000000000005"
        });
        let first = app
            .clone()
            .oneshot(reset_request(body.clone()))
            .await
            .unwrap();
        let first_status = first.status();
        let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
            .await
            .unwrap();
        let first_json: Value = serde_json::from_slice(&first_body).unwrap();
        assert_eq!(first_status, StatusCode::OK, "{first_json}");
        assert_eq!(first_json["outcome"], "uncertain");

        let second = app.oneshot(reset_request(body)).await.unwrap();
        let second_status = second.status();
        let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap();
        let second_json: Value = serde_json::from_slice(&second_body).unwrap();
        assert_eq!(second_status, StatusCode::OK, "{second_json}");
        assert_eq!(second_json["outcome"], "reset");
        let calls = session.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert_eq!(calls[0].1["creditId"], "credit-A");
        assert_eq!(*session.reads.lock().unwrap(), 4);
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "usage",
        routes: Some(routes),
        dependencies: &[
            "costs/local-scanner",
            "event-driver/queue-mutations",
            "store-bootstrap/capacity-retry-schema",
            "store-bootstrap/usage-resume-schema",
            "turn-driver/capacity-retry",
            "turn-driver/usage-resume",
        ],
    }
}
