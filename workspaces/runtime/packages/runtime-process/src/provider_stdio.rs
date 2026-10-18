//! Temporary stdio children behind the async provider seam. A task owns the
//! process group through bounded cleanup even if its caller drops. A reap timeout
//! reports the child identity; it does not prove termination or cap surviving children.
use crate::{child_priority::ChildPriority, operational_log::OperationalLog, provider_seam::*, *};
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use std::{io, process::Stdio};
use studio_runtime_providers::{DecodedJournalPayload, decode_journal_entry_borrowed};
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
                        write_commands(stdin, inbox),
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
                    Some(log) => tokio::task::spawn_blocking(move || log.flush())
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
) -> Result<(), AdapterError> {
    while let Some(command) = commands.recv().await {
        match command {
            AdapterCommand::Write {
                write,
                reply,
                observed,
            } => {
                let mut bytes = serde_json::to_vec(&write.message)
                    .map_err(|_| AdapterError::InvalidConfiguration)?;
                bytes.push(b'\n');
                stdin.write_all(&bytes).await.map_err(process_error)?;
                stdin.flush().await.map_err(process_error)?;
                if reply
                    .send(CommandReceipt::Written {
                        remote_id: None,
                        duplicate: false,
                    })
                    .is_err()
                {
                    return Err(AdapterError::ConsumerClosed);
                }
                observed.await.map_err(|_| AdapterError::ConsumerClosed)?;
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
    loop {
        let available = reader.fill_buf().await.map_err(process_error)?;
        if available.is_empty() {
            return Ok(false);
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |end| end + 1);
        *total = total
            .checked_add(count)
            .ok_or(AdapterError::InvalidConfiguration)?;
        let payload_count = count - usize::from(available[count - 1] == b'\n');
        if line.len() + payload_count > frame_limit {
            return Err(AdapterError::OutputLimit { limit: frame_limit });
        }
        if let Some(limit) = output_limit.filter(|limit| *total > *limit) {
            return Err(AdapterError::OutputLimit { limit });
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn failed_reap_reports_the_unconfirmed_child() {
        let error = super::reap_with_deadline::<()>(Some(12345), std::future::pending())
            .await
            .expect_err("bounded reap");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("12345"));
        assert!(error.to_string().contains("termination is unconfirmed"));
    }

    use super::*;
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
    async fn stdout_frame_limit_excludes_the_line_delimiter_at_codec_boundary() {
        let mut input = vec![b'x'; studio_runtime_providers::MAX_FRAME_BYTES];
        input.push(b'\n');
        let mut reader = BufReader::new(input.as_slice());
        let mut line = Vec::new();
        let mut total = 0;

        assert!(
            read_stdout_line(
                &mut reader,
                &mut line,
                &mut total,
                studio_runtime_providers::MAX_FRAME_BYTES,
                None,
            )
            .await
            .expect("codec-sized payload with newline is one frame")
        );
        assert_eq!(line.len(), studio_runtime_providers::MAX_FRAME_BYTES + 1);
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(total, studio_runtime_providers::MAX_FRAME_BYTES + 1);
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
