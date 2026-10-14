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
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::timeout,
};

#[tokio::test]
async fn installed_binary_uses_python_from_prefix_without_a_source_tree() {
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
    let fixture_result = StdCommand::new(prepared_python::prepared_python(&source))
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
    for name in [
        "codex_python.py",
        "codex_layout.py",
        "codex_cache_paths.py",
        "codex_progress_layout.py",
    ] {
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
            "#!{}\nimport pathlib, runpy, sys\nscript = pathlib.Path(sys.argv[2])\npathlib.Path(__file__).with_name('python-bootstrap-arg').write_text(str(script))\nsys.path.insert(0, str(script.parent))\nsys.argv = [str(script)]\nrunpy.run_path(str(script), run_name='__main__')\n",
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

    let mut child = Command::new(binary)
        .args(["--port", "0"])
        .current_dir(prefix.path())
        .env_clear()
        .env("HOME", &home)
        .env(
            "XDG_CACHE_HOME",
            std::env::var_os("XDG_CACHE_HOME").expect("isolated prepared Python cache"),
        )
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
            let mut stderr = String::new();
            if let Some(mut stream) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                stream.read_to_string(&mut stderr).await.unwrap();
            }
            panic!("installed server exited with {status}: {stderr}");
        }
        Ok(Err(error)) => panic!("reading installed server startup output failed: {error}"),
        Err(_) => {
            child.kill().await.unwrap();
            let status = child.wait().await.unwrap();
            let mut stderr = String::new();
            if let Some(mut stream) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                stream.read_to_string(&mut stderr).await.unwrap();
            }
            panic!("installed server did not start ({status}): {stderr}");
        }
    }
    assert!(state.join("canvas.sqlite3").exists());
    assert!(Path::new(&home).is_dir());
    assert_eq!(
        fs::read_to_string(binary_dir.join("python-bootstrap-arg")).unwrap(),
        scripts.join("codex_python.py").to_string_lossy()
    );
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
    let source = source.canonicalize().unwrap();
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
        .env(
            "XDG_CACHE_HOME",
            std::env::var_os("XDG_CACHE_HOME").expect("isolated prepared Python cache"),
        )
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    source_child.kill().await.unwrap();
    source_child.wait().await.unwrap();
    assert_eq!(
        fs::read_to_string(binary_dir.join("python-bootstrap-arg")).unwrap(),
        source.join("codex_python.py").to_string_lossy()
    );
}
