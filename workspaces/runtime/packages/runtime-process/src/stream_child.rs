//! Owned, bounded pipe child. Callers supply an explicitly selected environment.
//! Drop requests group termination; the retained actor kills and reaps its own child.
//! Output ordering is per pipe. Completion is published only after both pipes drain.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, ProcessGroup};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};

pub const OUTPUT_CHUNK_BYTES: usize = 8192;
pub const OUTPUT_SLOTS: usize = 8;
pub const INPUT_SLOTS: usize = 4;
pub const INPUT_BYTES: usize = 128 * 1024;
pub const INPUT_DEADLINE: Duration = Duration::from_secs(10);
pub const REAP_DEADLINE: Duration = Duration::from_secs(5);

pub struct ChildSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    /// Complete environment. No ambient variable is inherited.
    pub environment: BTreeMap<OsString, OsString>,
    pub timeout: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitReason {
    Completed,
    Cancelled,
    TimedOut,
    IoFailure,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildEvent {
    Output {
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    Exited {
        code: Option<i32>,
        reason: ExitReason,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ChildError {
    #[error("child input exceeds bound")]
    InputTooLarge,
    #[error("child is closed")]
    Closed,
    #[error("child input outcome is unknown; do not repeat it")]
    Unknown,
    #[error("child cleanup failed")]
    Cleanup,
}
struct WriteRequest {
    bytes: Vec<u8>,
    close: bool,
    reply: oneshot::Sender<Result<(), ChildError>>,
}

pub struct StreamChild {
    input: mpsc::Sender<WriteRequest>,
    output: mpsc::Receiver<ChildEvent>,
    cancel: watch::Sender<bool>,
    completion: watch::Receiver<Option<Result<ChildEvent, ChildError>>>,
    exit_delivered: bool,
}

impl StreamChild {
    pub async fn spawn(spec: ChildSpec) -> io::Result<Self> {
        if spec.timeout.is_zero() || spec.timeout > Duration::from_secs(86_400) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid child deadline",
            ));
        }
        let mut command = tokio::process::Command::new(spec.program);
        command
            .args(spec.args)
            .current_dir(spec.cwd)
            .env_clear()
            .envs(spec.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut wrapped = CommandWrap::from(command);
        wrapped.wrap(ProcessGroup::leader());
        let child = wrapped.spawn()?;
        let (input_tx, input_rx) = mpsc::channel(INPUT_SLOTS);
        let (output_tx, output_rx) = mpsc::channel(OUTPUT_SLOTS);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(None);
        // This task is deliberately retained after handle drop to finish kill + wait.
        // Application shutdown must close handles before stopping the Tokio runtime.
        tokio::spawn(async move {
            let result = run_child(child, input_rx, output_tx, cancel_rx, spec.timeout).await;
            done_tx.send_replace(Some(result));
        });
        Ok(Self {
            input: input_tx,
            output: output_rx,
            cancel: cancel_tx,
            completion: done_rx,
            exit_delivered: false,
        })
    }
    pub async fn next_event(&mut self) -> Option<ChildEvent> {
        if self.exit_delivered {
            return None;
        }
        if let Some(output) = self.output.recv().await {
            return Some(output);
        }
        while self.completion.borrow().is_none() {
            if self.completion.changed().await.is_err() {
                break;
            }
        }
        self.exit_delivered = true;
        Some(
            self.completion
                .borrow()
                .clone()
                .and_then(Result::ok)
                .unwrap_or(ChildEvent::Exited {
                    code: None,
                    reason: ExitReason::IoFailure,
                }),
        )
    }
    pub async fn write(&self, bytes: Vec<u8>, close: bool) -> Result<(), ChildError> {
        if bytes.len() > INPUT_BYTES {
            return Err(ChildError::InputTooLarge);
        }
        let (reply, response) = oneshot::channel();
        // Queue admission times out before the write exists: safely not submitted.
        timeout(
            INPUT_DEADLINE,
            self.input.send(WriteRequest {
                bytes,
                close,
                reply,
            }),
        )
        .await
        .map_err(|_| ChildError::Closed)?
        .map_err(|_| ChildError::Closed)?;
        timeout(INPUT_DEADLINE, response)
            .await
            .map_err(|_| ChildError::Unknown)?
            .map_err(|_| ChildError::Unknown)?
    }
    pub async fn cancel(&self) -> Result<(), ChildError> {
        self.cancel.send_replace(true);
        Ok(())
    }
    pub async fn close(self) -> Result<(), ChildError> {
        self.cancel.send_replace(true);
        let mut completion = self.completion.clone();
        timeout(REAP_DEADLINE + REAP_DEADLINE, async {
            loop {
                if let Some(result) = completion.borrow().clone() {
                    return result.map(|_| ());
                }
                completion
                    .changed()
                    .await
                    .map_err(|_| ChildError::Cleanup)?;
            }
        })
        .await
        .map_err(|_| ChildError::Cleanup)?
    }
}
impl Drop for StreamChild {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

async fn run_child(
    mut child: Box<dyn ChildWrapper>,
    mut input: mpsc::Receiver<WriteRequest>,
    output: mpsc::Sender<ChildEvent>,
    mut cancel: watch::Receiver<bool>,
    lifetime: Duration,
) -> Result<ChildEvent, ChildError> {
    let mut stdin = child.stdin().take();
    let mut stdout = child.stdout().take().ok_or(ChildError::Cleanup)?;
    let mut stderr = child.stderr().take().ok_or(ChildError::Cleanup)?;
    let deadline = Instant::now() + lifetime;
    let mut out = [0; OUTPUT_CHUNK_BYTES];
    let mut err = [0; OUTPUT_CHUNK_BYTES];
    let (mut out_open, mut err_open) = (true, true);
    let mut exit = None;
    let mut reason = ExitReason::Completed;
    loop {
        if !out_open && !err_open && exit.is_some() {
            break;
        }
        let event = tokio::select! {
            biased;
            _ = cancel.changed() => { reason = ExitReason::Cancelled; break; }
            _ = tokio::time::sleep_until(deadline) => { reason = ExitReason::TimedOut; break; }
            status = child.wait(), if exit.is_none() => {
                match status { Ok(status) => exit = Some(status), Err(_) => { reason = ExitReason::IoFailure; break; } }
                None
            }
            request = input.recv(), if stdin.is_some() => {
                if let Some(request) = request
                    && let Some(pipe) = stdin.as_mut() {
                        let written = tokio::select! {
                            biased;
                            _ = cancel.changed() => Err(ChildError::Unknown),
                            result = timeout(INPUT_DEADLINE.min(deadline.saturating_duration_since(Instant::now())), pipe.write_all(&request.bytes)) =>
                                match result { Ok(Ok(())) => Ok(()), _ => Err(ChildError::Unknown) },
                        };
                        let failed = written.is_err();
                        if request.close { stdin.take(); }
                        if request.reply.send(written).is_err() {
                            // Receiver loss leaves the caller with an unknown outcome; never retry.
                        }
                        if failed { reason = ExitReason::IoFailure; break; }
                }
                None
            }
            read = stdout.read(&mut out), if out_open => match read {
                Ok(0) => { out_open = false; None },
                Ok(count) => Some(ChildEvent::Output { stream: OutputStream::Stdout, bytes: out[..count].to_vec() }),
                Err(_) => { reason = ExitReason::IoFailure; break; },
            },
            read = stderr.read(&mut err), if err_open => match read {
                Ok(0) => { err_open = false; None },
                Ok(count) => Some(ChildEvent::Output { stream: OutputStream::Stderr, bytes: err[..count].to_vec() }),
                Err(_) => { reason = ExitReason::IoFailure; break; },
            },
        };
        if let Some(event) = event {
            tokio::select! {
                biased;
                _ = cancel.changed() => { reason = ExitReason::Cancelled; break; },
                _ = tokio::time::sleep_until(deadline) => { reason = ExitReason::TimedOut; break; },
                sent = output.send(event) => if sent.is_err() { reason = ExitReason::Cancelled; break; },
            }
        }
    }
    // Signal only the group created above. Never reconstruct ownership from a saved PID.
    // ESRCH simply means the owned group has already exited; wait still proves reaping.
    if let Err(error) = child.start_kill()
        && error.raw_os_error() != Some(3)
    {
        return Err(ChildError::Cleanup);
    }
    let status = timeout(REAP_DEADLINE, child.wait())
        .await
        .map_err(|_| ChildError::Cleanup)?
        .map_err(|_| ChildError::Cleanup)?;
    Ok(ChildEvent::Exited {
        code: exit.unwrap_or(status).code(),
        reason,
    })
}

/// Bounded one-shot capture on the same owned actor. Callers choose the environment
/// and lifetime; neither command arguments nor output are included in errors.
pub struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub reason: ExitReason,
}

pub async fn capture(spec: ChildSpec) -> io::Result<Capture> {
    let mut child = StreamChild::spawn(spec).await?;
    let mut result = Capture {
        stdout: Vec::new(),
        stderr: Vec::new(),
        code: None,
        reason: ExitReason::IoFailure,
    };
    while let Some(event) = child.next_event().await {
        match event {
            ChildEvent::Output { stream, bytes } => {
                let buffer = match stream {
                    OutputStream::Stdout => &mut result.stdout,
                    OutputStream::Stderr => &mut result.stderr,
                };
                buffer.extend(bytes);
                if buffer.len() > 32768 {
                    buffer.drain(..buffer.len() - 32768);
                }
            }
            ChildEvent::Exited { code, reason } => {
                result.code = code;
                result.reason = reason;
            }
        }
    }
    child
        .close()
        .await
        .map_err(|_| io::Error::other("child cleanup failed"))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str, root: &std::path::Path) -> ChildSpec {
        ChildSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
            cwd: root.into(),
            environment: BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("MONITOR_TEST".into(), "explicit".into()),
            ]),
            timeout: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn input_eof_and_both_output_pipes_precede_exit() {
        let root = tempfile::tempdir().expect("child test fixture");
        let mut child = StreamChild::spawn(spec(
            "read line; printf '%s:%s' \"$line\" \"$MONITOR_TEST\"; printf stderr >&2",
            root.path(),
        ))
        .await
        .expect("child test fixture");
        child
            .write(b"hello\n".to_vec(), true)
            .await
            .expect("child test fixture");
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        while let Some(event) = child.next_event().await {
            match event {
                ChildEvent::Output {
                    stream: OutputStream::Stdout,
                    bytes,
                } => stdout.extend(bytes),
                ChildEvent::Output {
                    stream: OutputStream::Stderr,
                    bytes,
                } => stderr.extend(bytes),
                ChildEvent::Exited { code, reason } => {
                    assert_eq!(code, Some(0));
                    assert_eq!(reason, ExitReason::Completed);
                    assert_eq!(stdout, b"hello:explicit");
                    assert_eq!(stderr, b"stderr");
                }
            }
        }
        child.close().await.expect("child test fixture");
    }

    #[tokio::test]
    async fn cancellation_reaps_owned_process_group() {
        let root = tempfile::tempdir().expect("child test fixture");
        let mut child = StreamChild::spawn(spec("printf ready; read line", root.path()))
            .await
            .expect("child test fixture");
        assert!(matches!(
            child.next_event().await,
            Some(ChildEvent::Output { .. })
        ));
        child.cancel().await.expect("child test fixture");
        assert!(matches!(
            child.next_event().await,
            Some(ChildEvent::Exited {
                reason: ExitReason::Cancelled,
                ..
            })
        ));
        child.close().await.expect("child test fixture");
    }

    #[tokio::test]
    async fn handle_drop_keeps_cleanup_actor_until_reaped() {
        let root = tempfile::tempdir().expect("child test fixture");
        let mut child = StreamChild::spawn(spec("printf ready; read line", root.path()))
            .await
            .expect("child test fixture");
        assert!(matches!(
            child.next_event().await,
            Some(ChildEvent::Output { .. })
        ));
        let mut done = child.completion.clone();
        drop(child);
        done.wait_for(|result| result.is_some())
            .await
            .expect("child test fixture");
        assert!(matches!(
            done.borrow().as_ref(),
            Some(Ok(ChildEvent::Exited {
                reason: ExitReason::Cancelled,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_input_before_admission() {
        let root = tempfile::tempdir().expect("child test fixture");
        let child = StreamChild::spawn(spec("read line", root.path()))
            .await
            .expect("child test fixture");
        assert_eq!(
            child.write(vec![0; INPUT_BYTES + 1], false).await,
            Err(ChildError::InputTooLarge)
        );
        child.close().await.expect("child test fixture");
    }

    #[tokio::test]
    async fn environment_is_explicit_and_nonzero_exit_is_preserved() {
        let root = tempfile::tempdir().expect("child test fixture");
        let mut child = StreamChild::spawn(spec(
            "printf '%s:%s' \"${HOME-unset}\" \"$MONITOR_TEST\"; exit 7",
            root.path(),
        ))
        .await
        .expect("child test fixture");
        let mut output = Vec::new();
        while let Some(event) = child.next_event().await {
            match event {
                ChildEvent::Output { bytes, .. } => output.extend(bytes),
                ChildEvent::Exited { code, reason } => {
                    assert_eq!(code, Some(7));
                    assert_eq!(reason, ExitReason::Completed);
                }
            }
        }
        assert_eq!(output, b"unset:explicit");
    }

    #[tokio::test]
    async fn capture_retains_only_bounded_pipe_tails() {
        let root = tempfile::tempdir().expect("capture root");
        let result = capture(spec("printf '%040000d' 0; printf final >&2", root.path()))
            .await
            .expect("capture");
        assert_eq!(result.stdout.len(), 32768);
        assert!(result.stdout.iter().all(|byte| *byte == b'0'));
        assert_eq!(result.stderr, b"final");
        assert_eq!(result.code, Some(0));
    }

    #[tokio::test]
    async fn lifetime_timer_kills_and_reaps_without_more_output() {
        let root = tempfile::tempdir().expect("timer root");
        let mut child = StreamChild::spawn(spec("printf ready; read line", root.path()))
            .await
            .expect("timer child");
        assert!(matches!(
            child.next_event().await,
            Some(ChildEvent::Output { .. })
        ));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        assert!(matches!(
            child.next_event().await,
            Some(ChildEvent::Exited {
                reason: ExitReason::TimedOut,
                ..
            })
        ));
        child.close().await.expect("timer cleanup");
    }
}
