//! Desktop backend ownership rules ported from desktop/backend.cjs.
use crate::backend::Backend;
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
) -> Result<Option<Value>, String> {
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
                match response.json::<Value>().await {
                    Ok(value) => {
                        let pid = value["pid"]
                            .as_i64()
                            .filter(|pid| *pid > 0 && *pid <= i32::MAX as i64);
                        if !status.is_success()
                            || value["application"] != "codex-agents"
                            || value["protocol"] != 1
                            || !value["backendBuild"].as_str().is_some_and(|build| {
                                build.len() == 64
                                    && build
                                        .bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                            })
                            || pid.is_none()
                            || value["stateDir"].as_str() != state.to_str()
                        {
                            return Err("The existing server is incompatible or uses another state directory.".into());
                        }
                        if unsafe { libc::kill(pid.unwrap() as i32, 0) } != 0 {
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
    "CODEX_AGENTS_PYTHON",
    "SHELL",
    "LANG",
    "LC_ALL",
    "CODEX_AGENTS_SUPERVISOR_MODE",
    "CODEX_AGENTS_SUPERVISOR_FALLBACK",
];

fn saved_environment(
    mut environment: Environment,
    saved: &Value,
    state: &Path,
) -> Result<Environment, String> {
    if saved.get("environment").is_none() && saved.get("unsetEnvironment").is_none() {
        return Ok(environment);
    }
    let invalid = || {
        "The saved restart environment is invalid or uses a different state directory.".to_owned()
    };
    let values = saved["environment"].as_object().ok_or_else(invalid)?;
    let unset = saved["unsetEnvironment"].as_array().ok_or_else(invalid)?;
    if saved["version"] != 1 || saved["stateDir"].as_str() != state.to_str() {
        return Err(invalid());
    }
    for (key, value) in values {
        if (key != "PATH" && !RESTART_KEYS.contains(&key.as_str())) || value.as_str().is_none() {
            return Err(invalid());
        }
    }
    for key in unset {
        let key = key.as_str().ok_or_else(invalid)?;
        if !RESTART_KEYS.contains(&key) || values.contains_key(key) {
            return Err(invalid());
        }
        environment.remove(std::ffi::OsStr::new(key));
    }
    for (key, value) in values {
        environment.insert(key.into(), value.as_str().unwrap().into());
    }
    Ok(environment)
}

fn supervisor_mode(environment: &Environment, saved: &Value) -> Result<bool, String> {
    if saved
        .get("supervisorEnabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("The saved supervisor setting is invalid.".into());
    }
    Ok(environment
        .get(std::ffi::OsStr::new("CODEX_AGENTS_SUPERVISOR_MODE"))
        .map(|value| value == "1")
        .unwrap_or(saved["supervisorEnabled"] == true))
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

async fn check_command(
    path: &Path,
    args: &[&str],
    environment: &Environment,
    duration: Duration,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new(path);
    command
        .args(args)
        .env_clear()
        .envs(environment)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = tokio::time::timeout(duration, command.status())
        .await
        .map_err(|_| "The backend prerequisite check timed out.".to_owned())?
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("A backend prerequisite check failed.".into())
    }
}

async fn api_python(resources: &Path, environment: &Environment) -> Result<PathBuf, String> {
    const CHECK: &[&str] = &[
        "-c",
        "import sys; assert sys.version_info >= (3,11); import fastapi, httpx, pydantic, uvicorn",
    ];
    if environment.contains_key(std::ffi::OsStr::new("CODEX_AGENTS_PYTHON")) {
        let path = executable("python3", "CODEX_AGENTS_PYTHON", environment)?;
        check_command(&path, CHECK, environment, Duration::from_secs(10)).await
            .map_err(|_| "CODEX_AGENTS_PYTHON must provide Python 3.11+, FastAPI, Pydantic, Uvicorn and HTTPX.".to_owned())?;
        return Ok(path);
    }
    let requirements = resources.join("requirements.txt");
    if let Ok(bytes) = fs::read(requirements) {
        use sha2::{Digest, Sha256};
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let cache = environment
            .get(std::ffi::OsStr::new("XDG_CACHE_HOME"))
            .map(PathBuf::from)
            .or_else(|| {
                environment.get(std::ffi::OsStr::new("HOME")).map(|home| {
                    PathBuf::from(home).join(if cfg!(target_os = "macos") {
                        "Library/Caches"
                    } else {
                        ".cache"
                    })
                })
            });
        if let Some(cache) = cache {
            let path = cache
                .join("codex-agents/python")
                .join(digest)
                .join("bin/python");
            if path.is_file()
                && check_command(&path, CHECK, environment, Duration::from_secs(10))
                    .await
                    .is_ok()
            {
                return Ok(path);
            }
        }
    }
    let path = executable("python3", "CODEX_AGENTS_PYTHON", environment)?;
    check_command(&path, CHECK, environment, Duration::from_secs(10))
        .await
        .map_err(|_| {
            "Prepare the managed Studio Python environment before starting the backend.".to_owned()
        })?;
    Ok(path)
}

pub async fn ensure(backend: &Backend, state: &Path, resources: &Path) -> Result<Value, String> {
    ensure_with_environment(backend, state, resources, env::vars_os().collect()).await
}

async fn ensure_with_environment(
    backend: &Backend,
    state: &Path,
    resources: &Path,
    environment: Environment,
) -> Result<Value, String> {
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let state = state.canonicalize().map_err(|e| e.to_string())?;
    let saved = match fs::read(state.join("background-recovery.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("Cannot read backend recovery settings: {e}"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Null,
        Err(error) => return Err(error.to_string()),
    };
    let supervisor = supervisor_mode(&environment, &saved)?;
    let attempt = Duration::from_secs(5);
    let total = Duration::from_secs(15);
    if let Some(value) = identity(backend, &state, attempt, total).await? {
        if supervisor && value["supervisorMode"] != true && value["supervisorFallback"] != true {
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
    let development = resources.join("workspaces/runtime/apps/server/src");
    let scripts = if development.join("codex-canvas").is_file() {
        development
    } else {
        resources.join("scripts")
    };
    let script = scripts.join("codex-canvas");
    if !script.is_file() {
        return Err("The packaged Python backend is missing.".into());
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
    let python = api_python(resources, &environment).await?;
    let codex = executable("codex", "CODEX_BIN", &environment)?;
    let node = executable("node", "CODEX_NODE", &environment)?;
    environment.insert(
        "CODEX_AGENTS_STATE_DIR".into(),
        state.clone().into_os_string(),
    );
    if supervisor {
        let supervisor_script = scripts.join("codex_process_supervisor.py");
        let supervisor_script = supervisor_script
            .to_str()
            .ok_or("Invalid supervisor path.")?;
        let state_text = state.to_str().ok_or("Invalid state path.")?;
        check_command(&python, &["-B", supervisor_script, "--check", "--state", state_text], &environment, Duration::from_secs(10)).await
            .map_err(|_| "Supervisor mode is enabled but no compatible supervisor is ready. The backend was not started.".to_owned())?;
    }
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
    let mut command = Command::new(python);
    command
        .arg("-B")
        .arg(script)
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
            fs::write(state.join("canvas.pid"), format!("{}\n", value["pid"]))
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
        let restored = saved_environment(environment.clone(), &saved, Path::new("/state")).unwrap();
        assert!(!restored.contains_key(std::ffi::OsStr::new("CODEX_HOME")));
        assert_eq!(
            restored.get(std::ffi::OsStr::new("HOME")).unwrap(),
            "/home/user"
        );
        assert_eq!(
            restored.get(std::ffi::OsStr::new("CODEX_BIN")).unwrap(),
            "/bin/codex"
        );
        assert!(saved_environment(environment.clone(), &saved, Path::new("/other")).is_err());
        for (key, value) in [
            ("environment", serde_json::json!({"UNAPPROVED":"value"})),
            ("unsetEnvironment", serde_json::json!(["PATH"])),
            ("unsetEnvironment", serde_json::json!(["CODEX_BIN"])),
        ] {
            let mut invalid = saved.clone();
            invalid[key] = value;
            assert!(saved_environment(environment.clone(), &invalid, Path::new("/state")).is_err());
        }
        assert!(
            supervisor_mode(
                &environment,
                &serde_json::json!({"supervisorEnabled":"yes"})
            )
            .is_err()
        );
        assert!(
            supervisor_mode(&environment, &serde_json::json!({"supervisorEnabled":true})).unwrap()
        );
    }

    #[tokio::test]
    async fn startup_uses_saved_environment_attaches_once_and_requires_supervisor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let scripts = root.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::create_dir_all(root.join("web/dist")).unwrap();
        fs::write(root.join("web/dist/index.html"), "fixture").unwrap();
        // The launcher fixture uses only Python's standard library. Stub imports
        // test prerequisite invocation without modifying any installed environment.
        for name in ["fastapi", "httpx", "pydantic", "uvicorn"] {
            fs::write(scripts.join(format!("{name}.py")), "").unwrap();
        }
        fs::write(scripts.join("codex-canvas"), r#"
import http.server, json, os, sys, threading
from pathlib import Path
state = Path(os.environ['CODEX_AGENTS_STATE_DIR'])
(state / 'launched.json').write_text(json.dumps({'home':os.environ.get('CODEX_HOME'), 'language':os.environ.get('LANG')}))
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        body = json.dumps({'application':'codex-agents','protocol':1,'pid':os.getpid(),'stateDir':str(state),'backendBuild':'a'*64}).encode()
        self.send_response(200)
        self.send_header('Content-Length',str(len(body)))
        self.end_headers()
        self.wfile.write(body)
server = http.server.HTTPServer(('127.0.0.1',int(sys.argv[-1])),Handler)
threading.Timer(15, lambda: os._exit(0)).start()
server.serve_forever()
"#).unwrap();
        let mut environment: Environment = env::vars_os().collect();
        let python_key = if environment.contains_key(std::ffi::OsStr::new("STUDIO_FIXTURE_PYTHON"))
        {
            "STUDIO_FIXTURE_PYTHON"
        } else {
            "CODEX_AGENTS_PYTHON"
        };
        let python = executable("python3", python_key, &environment).unwrap();
        environment.insert("CODEX_AGENTS_PYTHON".into(), python.into_os_string());
        environment.insert("CODEX_BIN".into(), "/usr/bin/true".into());
        environment.insert("CODEX_NODE".into(), "/usr/bin/true".into());
        environment.insert("PYTHONPATH".into(), scripts.into_os_string());
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
        let _process = FixtureProcess(first["pid"].as_i64().unwrap() as i32);
        let second = ensure_with_environment(&backend, &state, &root, environment.clone())
            .await
            .unwrap();
        assert_eq!(first["pid"], second["pid"]);
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
        assert!(
            ensure_with_environment(&backend, &state, &root, environment)
                .await
                .unwrap_err()
                .contains("no compatible supervisor")
        );
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
                reply(&mut stream, &serde_json::json!({"application":"codex-agents", "protocol":1, "pid":std::process::id(), "stateDir":"/expected", "backendBuild":build}).to_string()).await;
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
            reply(&mut stream,&serde_json::json!({"application":"codex-agents","protocol":1,"pid":std::process::id(),"stateDir":"/other", "backendBuild":"a".repeat(64)}).to_string()).await;
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
