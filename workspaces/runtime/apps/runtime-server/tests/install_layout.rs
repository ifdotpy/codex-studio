#![cfg(unix)]
#![allow(clippy::unwrap_used)]

#[path = "../../../packages/runtime-services/tests/support/prepared_python.rs"]
mod prepared_python;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    time::Duration,
};
use studio_runtime_store::path_identity::CanonicalPath;
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::Command,
    time::timeout,
};

const STARTUP_TRACE_ENV: &str = "CODEX_STUDIO_STARTUP_TRACE";
const MAX_STARTUP_STDERR_BYTES: u64 = 4096;
const STARTUP_STDERR_CAPTURE_TIMEOUT: Duration = Duration::from_secs(1);

async fn startup_stderr(child: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncReadExt;

    let Some(stream) = child.stderr.take() else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let read = timeout(
        STARTUP_STDERR_CAPTURE_TIMEOUT,
        stream
            .take(MAX_STARTUP_STDERR_BYTES)
            .read_to_end(&mut bytes),
    )
    .await;
    let mut output = String::from_utf8_lossy(&bytes).into_owned();
    if !matches!(read, Ok(Ok(_))) {
        output.push_str("\n[stderr capture timed out or failed]");
        return output;
    }
    if bytes.len() as u64 == MAX_STARTUP_STDERR_BYTES {
        output.push_str("\n[stderr capture limit reached]");
    }
    output
}

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn installed_binary_uses_python_from_prefix_without_a_source_tree() {
    let startup_trace_owner = format!("const STARTUP_TRACE_ENV: &str = \"{STARTUP_TRACE_ENV}\";");
    assert!(include_str!("../src/main.rs").contains(&startup_trace_owner));
    let prefix = tempdir().unwrap();
    let workspace = prefix.path().join("workspace");
    let binary_dir = workspace.join("bin");
    let scripts = workspace.join("scripts");
    let bridge = scripts.join("claude_bridge");
    let home = prefix.path().join("fresh-home");
    let state = prefix.path().join("state");
    fs::create_dir_all(&binary_dir).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    fs::create_dir_all(&bridge).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&state).unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_fixture.py");
    let prepared = prepared_python::prepared_python(&source);
    let fixture_result = StdCommand::new(&prepared)
        .arg("-B")
        .arg(fixture)
        .arg(&state)
        .arg("dispatch")
        .output()
        .unwrap();
    assert!(
        fixture_result.status.success(),
        "{}",
        String::from_utf8_lossy(&fixture_result.stderr)
    );
    let system_python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 is available in the build environment");
    for name in ["codex_python.py", "codex_layout.py", "codex_cache_paths.py"] {
        fs::copy(source.join(name), scripts.join(name)).unwrap();
    }
    let skills = workspace.join(".agents/skills");
    for name in [
        "codex-orchestrator/SKILL.md",
        "codex-orchestrator/references/native-role.md",
        "codex-orchestrator/references/legacy-move-role.md",
        "codex-subagent/SKILL.md",
        "codex-subagent/references/native-role.md",
    ] {
        let target = skills.join(name);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../.agents/skills")
                .join(name),
            target,
        )
        .unwrap();
    }
    fs::copy(
        source.join("../../../../../requirements.txt"),
        workspace.join("requirements.txt"),
    )
    .unwrap();
    fs::write(
        binary_dir.join("python3"),
        format!(
            "#!{}\nimport pathlib, sys\npathlib.Path(__file__).with_name('python-invoked').write_text('unexpected')\nsys.exit(97)\n",
            system_python.display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        binary_dir.join("python3"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(bridge.join("bridge.mjs"), "export {};\n").unwrap();
    let binary = binary_dir.join("studio-runtime-server");
    fs::copy(env!("CARGO_BIN_EXE_studio-runtime-server"), &binary).unwrap();
    assert!(
        !workspace
            .join("workspaces/runtime/apps/server/src")
            .exists()
    );

    // Seed an isolated managed-cache layout with the already prepared venv.
    // The server must discover it without invoking the poisoned PATH Python.
    let cache = prefix.path().join("prepared-cache");
    let managed = StdCommand::new(&prepared)
        .args(["-c", "from pathlib import Path; from codex_python import managed_python; import sys; print(managed_python(Path(sys.argv[1])))"])
        .arg(&scripts)
        .env("PYTHONPATH", &scripts)
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .unwrap();
    assert!(
        managed.status.success(),
        "{}",
        String::from_utf8_lossy(&managed.stderr)
    );
    let managed = PathBuf::from(String::from_utf8(managed.stdout).unwrap().trim());
    let managed_venv = managed.parent().unwrap().parent().unwrap();
    fs::create_dir_all(managed_venv.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(prepared.parent().unwrap().parent().unwrap(), managed_venv).unwrap();

    let mut child = Command::new(binary)
        .args(["--port", "0"])
        .current_dir(prefix.path())
        .env_clear()
        .env("HOME", &home)
        .env(STARTUP_TRACE_ENV, "1")
        .env("XDG_CACHE_HOME", &cache)
        .env(
            "PATH",
            std::env::join_paths([
                binary_dir.as_path(),
                system_python.parent().unwrap(),
                Path::new("/usr/bin"),
                Path::new("/bin"),
            ])
            .unwrap(),
        )
        .env("XDG_STATE_HOME", prefix.path().join("xdg-state"))
        .env("TMPDIR", prefix.path())
        .env("CODEX_AGENTS_STATE_DIR", &state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut output = BufReader::new(stdout);
    let mut line = String::new();
    let read = timeout(Duration::from_secs(30), output.read_line(&mut line)).await;
    match read {
        Ok(Ok(count)) if count > 0 => {
            assert!(
                line.starts_with("Codex Canvas: http://127.0.0.1:"),
                "{line}"
            );
            assert_eq!(child.try_wait().unwrap(), None);
            child.kill().await.unwrap();
            child.wait().await.unwrap();
        }
        Ok(Ok(_)) => {
            let status = child.wait().await.unwrap();
            let stderr = startup_stderr(&mut child).await;
            panic!("installed server exited with {status}: {stderr}");
        }
        Ok(Err(error)) => panic!("reading installed server startup output failed: {error}"),
        Err(_) => {
            child.kill().await.unwrap();
            let status = child.wait().await.unwrap();
            let stderr = startup_stderr(&mut child).await;
            panic!("installed server did not start ({status}): {stderr}");
        }
    }
    assert!(state.join("canvas.sqlite3").exists());
    assert!(Path::new(&home).is_dir());
    assert!(!binary_dir.join("python-invoked").exists());
    let python_layout = StdCommand::new(&system_python)
        .args([
            "-c",
            "from codex_layout import REPOSITORY_ROOT, CLAUDE_BRIDGE_ROOT; print(f'{REPOSITORY_ROOT}\\n{CLAUDE_BRIDGE_ROOT}')",
        ])
        .env("PYTHONPATH", &scripts)
        .output()
        .unwrap();
    assert!(
        python_layout.status.success(),
        "{}",
        String::from_utf8_lossy(&python_layout.stderr)
    );
    assert_eq!(
        String::from_utf8(python_layout.stdout).unwrap().trim(),
        format!("{}\n{}", workspace.display(), bridge.display())
    );

    // Exercise the source-checkout fallback too: Python's layout module points
    // at the providers bridge in a checkout, while installed packages use the
    // scripts/claude_bridge copy above.
    let source = CanonicalPath::existing(&source).unwrap();
    let source_workspace = source
        .ancestors()
        .nth(5)
        .expect("source checkout root")
        .to_path_buf();
    let source_bridge = source_workspace.join("workspaces/providers/apps/claude-bridge");
    let source_layout = StdCommand::new(&system_python)
        .args([
            "-c",
            "from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, CLAUDE_BRIDGE_ROOT; print(f'{REPOSITORY_ROOT}\\n{SERVER_SOURCE_ROOT}\\n{CLAUDE_BRIDGE_ROOT}')",
        ])
        .env("PYTHONPATH", &source)
        .output()
        .unwrap();
    assert!(
        source_layout.status.success(),
        "{}",
        String::from_utf8_lossy(&source_layout.stderr)
    );
    assert_eq!(
        String::from_utf8(source_layout.stdout).unwrap().trim(),
        format!(
            "{}\n{}\n{}",
            source_workspace.display(),
            source.display(),
            source_bridge.display()
        )
    );

    let source_binary = PathBuf::from(env!("CARGO_BIN_EXE_studio-runtime-server"));
    let mut source_child = Command::new(source_binary)
        .args(["--port", "0"])
        .current_dir(prefix.path())
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", &cache)
        .env(
            "PATH",
            std::env::join_paths([
                binary_dir.as_path(),
                system_python.parent().unwrap(),
                Path::new("/usr/bin"),
                Path::new("/bin"),
            ])
            .unwrap(),
        )
        .env("XDG_STATE_HOME", prefix.path().join("source-xdg-state"))
        .env("TMPDIR", prefix.path())
        .env("CODEX_AGENTS_STATE_DIR", &state)
        .env("CODEX_AGENTS_SERVER_EXIT_ON_STDIN_CLOSE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let source_stdout = source_child.stdout.take().unwrap();
    let mut source_output = BufReader::new(source_stdout);
    let mut source_line = String::new();
    let source_read = timeout(
        Duration::from_secs(30),
        source_output.read_line(&mut source_line),
    )
    .await;
    assert!(
        matches!(source_read, Ok(Ok(count)) if count > 0)
            && source_line.starts_with("Codex Canvas: http://127.0.0.1:"),
        "source-layout server failed to start: {source_line}"
    );
    assert_eq!(source_child.try_wait().unwrap(), None);
    let port = source_line
        .trim()
        .strip_prefix("Codex Canvas: http://127.0.0.1:")
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /api/sync/stream?protocol=3&resources=%5B%7B%22kind%22%3A%22accounts%22%7D%5D HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    let stream_ready = timeout(Duration::from_secs(5), async {
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "sync stream ended before its initial event");
            response.extend_from_slice(&buffer[..count]);
            let text = String::from_utf8_lossy(&response);
            if text.contains("\r\n\r\n") && text.contains("event: resources") {
                assert!(
                    text.starts_with("HTTP/1.1 200"),
                    "unexpected SSE response: {text}"
                );
                break;
            }
        }
    })
    .await;
    assert!(
        stream_ready.is_ok(),
        "sync stream did not send its initial event"
    );

    drop(source_child.stdin.take());
    let status = match timeout(Duration::from_secs(10), source_child.wait()).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            source_child.kill().await.unwrap();
            let status = source_child.wait().await.unwrap();
            panic!("server did not finish graceful shutdown with an open sync stream: {status}");
        }
    };
    assert!(
        status.success(),
        "server exited unsuccessfully after shutdown: {status}"
    );
    drop(stream);
    assert!(!binary_dir.join("python-invoked").exists());
}
