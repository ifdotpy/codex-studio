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
    let vm_dir = vm_dir(&state);
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
    let vm_dir = vm_dir(&state);
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

fn vm_dir(state: &HttpState) -> std::path::PathBuf {
    std::env::var_os("CODEX_LINUX_VM_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&state.state_dir).join("linux-vm"))
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
mod tests {
    use super::*;
    use axum::body::Body;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use tower::ServiceExt;

    #[tokio::test]
    async fn settings_post_runs_through_http_with_a_fake_control_socket() {
        let state_dir = tempfile::tempdir().expect("temporary state directory");
        let vm_dir = state_dir.path().join("linux-vm");
        std::fs::create_dir_all(&vm_dir).expect("fixture operation succeeds");
        let listener =
            UnixListener::bind(vm_dir.join("control.sock")).expect("fixture operation succeeds");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().expect("fixture operation succeeds");
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .expect("fixture operation succeeds");
                let id = serde_json::from_str::<Value>(&line).expect("fixture operation succeeds")
                    ["id"]
                    .clone();
                let mut stream = reader.into_inner();
                writeln!(stream, "{}", json!({"id":id,"result":{"state":"stopped"}}))
                    .expect("fixture operation succeeds");
            }
        });
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            None,
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        );
        let app = crate::router::router(state).expect("valid router");
        let body = json!({"cpus":1,"memoryBytes":4294967296u64,"systemDiskBytes":17179869184u64,"dataDiskBytes":137438953472u64});
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/linux-vm/settings")
                    .header("host", "127.0.0.1:49171")
                    .header("x-canvas-token", "fixture-token")
                    .header(
                        crate::schema::API_SCHEMA_HASH_HEADER,
                        crate::schema::api_schema_hash(),
                    )
                    .header("content-type", "application/json")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:12345"
                            .parse::<std::net::SocketAddr>()
                            .expect("fixture operation succeeds"),
                    ))
                    .body(Body::from(body.to_string()))
                    .expect("fixture operation succeeds"),
            )
            .await
            .expect("fixture operation succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("fixture operation succeeds");
        let value = serde_json::from_slice::<Value>(&bytes).expect("fixture operation succeeds");
        assert_eq!(value["settings"]["cpus"], 1);
        assert_eq!(value["state"], "stopped");
        assert_eq!(value["allocatedDiskBytes"], 0);
        server.join().expect("fixture operation succeeds");
    }

    #[tokio::test]
    async fn settings_get_http_reports_defaults_and_post_refusal_writes_nothing() {
        let state_dir = tempfile::tempdir().expect("temporary state directory");
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            None,
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        );
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
        let before = std::fs::read_dir(state_dir.path().join("linux-vm"))
            .ok()
            .map(|entries| entries.count())
            .unwrap_or(0);
        let response = app
            .oneshot(request(
                axum::http::Method::POST,
                Some(json!({"cpus":0,"memoryBytes":4294967296u64,"systemDiskBytes":17179869184u64,"dataDiskBytes":137438953472u64})),
            ))
            .await
            .expect("fixture operation succeeds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let after = std::fs::read_dir(state_dir.path().join("linux-vm"))
            .ok()
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(before, after);
    }
}
