//! Keeps admitted HTTP mutations alive when their response connection closes.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use std::future::Future;
use std::panic;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

use crate::error::{ApiError, InternalErrorCause, InternalErrorKind, error_response};

#[derive(Clone, Copy)]
pub(crate) struct TrackedTaskPanicLogged;

#[derive(Clone, Copy)]
pub(crate) struct IncidentCaptureAttempted;

/// Owns admitted HTTP mutation futures until they complete or server shutdown drains them.
pub(crate) struct MutationTasks {
    admission_open: Mutex<bool>,
    tracker: TaskTracker,
}

#[derive(Clone, Copy, Debug)]
struct AdmissionClosed;

impl MutationTasks {
    pub(crate) fn new() -> Self {
        Self {
            admission_open: Mutex::new(true),
            tracker: TaskTracker::new(),
        }
    }

    fn admission(&self) -> MutexGuard<'_, bool> {
        self.admission_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn spawn<F, T>(&self, future: F) -> Result<JoinHandle<T>, AdmissionClosed>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let admission = self.admission();
        if !*admission {
            return Err(AdmissionClosed);
        }
        Ok(self.tracker.spawn(future))
    }

    pub(crate) async fn run_response<F>(
        &self,
        future: F,
        route: String,
        request_id: Option<String>,
        store: Option<Arc<studio_runtime_store::RuntimeStore>>,
    ) -> Response
    where
        F: Future<Output = Response> + Send + 'static,
    {
        let request_id = request_id.filter(|value| crate::middleware::safe_request_id(value));
        let tracked = async move {
            let mut response = match std::panic::AssertUnwindSafe(future).catch_unwind().await {
                Ok(response) => response,
                Err(payload) => {
                    // This task may outlive its HTTP waiter, so log before returning the normal 500.
                    crate::middleware::log_internal_error(
                        &route,
                        request_id.as_deref(),
                        InternalErrorKind::HandlerPanic,
                    );
                    drop(payload);
                    let mut response =
                        error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error");
                    response
                        .extensions_mut()
                        .insert(InternalErrorCause(InternalErrorKind::HandlerPanic));
                    response.extensions_mut().insert(TrackedTaskPanicLogged);
                    response
                }
            };
            let diagnostic = crate::middleware::http_failure(
                &response,
                &route,
                request_id.as_deref(),
                studio_runtime_schema::studio_errors::EffectCertainty::Unknown,
                None,
            );
            if diagnostic.is_some() {
                crate::middleware::capture_http_failure(store.as_deref(), diagnostic).await;
                response.extensions_mut().insert(IncidentCaptureAttempted);
            }
            response
        };
        let handle = match self.spawn(tracked) {
            Ok(handle) => handle,
            Err(AdmissionClosed) => {
                return error_response(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
            }
        };

        match handle.await {
            Ok(response) => response,
            Err(error) if error.is_panic() => panic::resume_unwind(error.into_panic()),
            Err(error) => {
                eprintln!("ERROR tracked HTTP mutation task failed to join: {error}");
                ApiError::internal_with_cause(InternalErrorKind::Unclassified).into_response()
            }
        }
    }

    /// Atomically prevents new admissions before the tracker is closed for draining.
    pub(crate) fn close_admission(&self) {
        let mut admission = self.admission();
        *admission = false;
        self.tracker.close();
    }

    /// Closes admission and waits for every already-registered operation to finish.
    pub(crate) async fn shutdown(&self) {
        self.close_admission();
        self.tracker.wait().await;
    }
}

/// Runs admitted API mutations in a tracker-owned task, independent of the response waiter.
pub(crate) async fn own_mutations(
    State(state): State<crate::HttpState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !is_api_mutation(request.method(), request.uri().path()) {
        return next.run(request).await;
    }

    let route = crate::middleware::diagnostic_route(&state, &request);
    let request_id = request
        .headers()
        .get("x-studio-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| crate::middleware::safe_request_id(value))
        .map(str::to_owned);
    state
        .mutation_tasks
        .run_response(next.run(request), route, request_id, state.store.clone())
        .await
}

fn is_api_mutation(method: &Method, path: &str) -> bool {
    (path == "/api" || path.starts_with("/api/"))
        && matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE")
}

#[cfg(test)]
mod tests {
    use super::{MutationTasks, TrackedTaskPanicLogged, own_mutations};
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::any;
    use axum::{Router, response::IntoResponse};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::Notify;
    use tower::ServiceExt;
    fn test_http_state(tasks: Arc<MutationTasks>) -> crate::HttpState {
        let mut state = crate::HttpState::new(
            "token".into(),
            "fixture".into(),
            std::env::temp_dir().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        );
        state.mutation_tasks = tasks;
        state
    }

    #[tokio::test]
    async fn dropped_response_waiter_keeps_failure_capture_and_sse_in_the_owned_lifetime() {
        use futures_util::StreamExt;
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("store"),
        );
        studio_runtime_services::entity_publication::install(&store)
            .await
            .expect("startup publication");
        let state = crate::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            root.path().to_string_lossy().into_owned(),
            49171,
            None,
            Some(store.clone()),
            None,
        );
        let tasks = state.mutation_tasks.clone();
        let started = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (handler_started, handler_finish, handler_calls, handler_store) = (
            started.clone(),
            finish.clone(),
            calls.clone(),
            store.clone(),
        );
        let app = Router::new()
            .route(
                "/api/owned-failure",
                any(move || {
                    let (started, finish, calls, store) = (
                        handler_started.clone(),
                        handler_finish.clone(),
                        handler_calls.clone(),
                        handler_store.clone(),
                    );
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        finish.notified().await;
                        let error = store
                            .read_async("owned failure fixture", |tx| {
                                tx.query_row(
                                    "SELECT value FROM missing_private_canary",
                                    [],
                                    |row| row.get::<_, String>(0),
                                )
                                .map_err(studio_runtime_store::StoreError::from)
                            })
                            .await
                            .expect_err("real SQLite failure");
                        crate::error::ApiError::stored(error).into_response()
                    }
                }),
            )
            .layer(from_fn_with_state(state.clone(), own_mutations))
            .layer(from_fn_with_state(
                state.clone(),
                crate::middleware::response_safety,
            ));
        let stream_request = Request::builder()
            .method("GET")
            .uri("/api/sync/stream?protocol=4&resources=%5B%7B%22kind%22%3A%22state%22%7D%5D")
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", "fixture-token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::empty())
            .expect("stream request");
        let response = crate::router(state.clone())
            .expect("actual stream router")
            .oneshot(stream_request)
            .await
            .expect("stream");
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let waiter = tokio::spawn(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/owned-failure")
                    .header("x-studio-request-id", "disconnected-failure")
                    .body(Body::empty())
                    .expect("request"),
            ),
        );
        started.notified().await;
        waiter.abort();
        assert!(waiter.await.expect_err("waiter aborted").is_cancelled());
        finish.notify_one();
        tasks.shutdown().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the admitted handler is never replayed"
        );
        let rows = store
            .read_async("failure evidence", |tx| {
                studio_runtime_services::studio_errors::list(
                    tx,
                    studio_runtime_services::event_write::unix_time().expect("time"),
                )
            })
            .await
            .expect("incident list");
        assert_eq!(
            rows["items"].as_array().expect("items").len(),
            1,
            "capture survives loss of its HTTP waiter: {rows}"
        );
        assert!(!rows.to_string().contains("private_canary"));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(chunk) = stream.next().await {
                let text = String::from_utf8(chunk.expect("frame").to_vec()).expect("UTF8");
                for data in text.lines().filter_map(|line| line.strip_prefix("data: ")) {
                    let frame: serde_json::Value = serde_json::from_str(data).expect("SSE JSON");
                    for version in frame["resourceVersions"].as_array().into_iter().flatten() {
                        for row in version["entityChanges"]["documents"]
                            .as_array()
                            .into_iter()
                            .flatten()
                        {
                            if row["value"]["diagnostics"]["context"]["requestId"]
                                == "disconnected-failure"
                            {
                                assert_eq!(row["value"]["diagnostics"]["certainty"], "unknown");
                                return;
                            }
                        }
                    }
                }
            }
            panic!("stream ended before the owned failure");
        })
        .await
        .expect("owned failure arrives through actual SSE");
    }

    #[tokio::test]
    async fn dropped_response_waiter_does_not_cancel_admitted_mutation() {
        let tasks = Arc::new(MutationTasks::new());
        let commits = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let route_commits = Arc::clone(&commits);
        let route_started = Arc::clone(&started);
        let route_finish = Arc::clone(&finish);
        let app = Router::new()
            .route(
                "/api/write",
                any(move || {
                    let commits = Arc::clone(&route_commits);
                    let started = Arc::clone(&route_started);
                    let finish = Arc::clone(&route_finish);
                    async move {
                        started.notify_one();
                        finish.notified().await;
                        commits.fetch_add(1, Ordering::SeqCst);
                        "committed".into_response()
                    }
                }),
            )
            .layer(from_fn_with_state(
                test_http_state(Arc::clone(&tasks)),
                own_mutations,
            ));

        let request = tokio::spawn(
            app.clone().oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/write")
                    .body(Body::empty())
                    .expect("request"),
            ),
        );
        started.notified().await;
        request.abort();
        let join_error = request
            .await
            .expect_err("aborted request task is cancelled");
        assert!(join_error.is_cancelled());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        finish.notify_one();

        tasks.shutdown().await;
        assert_eq!(commits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shutdown_closes_admission_and_drains_registered_work() {
        let tasks = Arc::new(MutationTasks::new());
        let started = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let route_started = Arc::clone(&started);
        let route_finish = Arc::clone(&finish);
        let app = Router::new()
            .route(
                "/api/write",
                any(move || {
                    let started = Arc::clone(&route_started);
                    let finish = Arc::clone(&route_finish);
                    async move {
                        started.notify_one();
                        finish.notified().await;
                        "done"
                    }
                }),
            )
            .layer(from_fn_with_state(
                test_http_state(Arc::clone(&tasks)),
                own_mutations,
            ));
        let request = tokio::spawn(
            app.clone().oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/write")
                    .body(Body::empty())
                    .expect("request"),
            ),
        );

        started.notified().await;

        // The route started only after the owner admitted the request.
        let shutdown = tasks.shutdown();
        tokio::pin!(shutdown);
        assert!(futures_util::poll!(shutdown.as_mut()).is_pending());

        let closed = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/write")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("service response");
        assert_eq!(closed.status(), StatusCode::SERVICE_UNAVAILABLE);

        finish.notify_one();
        request.await.expect("request task").expect("response");
        shutdown.await;
    }

    #[tokio::test]
    async fn dropping_read_only_request_still_cancels_its_handler() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let tasks = Arc::new(MutationTasks::new());
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let route_started = Arc::clone(&started);
        let route_dropped = Arc::clone(&dropped);
        let app = Router::new()
            .route(
                "/api/read",
                any(move || {
                    let started = Arc::clone(&route_started);
                    let dropped = Arc::clone(&route_dropped);
                    async move {
                        let _guard = DropFlag(dropped);
                        started.notify_one();
                        std::future::pending::<()>().await;
                        "unreachable"
                    }
                }),
            )
            .layer(from_fn_with_state(
                test_http_state(Arc::clone(&tasks)),
                own_mutations,
            ));
        let request = tokio::spawn(
            app.oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/read")
                    .body(Body::empty())
                    .expect("request"),
            ),
        );
        started.notified().await;
        request.abort();
        let join_error = request
            .await
            .expect_err("aborted request task is cancelled");
        assert!(join_error.is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn tracked_handler_panic_becomes_a_context_logged_internal_error() {
        async fn fixture_panics() -> &'static str {
            panic!("fixture panic")
        }

        let tasks = Arc::new(MutationTasks::new());
        let app = Router::new()
            .route("/api/write", any(fixture_panics))
            .layer(from_fn_with_state(test_http_state(tasks), own_mutations));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/write")
                    .header("x-studio-request-id", "panic-request")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("service response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            response
                .extensions()
                .get::<TrackedTaskPanicLogged>()
                .is_some()
        );
    }
}
