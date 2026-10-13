use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::router::HttpState;

use super::manifest::{OperationStatus, RouteGroup, get as register_get};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![register_get(
        "/api/desktop",
        OperationStatus::Implemented,
        get,
    )])
}

pub(crate) async fn get(State(state): State<HttpState>) -> Json<Value> {
    Json(status(&state))
}

fn status(state: &HttpState) -> Value {
    let native = state.native_path.as_ref().map(|path| {
        json!({
            "path": path, "sourcePath": path, "version": "unknown"
        })
    });
    let native_runtime = json!({
        "status": if native.is_some() { "ready" } else { "failed" },
        "checkedAt": null,
        "selected": native,
        "candidates": [], "accounts": {},
        "error": if state.native_path.is_none() { Some("No native provider configured") } else { None }
    });
    json!({
        "application": "codex-agents", "protocol": 1, "mobileProtocol": 1,
        "backendBuild": "rust-runtime-http-skeleton", "nativeRuntime": native_runtime,
        "providerVersions": {"checkedAt": null, "providers": [], "warnings": []},
        "browser": {"enabled": false, "reason": "Browser diagnostics are not implemented"},
        "liveUpdate": null, "restartEnvironment": {}, "publicOrigin": null,
        "pid": std::process::id(), "supervisorMode": false, "supervisorRequired": false,
        "supervisorError": null, "supervisor": null, "supervisorFallback": false,
        "supervisorNotice": null, "stateDir": state.state_dir, "linuxVm": null
    })
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "desktop",
        routes: Some(routes),
        dependencies: &["cross-server/remote-events", "federation-driver/rooms"],
    }
}
