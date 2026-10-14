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
        "providerVersions": state.provider_versions.as_ref().map_or_else(
            || json!({"checkedAt": null, "providers": [], "warnings": []}),
            |monitor| monitor.status(),
        ),
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

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, extract::ConnectInfo, http::Request};
    use serde_json::Value;
    use std::{path::Path, sync::Arc, time::Instant};
    use studio_runtime_services::provider_versions::{
        ActiveProviderConnections, Clock, ProviderPortError, ProviderProfile,
        ProviderVersionMonitor, ProviderVersionStore, SystemVersionProbe, UpstreamFetch,
        UpstreamReply, VersionFacts, VersionProbe,
    };
    use tower::ServiceExt;

    fn prepared_python(source: &Path) -> std::path::PathBuf {
        std::env::var_os("CODEX_AGENTS_PYTHON")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let output = std::process::Command::new("python3")
                    .arg(source.join("codex_python.py"))
                    .output()
                    .expect("prepared Python interpreter resolves");
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                String::from_utf8(output.stdout)
                    .expect("Python interpreter path is UTF-8")
                    .trim()
                    .into()
            })
    }

    struct LiveSessions {
        claude_binary: String,
    }
    impl ActiveProviderConnections for LiveSessions {
        fn snapshot(&self) -> Vec<ProviderProfile> {
            // Offline and registry-only accounts are deliberately absent, as in Python tick().
            vec![
                ProviderProfile {
                    account_key: "connected".into(),
                    provider: "codex".into(),
                    configured_binary: None,
                    server_identity: "server-7".into(),
                    connection_id: Some("connection-9".into()),
                },
                ProviderProfile {
                    account_key: "claude-connected".into(),
                    provider: "claude".into(),
                    configured_binary: Some(self.claude_binary.clone()),
                    server_identity: "server-8".into(),
                    connection_id: Some("connection-11".into()),
                },
            ]
        }
    }

    struct Probe {
        production: SystemVersionProbe,
    }
    impl VersionProbe for Probe {
        fn probe(
            &self,
            profile: &ProviderProfile,
            deadline: Instant,
        ) -> Result<VersionFacts, ProviderPortError> {
            if profile.provider == "claude" {
                self.production.probe(profile, deadline)
            } else {
                Ok(VersionFacts {
                    running_version: Some("0.153.3".into()),
                    installed_version: Some("0.153.3".into()),
                    available_version: Some("0.153.5".into()),
                    ..VersionFacts::default()
                })
            }
        }
    }

    struct Fetch;
    impl UpstreamFetch for Fetch {
        fn fetch(
            &self,
            url: &str,
            _: Option<&str>,
            _: Instant,
        ) -> Result<UpstreamReply, ProviderPortError> {
            if url.contains("anthropic") {
                return Err(ProviderPortError);
            }
            Ok(UpstreamReply {
                status: 200,
                etag: Some("fixture-etag".into()),
                latest: Some("0.154.0".into()),
            })
        }
    }

    struct FixtureClock;
    impl Clock for FixtureClock {
        fn now(&self) -> f64 {
            200_000.0
        }
    }

    struct FixtureStore;
    impl ProviderVersionStore for FixtureStore {
        fn upstream_enabled(&self) -> bool {
            true
        }
        fn persist_warnings(
            &self,
            _: &[String],
            _: &[Value],
        ) -> Result<(), studio_runtime_services::provider_versions::ProviderVersionsError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn desktop_http_keeps_empty_provider_versions_without_monitor_handle() {
        let state_dir = tempfile::tempdir().expect("isolated state directory");
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
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/desktop")
                    .header("host", "127.0.0.1:49171")
                    .header("x-canvas-token", "fixture-token")
                    .header(
                        crate::schema::API_SCHEMA_HASH_HEADER,
                        crate::schema::api_schema_hash(),
                    )
                    .extension(ConnectInfo(
                        "127.0.0.1:12345"
                            .parse::<std::net::SocketAddr>()
                            .expect("fixture address"),
                    ))
                    .body(axum::body::Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("valid response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("bounded response body");
        let value: Value = serde_json::from_slice(&body).expect("JSON response");
        assert_eq!(
            value["providerVersions"],
            serde_json::json!({"checkedAt":null,"providers":[],"warnings":[]})
        );
    }

    #[tokio::test]
    async fn desktop_http_projects_only_live_connection_like_python() {
        let temporary = tempfile::tempdir().expect("isolated cache directory");
        let claude_binary = temporary.path().join("claude-version-fixture");
        std::fs::write(&claude_binary, "#!/bin/sh\nprintf '2.1.279\\n'\n")
            .expect("Claude CLI test double written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&claude_binary, std::fs::Permissions::from_mode(0o700))
                .expect("Claude CLI test double made executable");
        }
        let monitor = ProviderVersionMonitor::start(
            Arc::new(FixtureStore),
            Some(Arc::new(LiveSessions {
                claude_binary: claude_binary.to_string_lossy().into_owned(),
            })),
            temporary.path().join("cache.json"),
            Arc::new(Probe {
                production: SystemVersionProbe,
            }),
            Arc::new(Fetch),
            Arc::new(FixtureClock),
        )
        .expect("monitor starts without real probes or network");
        assert!(monitor.wait_until_checked(std::time::Duration::from_secs(5)));

        let state_dir = tempfile::tempdir().expect("isolated state directory");
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            None,
            state_dir.path().to_string_lossy().into_owned(),
            49171,
            None,
            None,
            None,
        )
        .with_provider_versions(monitor.clone());
        let app = crate::router::router(state).expect("valid router");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/desktop")
                    .header("host", "127.0.0.1:49171")
                    .header("x-canvas-token", "fixture-token")
                    .header(
                        crate::schema::API_SCHEMA_HASH_HEADER,
                        crate::schema::api_schema_hash(),
                    )
                    .extension(ConnectInfo(
                        "127.0.0.1:12345"
                            .parse::<std::net::SocketAddr>()
                            .expect("fixture address"),
                    ))
                    .body(axum::body::Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("valid response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("bounded response body");
        let actual: Value = serde_json::from_slice(&body).expect("JSON response");

        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python(&source);
        let oracle = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../runtime-services/tests/provider_versions/projection_oracle.py");
        let expected = std::process::Command::new(python)
            .arg(oracle)
            .output()
            .expect("Python projection oracle launches");
        assert!(
            expected.status.success(),
            "{}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let expected: Value = serde_json::from_slice(&expected.stdout).expect("Python oracle JSON");
        assert_eq!(actual["providerVersions"], expected);
        assert_eq!(
            actual["providerVersions"]["providers"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            actual["providerVersions"]["providers"][0]["accountKey"],
            "connected"
        );
        assert_eq!(
            actual["providerVersions"]["providers"][1]["accountKey"],
            "claude-connected"
        );
        assert_eq!(
            actual["providerVersions"]["providers"][1]["runningVersion"],
            Value::Null,
            "Claude executable version must not be presented as the running session version"
        );
        assert_eq!(
            actual["providerVersions"]["providers"][1]["message"],
            "The exact version already running in Claude sessions is unknown.",
            "the Claude projection uses the production probe's running-session note"
        );
        monitor.shutdown();
    }
}
