//! HTTP adaptation for project secrets and the platform-bound vault actions.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post};
use crate::error::ApiError;
use crate::router::HttpState;
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::project_secrets::{self, GuestRpc, ProjectSecretsError};

const GUEST_RPC: &[Dependency] = &[Dependency::new("linux-vm/guest-rpc")];
const VAULT_DESK: &[Dependency] = &[Dependency::new("macos-platform/layr-desk")];
const VAULT_UNLOCK: &[Dependency] = &[
    Dependency::new("linux-vm/guest-rpc"),
    Dependency::new("macos-platform/layr-desk"),
];
const VAULT_GUEST_DESK: &[Dependency] = &[
    Dependency::new("linux-vm/guest-rpc"),
    Dependency::new("macos-platform/layr-desk"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/project-secrets/list",
            OperationStatus::partial(GUEST_RPC),
            list_secrets,
        ),
        post(
            "/api/project-secrets/remove",
            OperationStatus::partial(GUEST_RPC),
            remove_secret,
        ),
        post(
            "/api/project-secrets/set",
            OperationStatus::partial(GUEST_RPC),
            set_secret,
        ),
        post(
            "/api/vault/aws-login",
            OperationStatus::partial(VAULT_DESK),
            aws_login,
        ),
        post(
            "/api/vault/pull",
            OperationStatus::partial(VAULT_GUEST_DESK),
            pull,
        ),
        post(
            "/api/vault/setup",
            OperationStatus::partial(VAULT_GUEST_DESK),
            setup,
        ),
        post(
            "/api/vault/setup/facts",
            OperationStatus::partial(VAULT_GUEST_DESK),
            setup_facts,
        ),
        post(
            "/api/vault/unlock",
            OperationStatus::partial(VAULT_UNLOCK),
            unlock,
        ),
    ])
}

fn guest(state: &HttpState) -> Result<std::sync::Arc<dyn GuestRpc>, ApiError> {
    state.project_secrets_guest.clone().ok_or_else(|| {
        dependency_unavailable("POST", "/api/project-secrets/list", GUEST_RPC)
            .unwrap_or_else(|_| ApiError::internal())
    })
}

fn request_string<'a>(body: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::validation(vec![Value::String("body".to_owned())]))
}

fn guest_error(error: ProjectSecretsError) -> ApiError {
    let status = match error.code() {
        Some("invalid_params" | "not_found" | "id_conflict") => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    let mut detail = error.detail().to_owned();
    if error.uncertain() {
        detail.push_str(" The change may have applied; list the names to check.");
    }
    ApiError::new(status, detail)
}

async fn list_secrets(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?;
    project_secrets::list_names(guest(&state)?.as_ref(), project_id)
        .map(Json)
        .map_err(guest_error)
}

async fn set_secret(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?;
    let name = request_string(&body, "name")?;
    let value = request_string(&body, "value")?;
    let request_id = request_string(&body, "requestId")?;
    project_secrets::set_secret(guest(&state)?.as_ref(), project_id, name, value, request_id)
        .map(Json)
        .map_err(guest_error)
}

async fn remove_secret(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?;
    let name = request_string(&body, "name")?;
    let request_id = request_string(&body, "requestId")?;
    project_secrets::remove_secret(guest(&state)?.as_ref(), project_id, name, request_id)
        .map(Json)
        .map_err(guest_error)
}

async fn aws_login(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    unavailable("POST", "/api/vault/aws-login", VAULT_DESK)
}

async fn pull(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    unavailable("POST", "/api/vault/pull", VAULT_DESK)
}

async fn setup(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    unavailable("POST", "/api/vault/setup", VAULT_GUEST_DESK)
}

async fn setup_facts(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    unavailable("POST", "/api/vault/setup/facts", VAULT_DESK)
}

async fn unlock(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    unavailable("POST", "/api/vault/unlock", VAULT_UNLOCK)
}

fn unavailable(
    method: &str,
    path: &str,
    dependencies: &[Dependency],
) -> Result<Json<Value>, ApiError> {
    Err(dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal()))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "host_secrets",
        routes: Some(routes),
        dependencies: &[
            "linux-vm/guest-rpc",
            "macos-platform/layr-desk",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::{GuestRpc, routes};
    use crate::router::HttpState;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tower::ServiceExt;

    #[derive(Default)]
    struct FakeGuest {
        calls: Mutex<Vec<(String, Value, Option<String>)>>,
    }

    impl GuestRpc for FakeGuest {
        fn call(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            timeout: Duration,
        ) -> Result<Value, studio_runtime_services::project_secrets::GuestCallError> {
            assert_eq!(timeout, Duration::from_secs(30));
            self.calls.lock().unwrap().push((
                method.to_owned(),
                params.clone(),
                request_id.map(str::to_owned),
            ));
            Ok(json!({"names":["TOKEN"]}))
        }
    }

    fn app(state_dir: &std::path::Path, guest: Option<Arc<dyn GuestRpc>>) -> axum::Router {
        let authority = crate::schema::operations().expect("OpenAPI parses");
        let (router, _) = crate::routes::compose::compose(vec![routes()], authority)
            .expect("host secrets routes compose");
        let state = HttpState::new(
            "test-token".to_owned(),
            "test-workspace".to_owned(),
            None,
            state_dir.to_string_lossy().into_owned(),
            1,
            None,
            None,
            None,
        );
        let state = match guest {
            Some(guest) => state.with_project_secrets_guest_rpc(guest),
            None => state,
        };
        router.with_state(state)
    }

    #[tokio::test]
    async fn project_secret_set_calls_guest_and_never_returns_the_value() {
        let state = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeGuest::default());
        let app = app(state.path(), Some(fake.clone()));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/project-secrets/set")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"projectId":"p1","name":"TOKEN","value":"generated-test-value","requestId":"req-1"}).to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response, json!({"projectId":"p1","names":["TOKEN"]}));
        assert!(!response.to_string().contains("generated-test-value"));
        assert_eq!(
            fake.calls.lock().unwrap()[0].2.as_deref(),
            Some("secrets-set:req-1")
        );
    }

    #[tokio::test]
    async fn project_secret_route_refuses_without_guest_and_writes_nothing() {
        let state = tempfile::tempdir().unwrap();
        let app = app(state.path(), None);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/project-secrets/set")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"projectId":"p1","name":"TOKEN","value":"generated-test-value","requestId":"req-1"}).to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("generated-test-value"));
    }
}
