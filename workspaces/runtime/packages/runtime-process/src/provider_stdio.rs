//! Temporary stdio children behind the async provider seam. A task owns the
//! process group through bounded cleanup even if its caller drops. A reap timeout
//! reports the child identity; it does not prove termination or cap surviving children.
use crate::{child_priority::ChildPriority, operational_log::OperationalLog, provider_seam::*, *};
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use serde::Serialize;
use std::{
    io::{self, Write as IoWrite},
    process::Stdio,
    sync::OnceLock,
};
use studio_runtime_providers::{
    DecodedJournalPayload, Method, RequestId, decode_journal_entry_borrowed,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, watch},
    time::Instant,
};

// POSIX ESRCH: the process group exited before the stop signal.
const NO_SUCH_PROCESS: i32 = 3;
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
const STDERR_LINE_LIMIT: usize = 64 * 1024;
// Set to 1 only for the announced provider stdin timing profile.
const STDIN_TIMING_DIAGNOSTICS_ENV: &str = "CODEX_RUNTIME_STDIN_TIMING_DIAGNOSTICS";
const STDIN_TIMING_DIAGNOSTIC_PREFIX: &[u8] = b"studio_provider_stdin_write ";

static STDIN_TIMING_DIAGNOSTICS_ENABLED: OnceLock<bool> = OnceLock::new();

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StdinTimingDiagnostic<'a> {
    provider_handle: &'a str,
    generation: Generation,
    operation_id: &'a OperationId,
    request_id: RequestId,
    method: &'static str,
    stdin_flushed_at_ns: u128,
}

fn stdin_timing_diagnostics_enabled() -> bool {
    *STDIN_TIMING_DIAGNOSTICS_ENABLED
        .get_or_init(|| std::env::var(STDIN_TIMING_DIAGNOSTICS_ENV).is_ok_and(|value| value == "1"))
}

fn is_turn_start(write: &PreparedWrite) -> bool {
    write
        .message()
        .get("method")
        .and_then(serde_json::Value::as_str)
        .and_then(Method::from_wire)
        == Some(Method::TurnStart)
}

fn stdin_timing_diagnostic_line(
    handle: &ProviderHandle,
    generation: Generation,
    write: &PreparedWrite,
    stdin_flushed_at_ns: u128,
) -> Option<Vec<u8>> {
    let method = write
        .message()
        .get("method")
        .and_then(serde_json::Value::as_str)
        .and_then(Method::from_wire)?;
    if method != Method::TurnStart {
        return None;
    }
    let request_id = serde_json::from_value(write.message().get("id")?.clone()).ok()?;
    let diagnostic = StdinTimingDiagnostic {
        provider_handle: handle.as_str(),
        generation,
        operation_id: write.operation_id(),
        request_id,
        method: method.as_str(),
        stdin_flushed_at_ns,
    };
    let payload = serde_json::to_vec(&diagnostic).ok()?;
    let mut line = Vec::with_capacity(STDIN_TIMING_DIAGNOSTIC_PREFIX.len() + payload.len() + 1);
    line.extend_from_slice(STDIN_TIMING_DIAGNOSTIC_PREFIX);
    line.extend_from_slice(&payload);
    line.push(b'\n');
    Some(line)
}

fn write_stdin_timing_diagnostic_best_effort(line: &[u8]) {
    // The verifier treats a missing diagnostic as incomplete; diagnostic I/O
    // errors must never alter provider write or receipt behavior.
    match io::stderr().lock().write_all(line) {
        Ok(()) | Err(_) => {}
    }
}

async fn reap_with_deadline<T>(
    pid: Option<u32>,
    wait: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(REAP_GRACE, wait)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("provider child {pid:?} outlived reap grace; termination is unconfirmed"),
            ))
        })
}

pub struct StdioProviderTaskFactory {
    /// End of a bounded one-off exchange; `None` for a persistent account session.
    pub deadline: Option<Instant>,
    /// Bytes in one stdout line, the provider's JSON-RPC frame.
    pub max_frame_bytes: usize,
    /// Total stdout bytes of a bounded one-off exchange; `None` for a persistent
    /// account session, whose stdout is bounded per frame only.
    pub max_output_bytes: Option<usize>,
    pub stderr_log: Option<OperationalLog>,
}

impl ProviderTaskFactory for StdioProviderTaskFactory {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        self.open_with_priority(handle, options, shutdown, ChildPriority::Normal)
    }
}

impl StdioProviderTaskFactory {
    /// Open a provider child with the caller's explicit scheduling policy.
    /// Shared helper callers use the trait implementation's inherited priority.
    pub fn open_with_priority(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
        priority: ChildPriority,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Err(AdapterError::Timeout);
            }
            let generation = options.expected_generation.unwrap_or(Generation(1));
            let Some(executable) = options.launch.command.first() else {
                return Err(AdapterError::InvalidConfiguration);
            };
            if self.max_frame_bytes == 0
                || self.max_output_bytes == Some(0)
                || generation.0 == 0
                || options.launch.handled_cursor != 0
                || !options.launch.retained_response_ids.is_empty()
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            let mut command = Command::new(executable);
            command
                .args(&options.launch.command[1..])
                .env_clear()
                .envs(&options.launch.environment)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(cwd) = &options.launch.cwd {
                command.current_dir(cwd);
            }
            let mut wrapped = CommandWrap::from(command);
            wrapped.wrap(ProcessGroup::leader());
            priority.apply_before_spawn(&mut wrapped);
            let mut child = wrapped.spawn().map_err(process_error)?;
            let (commands, inbox) = mpsc::channel(LANE_CAPACITY);
            let (entries, journal) = mpsc::channel(LANE_CAPACITY);
            let diagnostic_handle = handle.clone();
            let (state, status) = watch::channel(AttachmentStatus {
                handle: handle.clone(),
                open_attempted: true,
                info: Some(AttachmentInfo {
                    handle,
                    generation,
                    pid: child.id().unwrap_or(0),
                    acknowledged: EventCursor(0),
                    resumed: false,
                }),
                initialization: InitializationStatus::AwaitingReply,
            });
            let deadline = self.deadline;
            let frame_limit = self.max_frame_bytes;
            let output_limit = self.max_output_bytes;
            let stderr_log = self.stderr_log.clone();
            let stderr_log_for_flush = stderr_log.clone();
            let mut stop = shutdown.subscribe();
            let task = tokio::spawn(async move {
                // Start the stderr owner before selecting on shutdown. Even an
                // immediately cancelled exchange must retain and drain this pipe.
                let stderr_drain = child.stderr().take().map(|mut stderr| {
                    let log = stderr_log.clone();
                    tokio::spawn(async move { drain_stderr(&mut stderr, log.as_ref()).await })
                });
                let stderr_available = stderr_drain.is_some();
                let exchange = async {
                    let stdin = child
                        .stdin()
                        .take()
                        .ok_or(AdapterError::InvalidConfiguration)?;
                    let stdout = child
                        .stdout()
                        .take()
                        .ok_or(AdapterError::InvalidConfiguration)?;
                    if !stderr_available {
                        return Err(AdapterError::InvalidConfiguration);
                    }
                    tokio::try_join!(
                        write_commands(stdin, inbox, diagnostic_handle, generation,),
                        read_entries(
                            stdout,
                            entries,
                            options.clock,
                            generation,
                            frame_limit,
                            output_limit
                        )
                    )?;
                    Ok(())
                };
                let bounded = async {
                    match deadline {
                        Some(deadline) => tokio::time::timeout_at(deadline, exchange)
                            .await
                            .unwrap_or(Err(AdapterError::Timeout)),
                        None => exchange.await,
                    }
                };
                let result = tokio::select! {
                    biased;
                    _ = Shutdown::cancelled(&mut stop) => Ok(()),
                    result = bounded => result,
                };
                // Always wait, including when the group already exited (ESRCH).
                let pid = child.id();
                let killed = child.start_kill();
                let reaped = reap_with_deadline(pid, child.wait()).await;
                let cleanup = killed
                    .or_else(|error| {
                        if error.raw_os_error() == Some(NO_SUCH_PROCESS)
                            || (reaped.is_ok()
                                && crate::owned_process_group::vanished_after_reap(pid, &error))
                        {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .and(reaped.map(|_| ()))
                    .map_err(process_error);
                let stderr_result = match stderr_drain {
                    Some(mut stderr_drain) => {
                        match tokio::time::timeout(REAP_GRACE, &mut stderr_drain).await {
                            Ok(Ok(result)) => result,
                            Ok(Err(_)) => Err(AdapterError::TaskFailed),
                            Err(_) => {
                                stderr_drain.abort();
                                Err(AdapterError::Timeout)
                            }
                        }
                    }
                    None => Ok(()),
                };
                // The log writer is a separate thread; completion includes its flush.
                let stderr_flush = match stderr_log_for_flush {
                    Some(log) => crate::blocking_pool::spawn_blocking(
                        "runtime-process/provider_stdio",
                        move || log.flush(),
                    )
                    .await
                    .map_err(|_| AdapterError::TaskFailed)
                    .and_then(|result| result.map_err(process_error)),
                    None => Ok(()),
                };
                let cleanup_error = cleanup.as_ref().err().map(ToString::to_string);
                drop(state);
                TaskExit {
                    result: cleanup.and(result).and(stderr_result).and(stderr_flush),
                    cleanup_error,
                    recovery_needed: None,
                }
            });
            Ok(ProviderAttachment::from_task(
                commands, journal, status, shutdown, task,
            ))
        })
    }
}

pub(crate) async fn drain_stderr(
    stderr: &mut (impl AsyncRead + Unpin),
    log: Option<&OperationalLog>,
) -> Result<(), AdapterError> {
    if log.is_none() {
        tokio::io::copy(stderr, &mut tokio::io::sink())
            .await
            .map_err(process_error)?;
        return Ok(());
    }
    let log = log.ok_or(AdapterError::InvalidConfiguration)?;
    let mut reader = BufReader::new(stderr);
    let mut bytes = [0_u8; 8192];
    let mut line = Vec::new();
    let mut discarding = false;
    loop {
        let count = tokio::io::AsyncReadExt::read(&mut reader, &mut bytes)
            .await
            .map_err(process_error)?;
        if count == 0 {
            if discarding {
                log.note_dropped_line();
            } else if !line.is_empty() {
                log.write_line(&line);
            }
            return Ok(());
        }
        for byte in &bytes[..count] {
            if !discarding {
                if line.len() == STDERR_LINE_LIMIT {
                    line.clear();
                    discarding = true;
                } else {
                    line.push(*byte);
                }
            }
            if *byte == b'\n' {
                if discarding {
                    log.note_dropped_line();
                    discarding = false;
                } else {
                    log.write_line(&line);
                    line.clear();
                }
            }
        }
    }
}

fn process_error(error: io::Error) -> AdapterError {
    AdapterError::Supervisor(SupervisorError::Io(error))
}

async fn write_commands(
    mut stdin: impl AsyncWrite + Unpin,
    mut commands: mpsc::Receiver<AdapterCommand>,
    handle: ProviderHandle,
    generation: Generation,
) -> Result<(), AdapterError> {
    while let Some(command) = commands.recv().await {
        match command {
            AdapterCommand::Write {
                write,
                reply,
                observed,
            } => {
                write_one_command(
                    &mut stdin,
                    write,
                    reply,
                    observed,
                    WriterTimingDiagnostics {
                        handle: &handle,
                        generation,
                        enabled: stdin_timing_diagnostics_enabled(),
                        sample_timestamp: crate::clock::monotonic_ns,
                        emit_diagnostic: write_stdin_timing_diagnostic_best_effort,
                    },
                )
                .await?;
            }
            AdapterCommand::Ack { cursor, reply } => {
                if reply.send(CommandReceipt::Acknowledged(cursor)).is_err() {
                    return Err(AdapterError::ConsumerClosed);
                }
            }
        }
    }
    Err(AdapterError::ConsumerClosed)
}

struct WriterTimingDiagnostics<'a, Sample, Emit> {
    handle: &'a ProviderHandle,
    generation: Generation,
    enabled: bool,
    sample_timestamp: Sample,
    emit_diagnostic: Emit,
}

async fn write_one_command<Sample, Emit>(
    stdin: &mut (impl AsyncWrite + Unpin),
    write: PreparedWrite,
    reply: tokio::sync::oneshot::Sender<CommandReceipt>,
    observed: tokio::sync::oneshot::Receiver<()>,
    timing: WriterTimingDiagnostics<'_, Sample, Emit>,
) -> Result<(), AdapterError>
where
    Sample: FnOnce() -> u128,
    Emit: FnOnce(&[u8]),
{
    let mut bytes =
        serde_json::to_vec(&write.message).map_err(|_| AdapterError::InvalidConfiguration)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.map_err(process_error)?;
    stdin.flush().await.map_err(process_error)?;
    let stdin_flushed_at_ns = if timing.enabled && is_turn_start(&write) {
        Some((timing.sample_timestamp)())
    } else {
        None
    };
    let receipt_sent = reply.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    if let Some(stdin_flushed_at_ns) = stdin_flushed_at_ns
        && let Some(line) = stdin_timing_diagnostic_line(
            timing.handle,
            timing.generation,
            &write,
            stdin_flushed_at_ns,
        )
    {
        (timing.emit_diagnostic)(&line);
    }
    if receipt_sent.is_err() {
        return Err(AdapterError::ConsumerClosed);
    }
    observed.await.map_err(|_| AdapterError::ConsumerClosed)
}

async fn read_entries(
    stdout: impl AsyncRead + Unpin,
    entries: mpsc::Sender<JournalEntry>,
    clock: SessionClock,
    generation: Generation,
    frame_limit: usize,
    output_limit: Option<usize>,
) -> Result<(), AdapterError> {
    let mut reader = BufReader::new(stdout);
    let mut total = 0_usize;
    let mut cursor = 0_u64;
    let mut line = Vec::new();
    loop {
        if !read_stdout_line(
            &mut reader,
            &mut line,
            &mut total,
            frame_limit,
            output_limit,
        )
        .await?
        {
            return Err(SupervisorError::ConnectionLost.into());
        }
        cursor += 1;
        let decoded = decode_journal_entry_borrowed(cursor, generation.0, &line);
        let payload = match decoded.payload {
            DecodedJournalPayload::Frame(frame) => JournalPayload::Frame(frame),
            DecodedJournalPayload::RawStdout { bytes, error } => {
                JournalPayload::RawStdout { bytes, error }
            }
        };
        entries
            .send(JournalEntry {
                received_at_ms: clock.now_ms(),
                cursor: EventCursor(cursor),
                generation,
                payload,
            })
            .await
            .map_err(|_| AdapterError::ConsumerClosed)?;
    }
}

async fn read_stdout_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
    total: &mut usize,
    frame_limit: usize,
    output_limit: Option<usize>,
) -> Result<bool, AdapterError> {
    line.clear();
    let wire_limit = frame_limit
        .checked_add(studio_runtime_providers::MAX_FRAME_DELIMITER_BYTES)
        .ok_or(AdapterError::InvalidConfiguration)?;
    loop {
        let available = reader.fill_buf().await.map_err(process_error)?;
        if available.is_empty() {
            if studio_runtime_providers::frame_payload_bytes(line) > frame_limit {
                return Err(AdapterError::OutputLimit { limit: frame_limit });
            }
            return Ok(false);
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |end| end + 1);
        *total = total
            .checked_add(count)
            .ok_or(AdapterError::InvalidConfiguration)?;
        let frame_len = line
            .len()
            .checked_add(count)
            .ok_or(AdapterError::InvalidConfiguration)?;
        if frame_len > wire_limit {
            return Err(AdapterError::OutputLimit { limit: frame_limit });
        }
        if let Some(limit) = output_limit.filter(|limit| *total > *limit) {
            return Err(AdapterError::OutputLimit { limit });
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        let payload_len = if line.last() == Some(&b'\n') {
            studio_runtime_providers::frame_payload_bytes(line)
        } else {
            studio_runtime_providers::frame_prefix_payload_bytes(line)
        };
        if payload_len > frame_limit {
            return Err(AdapterError::OutputLimit { limit: frame_limit });
        }
        if line.last() == Some(&b'\n') {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };
    use tokio::io::AsyncWrite;

    struct FlushWitness {
        flushed: Arc<AtomicBool>,
        fail_write: bool,
    }

    impl AsyncWrite for FlushWitness {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.fail_write {
                Poll::Ready(Err(io::Error::other("injected write failure")))
            } else {
                Poll::Ready(Ok(bytes.len()))
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushed.store(true, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn turn_start_write() -> PreparedWrite {
        let decoded = decode_journal_entry_borrowed(
            1,
            7,
            br#"{"jsonrpc":"2.0","id":"request-1","method":"turn/start","params":{}}"#,
        );
        let DecodedJournalPayload::Frame(frame) = decoded.payload else {
            panic!("test request decodes as a frame");
        };
        PreparedWrite::new(OperationId("attempt-op".into()), &frame).expect("test request prepares")
    }

    #[test]
    fn provider_stdin_timing_line_matches_shared_golden_contract() {
        let decoded = decode_journal_entry_borrowed(
            1,
            7,
            br#"{"jsonrpc":"2.0","id":"request-1","method":"turn/start","params":{}}"#,
        );
        let DecodedJournalPayload::Frame(frame) = decoded.payload else {
            panic!("golden request decodes as a frame");
        };
        let write =
            PreparedWrite::new(OperationId("attempt-op".into()), &frame).expect("request prepares");
        let line = stdin_timing_diagnostic_line(
            &ProviderHandle::for_account("default"),
            Generation(7),
            &write,
            1_234_567_890,
        )
        .expect("TurnStart produces timing evidence");
        assert_eq!(
            line,
            include_str!("../tests/fixtures/provider-stdin-timing-diagnostic.log").as_bytes()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_reap_reports_the_unconfirmed_child() {
        let error = super::reap_with_deadline::<()>(Some(12345), std::future::pending())
            .await
            .expect_err("bounded reap");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("12345"));
        assert!(error.to_string().contains("termination is unconfirmed"));
    }

    #[tokio::test]
    async fn stdin_timing_diagnostic_follows_flush_even_if_receipt_receiver_closed() {
        let flushed = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(AtomicUsize::new(0));
        let emitted = Arc::new(AtomicUsize::new(0));
        let mut stdin = FlushWitness {
            flushed: Arc::clone(&flushed),
            fail_write: false,
        };
        let (reply, receiver) = tokio::sync::oneshot::channel();
        drop(receiver);
        let (_observation, observed) = tokio::sync::oneshot::channel();
        let sample_flushed = Arc::clone(&flushed);
        let sample_count = Arc::clone(&samples);
        let emit_flushed = Arc::clone(&flushed);
        let emitted_count = Arc::clone(&emitted);

        let result = write_one_command(
            &mut stdin,
            turn_start_write(),
            reply,
            observed,
            WriterTimingDiagnostics {
                handle: &ProviderHandle::for_account("default"),
                generation: Generation(7),
                enabled: true,
                sample_timestamp: move || {
                    assert!(sample_flushed.load(Ordering::SeqCst));
                    sample_count.fetch_add(1, Ordering::SeqCst);
                    123
                },
                emit_diagnostic: move |line: &[u8]| {
                    assert!(emit_flushed.load(Ordering::SeqCst));
                    assert!(line.starts_with(STDIN_TIMING_DIAGNOSTIC_PREFIX));
                    emitted_count.fetch_add(1, Ordering::SeqCst);
                },
            },
        )
        .await;

        assert!(matches!(result, Err(AdapterError::ConsumerClosed)));
        assert_eq!(samples.load(Ordering::SeqCst), 1);
        assert_eq!(emitted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_stdin_write_does_not_sample_or_emit_timing() {
        let flushed = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(AtomicUsize::new(0));
        let emitted = Arc::new(AtomicUsize::new(0));
        let mut stdin = FlushWitness {
            flushed: Arc::clone(&flushed),
            fail_write: true,
        };
        let (reply, _receiver) = tokio::sync::oneshot::channel();
        let (_observation, observed) = tokio::sync::oneshot::channel();
        let sample_count = Arc::clone(&samples);
        let emitted_count = Arc::clone(&emitted);

        let result = write_one_command(
            &mut stdin,
            turn_start_write(),
            reply,
            observed,
            WriterTimingDiagnostics {
                handle: &ProviderHandle::for_account("default"),
                generation: Generation(7),
                enabled: true,
                sample_timestamp: move || {
                    sample_count.fetch_add(1, Ordering::SeqCst);
                    123
                },
                emit_diagnostic: move |_: &[u8]| {
                    emitted_count.fetch_add(1, Ordering::SeqCst);
                },
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(AdapterError::Supervisor(SupervisorError::Io(_)))
        ));
        assert_eq!(samples.load(Ordering::SeqCst), 0);
        assert_eq!(emitted.load(Ordering::SeqCst), 0);
        assert!(!flushed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stdout_line_buffer_reuses_its_allocation_between_frames() {
        let mut reader = BufReader::new(&b"first\nsecond\n"[..]);
        let mut line = Vec::new();
        let mut total = 0;

        assert!(
            read_stdout_line(&mut reader, &mut line, &mut total, 16, Some(32))
                .await
                .expect("first bounded frame")
        );
        let allocation = (line.as_ptr(), line.capacity());
        assert_eq!(line, b"first\n");

        assert!(
            read_stdout_line(&mut reader, &mut line, &mut total, 16, Some(32))
                .await
                .expect("second bounded frame")
        );
        assert_eq!(line, b"second\n");
        assert_eq!((line.as_ptr(), line.capacity()), allocation);
        assert_eq!(total, b"first\nsecond\n".len());
    }

    #[tokio::test]
    async fn stdout_frame_limit_handles_lf_crlf_and_oversized_payloads() {
        let frame_limit = studio_runtime_providers::MAX_FRAME_BYTES;
        let mut lf_input = vec![b'x'; frame_limit];
        lf_input.push(b'\n');
        let mut lf_reader = BufReader::new(lf_input.as_slice());
        let mut line = Vec::new();
        let mut total = 0;

        assert!(
            read_stdout_line(&mut lf_reader, &mut line, &mut total, frame_limit, None,)
                .await
                .expect("codec-sized LF frame")
        );
        assert_eq!(
            studio_runtime_providers::frame_payload_bytes(&line),
            frame_limit
        );
        assert_eq!(line.len(), frame_limit + 1);
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(total, frame_limit + 1);

        let mut crlf_input = vec![b'x'; frame_limit];
        crlf_input.extend_from_slice(b"\r\n");
        // Place CR at the end of the first BufReader fill to exercise CRLF split across reads.
        let mut crlf_reader = BufReader::with_capacity(frame_limit + 1, crlf_input.as_slice());
        let mut line = Vec::new();
        let mut total = 0;
        assert!(
            read_stdout_line(&mut crlf_reader, &mut line, &mut total, frame_limit, None,)
                .await
                .expect("codec-sized split CRLF frame")
        );
        assert_eq!(
            studio_runtime_providers::frame_payload_bytes(&line),
            frame_limit
        );
        assert_eq!(line.len(), frame_limit + 2);
        assert_eq!(total, frame_limit + 2);

        let mut oversized = vec![b'x'; frame_limit + 1];
        oversized.extend_from_slice(b"\r\n");
        let mut reader = BufReader::with_capacity(frame_limit + 2, oversized.as_slice());
        let mut line = Vec::new();
        let mut total = 0;
        assert!(matches!(
            read_stdout_line(&mut reader, &mut line, &mut total, frame_limit, None).await,
            Err(AdapterError::OutputLimit { limit }) if limit == frame_limit
        ));

        let mut unterminated = vec![b'x'; frame_limit];
        unterminated.push(b'\r');
        let mut reader = BufReader::new(unterminated.as_slice());
        let mut line = Vec::new();
        let mut total = 0;
        assert!(matches!(
            read_stdout_line(&mut reader, &mut line, &mut total, frame_limit, None).await,
            Err(AdapterError::OutputLimit { limit }) if limit == frame_limit
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn raw_unknown_output_counts_toward_the_total_limit() {
        let (entries, mut journal) = mpsc::channel(LANE_CAPACITY);
        let input = b"not-json\nnot-json\nnot-json\n";
        assert!(matches!(
            read_entries(
                &input[..],
                entries,
                SessionClock::default(),
                Generation(1),
                16,
                Some(20)
            )
            .await,
            Err(AdapterError::OutputLimit { limit: 20 })
        ));
        for cursor in [1, 2] {
            let entry = journal.recv().await.expect("bounded raw entry");
            assert_eq!(entry.cursor, EventCursor(cursor));
            assert!(matches!(entry.payload, JournalPayload::RawStdout { .. }));
        }
        assert!(journal.recv().await.is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn raw_frame_is_bounded_before_decoding() {
        let (entries, mut journal) = mpsc::channel(LANE_CAPACITY);
        assert!(matches!(
            read_entries(
                &b"123456789"[..],
                entries,
                SessionClock::default(),
                Generation(1),
                8,
                Some(100)
            )
            .await,
            Err(AdapterError::OutputLimit { limit: 8 })
        ));
        assert!(journal.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_session_output_is_bounded_per_frame_only() {
        let (entries, mut journal) = mpsc::channel(LANE_CAPACITY);
        let input = b"not-json\nnot-json\nnot-json\n";
        assert!(matches!(
            read_entries(
                &input[..],
                entries,
                SessionClock::default(),
                Generation(1),
                16,
                None
            )
            .await,
            Err(AdapterError::Supervisor(SupervisorError::ConnectionLost))
        ));
        for cursor in [1, 2, 3] {
            let entry = journal.recv().await.expect("unbounded total entry");
            assert_eq!(entry.cursor, EventCursor(cursor));
        }
    }
}
