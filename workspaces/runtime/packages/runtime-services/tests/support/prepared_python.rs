use std::{
    io::{Read, Seek, SeekFrom},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const PYTHON_ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn output_with_timeout(
    command: &mut Command,
    label: &std::path::Path,
    timeout: Duration,
) -> Result<Output, String> {
    let label = label.display();
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
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{label}: Python oracle exceeded {} second timeout",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{label}: could not wait for Python oracle: {error}"
                ));
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
