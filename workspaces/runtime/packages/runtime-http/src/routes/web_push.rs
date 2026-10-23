//! Web Push of sign-in alerts to the owner's devices (docs/web-push.md). A device of the
//! owner reads the server's public key, subscribes, and can stop the pushes of any device. The
//! subscription endpoint and keys stay on the server; the routes return only IDs and names.

use axum::{Json, extract::State, http::StatusCode};
use studio_runtime_services::web_push;

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, generated, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/web-push", OperationStatus::Implemented, push_state),
        post(
            "/api/web-push/subscriptions",
            OperationStatus::Implemented,
            subscribe,
        ),
        post("/api/web-push/remove", OperationStatus::Implemented, remove),
    ])
}

fn push_error(error: web_push::WebPushError) -> ApiError {
    match error {
        web_push::WebPushError::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        web_push::WebPushError::Crypto | web_push::WebPushError::Io(_) => ApiError::internal(),
    }
}

fn device(row: &web_push::Subscription) -> generated::WebPushSubscription {
    generated::WebPushSubscription {
        id: row.id.clone(),
        device: row.device.clone(),
        created: row.created,
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, web_push::WebPushError> + Send + 'static,
) -> Result<T, ApiError> {
    studio_runtime_process::blocking_pool::spawn_blocking("runtime-http/routes/web_push", work)
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(push_error)
}

async fn push_state(
    State(state): State<HttpState>,
) -> Result<Json<generated::WebPushState>, ApiError> {
    let root = std::path::PathBuf::from(&state.state_dir);
    blocking(move || {
        let key = web_push::ServerKey::load_or_create(&root)?;
        let rows = web_push::Subscriptions::new(&root).list()?;
        Ok(generated::WebPushState {
            public_key: key.public_key(),
            ready: web_push::subject(&root).is_some(),
            subscriptions: rows.iter().map(device).collect(),
        })
    })
    .await
    .map(Json)
}

async fn subscribe(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::WebPushSubscribeRequest>,
) -> Result<Json<generated::WebPushSubscription>, ApiError> {
    let root = std::path::PathBuf::from(&state.state_dir);
    blocking(move || {
        let row = web_push::Subscriptions::new(&root).add(
            web_push::NewSubscription {
                endpoint: body.endpoint,
                p256dh: body.p256dh,
                auth: body.auth,
                device: body.device,
            },
            false,
            web_push::now_seconds(),
        )?;
        Ok(device(&row))
    })
    .await
    .map(Json)
}

async fn remove(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::WebPushRemoveRequest>,
) -> Result<Json<generated::WebPushRemoveResponse>, ApiError> {
    let root = std::path::PathBuf::from(&state.state_dir);
    blocking(move || {
        let removed = web_push::Subscriptions::new(&root).remove(&body.id)?;
        Ok(generated::WebPushRemoveResponse { removed })
    })
    .await
    .map(Json)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "web_push", routes: Some(routes), dependencies: &[],
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// The P-256 base point: a valid uncompressed point for a fixture subscription.
    const POINT: &str =
        "BGsX0fLhLEJH-Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU";

    fn request(path: &str, method: Method, body: Option<Value>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", "fixture-token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(axum::extract::ConnectInfo(
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("fixture peer address parses"),
            ));
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
            .expect("request builds")
    }

    async fn call(
        app: &axum::Router,
        path: &str,
        method: Method,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(request(path, method, body))
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_device_subscribes_is_listed_without_its_keys_and_can_be_removed() {
        let state_dir = tempfile::tempdir().expect("temporary state directory");
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        );
        let app = crate::router::router(state).expect("valid router");
        let (status, first) = call(&app, "/api/web-push", Method::GET, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["subscriptions"], json!([]));
        assert_eq!(first["ready"], false, "no https remote access origin");
        assert!(
            first["publicKey"]
                .as_str()
                .is_some_and(|key| key.len() == 87)
        );
        let subscription = json!({
            "endpoint": "https://web.push.apple.com/fixture", "p256dh": POINT,
            "auth": "AAECAwQFBgcICQoLDA0ODw", "device": "iPhone"
        });
        let (status, row) = call(
            &app,
            "/api/web-push/subscriptions",
            Method::POST,
            Some(subscription.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{row}");
        assert_eq!(row["device"], "iPhone");
        let (_, listed) = call(&app, "/api/web-push", Method::GET, None).await;
        assert_eq!(listed["publicKey"], first["publicKey"]);
        let listed = listed["subscriptions"].to_string();
        assert!(listed.contains("iPhone"));
        assert!(
            !listed.contains("web.push.apple.com") && !listed.contains(POINT),
            "the endpoint and keys stay on the server"
        );
        let mut bad = subscription;
        bad["endpoint"] = json!("http://10.0.0.5/push");
        let (status, _) = call(&app, "/api/web-push/subscriptions", Method::POST, Some(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (_, removed) = call(
            &app,
            "/api/web-push/remove",
            Method::POST,
            Some(json!({"id": row["id"]})),
        )
        .await;
        assert_eq!(removed, json!({"removed": true}));
        let (_, listed) = call(&app, "/api/web-push", Method::GET, None).await;
        assert_eq!(listed["subscriptions"], json!([]));
    }
}
