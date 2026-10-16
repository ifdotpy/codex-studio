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
    ) -> Response
    where
        F: Future<Output = Response> + Send + 'static,
    {
        let request_id = request_id.filter(|value| crate::middleware::safe_request_id(value));
        let tracked = async move {
            match std::panic::AssertUnwindSafe(future).catch_unwind().await {
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
            }
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
    State(tasks): State<Arc<MutationTasks>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !is_api_mutation(request.method(), request.uri().path()) {
        return next.run(request).await;
    }

    let route = format!("{} {}", request.method(), request.uri().path());
    let request_id = request
        .headers()
        .get("x-studio-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| crate::middleware::safe_request_id(value))
        .map(str::to_owned);
    tasks
        .run_response(next.run(request), route, request_id)
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
            .layer(from_fn_with_state(Arc::clone(&tasks), own_mutations));

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
        let _ = request.await;
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
            .layer(from_fn_with_state(Arc::clone(&tasks), own_mutations));
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
        tasks.close_admission();
        let shutdown_tasks = Arc::clone(&tasks);
        let shutdown = tokio::spawn(async move { shutdown_tasks.shutdown().await });
        assert!(!shutdown.is_finished());

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
        shutdown.await.expect("shutdown task");
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
            .layer(from_fn_with_state(Arc::clone(&tasks), own_mutations));
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
        let _ = request.await;
        assert!(dropped.load(Ordering::SeqCst));
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn tracked_handler_panic_becomes_a_context_logged_internal_error() {
        let tasks = Arc::new(MutationTasks::new());
        let app = Router::new()
            .route(
                "/api/write",
                any(|| async {
                    Result::<&str, &str>::Err("fixture panic").expect("fixture panic")
                }),
            )
            .layer(from_fn_with_state(tasks, own_mutations));
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
