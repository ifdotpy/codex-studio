//! Linux VM resource settings routes. All host control operations cross the fakeable RPC port.

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use studio_runtime_services::{linux_vm_guest::UnixVmControlRpc, linux_vm_settings};

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, generated, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/linux-vm/settings",
            OperationStatus::Implemented,
            get_settings,
        ),
        post(
            "/api/linux-vm/settings",
            OperationStatus::Implemented,
            set_settings,
        ),
    ])
}

async fn get_settings(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let vm_dir = state.vm_state_dir;
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/linux_vm_settings",
        move || settings_response(&vm_dir, None),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map(Json)
}

async fn set_settings(
    State(state): State<HttpState>,
    ApiJson(settings): ApiJson<generated::LinuxVMSettings>,
) -> Result<Json<Value>, ApiError> {
    let vm_dir = state.vm_state_dir;
    let requested: linux_vm_settings::VmSettings =
        serde_json::from_value(serde_json::to_value(settings).map_err(|_| ApiError::internal())?)
            .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Invalid VM settings"))?;
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/linux_vm_settings",
        move || settings_response(&vm_dir, Some(requested)),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map(Json)
}

fn settings_response(
    vm_dir: &std::path::Path,
    write: Option<linux_vm_settings::VmSettings>,
) -> Result<Value, ApiError> {
    let control = UnixVmControlRpc::new(vm_dir);
    if let Some(settings) = write {
        linux_vm_settings::set_settings(vm_dir, settings, &control).map_err(settings_error)?;
    }
    let settings = linux_vm_settings::get_settings(vm_dir).map_err(settings_error)?;
    let status = linux_vm_settings::status(vm_dir, &control).map_err(settings_error)?;
    Ok(json!({"settings": settings, "state": status["state"],
        "allocatedDiskBytes": status.get("allocatedDiskBytes").cloned().unwrap_or(Value::Null)}))
}

fn settings_error(error: linux_vm_settings::SettingsError) -> ApiError {
    use linux_vm_settings::SettingsError;
    match error {
        SettingsError::Invalid(_) | SettingsError::Running | SettingsError::DiskShrink => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        SettingsError::Control(_) => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
        SettingsError::Io(_) | SettingsError::Json(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "linux_vm_settings", routes: Some(routes), dependencies: &[],
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio_util::task::AbortOnDropHandle;
    use tower::ServiceExt;

    fn settings_request(
        method: Method,
        body: Option<Value>,
        token: &str,
        schema_hash: &str,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri("/api/linux-vm/settings")
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", token)
            .header(crate::schema::API_SCHEMA_HASH_HEADER, schema_hash)
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
            .expect("settings request builds")
    }

    #[tokio::test]
    async fn settings_http_authorizes_validates_writes_and_reads_back_without_vm_execution() {
        let state_dir = tempfile::tempdir().expect("temporary state directory");
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        )
        .with_vm_state_dir(state_dir.path().join("private-vm"));
        let app = crate::router::router(state).expect("valid router");
        let body = json!({"cpus":1,"memoryBytes":4294967296u64,"systemDiskBytes":17179869184u64,"dataDiskBytes":137438953472u64});
        let schema_hash = crate::schema::api_schema_hash();

        let response = app
            .clone()
            .oneshot(settings_request(
                Method::POST,
                Some(body.clone()),
                "wrong-token",
                schema_hash,
            ))
            .await
            .expect("unauthorized response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(settings_request(
                Method::POST,
                Some(body.clone()),
                "fixture-token",
                "stale-schema",
            ))
            .await
            .expect("schema mismatch response");
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);

        let vm_dir = state_dir.path().join("private-vm");
        std::fs::create_dir_all(&vm_dir).expect("fake VM state directory created");
        let listener =
            UnixListener::bind(vm_dir.join("control.sock")).expect("fake control socket binds");
        let server = AbortOnDropHandle::new(tokio::spawn(async move {
            // Set, response status, and subsequent GET status. This fake supports
            // status queries only, so the fixture cannot execute a VM action.
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.expect("status request");
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("fake control reads request");
                let request = serde_json::from_str::<Value>(&line).expect("request parses");
                assert_eq!(request["method"], "host.status");
                let mut stream = reader.into_inner();
                let reply = json!({"id":request["id"],"result":{"state":"stopped"}});
                stream
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .expect("fake control sends status");
            }
        }));
        let response = app
            .clone()
            .oneshot(settings_request(
                Method::POST,
                Some(body),
                "fixture-token",
                schema_hash,
            ))
            .await
            .expect("settings POST response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("POST response body reads");
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let value = serde_json::from_slice::<Value>(&bytes).expect("fixture operation succeeds");
        assert_eq!(value["settings"]["cpus"], 1);
        assert_eq!(value["state"], "stopped");
        assert_eq!(value["allocatedDiskBytes"], 0);

        let response = app
            .oneshot(settings_request(
                Method::GET,
                None,
                "fixture-token",
                schema_hash,
            ))
            .await
            .expect("settings GET response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("GET response body reads");
        let readback = serde_json::from_slice::<Value>(&bytes).expect("GET response parses");
        assert_eq!(readback["settings"], value["settings"]);
        assert_eq!(readback["state"], "stopped");
        assert_eq!(
            serde_json::from_slice::<Value>(
                &std::fs::read(vm_dir.join("settings.json"))
                    .expect("settings use the injected directory")
            )
            .expect("persisted settings"),
            value["settings"]
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("socket responder stops")
            .expect("socket responder succeeds");
    }

    #[tokio::test]
    async fn settings_get_http_reports_defaults_and_post_refusal_writes_nothing() {
        let state_dir = tempfile::tempdir().expect("temporary state directory");
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        )
        .with_vm_state_dir(state_dir.path().join("private-vm"));
        let app = crate::router::router(state).expect("valid router");
        let request = |method: axum::http::Method, body: Option<Value>| {
            let mut builder = axum::http::Request::builder()
                .method(method)
                .uri("/api/linux-vm/settings")
                .header("host", "127.0.0.1:49171")
                .header("x-canvas-token", "fixture-token")
                .header(
                    crate::schema::API_SCHEMA_HASH_HEADER,
                    crate::schema::api_schema_hash(),
                )
                .extension(axum::extract::ConnectInfo(
                    "127.0.0.1:12345"
                        .parse::<std::net::SocketAddr>()
                        .expect("fixture operation succeeds"),
                ));
            if body.is_some() {
                builder = builder.header("content-type", "application/json");
            }
            builder
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .expect("fixture operation succeeds")
        };
        let response = app
            .clone()
            .oneshot(request(axum::http::Method::GET, None))
            .await
            .expect("fixture operation succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("fixture operation succeeds"),
        )
        .expect("fixture operation succeeds");
        assert_eq!(value["state"], "stopped");
        assert_eq!(value["settings"]["memoryBytes"], 4 * 1024 * 1024 * 1024u64);
        let vm_dir = state_dir.path().join("private-vm");
        std::fs::create_dir_all(&vm_dir).expect("private settings directory");
        let settings_file = vm_dir.join("settings.json");
        let before = serde_json::to_vec(&value["settings"]).expect("existing settings");
        std::fs::write(&settings_file, &before).expect("seed existing settings");
        let response = app
            .oneshot(request(
                axum::http::Method::POST,
                Some(json!({"cpus":0,"memoryBytes":4294967296u64,"systemDiskBytes":17179869184u64,"dataDiskBytes":137438953472u64})),
            ))
            .await
            .expect("fixture operation succeeds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            std::fs::read(&settings_file).expect("settings retained"),
            before
        );
        let entries = std::fs::read_dir(&vm_dir)
            .expect("settings directory retained")
            .map(|entry| entry.expect("readable entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [std::ffi::OsString::from("settings.json")]);
    }
}
