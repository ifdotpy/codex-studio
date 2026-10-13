//! Temporary stdio children behind the async provider seam. A task owns the
//! process group through bounded cleanup even if its caller drops. A reap timeout
//! reports the child identity; it does not prove termination or cap surviving children.
use crate::{provider_seam::*, *};
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use std::{io, process::Stdio};
use studio_runtime_providers::{DecodedJournalPayload, decode_journal_entry};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, watch},
    time::Instant,
};

// POSIX ESRCH: the process group exited before the stop signal.
const NO_SUCH_PROCESS: i32 = 3;
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

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
    pub deadline: Instant,
    pub max_frame_bytes: usize,
    pub max_output_bytes: usize,
}

impl ProviderTaskFactory for StdioProviderTaskFactory {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            if Instant::now() >= self.deadline {
                return Err(AdapterError::Timeout);
            }
            let generation = options.expected_generation.unwrap_or(Generation(1));
            let Some(executable) = options.launch.command.first() else {
                return Err(AdapterError::InvalidConfiguration);
            };
            if self.max_frame_bytes == 0
                || self.max_output_bytes == 0
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
                .current_dir(&options.launch.cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = CommandWrap::from(command)
                .wrap(ProcessGroup::leader())
                .spawn()
                .map_err(process_error)?;
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
            let mut stop = shutdown.subscribe();
            let task = tokio::spawn(async move {
                let exchange = async {
                    let stdin = child
                        .stdin()
                        .take()
                        .ok_or(AdapterError::InvalidConfiguration)?;
                    let stdout = child
                        .stdout()
                        .take()
                        .ok_or(AdapterError::InvalidConfiguration)?;
                    let mut stderr = child
                        .stderr()
                        .take()
                        .ok_or(AdapterError::InvalidConfiguration)?;
                    tokio::try_join!(
                        write_commands(stdin, inbox),
                        read_entries(
                            stdout,
                            entries,
                            options.clock,
                            generation,
                            frame_limit,
                            output_limit
                        ),
                        async {
                            tokio::io::copy(&mut stderr, &mut tokio::io::sink())
                                .await
                                .map_err(process_error)?;
                            Ok(())
                        },
                    )?;
                    Ok(())
                };
                let result = tokio::select! {
                    biased;
                    _ = Shutdown::cancelled(&mut stop) => Ok(()),
                    result = tokio::time::timeout_at(deadline, exchange) => result.unwrap_or(Err(AdapterError::Timeout)),
                };
                // Always wait, including when the group already exited (ESRCH).
                let pid = child.id();
                let killed = child.start_kill();
                let reaped = reap_with_deadline(pid, child.wait()).await;
                let cleanup = killed
                    .or_else(|error| {
                        if error.raw_os_error() == Some(NO_SUCH_PROCESS) {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .and(reaped.map(|_| ()))
                    .map_err(process_error);
                let cleanup_error = cleanup.as_ref().err().map(ToString::to_string);
                drop(state);
                TaskExit {
                    result: cleanup.and(result),
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
    output_limit: usize,
) -> Result<(), AdapterError> {
    let mut reader = BufReader::new(stdout);
    let mut total = 0_usize;
    let mut cursor = 0_u64;
    loop {
        let mut line = Vec::new();
        loop {
            let available = reader.fill_buf().await.map_err(process_error)?;
            if available.is_empty() {
                return Err(SupervisorError::ConnectionLost.into());
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |end| end + 1);
            total = total
                .checked_add(count)
                .ok_or(AdapterError::InvalidConfiguration)?;
            if line.len() + count > frame_limit || total > output_limit {
                return Err(AdapterError::InvalidConfiguration);
            }
            line.extend_from_slice(&available[..count]);
            reader.consume(count);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        cursor += 1;
        let decoded = decode_journal_entry(cursor, generation.0, line);
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
                20
            )
            .await,
            Err(AdapterError::InvalidConfiguration)
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
                100
            )
            .await,
            Err(AdapterError::InvalidConfiguration)
        ));
        assert!(journal.recv().await.is_none());
    }
}
