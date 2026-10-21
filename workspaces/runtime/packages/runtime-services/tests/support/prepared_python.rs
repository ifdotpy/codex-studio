use std::{
    io::{Read, Seek, SeekFrom},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

pub(crate) const PYTHON_ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Parent directory for disposable SQLite test state and Python oracle scratch.
/// SQLite syncs every commit, so on a disk-backed TMPDIR these fixtures spend
/// most of their time in fsync and slow down sharply under shared I/O load.
/// Prefer a memory filesystem: `STUDIO_RUNTIME_CORPUS_TMP` when set, else the
/// Linux `/dev/shm` mount, else the process temporary directory.
#[allow(dead_code)]
#[allow(clippy::disallowed_methods)]
pub(crate) fn disposable_state_parent() -> std::path::PathBuf {
    if let Some(directory) = std::env::var_os("STUDIO_RUNTIME_CORPUS_TMP") {
        return directory.into();
    }
    let shared_memory = std::path::Path::new("/dev/shm");
    if cfg!(target_os = "linux") && shared_memory.is_dir() {
        return shared_memory.to_owned();
    }
    std::env::temp_dir()
}

/// A private disposable directory beneath [`disposable_state_parent`].
#[allow(dead_code)]
pub(crate) fn disposable_state_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("runtime-test-")
        .tempdir_in(disposable_state_parent())
        .expect("disposable test state directory")
}

#[allow(dead_code)]
pub(crate) struct IsolatedTestEnvironment {
    root: tempfile::TempDir,
}

#[allow(dead_code)]
impl IsolatedTestEnvironment {
    pub(crate) fn new() -> Self {
        Self {
            root: disposable_state_dir(),
        }
    }

    #[allow(clippy::disallowed_methods)]
    pub(crate) fn apply(&self, command: &mut Command) {
        let root = self.root.path();
        for (name, directory) in [
            ("HOME", "home"),
            ("CODEX_HOME", "codex-home"),
            ("CLAUDE_CONFIG_DIR", "claude-config"),
            ("TMPDIR", "tmp"),
        ] {
            let path = root.join(directory);
            std::fs::create_dir_all(&path).expect("create isolated test directory");
            command.env(name, path);
        }
    }
}

fn terminate_child(child: &mut Child, label: &std::path::Path) -> Result<(), String> {
    if let Err(error) = child.kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        return Err(format!(
            "{}: could not stop Python oracle: {error}",
            label.display()
        ));
    }
    child
        .wait()
        .map(|_| ())
        .map_err(|error| format!("{}: could not reap Python oracle: {error}", label.display()))
}

#[allow(clippy::disallowed_methods)]
pub(crate) fn output_with_timeout(
    command: &mut Command,
    label: &std::path::Path,
    timeout: Duration,
) -> Result<Output, String> {
    let path = label;
    let label = path.display();
    let mut stdout = tempfile::tempfile().map_err(|error| format!("{label}: {error}"))?;
    let mut stderr = tempfile::tempfile().map_err(|error| format!("{label}: {error}"))?;
    command
        .stdout(Stdio::from(
            stdout
                .try_clone()
                .map_err(|error| format!("{label}: {error}"))?,
        ))
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .map_err(|error| format!("{label}: {error}"))?,
        ));
    let mut child = command
        .spawn()
        .map_err(|error| format!("{label}: could not start Python oracle: {error}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let timeout_error = format!(
                    "{label}: Python oracle exceeded {} second timeout",
                    timeout.as_secs()
                );
                return match terminate_child(&mut child, path) {
                    Ok(()) => Err(timeout_error),
                    Err(cleanup_error) => Err(format!("{timeout_error}; {cleanup_error}")),
                };
            }
            Err(error) => {
                let wait_error = format!("{label}: could not wait for Python oracle: {error}");
                return match terminate_child(&mut child, path) {
                    Ok(()) => Err(wait_error),
                    Err(cleanup_error) => Err(format!("{wait_error}; {cleanup_error}")),
                };
            }
        }
    };
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout
        .seek(SeekFrom::Start(0))
        .and_then(|_| stdout.read_to_end(&mut stdout_bytes))
        .map_err(|error| format!("{label}: could not read Python oracle stdout: {error}"))?;
    stderr
        .seek(SeekFrom::Start(0))
        .and_then(|_| stderr.read_to_end(&mut stderr_bytes))
        .map_err(|error| format!("{label}: could not read Python oracle stderr: {error}"))?;
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

pub(crate) fn prepared_python(source: &std::path::Path) -> std::path::PathBuf {
    std::env::var_os("CODEX_AGENTS_PYTHON")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let resolver = source.join("codex_python.py");
            let mut command = Command::new("python3");
            command.arg(&resolver);
            let output = output_with_timeout(&mut command, &resolver, PYTHON_ORACLE_TIMEOUT)
                .unwrap_or_else(|error| panic!("resolve prepared Python interpreter: {error}"));
            assert!(
                output.status.success(),
                "prepared Python interpreter resolves: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::path::PathBuf::from(
                String::from_utf8(output.stdout)
                    .expect("Python interpreter path is UTF-8")
                    .trim(),
            )
        })
}
