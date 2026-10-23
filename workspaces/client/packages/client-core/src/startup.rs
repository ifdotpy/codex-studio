//! Desktop backend ownership rules ported from desktop/backend.cjs.
use crate::backend::Backend;
use serde::Deserialize;
#[cfg(test)]
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fs::{self, OpenOptions},
    io,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::time::{Instant, sleep};

// Desktop HTTP identity mirrors backend.cjs; backend.test.mjs checks parity.
// Backend builds are SHA-256 encoded as lowercase hex.
const DESKTOP_PROTOCOL_VERSION: u8 = 1;
const BACKEND_BUILD_HEX_LENGTH: usize = 64;
// Version of persisted recovery environment records.
const RESTART_ENVIRONMENT_VERSION: u8 = 1;

#[derive(Debug, Deserialize, PartialEq, Eq)]
enum DesktopApplication {
    #[serde(rename = "codex-agents")]
    Studio,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
enum BackendKind {
    #[serde(rename = "rust")]
    Rust,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopIdentity {
    #[serde(rename = "application")]
    _application: DesktopApplication,
    protocol: u8,
    #[serde(rename = "backendKind")]
    _backend_kind: BackendKind,
    backend_build: String,
    pid: i32,
    state_dir: PathBuf,
    #[serde(default)]
    pub supervisor_mode: bool,
    #[serde(default)]
    pub supervisor_fallback: bool,
}

// Recovery also contains settings owned by the platform recovery helper.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedRecovery {
    version: Option<u8>,
    state_dir: Option<PathBuf>,
    #[serde(default, deserialize_with = "present")]
    environment: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "present")]
    unset_environment: Option<Vec<String>>,
    #[serde(default)]
    supervisor_enabled: bool,
}

// Missing launch fields describe preference-only records; explicit null is invalid.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn refused(error: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(value) = source {
        if value
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::ConnectionRefused)
        {
            return true;
        }
        source = value.source();
    }
    false
}

pub async fn identity(
    backend: &Backend,
    state: &Path,
    attempt: Duration,
    total: Duration,
) -> Result<Option<DesktopIdentity>, String> {
    let deadline = Instant::now() + total;
    let mut uncertain = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("The local backend did not confirm its identity in time. No replacement backend was started.".into());
        }
        let response = backend.desktop_probe(attempt.min(remaining)).await;
        let response: Result<(), reqwest::Error> = match response {
            Ok(response) => {
                let status = response.status();
                match response.json::<DesktopIdentity>().await {
                    Ok(value) => {
                        if !status.is_success()
                            || value.protocol != DESKTOP_PROTOCOL_VERSION
                            || value.backend_build.len() != BACKEND_BUILD_HEX_LENGTH
                            || !value
                                .backend_build
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                            || value.pid <= 0
                            || value.state_dir != state
                        {
                            return Err("The existing server is incompatible or uses another state directory.".into());
                        }
                        // SAFETY: identity parsing proves pid is a positive process identifier; signal 0 only probes existence.
                        if unsafe { libc::kill(value.pid, 0) } != 0 {
                            return Err("The backend process is no longer available.".into());
                        }
                        return Ok(Some(value));
                    }
                    Err(error) if error.is_timeout() => Err(error),
                    Err(_) => {
                        return Err(
                            "The backend port is occupied by an incompatible server.".into()
                        );
                    }
                }
            }
            Err(error) => Err(error),
        };
        if let Err(error) = response {
            if refused(&error) && !uncertain {
                return Ok(None);
            }
            if !error.is_timeout() && !refused(&error) {
                return Err(format!("Cannot verify the local backend: {error}"));
            }
            // A later refusal cannot undo evidence of an uncertain live server.
            uncertain = true;
        }
        sleep(Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
}

type Environment = BTreeMap<OsString, OsString>;
const RESTART_KEYS: &[&str] = &[
    "CODEX_HOME",
    "CODEX_CANVAS_CWD",
    "CODEX_CANVAS_CONCURRENCY",
    "CODEX_BIN",
    "SHELL",
    "LANG",
    "LC_ALL",
    "CODEX_AGENTS_SUPERVISOR_MODE",
    "CODEX_AGENTS_SUPERVISOR_FALLBACK",
];

fn saved_environment(
    mut environment: Environment,
    saved: &SavedRecovery,
    state: &Path,
) -> Result<Environment, String> {
    if saved.environment.is_none() && saved.unset_environment.is_none() {
        return Ok(environment);
    }
    let invalid = || {
        "The saved restart environment is invalid or uses a different state directory.".to_owned()
    };
    let values = saved.environment.as_ref().ok_or_else(invalid)?;
    let unset = saved.unset_environment.as_ref().ok_or_else(invalid)?;
    if saved.version != Some(RESTART_ENVIRONMENT_VERSION)
        || saved.state_dir.as_deref() != Some(state)
    {
        return Err(invalid());
    }
    for key in values.keys() {
        if key != "PATH" && !RESTART_KEYS.contains(&key.as_str()) {
            return Err(invalid());
        }
    }
    for key in unset {
        if !RESTART_KEYS.contains(&key.as_str()) || values.contains_key(key) {
            return Err(invalid());
        }
        environment.remove(std::ffi::OsStr::new(key));
    }
    for (key, value) in values {
        environment.insert(key.into(), value.into());
    }

    Ok(environment)
}

fn supervisor_mode(environment: &Environment, saved: &SavedRecovery) -> bool {
    environment
        .get(std::ffi::OsStr::new("CODEX_AGENTS_SUPERVISOR_MODE"))
        .map(|value| value == "1")
        .unwrap_or(saved.supervisor_enabled)
}

fn executable(name: &str, key: &str, environment: &Environment) -> Result<PathBuf, String> {
    let candidates = if let Some(path) = environment.get(std::ffi::OsStr::new(key)) {
        vec![PathBuf::from(path)]
    } else {
        env::split_paths(
            environment
                .get(std::ffi::OsStr::new("PATH"))
                .map(OsString::as_os_str)
                .unwrap_or_default(),
        )
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from))
        .map(|path| path.join(name))
        .collect()
    };
    candidates
        .into_iter()
        .find(|path| {
            path.is_absolute()
                && fs::metadata(path)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| format!("Install {name}, or set {key} to its absolute executable path."))
}

fn runtime_server(resources: &Path, environment: &Environment) -> Result<PathBuf, String> {
    if environment.contains_key(std::ffi::OsStr::new("CODEX_RUNTIME_SERVER")) {
        return executable("studio-runtime-server", "CODEX_RUNTIME_SERVER", environment);
    }
    let name = if cfg!(windows) {
        "studio-runtime-server.exe"
    } else {
        "studio-runtime-server"
    };
    let candidates = [
        resources.join("bin").join(name),
        resources.join("target/release").join(name),
        resources.join("target/debug").join(name),
    ];
    candidates.into_iter().find(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| "The Rust runtime server is missing. Build or package studio-runtime-server, or set CODEX_RUNTIME_SERVER to its absolute executable path.".into())
}

fn backend_build(binary: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::BufRead;
    let file = fs::File::open(binary).map_err(|error| error.to_string())?;
    let mut reader = io::BufReader::new(file);
    let mut hash = Sha256::new();
    loop {
        let buffer = reader.fill_buf().map_err(|error| error.to_string())?;
        if buffer.is_empty() {
            break;
        }
        hash.update(buffer);
        let length = buffer.len();
        reader.consume(length);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub async fn ensure(
    backend: &Backend,
    state: &Path,
    resources: &Path,
) -> Result<DesktopIdentity, String> {
    ensure_with_environment(backend, state, resources, env::vars_os().collect()).await
}

async fn ensure_with_environment(
    backend: &Backend,
    state: &Path,
    resources: &Path,
    environment: Environment,
) -> Result<DesktopIdentity, String> {
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let state = state.canonicalize().map_err(|e| e.to_string())?;
    let saved = match fs::read(state.join("background-recovery.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("Cannot read backend recovery settings: {e}"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => SavedRecovery::default(),
        Err(error) => return Err(error.to_string()),
    };
    let supervisor = supervisor_mode(&environment, &saved);
    let attempt = Duration::from_secs(5);
    let total = Duration::from_secs(15);
    if let Some(value) = identity(backend, &state, attempt, total).await? {
        if supervisor && !value.supervisor_mode && !value.supervisor_fallback {
            return Err("Supervisor mode is enabled, but the running backend does not use it. Use the coordinated restart preflight before attaching.".into());
        }
        backend.authorize();
        return Ok(value);
    }
    let mut environment = saved_environment(environment, &saved, &state)?;
    let port = backend.port();
    if port < 1024 {
        return Err("Backend port must be between 1024 and 65535.".into());
    }
    let renderer = if resources
        .join("workspaces/client/apps/web/dist/index.html")
        .is_file()
    {
        resources.join("workspaces/client/apps/web/dist/index.html")
    } else {
        resources.join("web/dist/index.html")
    };
    if !renderer.is_file() {
        return Err("The packaged renderer is missing.".into());
    }
    let runtime = runtime_server(resources, &environment)?;
    let build = backend_build(&runtime)?;
    let codex = executable("codex", "CODEX_BIN", &environment)?;
    let node = executable("node", "CODEX_NODE", &environment)?;
    environment.insert(
        "CODEX_AGENTS_STATE_DIR".into(),
        state.clone().into_os_string(),
    );
    environment.insert(
        "CODEX_AGENTS_SUPERVISOR_MODE".into(),
        if supervisor { "1" } else { "0" }.into(),
    );
    environment.insert("CODEX_BIN".into(), codex.clone().into_os_string());
    environment.insert("CODEX_NODE".into(), node.into_os_string());
    let mut paths = vec![codex.parent().unwrap().to_path_buf()];
    paths.extend(env::split_paths(
        environment
            .get(std::ffi::OsStr::new("PATH"))
            .map(OsString::as_os_str)
            .unwrap_or(std::ffi::OsStr::new("/usr/bin:/bin")),
    ));
    environment.insert(
        "PATH".into(),
        env::join_paths(paths).map_err(|e| e.to_string())?,
    );
    environment.insert("CODEX_BACKEND_BUILD".into(), build.clone().into());
    let home = environment
        .get(std::ffi::OsStr::new("HOME"))
        .ok_or("Missing home directory.")?;
    let log = state.join("canvas.log");
    let output = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .map_err(|e| e.to_string())?;
    let mut command = Command::new(runtime);
    command
        .arg("--port")
        .arg(port.to_string())
        .current_dir(home)
        .env_clear()
        .envs(&environment)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(output));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    for _ in 0..100 {
        sleep(Duration::from_millis(100)).await;
        if let Some(value) = identity(backend, &state, attempt, total).await? {
            if value.backend_build != build {
                return Err(
                    "The started Rust backend identity does not match the selected executable."
                        .into(),
                );
            }
            fs::write(state.join("canvas.pid"), format!("{}\n", value.pid))
                .map_err(|e| e.to_string())?;
            backend.authorize();
            return Ok(value);
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "Backend exited ({status}). Read {}. Another runtime may own this state directory.",
                log.display()
            ));
        }
    }
    Err(format!(
        "Backend startup has not completed. Process {}. Read {}.",
        child.id(),
        log.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_launch_environment_preserves_identity_and_explicit_unsets() {
        let environment = Environment::from([
            ("CODEX_HOME".into(), "inherited".into()),
            ("HOME".into(), "/home/user".into()),
        ]);
        let saved = serde_json::json!({"version":1, "stateDir":"/state", "environment":{"CODEX_BIN":"/bin/codex", "PATH":"/bin"}, "unsetEnvironment":["CODEX_HOME"]});
        let restored = saved_environment(
            environment.clone(),
            &serde_json::from_value(saved.clone()).unwrap(),
            Path::new("/state"),
        )
        .unwrap();
        assert!(!restored.contains_key(std::ffi::OsStr::new("CODEX_HOME")));
        assert_eq!(
            restored.get(std::ffi::OsStr::new("HOME")).unwrap(),
            "/home/user"
        );
        assert_eq!(
            restored.get(std::ffi::OsStr::new("CODEX_BIN")).unwrap(),
            "/bin/codex"
        );
        assert!(
            saved_environment(
                environment.clone(),
                &serde_json::from_value(saved.clone()).unwrap(),
                Path::new("/other")
            )
            .is_err()
        );
        for (key, value) in [
            ("environment", serde_json::json!({"UNAPPROVED":"value"})),
            ("unsetEnvironment", serde_json::json!(["PATH"])),
            ("unsetEnvironment", serde_json::json!(["CODEX_BIN"])),
        ] {
            let mut invalid = saved.clone();
            invalid[key] = value;
            assert!(
                saved_environment(
                    environment.clone(),
                    &serde_json::from_value(invalid).unwrap(),
                    Path::new("/state")
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<SavedRecovery>(serde_json::json!({"supervisorEnabled":"yes"}))
                .is_err()
        );
        for invalid in [
            serde_json::json!({"environment":null}),
            serde_json::json!({"unsetEnvironment":null}),
            serde_json::json!({"supervisorEnabled":null}),
        ] {
            assert!(serde_json::from_value::<SavedRecovery>(invalid).is_err());
        }
        assert!(supervisor_mode(
            &environment,
            &serde_json::from_value(serde_json::json!({"supervisorEnabled":true})).unwrap()
        ));
    }

    #[tokio::test]
    async fn startup_uses_saved_environment_attaches_once_and_requires_supervisor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("web/dist")).unwrap();
        fs::write(root.join("web/dist/index.html"), "fixture").unwrap();
        let mut environment: Environment = env::vars_os().collect();
        let node = executable("node", "CODEX_NODE", &environment).unwrap();
        let runtime = root.join("fixture-runtime");
        fs::write(&runtime, format!(r#"#!{}
const http = require("node:http"), fs = require("node:fs"), path = require("node:path");
const state = process.env.CODEX_AGENTS_STATE_DIR;
fs.writeFileSync(path.join(state, "launched.json"), JSON.stringify({{home: process.env.CODEX_HOME ?? null, language: process.env.LANG}}));
http.createServer((request, response) => {{
  const body = JSON.stringify({{application:"codex-agents", protocol:1, backendKind:"rust", pid:process.pid, stateDir:state, backendBuild:process.env.CODEX_BACKEND_BUILD}});
  response.setHeader("Content-Length", Buffer.byteLength(body));
  response.end(body);
}}).listen(Number(process.argv.at(-1)), "127.0.0.1");
const fixtureLifetimeMs = 15000;
setTimeout(() => process.exit(0), fixtureLifetimeMs);
"#, node.display())).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        environment.insert("CODEX_RUNTIME_SERVER".into(), runtime.into_os_string());
        environment.insert("CODEX_AGENTS_PYTHON".into(), "/nonexistent".into());
        environment.insert("CODEX_BIN".into(), node.clone().into_os_string());
        environment.insert("CODEX_NODE".into(), node.into_os_string());
        environment.insert("CODEX_HOME".into(), "must-not-inherit".into());
        environment.insert("CODEX_AGENTS_SUPERVISOR_MODE".into(), "0".into());
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        fs::write(state.join("background-recovery.json"), serde_json::json!({"version":1,"stateDir":state,"environment":{"LANG":"C"},"unsetEnvironment":["CODEX_HOME"]}).to_string()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let backend = Backend::desktop_client(&format!("http://{address}")).unwrap();
        let first = ensure_with_environment(&backend, &state, &root, environment.clone())
            .await
            .unwrap();
        struct FixtureProcess(i32);
        impl Drop for FixtureProcess {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.0, libc::SIGTERM);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }
        let _process = FixtureProcess(first.pid);
        let second = ensure_with_environment(&backend, &state, &root, environment.clone())
            .await
            .unwrap();
        assert_eq!(first.pid, second.pid);
        let launched: Value =
            serde_json::from_slice(&fs::read(state.join("launched.json")).unwrap()).unwrap();
        assert!(launched["home"].is_null());
        assert_eq!(launched["language"], "C");
        environment.insert("CODEX_AGENTS_SUPERVISOR_MODE".into(), "1".into());
        assert!(
            ensure_with_environment(&backend, &state, &root, environment.clone())
                .await
                .unwrap_err()
                .contains("coordinated restart")
        );
        drop(_process);
    }

    #[tokio::test]
    async fn existing_backend_must_supply_a_valid_build_identity() {
        use crate::backend::tests::{read_http, reply};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend =
            Backend::desktop_client(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            for build in [
                Value::Null,
                Value::String("invalid".into()),
                Value::String("a".repeat(64)),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http(&mut stream).await;
                reply(&mut stream, &serde_json::json!({"application":"codex-agents", "protocol":1, "backendKind":"rust", "pid":std::process::id(), "stateDir":"/expected", "backendBuild":build, "diskSpace":{"level":null,"message":null}}).to_string()).await;
            }
        });
        for valid in [false, false, true] {
            let result = identity(
                &backend,
                Path::new("/expected"),
                Duration::from_secs(1),
                Duration::from_secs(2),
            )
            .await;
            assert_eq!(result.is_ok(), valid);
            if valid {
                assert!(result.unwrap().is_some());
            }
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_state_is_not_authorized_and_timeout_then_refusal_is_not_absence() {
        use crate::backend::tests::{read_http, reply};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend =
            Backend::desktop_client(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http(&mut stream).await;
            reply(&mut stream,&serde_json::json!({"application":"codex-agents","protocol":1,"backendKind":"rust","pid":std::process::id(),"stateDir":"/other", "backendBuild":"a".repeat(64)}).to_string()).await;
        });
        assert!(
            identity(
                &backend,
                Path::new("/expected"),
                Duration::from_millis(50),
                Duration::from_millis(300)
            )
            .await
            .unwrap_err()
            .contains("another state")
        );
        server.await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend =
            Backend::desktop_client(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            sleep(Duration::from_millis(80)).await;
        });
        assert!(
            identity(
                &backend,
                Path::new("/expected"),
                Duration::from_millis(30),
                Duration::from_millis(300)
            )
            .await
            .unwrap_err()
            .contains("No replacement")
        );
        server.await.unwrap();
    }
}
