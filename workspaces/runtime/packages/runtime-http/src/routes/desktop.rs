use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{self, Read};

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

fn backend_build_from(
    launcher_value: Option<&str>,
    executable_build: &str,
) -> Result<String, &'static str> {
    if launcher_value
        .filter(|value| is_sha256_hex(value))
        .is_some_and(|value| value != executable_build)
    {
        return Err("CODEX_BACKEND_BUILD does not match this executable");
    }
    Ok(executable_build.to_owned())
}

fn sha256_reader(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(data_encoding::HEXLOWER.encode(&hasher.finalize()))
}

fn executable_build() -> io::Result<String> {
    let executable_path = std::env::current_exe()?;
    sha256_reader(std::fs::File::open(executable_path)?)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

static BACKEND_BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Hash the actual executable before serving so `/api/desktop` never performs
/// blocking file I/O on the first request. Returns the build, which the server
/// also records as the owner of its state directory.
pub fn initialize_backend_identity() -> io::Result<String> {
    let actual_build = executable_build()?;
    let launcher_build = std::env::var("CODEX_BACKEND_BUILD").ok();
    let actual_build = backend_build_from(launcher_build.as_deref(), &actual_build)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    match BACKEND_BUILD.set(actual_build.clone()) {
        Ok(()) => Ok(actual_build),
        Err(_) if BACKEND_BUILD.get() == Some(&actual_build) => Ok(actual_build),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime backend identity was already initialized differently",
        )),
    }
}

fn backend_build() -> String {
    BACKEND_BUILD
        .get_or_init(|| executable_build().expect("runtime executable is readable"))
        .clone()
}

fn status(state: &HttpState) -> Value {
    let supervisor = state
        .diagnostics_supervisor
        .as_ref()
        .map(|port| port.snapshot());
    let native_runtime = state
        .native_runtime
        .as_ref()
        .and_then(|runtime| runtime.read_state().ok().flatten())
        .map(|snapshot| snapshot.record().clone())
        .unwrap_or_else(|| {
            json!({
                "status": "checking", "checkedAt": null, "selected": null,
                "candidates": [], "accounts": {}
            })
        });
    json!({
        "application": "codex-agents", "protocol": 1, "mobileProtocol": 1,
        "backendKind": "rust",
        "backendBuild": backend_build(), "nativeRuntime": native_runtime,
        "providerVersions": state.provider_versions.as_ref().map_or_else(
            || json!({"checkedAt": null, "providers": [], "warnings": []}),
            |monitor| monitor.status(),
        ),
        "browser": {"enabled": false, "reason": "Browser diagnostics are not implemented"},
        "liveUpdate": null, "restartEnvironment": {}, "publicOrigin": null,
        "pid": std::process::id(),
        "supervisorMode": supervisor.as_ref().is_some_and(|snapshot| snapshot.mode),
        "supervisorRequired": false,
        "supervisorError": null,
        "supervisor": supervisor.as_ref().and_then(|snapshot| snapshot.health.clone()),
        "supervisorFallback": supervisor.as_ref().is_some_and(|snapshot| snapshot.fallback),
        "supervisorNotice": supervisor.and_then(|snapshot| snapshot.notice),
        "stateDir": state.state_dir, "linuxVm": null,
        "diskSpace": studio_runtime_services::disk_space::status(
            std::path::Path::new(&state.state_dir),
        )
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
    use super::{backend_build_from, is_sha256_hex, sha256_reader};
    use axum::{body::to_bytes, extract::ConnectInfo, http::Request};
    use serde_json::Value;
    use std::{io::Cursor, path::Path, sync::Arc, time::Instant};
    use studio_runtime_services::provider_versions::{
        ActiveProviderConnections, Clock, ProviderPortError, ProviderProfile,
        ProviderVersionMonitor, ProviderVersionStore, SystemVersionProbe, UpstreamFetch,
        UpstreamReply, VersionFacts, VersionProbe,
    };
    use tower::ServiceExt;

    #[test]
    fn backend_build_uses_actual_executable_hash_and_rejects_launcher_mismatch() {
        let executable = b"runtime executable fixture";
        let actual = sha256_reader(Cursor::new(executable)).expect("fixture hashes");
        assert!(is_sha256_hex(&actual));
        assert_eq!(
            backend_build_from(Some(&actual), &actual),
            Ok(actual.clone())
        );
        assert_eq!(backend_build_from(None, &actual), Ok(actual.clone()));
        assert_eq!(
            backend_build_from(Some("invalid"), &actual),
            Ok(actual.clone())
        );
        assert_eq!(
            backend_build_from(Some(&"ab".repeat(32)), &actual),
            Err("CODEX_BACKEND_BUILD does not match this executable")
        );
    }

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
        changes: tokio::sync::watch::Sender<u64>,
    }
    impl ActiveProviderConnections for LiveSessions {
        fn snapshot(&self) -> Vec<ProviderProfile> {
            // Offline and registry-only accounts are deliberately absent, as in Python tick().
            vec![
                ProviderProfile {
                    account_key: "connected".into(),
                    provider: "codex".into(),
                    launched_binary: None,
                    configured_binary: None,
                    server_identity: "server-7".into(),
                    connection_id: Some("connection-9".into()),
                },
                ProviderProfile {
                    account_key: "claude-connected".into(),
                    provider: "claude".into(),
                    launched_binary: Some(self.claude_binary.clone()),
                    configured_binary: Some(self.claude_binary.clone()),
                    server_identity: "server-8".into(),
                    connection_id: Some("connection-11".into()),
                },
            ]
        }

        fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
            self.changes.subscribe()
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
        fn upstream_enabled(
            &self,
        ) -> Result<bool, studio_runtime_services::provider_versions::ProviderVersionsError>
        {
            Ok(true)
        }
        fn persist_warnings<'a>(
            &'a self,
            _: &'a [String],
            _: &'a [Value],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            studio_runtime_services::provider_versions::ProviderVersionsError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn desktop_http_keeps_empty_provider_versions_without_monitor_handle() {
        let state_dir = tempfile::tempdir().expect("isolated state directory");
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
        assert!(is_sha256_hex(
            value["backendBuild"].as_str().expect("build ID")
        ));
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
        let (changes, _) = tokio::sync::watch::channel(0);
        let monitor = ProviderVersionMonitor::start(
            Arc::new(FixtureStore),
            Some(Arc::new(LiveSessions {
                claude_binary: claude_binary.to_string_lossy().into_owned(),
                changes,
            })),
            temporary.path().join("cache.json"),
            Arc::new(Probe {
                production: SystemVersionProbe,
            }),
            Arc::new(Fetch),
            Arc::new(FixtureClock),
        )
        .await
        .expect("monitor starts without real probes or network");
        assert!(
            monitor
                .wait_until_checked(std::time::Duration::from_secs(5))
                .await
        );

        let state_dir = tempfile::tempdir().expect("isolated state directory");
        let state = crate::router::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
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
        monitor.shutdown().await;
    }
}
