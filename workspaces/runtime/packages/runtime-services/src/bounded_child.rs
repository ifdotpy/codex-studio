//! Shared bounded child execution for workspace Git operations.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};
use studio_runtime_process::child_priority::ChildPriority;

// Each test thread can expire one child deadline after its readiness handshake.
// This seam is absent from production builds and leaves termination unchanged.
#[cfg(test)]
thread_local! {
    pub(crate) static DEADLINE_OVERRIDE: std::cell::RefCell<Option<Box<dyn FnOnce() -> Instant>>> =
        const { std::cell::RefCell::new(None) };
}

const MAX_ERROR_CHARS: usize = 1200;

#[derive(Debug)]
pub(crate) enum ChildError {
    Failed(String),
    TimedOut,
    Other,
}

#[derive(Debug)]
pub(crate) struct PipeOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) stdout: PipeOutput,
}

pub(crate) struct Request<'a> {
    pub(crate) program: &'a OsStr,
    pub(crate) cwd: &'a OsStr,
    pub(crate) args: &'a [&'a str],
    pub(crate) environment: &'a [(&'a str, &'a str)],
    pub(crate) input: Option<&'a [u8]>,
    pub(crate) stdout_limit: usize,
    pub(crate) stderr_limit: usize,
    pub(crate) timeout: Duration,
}

/// Run a child with bounded output retention and one deadline for execution and pipe draining.
pub(crate) fn run(request: Request<'_>) -> Result<Output, ChildError> {
    run_with_environment(request, false)
}

/// Inherit helper inputs but remove every ambient Git override before applying fixed settings.
pub(crate) fn run_without_git_environment(request: Request<'_>) -> Result<Output, ChildError> {
    run_with_environment(request, true)
}

fn run_with_environment(request: Request<'_>, remove_git: bool) -> Result<Output, ChildError> {
    let Request {
        program,
        cwd,
        args,
        environment,
        input,
        stdout_limit,
        stderr_limit,
        timeout,
    } = request;
    let deadline = Instant::now() + timeout;
    let mut command = Command::new(program);
    if remove_git {
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(key);
            }
        }
    }
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .envs(environment.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    ChildPriority::from_environment().apply_before_spawn(&mut command);
    let mut child = command.spawn().map_err(|_| ChildError::Other)?;
    let stdout = child.stdout().take().ok_or(ChildError::Other)?;
    let stderr = child.stderr().take().ok_or(ChildError::Other)?;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    let stdout_reader =
        thread::spawn(move || drop(stdout_sender.send(read_bounded(stdout, stdout_limit))));
    let stderr_reader =
        thread::spawn(move || drop(stderr_sender.send(read_bounded(stderr, stderr_limit))));

    if let Some(input) = input
        && let Some(mut stdin) = child.stdin().take()
        && stdin.write_all(input).is_err()
    {
        terminate(&mut *child);
        detach(stdout_reader, stderr_reader);
        return Err(ChildError::Other);
    }

    #[cfg(test)]
    let deadline =
        DEADLINE_OVERRIDE.with_borrow_mut(|hook| hook.take().map_or(deadline, |ready| ready()));

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            ),
            Ok(None) => {
                terminate(&mut *child);
                detach(stdout_reader, stderr_reader);
                return Err(ChildError::TimedOut);
            }
            Err(_) => {
                terminate(&mut *child);
                detach(stdout_reader, stderr_reader);
                return Err(ChildError::Other);
            }
        }
    };

    // Reap ordinary descendants as soon as the command exits. A helper that escaped into a new
    // session may still own a pipe; recv_timeout below keeps that case within the same deadline.
    terminate(&mut *child);
    let stdout = match receive_until(&stdout_receiver, deadline) {
        Ok(output) => output.map_err(|_| ChildError::Other)?,
        Err(error) => {
            detach(stdout_reader, stderr_reader);
            return Err(error);
        }
    };
    let stderr = match receive_until(&stderr_receiver, deadline) {
        Ok(output) => output.map_err(|_| ChildError::Other)?,
        Err(error) => {
            detach(stdout_reader, stderr_reader);
            return Err(error);
        }
    };
    // Results were sent only after each reader reached EOF. Joining now cannot block on a pipe.
    let stdout_joined = stdout_reader.join().is_ok();
    let stderr_joined = stderr_reader.join().is_ok();
    if !stdout_joined || !stderr_joined {
        return Err(ChildError::Other);
    }
    if Instant::now() >= deadline {
        return Err(ChildError::TimedOut);
    }
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr.bytes)
            .trim()
            .chars()
            .take(MAX_ERROR_CHARS)
            .collect::<String>();
        return Err(ChildError::Failed(if message.is_empty() {
            "Git operation failed".to_owned()
        } else {
            message
        }));
    }
    Ok(Output { stdout })
}

fn receive_until<T>(receiver: &mpsc::Receiver<T>, deadline: Instant) -> Result<T, ChildError> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => ChildError::TimedOut,
            mpsc::RecvTimeoutError::Disconnected => ChildError::Other,
        })
}

fn detach(stdout_reader: thread::JoinHandle<()>, stderr_reader: thread::JoinHandle<()>) {
    drop(stdout_reader);
    drop(stderr_reader);
}

fn terminate(child: &mut dyn ChildWrapper) {
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let taskkill = std::path::Path::new(&system_root)
                .join("System32")
                .join("taskkill.exe");
            drop(
                Command::new(taskkill)
                    .args(["/PID", &pid, "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            );
        }
    }
    drop(child.start_kill());
    drop(child.try_wait());
}

fn read_bounded(mut pipe: impl Read, limit: usize) -> std::io::Result<PipeOutput> {
    let mut bytes = Vec::with_capacity(limit.min(16_384));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let retained = count.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    Ok(PipeOutput { bytes, truncated })
}

#[cfg(test)]
mod tests {
    use super::{ChildError, Request, read_bounded, run};
    use std::ffi::OsStr;
    use std::io::Cursor;
    use std::time::{Duration, Instant};

    #[test]
    fn output_reader_drains_excess_and_retains_only_its_limit() {
        let output = read_bounded(Cursor::new(vec![b'x'; 32_000]), 1024).expect("pipe bytes read");
        assert_eq!(output.bytes, vec![b'x'; 1024]);
        assert!(output.truncated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn agent_command_reports_configured_child_priority_after_stdin_handshake() {
        use std::os::unix::fs::PermissionsExt;
        use studio_runtime_process::child_priority::ChildPriority;

        let root = tempfile::tempdir().expect("isolated executable fixture");
        let program = root.path().join("priority-probe");
        std::fs::write(
            &program,
            "#!/bin/sh\nread -r ready\nps -o ni= -p $$\nionice -p $$\n",
        )
        .expect("probe script");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        let output = run(Request {
            program: program.as_os_str(),
            cwd: OsStr::new("/"),
            args: &[],
            environment: &[],
            input: Some(b"ready\n"),
            stdout_limit: 256,
            stderr_limit: 256,
            timeout: Duration::from_secs(30),
        })
        .expect("bounded caller completes");
        let text = String::from_utf8(output.stdout.bytes).expect("probe output");
        let lines = text.lines().map(str::trim).collect::<Vec<_>>();
        match ChildPriority::from_environment() {
            ChildPriority::Background => assert_eq!(lines, ["5", "best-effort: prio 7"]),
            ChildPriority::Normal => assert_eq!(
                lines[0],
                rustix::process::getpriority_process(None)
                    .expect("parent nice")
                    .to_string()
            ),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deadline_includes_pipes_held_by_a_helper_in_a_new_session() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temporary fixture");
        let marker = root.path().join("helper-pid");
        let ready = root.path().join("helper-ready");
        let parent_exited = root.path().join("parent-exited");
        let ready_fifo = root.path().join("helper-handshake");
        let fifo_status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(&ready_fifo)
            .status()
            .expect("create helper handshake FIFO");
        assert!(fifo_status.success(), "helper handshake FIFO created");
        let program = root.path().join("child");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nsetsid /bin/sh -c 'echo $$ > \"$1\"; echo ready > \"$3\"; printf ready > \"$2\"; exec sleep 60' _ '{}' '{}' '{}' &\nIFS= read -r _ < '{}'\necho ready > '{}'\nexit 0\n",
                marker.display(), ready_fifo.display(), ready.display(), ready_fifo.display(), parent_exited.display()
            ),
        )
        .expect("child script written");
        let mut permissions = std::fs::metadata(&program)
            .expect("child metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&program, permissions).expect("child executable");

        let started = Instant::now();
        let result = run(Request {
            program: program.as_os_str(),
            cwd: OsStr::new("/"),
            args: &[],
            environment: &[],
            input: None,
            stdout_limit: 16,
            stderr_limit: 16,
            timeout: Duration::from_secs(2),
        });
        if let Ok(pid) = std::fs::read_to_string(&marker) {
            let group = format!("-{}", pid.trim());
            drop(
                std::process::Command::new("/bin/kill")
                    .args(["-KILL", "--", &group])
                    .status(),
            );
        }
        assert!(
            ready.exists(),
            "helper completed its inherited-pipe handshake"
        );
        assert!(
            parent_exited.exists(),
            "direct child exited before pipe timeout"
        );
        assert!(matches!(result, Err(ChildError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
