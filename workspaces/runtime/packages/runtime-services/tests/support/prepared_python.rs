use std::{
    io::{Read, Seek, SeekFrom},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

pub(crate) const PYTHON_ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Parent directory for disposable test state; follows the platform TMPDIR contract.
#[allow(dead_code)]
pub(crate) fn disposable_state_parent() -> std::path::PathBuf {
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
    let parent = command
        .get_envs()
        .find_map(|(name, value)| {
            (name == "TMPDIR")
                .then_some(value)
                .flatten()
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(std::env::temp_dir);
    let fixture = label
        .parent()
        .and_then(std::path::Path::file_name)
        .unwrap_or_default()
        .to_string_lossy();
    let scratch = tempfile::Builder::new()
        .prefix(&format!("oracle-output-{fixture}-"))
        .tempdir_in(parent)
        .map_err(|error| format!("{}: oracle scratch: {error}", label.display()))?;
    command.env("TMPDIR", scratch.path());
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

/// Closed SQLite seed files retained in memory; a static cache must never own a TempDir.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct StateTemplate {
    files: [Vec<u8>; 2],
}

#[allow(dead_code)]
impl StateTemplate {
    const DATABASES: [&'static str; 2] = ["canvas.sqlite3", "analytics.sqlite3"];

    // Synchronous test seed preparation; this helper never runs in an HTTP handler.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn capture(root: &std::path::Path) -> Self {
        Self {
            files: Self::DATABASES
                .map(|name| std::fs::read(root.join(name)).expect("closed fixture database bytes")),
        }
    }

    // Synchronous test seed preparation; this helper never runs in an HTTP handler.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn write(&self, root: &std::path::Path) {
        for (name, bytes) in Self::DATABASES.into_iter().zip(&self.files) {
            std::fs::write(root.join(name), bytes).expect("clone cached fixture database");
        }
    }
}
