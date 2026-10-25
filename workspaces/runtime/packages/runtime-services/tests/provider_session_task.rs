#![allow(clippy::unwrap_used)]
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use studio_runtime_process::{
    EventCursor, Generation, MAX_SUPERVISOR_FRAME_BYTES, OperationId, ProviderHandle,
    ProviderLaunchSpec, provider_seam::*,
};
use studio_runtime_providers::{Frame, Method, RequestId, RequestOutcome, ResponseBody};
use studio_runtime_services::provider_session_task::*;
use tokio::sync::{mpsc, oneshot, watch};

struct Factory(Mutex<Option<ProviderAttachment>>);
impl ProviderTaskFactory for Factory {
    fn open(&self, _: ProviderHandle, _: OpenOptions, _: Shutdown) -> OpenFuture<'_> {
        let attachment = self.0.lock().unwrap().take().unwrap();
        Box::pin(async move { Ok(attachment) })
    }
}
enum Observed {
    Write(oneshot::Sender<CommandReceipt>),
    Ack(EventCursor, oneshot::Sender<CommandReceipt>),
}
struct Peer {
    events: mpsc::Sender<JournalEntry>,
    commands: mpsc::Receiver<Observed>,
    gone: oneshot::Receiver<()>,
    stopping: oneshot::Receiver<()>,
}
async fn stopped(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() || stop.changed().await.is_err() {
            return;
        }
    }
}
fn fake() -> (Arc<dyn ProviderTaskFactory>, Peer) {
    fake_with_stop_barrier(None)
}
fn fake_with_stop_barrier(
    barrier: Option<oneshot::Receiver<()>>,
) -> (Arc<dyn ProviderTaskFactory>, Peer) {
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(LANE_CAPACITY);
    let (events, journal) = mpsc::channel(LANE_CAPACITY);
    let (observed, commands_seen) = mpsc::channel(LANE_CAPACITY);
    let (gone, disappeared) = oneshot::channel();
    let (stopping, started_stop) = oneshot::channel();
    let handle = ProviderHandle::for_account("test");
    let (status, receiver) = watch::channel(AttachmentStatus {
        handle: handle.clone(),
        open_attempted: true,
        info: Some(AttachmentInfo {
            handle: handle.clone(),
            generation: Generation(1),
            pid: 1,
            acknowledged: EventCursor(0),
            resumed: false,
        }),
        initialization: InitializationStatus::AwaitingReply,
    });
    let task = tokio::spawn(async move {
        let mut initialized = false;
        let mut count = 0;
        loop {
            let command = tokio::select! { biased; _ = stopped(&mut stop) => break, command = inbox.recv() => command };
            let Some(command) = command else {
                break;
            };
            let (reply, receipt) = oneshot::channel();
            let (event, original, observation) = match command {
                AdapterCommand::Write {
                    reply: original,
                    observed,
                    ..
                } => {
                    count += 1;
                    (Observed::Write(reply), original, Some(observed))
                }
                AdapterCommand::Ack {
                    cursor,
                    reply: original,
                } => (Observed::Ack(cursor, reply), original, None),
            };
            tokio::select! { biased; _ = stopped(&mut stop) => { let _ignored = original.send(CommandReceipt::Unknown); break; }, _ = observed.send(event) => {} }
            let result = tokio::select! { biased; _ = stopped(&mut stop) => CommandReceipt::Unknown, result = receipt => result.unwrap_or(CommandReceipt::Unknown) };
            if count == 2
                && observation.is_some()
                && matches!(result, CommandReceipt::Written { .. })
            {
                initialized = true;
            }
            let needs_observation = result.requires_observation();
            let _ignored = original.send(result);
            if needs_observation && let Some(observation) = observation {
                tokio::select! { biased; _ = stopped(&mut stop) => break, result = observation => if result.is_err() { break; } }
            }
        }
        inbox.close();
        while let Ok(command) = inbox.try_recv() {
            let reply = match command {
                AdapterCommand::Write { reply, .. } | AdapterCommand::Ack { reply, .. } => reply,
            };
            let _ignored = reply.send(CommandReceipt::NotSent);
        }
        let _ignored = stopping.send(());
        if let Some(barrier) = barrier {
            let _ignored = barrier.await;
        }
        drop(status);
        let _ignored = gone.send(());
        TaskExit {
            result: Ok(()),
            cleanup_error: None,
            recovery_needed: (!initialized).then_some(InitializationRecovery {
                handle,
                generation: Some(Generation(1)),
                phase: InitializationStatus::AwaitingReply,
            }),
            provider_retained: false,
        }
    });
    let attachment = ProviderAttachment::from_task(commands, journal, receiver, shutdown, task);
    (
        Arc::new(Factory(Mutex::new(Some(attachment)))),
        Peer {
            events,
            commands: commands_seen,
            gone: disappeared,
            stopping: started_stop,
        },
    )
}
fn options() -> OpenOptions {
    OpenOptions {
        clock: SessionClock::default(),
        launch: ProviderLaunchSpec::new(vec!["fake".into()], BTreeMap::new(), "/tmp".into()),
        expected_generation: None,
    }
}
fn config() -> SessionTaskConfig {
    SessionTaskConfig {
        initialize_params: json!({}),
        generation: Generation(1),
        wire: studio_runtime_providers::ProviderKind::Codex.wire(),
        timeout_ms: 1000,
        initialize_operation: OperationId("init:attachment-1".into()),
        request_id_prefix: Some("attachment-1".into()),
        recovered: vec![],
    }
}
async fn open() -> (SessionHandle, Peer) {
    let (factory, peer) = fake();
    let session = SessionHandle::open(
        factory,
        ProviderHandle::for_account("test"),
        options(),
        config(),
    )
    .await
    .unwrap();
    (session, peer)
}
fn reply(id: RequestId) -> Frame {
    Frame::Response {
        id,
        body: ResponseBody::Result(json!({"ok":true})),
        fields: Default::default(),
    }
}
fn entry(cursor: u64, at: u64, payload: JournalPayload) -> JournalEntry {
    JournalEntry {
        cursor: EventCursor(cursor),
        generation: Generation(1),
        received_at_ms: at,
        payload,
    }
}
fn written(sender: oneshot::Sender<CommandReceipt>) {
    assert!(
        sender
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false
            })
            .is_ok()
    );
}
async fn write(peer: &mut Peer) -> oneshot::Sender<CommandReceipt> {
    match peer.commands.recv().await.unwrap() {
        Observed::Write(reply) => reply,
        Observed::Ack(_, _) => panic!("unexpected ACK"),
    }
}
async fn status(session: &mut SessionHandle, target: InitializationStatus) {
    loop {
        if session.status.borrow().initialization == target {
            return;
        }
        session.status.changed().await.unwrap();
    }
}
async fn ready(session: &mut SessionHandle, peer: &mut Peer) {
    let init = write(peer).await;
    assert!(
        init.send(CommandReceipt::CachedInitialize(json!({})))
            .is_ok()
    );
    written(write(peer).await);
    status(session, InitializationStatus::Ready).await;
}
fn spec(id: u64, timeout_ms: u64) -> RequestSpec {
    RequestSpec {
        id: RequestId::Number(id),
        operation_id: OperationId(format!("request:{id}")),
        method: Method::ModelList,
        params: json!({}),
        thread_id: None,
        lightweight: false,
        timeout_ms,
    }
}
async fn pending<F: std::future::Future>(future: F) {
    let mut future = Box::pin(future);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx).is_pending()))
            .await
    );
}

#[cfg(unix)]
#[test]
fn exited_stdio_provider_drains_beyond_journal_capacity_through_session_settlement() {
    assert_exited_stdio_session_tail(false);
}

#[cfg(target_os = "linux")]
#[test]
fn partial_stdio_write_at_exit_preserves_unknown_outcome_and_final_session_frames() {
    assert_exited_stdio_session_tail(true);
}

#[cfg(unix)]
fn assert_exited_stdio_session_tail(partial_write: bool) {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    use studio_runtime_process::provider_stdio::StdioProviderTaskFactory;

    const HANG_GUARD: Duration = Duration::from_secs(10);
    // Exceed both the adapter lane and the session's unsettled horizon. Lost
    // ACK service cannot be hidden by buffering the whole tail before closing.
    const FINAL_FRAMES: usize = MAX_UNSETTLED + 2 * LANE_CAPACITY + 1;
    const BLOCKED_REQUEST_BYTES: usize = 512 * 1024;
    let root = tempfile::tempdir().unwrap();
    let release = root.path().join("release");
    // Short-lived fixture setup also works on macOS, where rustix omits mknodat.
    #[allow(clippy::disallowed_methods)]
    let fifo = std::process::Command::new("mkfifo")
        .args(["-m", "600"])
        .arg(&release)
        .status()
        .expect("create provider release FIFO");
    assert!(fifo.success());
    let read_one_byte = if partial_write {
        r#"dd bs=1 count=1 of=/dev/null 2>/dev/null
printf '{"method":"fixture/write-progress"}\n'"#
    } else {
        ""
    };
    let script = format!(
        r#"IFS= read -r initialize
printf '{{"id":"attachment-1:1","result":{{}}}}\n'
IFS= read -r initialized
{read_one_byte}
IFS= read -r release < release
index=0
while [ "$index" -lt {FINAL_FRAMES} ]; do
    printf '{{"method":"fixture/final","index":%s}}\n' "$index"
    index=$((index + 1))
done"#
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut session, interrupted_ticket) = runtime.block_on(async {
        tokio::time::timeout(HANG_GUARD, async {
            let factory = Arc::new(StdioProviderTaskFactory {
                deadline: Some(tokio::time::Instant::now() + HANG_GUARD),
                max_frame_bytes: 1024,
                max_output_bytes: None,
                stderr_log: None,
            });
            let mut opts = options();
            opts.launch = ProviderLaunchSpec::new(
                vec!["/bin/sh".into(), "-c".into(), script],
                BTreeMap::new(),
                root.path().to_path_buf(),
            );
            let mut cfg = config();
            cfg.timeout_ms = HANG_GUARD.as_millis() as u64;
            let mut session = SessionHandle::open(
                factory,
                ProviderHandle::for_account("stdio-session-tail"),
                opts,
                cfg,
            )
            .await
            .unwrap();
            status(&mut session, InitializationStatus::Ready).await;
            let initial = session.events.recv().await.unwrap().journal.unwrap();
            assert_eq!(initial.cursor, EventCursor(1));
            session.control.settled(initial.cursor).await.unwrap();
            let ticket = if partial_write {
                let mut request = spec(2, HANG_GUARD.as_millis() as u64);
                request.params = json!({"padding": "x".repeat(BLOCKED_REQUEST_BYTES)});
                let ticket = session
                    .control
                    .submit(session.control.prepare(request).await.unwrap())
                    .await
                    .unwrap();
                let progress = session.events.recv().await.unwrap().journal.unwrap();
                assert_eq!(progress.cursor, EventCursor(2));
                session.control.settled(progress.cursor).await.unwrap();
                Some(ticket)
            } else {
                None
            };
            (session, ticket)
        })
        .await
        .expect("stdio session handshake hang guard")
    });
    let pid = session
        .status
        .borrow()
        .attachment
        .info
        .as_ref()
        .unwrap()
        .pid;
    let pid = Pid::from_raw(pid as i32).unwrap();
    // No task runs while the real child writes its bounded tail and exits.
    // The next owner poll must select leader exit, with stdout still to drain.
    std::fs::write(&release, b"exit\n").unwrap();
    waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
    )
    .unwrap();

    runtime.block_on(async {
        tokio::time::timeout(HANG_GUARD, async {
            let mut retained = vec![];
            let mut outcomes = vec![];
            while let Some(event) = session.events.recv().await {
                outcomes.extend(event.outcomes);
                if let Some(entry) = event.journal {
                    let cursor = entry.cursor;
                    retained.push(entry);
                    // A terminal close can race the final settlement. Completion
                    // owns its remaining journal evidence, as in the account caller.
                    match session.control.settled(cursor).await {
                        Ok(()) | Err(SessionTaskError::Closed) => {}
                        Err(error) => panic!("settlement failed: {error}"),
                    }
                }
            }
            let exit = session.completion().await;
            outcomes.extend(exit.outcomes);
            for event in exit.undelivered {
                outcomes.extend(event.outcomes);
                retained.extend(event.journal);
            }
            assert_eq!(
                retained.len(),
                FINAL_FRAMES,
                "every final frame is retained"
            );
            for (index, entry) in retained.into_iter().enumerate() {
                let initial_entries = if partial_write { 3 } else { 2 };
                assert_eq!(entry.cursor, EventCursor(index as u64 + initial_entries));
                assert_eq!(entry.generation, Generation(1));
                let JournalPayload::Frame(Frame::UnknownMethod(frame)) = entry.payload else {
                    panic!("expected final fixture notification");
                };
                assert_eq!(frame.fields.get("index"), Some(&json!(index)));
            }
            if let Some(ticket) = interrupted_ticket {
                let writes: Vec<_> = outcomes
                    .iter()
                    .filter_map(|outcome| match outcome {
                        SessionOutcome::Write {
                            ticket: Some(seen),
                            outcome,
                            ..
                        } if *seen == ticket => Some(*outcome),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    writes,
                    vec![WriteOutcome::Unknown],
                    "accepted stdin bytes must never be reported NotSent or replayed"
                );
            }
            let adapter = exit.adapter.expect("real stdio adapter completion");
            assert!(adapter.cleanup_error.is_none());
            assert!(matches!(
                adapter.result,
                Err(AdapterError::Supervisor(
                    studio_runtime_process::SupervisorError::ConnectionLost
                ))
            ));
            assert_eq!(
                waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                )
                .expect_err("the owned child was reaped"),
                rustix::io::Errno::CHILD,
            );
        })
        .await
        .expect("stdio session final drain hang guard");
    });
}

/// A requested shutdown keeps the provider's whole exit tail even when the
/// account has stopped consuming events and every bounded lane is full. In the
/// live incident streamed deltas filled the lanes and the bridge's final
/// `turn/completed`, written on stdin EOF, was lost behind them.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_keeps_the_stdout_tail_beyond_full_lanes_in_order() {
    use studio_runtime_process::provider_stdio::StdioProviderTaskFactory;

    const HANG_GUARD: Duration = Duration::from_secs(10);
    // Exceed the session's unsettled horizon and both bounded lanes.
    const DELTAS: usize = MAX_UNSETTLED + 2 * LANE_CAPACITY + 1;
    let root = tempfile::tempdir().unwrap();
    let script = format!(
        r#"IFS= read -r initialize
printf '{{"id":"attachment-1:1","result":{{}}}}\n'
IFS= read -r initialized
index=0
while [ "$index" -lt {DELTAS} ]; do
    printf '{{"method":"fixture/delta","index":%s}}\n' "$index"
    index=$((index + 1))
done
cat >/dev/null
printf '{{"method":"fixture/completed"}}\n'"#
    );
    tokio::time::timeout(HANG_GUARD, async {
        let factory = Arc::new(StdioProviderTaskFactory {
            deadline: None,
            max_frame_bytes: 1024,
            max_output_bytes: None,
            stderr_log: None,
        });
        let mut opts = options();
        opts.launch = ProviderLaunchSpec::new(
            vec!["/bin/sh".into(), "-c".into(), script],
            BTreeMap::new(),
            root.path().to_path_buf(),
        );
        let mut cfg = config();
        cfg.timeout_ms = HANG_GUARD.as_millis() as u64;
        let mut session = SessionHandle::open(
            factory,
            ProviderHandle::for_account("stdio-shutdown-tail"),
            opts,
            cfg,
        )
        .await
        .unwrap();
        status(&mut session, InitializationStatus::Ready).await;
        // Nothing consumes or settles events, like the stopped account worker.
        let exit = session.shutdown().await;
        let mut retained = vec![];
        while let Ok(event) = session.events.try_recv() {
            retained.extend(event.journal);
        }
        for event in exit.undelivered {
            retained.extend(event.journal);
        }
        assert_eq!(retained.len(), DELTAS + 2, "every frame is retained");
        for (index, entry) in retained.iter().enumerate() {
            assert_eq!(entry.cursor, EventCursor(index as u64 + 1));
        }
        for (index, entry) in retained[1..=DELTAS].iter().enumerate() {
            let JournalPayload::Frame(Frame::UnknownMethod(frame)) = &entry.payload else {
                panic!("expected a delta fixture notification");
            };
            assert_eq!(frame.fields.get("index"), Some(&json!(index)));
        }
        let JournalPayload::Frame(Frame::UnknownMethod(last)) = &retained[DELTAS + 1].payload
        else {
            panic!("expected the completed fixture notification");
        };
        assert_eq!(last.method, "fixture/completed");
        let adapter = exit.adapter.expect("real stdio adapter completion");
        assert!(adapter.result.is_ok(), "{:?}", adapter.result);
        assert!(adapter.cleanup_error.is_none());
    })
    .await
    .expect("stdio shutdown tail hang guard");
}

#[tokio::test(start_paused = true)]
async fn initialization_requires_reply_and_written_notification_receipt() {
    let (mut session, mut peer) = open().await;
    assert!(session.control.prepare(spec(2, 100)).await.is_err());
    written(write(&mut peer).await);
    peer.events
        .send(entry(
            1,
            0,
            JournalPayload::Frame(reply(RequestId::String("attachment-1:1".into()))),
        ))
        .await
        .unwrap();
    let initialized = write(&mut peer).await;
    status(&mut session, InitializationStatus::InitializedWritePending).await;
    assert!(session.control.prepare(spec(2, 100)).await.is_err());
    written(initialized);
    status(&mut session, InitializationStatus::Ready).await;
    let event = session.events.recv().await.unwrap();
    assert_eq!(event.journal.unwrap().cursor, EventCursor(1));
    pending(peer.commands.recv()).await; // no implicit ACK of initialize
    let exit = session.checked_shutdown().await;
    assert!(exit.result.is_ok());
    assert!(exit.adapter.unwrap().recovery_needed.is_none());
    peer.gone.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn durable_settlement_forwards_only_delivered_contiguous_prefix() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    for cursor in 1..=3 {
        peer.events
            .send(entry(
                cursor,
                0,
                JournalPayload::Stderr(json!({"text":"stderr"})),
            ))
            .await
            .unwrap();
        assert_eq!(
            session.events.recv().await.unwrap().journal.unwrap().cursor,
            EventCursor(cursor)
        );
    }
    assert!(matches!(
        session.control.settled(EventCursor(4)).await,
        Err(SessionTaskError::UndeliveredCursor)
    ));
    session.control.settled(EventCursor(3)).await.unwrap();
    pending(peer.commands.recv()).await;
    session.control.settled(EventCursor(1)).await.unwrap();
    let Observed::Ack(cursor, receipt) = peer.commands.recv().await.unwrap() else {
        panic!("expected ack")
    };
    assert_eq!(cursor, EventCursor(1));
    receipt
        .send(CommandReceipt::Acknowledged(cursor))
        .ok()
        .unwrap();
    session.control.settled(EventCursor(2)).await.unwrap();
    let Observed::Ack(cursor, receipt) = peer.commands.recv().await.unwrap() else {
        panic!("expected ack")
    };
    assert_eq!(cursor, EventCursor(3));
    receipt
        .send(CommandReceipt::Acknowledged(cursor))
        .ok()
        .unwrap();
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn every_journal_kind_and_previous_generation_reaches_consumer() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let raw = studio_runtime_providers::decode_journal_entry(0, 1, b"not-json\n".to_vec());
    let studio_runtime_providers::DecodedJournalPayload::RawStdout { bytes, error } = raw.payload
    else {
        panic!()
    };
    let entries = [
        JournalPayload::Frame(reply(RequestId::Number(999))),
        JournalPayload::RawStdout { bytes, error },
        JournalPayload::Stderr(json!({})),
        JournalPayload::Exit(json!({})),
        JournalPayload::Other {
            kind: "future".into(),
            payload: json!({}),
        },
    ];
    for (index, payload) in entries.into_iter().enumerate() {
        let mut journal = entry(index as u64 + 1, 0, payload);
        journal.generation = Generation(0);
        peer.events.send(journal).await.unwrap();
        let event = session.events.recv().await.unwrap();
        let entry = event.journal.unwrap();
        assert_eq!(entry.cursor.0, index as u64 + 1);
        assert_eq!(entry.generation, Generation(0));
        assert!(event.outcomes.is_empty());
        if index == 1 {
            assert!(
                matches!(entry.payload, JournalPayload::RawStdout { bytes, .. } if bytes == b"not-json\n")
            );
        }
    }
    pending(peer.commands.recv()).await;
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn recovered_deadline_emits_without_any_further_input() {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.timeout_ms = 100;
    cfg.recovered
        .push(recovered(RequestId::Number(42), Method::ModelList, 10));
    let mut session =
        SessionHandle::open(factory, ProviderHandle::for_account("test"), options(), cfg)
            .await
            .unwrap();
    ready(&mut session, &mut peer).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    let event = session.events.recv().await.unwrap();
    assert!(event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { ticket, outcome: RequestOutcome::Unknown } if ticket.id == RequestId::Number(42))));
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_write_does_not_block_deadline_or_shutdown_barrier() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let ticket = session
        .control
        .submit(session.control.prepare(spec(2, 10)).await.unwrap())
        .await
        .unwrap();
    let _unanswered = write(&mut peer).await;
    tokio::time::advance(Duration::from_millis(10)).await;
    let event = session.events.recv().await.unwrap();
    assert!(event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { ticket: seen, outcome: RequestOutcome::Unknown } if *seen == ticket)));
    let exit = session.checked_shutdown().await;
    peer.gone.await.unwrap();
    assert!(exit.outcomes.iter().any(|outcome| matches!(
        outcome,
        SessionOutcome::Write {
            outcome: WriteOutcome::Unknown,
            ..
        }
    )));
}

#[tokio::test(start_paused = true)]
async fn predeadline_arrival_admitted_after_deadline_is_on_time_and_mirror_is_late() {
    for (arrived, expected_late) in [(9, false), (11, true)] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        session
            .control
            .submit(session.control.prepare(spec(2, 10)).await.unwrap())
            .await
            .unwrap();
        written(write(&mut peer).await);
        session.events.recv().await.unwrap(); // Written
        tokio::time::advance(Duration::from_millis(10)).await;
        session.events.recv().await.unwrap(); // Unknown from the independent timer
        peer.events
            .send(entry(
                1,
                arrived,
                JournalPayload::Frame(reply(RequestId::Number(2))),
            ))
            .await
            .unwrap();
        let event = session.events.recv().await.unwrap();
        assert!(event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { outcome: RequestOutcome::Reply { late, .. }, .. } if *late == expected_late)));
        session.checked_shutdown().await;
    }
}

struct CountingFactory(AtomicUsize);
impl ProviderTaskFactory for CountingFactory {
    fn open(&self, _: ProviderHandle, _: OpenOptions, _: Shutdown) -> OpenFuture<'_> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(AdapterError::InvalidConfiguration) })
    }
}
#[tokio::test(start_paused = true)]
async fn constructor_validates_recovered_and_initialize_before_factory_open() {
    let factory = Arc::new(CountingFactory(AtomicUsize::new(0)));
    for case in 0..5 {
        let mut cfg = config();
        let mut opts = options();
        if case == 0 {
            cfg.initialize_params = json!({"large":"x".repeat(MAX_SUPERVISOR_FRAME_BYTES)});
        } else if case == 1 {
            cfg.recovered = vec![recovered(RequestId::Number(8), Method::ModelList, 10); 2];
        } else if case == 2 {
            cfg.recovered = vec![recovered(RequestId::Number(8), Method::Initialize, 10)];
        } else if case == 3 {
            cfg.recovered = vec![recovered(
                RequestId::String("x".repeat(1025)),
                Method::ModelList,
                10,
            )];
        } else {
            opts.expected_generation = Some(Generation(2));
        }
        assert!(
            SessionHandle::open(
                factory.clone(),
                ProviderHandle::for_account("test"),
                opts,
                cfg
            )
            .await
            .is_err()
        );
    }
    assert_eq!(factory.0.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn bounded_startup_replay_can_initialize_and_stop_with_full_consumer() {
    let (mut session, mut peer) = open().await;
    let init = write(&mut peer).await;
    for cursor in 1..=(LANE_CAPACITY * 2 + 1) as u64 {
        peer.events
            .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
    }
    // Exactly 16 consumer slots + one retained journal + 16 adapter slots.
    pending(
        peer.events
            .send(entry(100, 0, JournalPayload::Stderr(json!({})))),
    )
    .await;
    assert_eq!(session.events.len(), LANE_CAPACITY);
    assert_eq!(peer.events.capacity(), 0);
    init.send(CommandReceipt::CachedInitialize(json!({})))
        .ok()
        .unwrap();
    written(write(&mut peer).await);
    status(&mut session, InitializationStatus::Ready).await;
    assert!(matches!(
        session.control.settled(EventCursor(17)).await,
        Err(SessionTaskError::UndeliveredCursor)
    ));
    let exit = session.checked_shutdown().await;
    peer.gone.await.unwrap();
    assert_eq!(exit.undelivered.len(), 17);
    assert_eq!(
        exit.undelivered[0].journal.as_ref().unwrap().cursor,
        EventCursor(17)
    );
    assert!(exit.adapter.unwrap().recovery_needed.is_none());
}

#[tokio::test(start_paused = true)]
async fn interrupted_initialization_keeps_journal_and_ack_service_alive() {
    for initialized in [false, true] {
        for bytes_written in [0, 1] {
            let (mut session, mut peer) = open().await;
            let mut receipt = write(&mut peer).await;
            if initialized {
                receipt
                    .send(CommandReceipt::CachedInitialize(json!({})))
                    .ok()
                    .unwrap();
                receipt = write(&mut peer).await;
            }
            receipt
                .send(CommandReceipt::WriteInterrupted { bytes_written })
                .ok()
                .unwrap();
            peer.events
                .send(entry(1, 0, JournalPayload::Stderr(json!({"tail":true}))))
                .await
                .unwrap();
            let event = session.events.recv().await.unwrap();
            assert_eq!(event.journal.unwrap().cursor, EventCursor(1));
            session.control.settled(EventCursor(1)).await.unwrap();
            let Observed::Ack(cursor, reply) = peer.commands.recv().await.unwrap() else {
                panic!("interrupted initialization must not replay");
            };
            reply
                .send(CommandReceipt::Acknowledged(cursor))
                .ok()
                .unwrap();
            session.checked_shutdown().await;
            assert!(peer.commands.recv().await.is_none());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn interrupted_write_receipts_keep_session_settlement_alive_without_replay() {
    for (bytes_written, expected) in [(0, WriteOutcome::NotSent), (1, WriteOutcome::Unknown)] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        let ticket = session
            .control
            .submit(session.control.prepare(spec(2, 100)).await.unwrap())
            .await
            .unwrap();
        write(&mut peer)
            .await
            .send(CommandReceipt::WriteInterrupted { bytes_written })
            .ok()
            .unwrap();
        let event = session.events.recv().await.unwrap();
        assert_eq!(event.outcomes.iter().filter(|outcome| matches!(outcome,
            SessionOutcome::Write { ticket: Some(seen), outcome, .. } if *seen == ticket && *outcome == expected
        )).count(), 1);
        peer.events
            .send(entry(1, 0, JournalPayload::Stderr(json!({"tail":true}))))
            .await
            .unwrap();
        let event = session.events.recv().await.unwrap();
        assert_eq!(event.journal.unwrap().cursor, EventCursor(1));
        session.control.settled(EventCursor(1)).await.unwrap();
        let Observed::Ack(cursor, reply) = peer.commands.recv().await.unwrap() else {
            panic!("interrupted writes must not be replayed");
        };
        reply
            .send(CommandReceipt::Acknowledged(cursor))
            .ok()
            .unwrap();
        session.checked_shutdown().await;
        assert!(peer.commands.recv().await.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn unknown_and_not_sent_write_receipts_are_distinct_and_never_retried() {
    for rejected in [false, true] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        let ticket = session
            .control
            .submit(session.control.prepare(spec(2, 100)).await.unwrap())
            .await
            .unwrap();
        let receipt = write(&mut peer).await;
        if rejected {
            receipt.send(CommandReceipt::NotSent).ok().unwrap();
        } else {
            drop(receipt);
        }
        let mut delivered = vec![];
        let exit = if rejected {
            delivered = session.events.recv().await.unwrap().outcomes;
            assert!(session.control.prepare(spec(3, 100)).await.is_ok());
            session.checked_shutdown().await
        } else {
            session.checked_completion().await
        };
        if !rejected {
            assert!(exit.result.is_err());
        }
        peer.gone.await.unwrap();
        let outcomes: Vec<_> = delivered
            .into_iter()
            .chain(exit.outcomes.into_iter())
            .chain(
                exit.undelivered
                    .into_iter()
                    .flat_map(|event| event.outcomes),
            )
            .collect();
        let expected = if rejected {
            WriteOutcome::NotSent
        } else {
            WriteOutcome::Unknown
        };
        assert_eq!(outcomes.iter().filter(|outcome| matches!(outcome, SessionOutcome::Write { ticket: Some(seen), outcome, .. } if *seen == ticket && *outcome == expected)).count(), 1);
        if rejected {
            assert!(!outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { ticket: seen, outcome: RequestOutcome::Unknown } if *seen == ticket)));
        }
        assert!(peer.commands.recv().await.is_none());
        assert!(session.control.prepare(spec(3, 100)).await.is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn callbacks_do_not_register_or_steal_outgoing_request_correlation() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let ticket = session
        .control
        .submit(session.control.prepare(spec(2, 100)).await.unwrap())
        .await
        .unwrap();
    written(write(&mut peer).await);
    session.events.recv().await.unwrap();
    let callback = session
        .control
        .prepare_frame(
            OperationId("callback:2".into()),
            &reply(RequestId::Number(2)),
        )
        .unwrap();
    session.control.submit_frame(callback).await.unwrap();
    written(write(&mut peer).await);
    let callback_receipt = session.events.recv().await.unwrap();
    assert!(matches!(
        &callback_receipt.outcomes[0],
        SessionOutcome::Write {
            ticket: None,
            outcome: WriteOutcome::Written,
            ..
        }
    ));
    peer.events
        .send(entry(1, 0, JournalPayload::Frame(reply(ticket.id.clone()))))
        .await
        .unwrap();
    let event = session.events.recv().await.unwrap();
    assert!(
        matches!(&event.outcomes[0], SessionOutcome::Resolved { ticket: seen, outcome: RequestOutcome::Reply { .. } } if *seen == ticket)
    );
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn incomplete_initialization_shutdown_surfaces_recovery_after_adapter_join() {
    let (mut session, mut peer) = open().await;
    let _unanswered = write(&mut peer).await;
    let exit = session.checked_shutdown().await;
    peer.gone.await.unwrap();
    let recovery = exit.adapter.unwrap().recovery_needed.unwrap();
    assert_eq!(recovery.handle, ProviderHandle::for_account("test"));
    assert_eq!(recovery.generation, Some(Generation(1)));
}

#[tokio::test(start_paused = true)]
async fn before_at_and_after_deadline_arrivals_keep_classification_under_full_backlog() {
    for (arrived, expected_late) in [(9, false), (10, true), (11, true)] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        session
            .control
            .submit(session.control.prepare(spec(2, 10)).await.unwrap())
            .await
            .unwrap();
        written(write(&mut peer).await);
        session.events.recv().await.unwrap();
        // Fill the consumer and the one pending transition before advancing time.
        for cursor in 1..=(LANE_CAPACITY * 2) as u64 {
            peer.events
                .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
                .await
                .unwrap();
        }
        tokio::time::advance(Duration::from_millis(arrived)).await;
        peer.events
            .send(entry(
                33,
                arrived,
                JournalPayload::Frame(reply(RequestId::Number(2))),
            ))
            .await
            .unwrap();
        pending(
            peer.events
                .send(entry(34, arrived, JournalPayload::Stderr(json!({})))),
        )
        .await;
        tokio::time::advance(Duration::from_millis(20 - arrived)).await;
        let response = loop {
            let event = session.events.recv().await.unwrap();
            if event
                .journal
                .as_ref()
                .is_some_and(|entry| entry.cursor == EventCursor(33))
            {
                break event;
            }
        };
        assert!(response.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { outcome: RequestOutcome::Reply { late, .. }, .. } if *late == expected_late)));
        session.checked_shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn unsettled_journal_horizon_is_bounded_until_durable_prefix_advances() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    for cursor in 1..=MAX_UNSETTLED as u64 {
        peer.events
            .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
        session.events.recv().await.unwrap();
    }
    for cursor in MAX_UNSETTLED as u64 + 1..=(MAX_UNSETTLED + LANE_CAPACITY) as u64 {
        peer.events
            .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
    }
    pending(
        peer.events
            .send(entry(1000, 0, JournalPayload::Stderr(json!({})))),
    )
    .await;
    assert!(session.events.is_empty());
    session.control.settled(EventCursor(1)).await.unwrap();
    let Observed::Ack(cursor, receipt) = peer.commands.recv().await.unwrap() else {
        panic!("expected ack")
    };
    receipt
        .send(CommandReceipt::Acknowledged(cursor))
        .ok()
        .unwrap();
    assert_eq!(
        session.events.recv().await.unwrap().journal.unwrap().cursor,
        EventCursor(65)
    );
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn exited_generation_entries_do_not_hold_back_the_initialize_reply() {
    let (mut session, mut peer) = open().await;
    written(write(&mut peer).await);
    // A full unsettled bound of an exited generation's entries, none settled.
    for cursor in 1..=MAX_UNSETTLED as u64 {
        let mut exited = entry(cursor, 0, JournalPayload::Stderr(json!({})));
        exited.generation = Generation(0);
        peer.events.send(exited).await.unwrap();
        session.events.recv().await.unwrap();
    }
    peer.events
        .send(entry(
            MAX_UNSETTLED as u64 + 1,
            0,
            JournalPayload::Frame(reply(RequestId::String("attachment-1:1".into()))),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        written(write(&mut peer).await);
        status(&mut session, InitializationStatus::Ready).await;
    })
    .await
    .expect("the initialize reply is read behind the exited generation's entries");
    session.checked_shutdown().await;
}

#[cfg(target_os = "linux")]
use crate::process_support as support;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn disposable_supervisor_session_preserves_replay_and_settlement_across_reattach() {
    use support::*;
    let root = DisposableState::new();
    // Startup completes by handshake, before the session's clock/guard starts.
    let mut supervisor = SupervisorProcess::start(&root);
    tokio::time::timeout(HANG_GUARD, async {
        let script = r#"
while IFS= read -r line; do
 id=$(rpc_id "$line")
 case "$line" in
  *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id";;
  *'"method":"initialized"'*)
   printf 'raw stdout\n'
   printf 'stderr entry\n' >&2
   printf '{"method":"observed/initialized"}\n';;
  *'"method":"model/list"'*) printf '{"id":%s,"result":{"models":[]}}\n' "$id";;
 esac
done
"#;
        let launch = ProviderLaunchSpec::new(shell_command(script), support::child_environment(&root.0), root.0.clone());
        let factory: Arc<dyn ProviderTaskFactory> = Arc::new(studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(root.identity()));
        let mut opts = options();
        opts.launch = launch.clone();
        let mut cfg = config();
        cfg.timeout_ms = HANG_GUARD.as_millis() as u64;
        let mut first = SessionHandle::open(factory.clone(), ProviderHandle::for_account("test"), opts, cfg).await.unwrap();
        status(&mut first, InitializationStatus::Ready).await;
        supervisor.own_child(first.status.borrow().attachment.info.as_ref().unwrap().pid);
        let initial = first.events.recv().await.unwrap().journal.unwrap();
        assert_eq!(initial.cursor, EventCursor(1));
        first.control.settled(initial.cursor).await.unwrap();
        loop {
            if first.status.borrow().attachment.info.as_ref().unwrap().acknowledged == initial.cursor { break; }
            first.status.changed().await.unwrap();
        }
        let ticket = first.control.submit(first.control.prepare(spec(2, HANG_GUARD.as_millis() as u64)).await.unwrap()).await.unwrap();
        let mut raw = false;
        let mut stderr = false;
        let mut responded = false;
        let mut replay_cursor = None;
        while !(raw && stderr && responded) {
            let event = first.events.recv().await.unwrap();
            if let Some(entry) = &event.journal {
                replay_cursor.get_or_insert(entry.cursor);
                raw |= matches!(entry.payload, JournalPayload::RawStdout { .. });
                stderr |= matches!(entry.payload, JournalPayload::Stderr(_));
            }
            responded |= event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { ticket: seen, outcome: RequestOutcome::Reply { .. } } if *seen == ticket));
        }
        assert_eq!(first.status.borrow().attachment.info.as_ref().unwrap().acknowledged, EventCursor(1));
        let pid = first.status.borrow().attachment.info.as_ref().unwrap().pid;
        assert!(first.checked_shutdown().await.adapter.unwrap().recovery_needed.is_none());
        let mut opts = options();
        opts.launch = launch;
        opts.launch.handled_cursor = 1;
        opts.expected_generation = Some(Generation(1));
        let mut cfg = config();
        cfg.request_id_prefix = Some("attachment-2".into());
        cfg.initialize_operation = OperationId("init:attachment-2".into());
        cfg.timeout_ms = HANG_GUARD.as_millis() as u64;
        let mut second = SessionHandle::open(factory, ProviderHandle::for_account("test"), opts, cfg).await.unwrap();
        status(&mut second, InitializationStatus::Ready).await;
        assert_eq!(second.status.borrow().attachment.info.as_ref().unwrap().pid, pid);
        let replay = second.events.recv().await.unwrap().journal.unwrap();
        assert_eq!(Some(replay.cursor), replay_cursor);
        assert_eq!(replay.generation, Generation(1));
        assert!(second.checked_shutdown().await.adapter.unwrap().recovery_needed.is_none());
    }).await.expect("disposable supervisor session exceeded local watchdog");
}

async fn claude_ready() -> (SessionHandle, Peer) {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.wire = studio_runtime_providers::ProviderKind::Claude.wire();
    let mut session =
        SessionHandle::open(factory, ProviderHandle::for_account("test"), options(), cfg)
            .await
            .unwrap();
    ready(&mut session, &mut peer).await;
    (session, peer)
}
fn serialized(id: u64, timeout_ms: u64) -> RequestSpec {
    RequestSpec {
        method: Method::TurnStart,
        thread_id: Some("thread".into()),
        ..spec(id, timeout_ms)
    }
}
#[tokio::test(start_paused = true)]
async fn claude_prepare_rejects_missing_thread_before_durable_submission() {
    let (mut session, mut peer) = claude_ready().await;
    let mut invalid = serialized(2, 100);
    invalid.thread_id = None;
    assert!(matches!(
        session.control.prepare(invalid).await,
        Err(SessionTaskError::Protocol(
            studio_runtime_providers::SessionError::MissingThreadId
        ))
    ));
    pending(peer.commands.recv()).await;
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn expired_serialized_request_is_not_sent_and_next_request_is_released() {
    let (mut session, mut peer) = claude_ready().await;
    let first = session
        .control
        .submit(session.control.prepare(serialized(2, 100)).await.unwrap())
        .await
        .unwrap();
    written(write(&mut peer).await);
    session.events.recv().await.unwrap();
    let second = session
        .control
        .submit(session.control.prepare(serialized(3, 5)).await.unwrap())
        .await
        .unwrap();
    let third = session
        .control
        .submit(session.control.prepare(serialized(4, 100)).await.unwrap())
        .await
        .unwrap();
    pending(peer.commands.recv()).await;
    tokio::time::advance(Duration::from_millis(5)).await;
    let expired = session.events.recv().await.unwrap();
    assert!(
        matches!(&expired.outcomes[0], SessionOutcome::Write { ticket: Some(ticket), outcome: WriteOutcome::NotSent, .. } if *ticket == second)
    );
    peer.events
        .send(entry(1, 5, JournalPayload::Frame(reply(first.id))))
        .await
        .unwrap();
    written(write(&mut peer).await);
    loop {
        let event = session.events.recv().await.unwrap();
        if event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Write { ticket: Some(ticket), outcome: WriteOutcome::Written, .. } if *ticket == third)) { break; }
    }
    pending(peer.commands.recv()).await;
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn expiry_batch_preserves_all_queued_request_outcomes_in_order() {
    // One expiry transition holds this entire batch; the running predecessor
    // prevents queued writes until the session retires their expired tickets.
    const EXPIRY_BATCH_REQUESTS: u64 = 128;
    const QUEUED_TIMEOUT_MS: u64 = 5;
    const RUNNING_TIMEOUT_MS: u64 = 100;
    let (mut session, mut peer) = claude_ready().await;
    let first = session
        .control
        .submit(
            session
                .control
                .prepare(serialized(2, RUNNING_TIMEOUT_MS))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    written(write(&mut peer).await);
    session.events.recv().await.unwrap();
    let mut tickets = Vec::new();
    for id in 3..3 + EXPIRY_BATCH_REQUESTS {
        tickets.push(
            session
                .control
                .submit(
                    session
                        .control
                        .prepare(serialized(id, QUEUED_TIMEOUT_MS))
                        .await
                        .unwrap(),
                )
                .await
                .unwrap(),
        );
    }
    let successor = session
        .control
        .submit(
            session
                .control
                .prepare(serialized(3 + EXPIRY_BATCH_REQUESTS, RUNNING_TIMEOUT_MS))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    pending(peer.commands.recv()).await;
    tokio::time::advance(Duration::from_millis(QUEUED_TIMEOUT_MS)).await;
    let expired = session.events.recv().await.unwrap();
    assert_eq!(expired.outcomes.len(), tickets.len());
    for (outcome, ticket) in expired.outcomes.iter().zip(&tickets) {
        assert!(matches!(outcome, SessionOutcome::Write {
            ticket: Some(actual), outcome: WriteOutcome::NotSent, ..
        } if actual == ticket));
    }
    pending(peer.commands.recv()).await;
    peer.events
        .send(entry(
            1,
            QUEUED_TIMEOUT_MS,
            JournalPayload::Frame(reply(first.id)),
        ))
        .await
        .unwrap();
    written(write(&mut peer).await);
    loop {
        let event = session.events.recv().await.unwrap();
        if event.outcomes.iter().any(|outcome| {
            matches!(outcome, SessionOutcome::Write {
            ticket: Some(ticket), outcome: WriteOutcome::Written, ..
        } if *ticket == successor)
        }) {
            break;
        }
    }
    pending(peer.commands.recv()).await;
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn definitive_not_sent_releases_serialized_successor_without_closing_session() {
    let (mut session, mut peer) = claude_ready().await;
    let first = session
        .control
        .submit(session.control.prepare(serialized(2, 100)).await.unwrap())
        .await
        .unwrap();
    let first_receipt = write(&mut peer).await;
    let second = session
        .control
        .submit(session.control.prepare(serialized(3, 100)).await.unwrap())
        .await
        .unwrap();
    first_receipt.send(CommandReceipt::NotSent).ok().unwrap();
    written(write(&mut peer).await);
    let mut not_sent = false;
    let mut sent = false;
    while !(not_sent && sent) {
        let event = session.events.recv().await.unwrap();
        not_sent |= event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Write { ticket: Some(ticket), outcome: WriteOutcome::NotSent, .. } if *ticket == first));
        sent |= event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Write { ticket: Some(ticket), outcome: WriteOutcome::Written, .. } if *ticket == second));
    }
    assert!(session.control.prepare(serialized(4, 100)).await.is_ok());
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn full_command_and_event_lanes_do_not_block_cancellation_or_join() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let control = session.control.clone();
    let mut prepared = vec![];
    for id in 2..LANE_CAPACITY as u64 + 3 {
        prepared.push(control.prepare(spec(id, 100)).await.unwrap());
    }
    for cursor in 1..=(LANE_CAPACITY * 2 + 1) as u64 {
        peer.events
            .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
    }
    pending(
        peer.events
            .send(entry(100, 0, JournalPayload::Stderr(json!({})))),
    )
    .await;
    let mut submitted = vec![];
    for request in prepared {
        let mut submit = Box::pin(control.submit(request));
        pending(submit.as_mut()).await;
        submitted.push(submit);
    }
    control.cancel();
    session.checked_shutdown().await;
    peer.gone.await.unwrap();
    for submit in submitted {
        assert!(submit.await.is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn cancelled_completion_wait_retains_adapter_stop_barrier() {
    let (release, barrier) = oneshot::channel();
    let (factory, mut peer) = fake_with_stop_barrier(Some(barrier));
    let mut session = SessionHandle::open(
        factory,
        ProviderHandle::for_account("test"),
        options(),
        config(),
    )
    .await
    .unwrap();
    ready(&mut session, &mut peer).await;
    session.control.cancel();
    peer.stopping.await.unwrap();
    pending(session.completion()).await;
    release.send(()).unwrap();
    let exit = session.checked_shutdown().await;
    peer.gone.await.unwrap();
    assert!(exit.adapter.unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn prepared_request_cannot_cross_attachment_owners() {
    let (mut first, mut peer_first) = open().await;
    ready(&mut first, &mut peer_first).await;
    let (mut second, mut peer_second) = open().await;
    ready(&mut second, &mut peer_second).await;
    let prepared = first.control.prepare(spec(2, 100)).await.unwrap();
    assert!(matches!(
        second.control.submit(prepared).await,
        Err(SessionTaskError::Invalid)
    ));
    pending(peer_second.commands.recv()).await;
    first.checked_shutdown().await;
    second.checked_shutdown().await;
}

struct OpenGuard(Option<oneshot::Sender<()>>);
impl Drop for OpenGuard {
    fn drop(&mut self) {
        if let Some(dropped) = self.0.take() {
            let _ignored = dropped.send(());
        }
    }
}
struct PendingFactory(Mutex<Option<(oneshot::Sender<()>, OpenGuard)>>);
impl ProviderTaskFactory for PendingFactory {
    fn open(&self, _: ProviderHandle, _: OpenOptions, _: Shutdown) -> OpenFuture<'_> {
        let (entered, guard) = self.0.lock().unwrap().take().unwrap();
        Box::pin(async move {
            let _guard = guard;
            entered.send(()).unwrap();
            std::future::pending().await
        })
    }
}
#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_factory_open_before_attachment_exists() {
    let (entered, barrier) = oneshot::channel();
    let (dropped, cleaned) = oneshot::channel();
    let factory = Arc::new(PendingFactory(Mutex::new(Some((
        entered,
        OpenGuard(Some(dropped)),
    )))));
    let mut session = SessionHandle::open(
        factory,
        ProviderHandle::for_account("test"),
        options(),
        config(),
    )
    .await
    .unwrap();
    barrier.await.unwrap();
    let exit = session.checked_shutdown().await;
    cleaned.await.unwrap();
    assert!(exit.result.is_ok());
    assert!(exit.adapter.is_none());
}

/// A request the provider still owes a reply, even one whose outcome is
/// Unknown after its deadline, never holds back a new preparation.
#[tokio::test(start_paused = true)]
async fn unresolved_requests_never_hold_back_a_preparation() {
    /// More unresolved requests than any former per-session window.
    const UNRESOLVED: u64 = 100;
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    for id in 2..UNRESOLVED + 2 {
        let request = session.control.prepare(spec(id, 100)).await.unwrap();
        session.control.submit(request).await.unwrap();
        written(write(&mut peer).await);
        session.events.recv().await.unwrap();
    }
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(
        session.events.recv().await.unwrap().outcomes.len(),
        UNRESOLVED as usize
    );
    session.control.prepare(spec(1000, 100)).await.unwrap();
    session.checked_shutdown().await;
}

fn recovered(id: RequestId, method: Method, deadline_ms: u64) -> RecoveredRequest {
    RecoveredRequest {
        ticket: studio_runtime_providers::RequestTicket {
            generation: 1,
            id,
            method,
        },
        operation_id: OperationId("recovered".into()),
        thread_id: None,
        lightweight: false,
        deadline_ms,
        unknown: false,
        remote_id: None,
    }
}
#[tokio::test(start_paused = true)]
async fn review_shutdown_retains_adapter_buffered_journal() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    for cursor in 1..=16 {
        peer.events
            .try_send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .unwrap();
    }
    let exit = session.checked_shutdown().await;
    let returned = exit
        .undelivered
        .iter()
        .filter(|event| event.journal.is_some())
        .count();
    let mut delivered = 0;
    while let Ok(event) = session.events.try_recv() {
        delivered += usize::from(event.journal.is_some());
    }
    assert_eq!(
        returned + delivered,
        16,
        "shutdown discarded buffered journal envelopes"
    );
}

/// A request reply that only reaches the shutdown tail still resolves its
/// request, and the exit does not also report the request as unknown. Before
/// this, the tail's `turn/started` was applied while its `turn/start` reply
/// was not: the start was neither accepted nor unknown, and the restart that
/// followed left the in-flight turn interrupted without auto-wake.
#[tokio::test(start_paused = true)]
async fn shutdown_tail_reply_resolves_its_request_instead_of_unknown() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let ticket = session
        .control
        .submit(session.control.prepare(spec(2, 60_000)).await.unwrap())
        .await
        .unwrap();
    written(write(&mut peer).await);
    session.events.recv().await.unwrap();
    // The consumer stopped reading: the session lane and its pending slot
    // fill, so the reply stays in the attachment journal until shutdown.
    for cursor in 1..=(LANE_CAPACITY as u64 + 1) {
        peer.events
            .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
    }
    let reply_cursor = LANE_CAPACITY as u64 + 2;
    peer.events
        .send(entry(
            reply_cursor,
            0,
            JournalPayload::Frame(reply(ticket.id.clone())),
        ))
        .await
        .unwrap();
    let exit = session.checked_shutdown().await;
    let tail = exit
        .undelivered
        .iter()
        .find(|event| {
            event
                .journal
                .as_ref()
                .is_some_and(|journal| journal.cursor == EventCursor(reply_cursor))
        })
        .expect("the reply is part of the shutdown tail");
    assert!(tail.outcomes.iter().any(|outcome| matches!(
        outcome,
        SessionOutcome::Resolved { ticket: seen, outcome: RequestOutcome::Reply { late: false, .. } }
            if *seen == ticket
    )));
    assert!(!exit.outcomes.iter().any(|outcome| matches!(
        outcome,
        SessionOutcome::Resolved { ticket: seen, .. } if *seen == ticket
    )));
}

#[tokio::test(start_paused = true)]
async fn review_dropped_buffered_submission_fences_session() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let ctl = session.control.clone();
    let prepared = ctl.prepare(spec(2, 100)).await.unwrap();
    let mut submit = Box::pin(ctl.submit(prepared));
    pending(submit.as_mut()).await;
    let first = write(&mut peer).await; // admission reply has been buffered
    drop(submit);
    written(first);
    let prepared = ctl.prepare(spec(3, 100)).await;
    let admitted_after_abandonment = prepared.is_ok();
    let exit = session.checked_shutdown().await;
    let mut outcomes = exit.outcomes;
    for event in exit.undelivered {
        outcomes.extend(event.outcomes);
    }
    while let Ok(event) = session.events.try_recv() {
        outcomes.extend(event.outcomes);
    }
    assert!(outcomes.iter().any(|o| matches!(o, SessionOutcome::Write { ticket: Some(t), .. } if t.id == RequestId::Number(2))));
    assert!(
        !admitted_after_abandonment,
        "dropping an unobserved submit result leaves admission live"
    );
}

#[tokio::test(start_paused = true)]
async fn review_full_callback_queue_retains_taken_response_and_resolution() {
    let (mut session, mut peer) = claude_ready().await;
    let ctl = session.control.clone();
    let first = ctl
        .submit(ctl.prepare(serialized(2, 1000)).await.unwrap())
        .await
        .unwrap();
    written(write(&mut peer).await);
    session.events.recv().await.unwrap();
    ctl.submit(ctl.prepare(serialized(3, 1000)).await.unwrap())
        .await
        .unwrap();
    let frame = reply(RequestId::String("callback".into()));
    ctl.submit_frame(
        ctl.prepare_frame(OperationId("callback:0".into()), &frame)
            .unwrap(),
    )
    .await
    .unwrap();
    let blocked = write(&mut peer).await;
    for i in 1..=34 {
        ctl.submit_frame(
            ctl.prepare_frame(OperationId(format!("callback:{i}")), &frame)
                .unwrap(),
        )
        .await
        .unwrap();
    }
    peer.events
        .send(entry(1, 0, JournalPayload::Frame(reply(first.id.clone()))))
        .await
        .unwrap();
    let event = session.events.recv().await.unwrap();
    assert!(event.outcomes.iter().any(|o| matches!(o, SessionOutcome::Resolved { ticket, outcome: RequestOutcome::Reply { .. } } if *ticket == first)));
    assert!(
        event
            .journal
            .as_ref()
            .is_some_and(|j| j.cursor == EventCursor(1))
    );
    let exit = session.checked_shutdown().await;
    drop(blocked);
    let mut preserved = event.journal.is_some()
        || exit.undelivered.iter().any(|e| {
            e.journal
                .as_ref()
                .is_some_and(|j| j.cursor == EventCursor(1))
        });
    while let Ok(e) = session.events.try_recv() {
        preserved |= e
            .journal
            .as_ref()
            .is_some_and(|j| j.cursor == EventCursor(1));
    }
    assert!(
        preserved,
        "reply removed from journal then discarded when releasing successor hits full callback queue; exit={:?}",
        exit.result
    );
}

// Retained child environment verification is implemented only on Linux.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn review_numeric_request_recovery_needs_hidden_receipt_mapping() {
    use support::*;
    let root = DisposableState::new();
    // Startup completes by handshake, before the session's clock/guard starts.
    let mut supervisor = SupervisorProcess::start(&root);
    tokio::time::timeout(HANG_GUARD, async {
        let script = r#"
saved=
while IFS= read -r line; do
 id=$(rpc_id "$line")
 case "$line" in
  *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id";;
  *'"method":"model/list"'*) saved=$id;;
  *'"method":"thread/unsubscribe"'*) [ -z "$saved" ] || printf '{"id":%s,"result":{"models":[]}}\n' "$saved";;
 esac
done
"#;
        let launch = ProviderLaunchSpec::new(shell_command(script), child_environment(&root.0), root.0.clone());
        let factory: Arc<dyn ProviderTaskFactory> = Arc::new(studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(root.identity()));
        let mut opts = options(); opts.launch=launch.clone();
        let mut cfg=config(); cfg.timeout_ms=HANG_GUARD.as_millis() as u64;
        let mut first = SessionHandle::open(factory.clone(),ProviderHandle::for_account("test"),opts,cfg).await.unwrap();
        status(&mut first, InitializationStatus::Ready).await;
        supervisor.own_child(first.status.borrow().attachment.info.as_ref().unwrap().pid);
        let initial = first.events.recv().await.unwrap().journal.unwrap();
        first.control.settled(initial.cursor).await.unwrap();
        loop { if first.status.borrow().attachment.info.as_ref().unwrap().acknowledged == initial.cursor { break; } first.status.changed().await.unwrap(); }
        let ticket = first.control.submit(first.control.prepare(spec(77,HANG_GUARD.as_millis() as u64)).await.unwrap()).await.unwrap();
        let recovery = loop { let e=first.events.recv().await.unwrap(); if let Some(r) = e.outcomes.into_iter().find_map(|o| match o { SessionOutcome::Write { outcome: WriteOutcome::Written, recovery, .. } => recovery, _ => None }) { break r; } };
        assert!(recovery.remote_id.is_some());
        first.checked_shutdown().await;
        let mut opts=options(); opts.launch=launch; opts.expected_generation=Some(Generation(1));
        let mut cfg=config(); cfg.timeout_ms=HANG_GUARD.as_millis() as u64; cfg.initialize_operation=OperationId("second:init".into()); cfg.request_id_prefix=Some("second".into()); cfg.recovered=vec![recovery];
        let mut second=SessionHandle::open(factory,ProviderHandle::for_account("test"),opts,cfg).await.unwrap();
        status(&mut second,InitializationStatus::Ready).await;
        let release=Frame::Notification(studio_runtime_providers::NotificationFrame { method: Method::ThreadUnsubscribe,params:None,fields:Default::default() });
        second.control.submit_frame(second.control.prepare_frame(OperationId("release".into()), &release).unwrap()).await.unwrap();
        let event=loop { let e=second.events.recv().await.unwrap(); if e.journal.is_some() {break e;} };
        let resolved=event.outcomes.iter().any(|o|matches!(o, SessionOutcome::Resolved { ticket:t,outcome:RequestOutcome::Reply {..} } if *t==ticket));
        second.checked_shutdown().await;
        assert!(resolved,"retained local request 77 replayed as orphan: no public session receipt exposes remote mapping");
    }).await.expect("disposable recovery probe watchdog");
}

#[tokio::test(start_paused = true)]
async fn review_backlogged_on_time_reply_must_not_send_expired_successor() {
    let (mut session, mut peer) = claude_ready().await;
    let ctl = session.control.clone();
    let first = ctl
        .submit(ctl.prepare(serialized(2, 1000)).await.unwrap())
        .await
        .unwrap();
    written(write(&mut peer).await);
    session.events.recv().await.unwrap();
    let second = ctl
        .submit(ctl.prepare(serialized(3, 10)).await.unwrap())
        .await
        .unwrap();
    for cursor in 1..=32 {
        peer.events
            .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
    }
    peer.events
        .send(entry(33, 0, JournalPayload::Frame(reply(first.id))))
        .await
        .unwrap();
    pending(
        peer.events
            .send(entry(34, 0, JournalPayload::Stderr(json!({})))),
    )
    .await;
    tokio::time::advance(Duration::from_millis(20)).await;
    loop {
        let e = session.events.recv().await.unwrap();
        if e.journal
            .as_ref()
            .is_some_and(|j| j.cursor == EventCursor(33))
        {
            break;
        }
    }
    let sent = matches!(peer.commands.try_recv(), Ok(Observed::Write(_)));
    let exit = session.checked_shutdown().await;
    assert!(
        !sent,
        "expired serialized successor {:?} sent after backlog release, shutdown={:?}",
        second, exit.result
    );
}

#[tokio::test(start_paused = true)]
async fn callback_admission_drop_fences_but_retained_future_survives_select() {
    for abandon in [false, true] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        let control = session.control.clone();
        let prepared = control
            .prepare_frame(
                OperationId("callback".into()),
                &reply(RequestId::String("callback".into())),
            )
            .unwrap();
        let mut future = Box::pin(control.submit_frame(prepared));
        pending(future.as_mut()).await;
        let receipt = write(&mut peer).await;
        // An unrelated select winner only borrows the retained admission future.
        tokio::select! { biased; _ = std::future::ready(()) => {}, _ = &mut future => panic!("biased barrier") }
        if abandon {
            drop(future);
        } else {
            future.await.unwrap();
        }
        written(receipt);
        let prepared = control.prepare(spec(3, 100)).await;
        assert_eq!(prepared.is_err(), abandon);
        let exit = session.checked_shutdown().await;
        let mut all = exit.outcomes;
        for e in exit.undelivered {
            all.extend(e.outcomes);
        }
        while let Ok(e) = session.events.try_recv() {
            all.extend(e.outcomes);
        }
        assert_eq!(all.iter().filter(|o| matches!(o, SessionOutcome::Write { operation_id, .. } if operation_id.0 == "callback")).count(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_and_lost_ack_return_locally_settled_unacknowledged_prefix() {
    for lost_receipt in [false, true] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        let control = session.control.clone();
        let blocked = if !lost_receipt {
            control
                .submit_frame(
                    control
                        .prepare_frame(
                            OperationId("blocked".into()),
                            &reply(RequestId::String("callback".into())),
                        )
                        .unwrap(),
                )
                .await
                .unwrap();
            Some(write(&mut peer).await)
        } else {
            None
        };
        peer.events
            .send(entry(1, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
        session.events.recv().await.unwrap();
        control.settled(EventCursor(1)).await.unwrap();
        let exit = if lost_receipt {
            let Observed::Ack(cursor, reply) = peer.commands.recv().await.unwrap() else {
                panic!("ack expected");
            };
            assert_eq!(cursor, EventCursor(1));
            drop(reply);
            session.checked_completion().await
        } else {
            let exit = session.checked_shutdown().await;
            assert!(exit.result.is_ok());
            exit
        };
        assert_eq!(exit.pending_acknowledgement, Some(EventCursor(1)));
        drop(blocked);
        peer.gone.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn recovered_unknown_does_not_reset_deadline_or_repeat_uncertainty() {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    let mut r = recovered(RequestId::Number(42), Method::ModelList, 5);
    r.unknown = true;
    cfg.recovered.push(r);
    let mut session =
        SessionHandle::open(factory, ProviderHandle::for_account("test"), options(), cfg)
            .await
            .unwrap();
    ready(&mut session, &mut peer).await;
    tokio::time::advance(Duration::from_millis(20)).await;
    pending(session.events.recv()).await;
    peer.events
        .send(entry(
            1,
            4,
            JournalPayload::Frame(reply(RequestId::Number(42))),
        ))
        .await
        .unwrap();
    let event = session.events.recv().await.unwrap();
    assert!(event.outcomes.iter().any(|o| matches!(
        o,
        SessionOutcome::Resolved {
            outcome: RequestOutcome::Reply { late: false, .. },
            ..
        }
    )));
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
#[ignore = "probabilistic contention probe; select explicitly outside the fast tier"]
async fn replenished_journal_and_settlement_lanes_allow_command_and_receipt_progress() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let control = session.control.clone();
    let events = peer.events.clone();
    let (stop, mut stopping) = watch::channel(false);
    let producer = tokio::spawn(async move {
        for cursor in 1.. {
            tokio::select! {
                _ = stopping.changed() => break,
                _ = events.send(entry(cursor, 0, JournalPayload::Stderr(json!({})))) => {}
            }
        }
    });
    // The producer stays live until both admission and the real receipt finish.
    let operation = async {
        let prepared = control.prepare(spec(2, 10000)).await.unwrap();
        control.submit(prepared).await.unwrap()
    };
    tokio::pin!(operation);
    let mut admitted = false;
    let mut written = false;
    let mut deliveries = 0;
    while !admitted || !written || deliveries < 32 {
        tokio::select! {
            _ = &mut operation, if !admitted => admitted = true,
            observed = peer.commands.recv() => match observed.unwrap() {
                Observed::Write(reply) => { reply.send(CommandReceipt::Written { remote_id: Some(10), duplicate: false }).ok().unwrap(); }
                Observed::Ack(cursor, reply) => { reply.send(CommandReceipt::Acknowledged(cursor)).ok().unwrap(); }
            },
            event = session.events.recv() => {
                let event = event.unwrap();
                written |= event.outcomes.iter().any(|o| matches!(o, SessionOutcome::Write { outcome: WriteOutcome::Written, .. }));
                if let Some(journal) = event.journal {
                    deliveries += 1;
                    assert!(deliveries < 4096, "command starved by replenished lanes");
                    control.settled(journal.cursor).await.unwrap();
                }
            }
        }
    }
    stop.send(true).unwrap();
    producer.await.unwrap();
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn written_correlation_survives_reply_resolving_before_receipt() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let ticket = session
        .control
        .submit(session.control.prepare(spec(2, 100)).await.unwrap())
        .await
        .unwrap();
    let receipt = write(&mut peer).await;
    peer.events
        .send(entry(1, 1, JournalPayload::Frame(reply(ticket.id.clone()))))
        .await
        .unwrap();
    let reply = session.events.recv().await.unwrap();
    assert!(
        reply
            .outcomes
            .iter()
            .any(|o| matches!(o, SessionOutcome::Resolved { .. }))
    );
    receipt
        .send(CommandReceipt::Written {
            remote_id: Some(99),
            duplicate: false,
        })
        .ok()
        .unwrap();
    let event = session.events.recv().await.unwrap();
    let recovery = event
        .outcomes
        .into_iter()
        .find_map(|o| match o {
            SessionOutcome::Write { recovery, .. } => recovery,
            _ => None,
        })
        .unwrap();
    assert_eq!(recovery.ticket, ticket);
    assert_eq!(recovery.remote_id, Some(99));
    assert_eq!(recovery.deadline_ms, 100);
    session.checked_shutdown().await;
}
// Retained child environment verification is implemented only on Linux.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn closure_crash_before_written_persistence_keeps_unknown_without_resend() {
    use support::*;
    let root = DisposableState::new();
    // Startup completes by handshake, before the session's clock/guard starts.
    let mut supervisor = SupervisorProcess::start(&root);
    tokio::time::timeout(HANG_GUARD, async {
        let script = r#"
saved=
count=0
while IFS= read -r line; do
 id=$(rpc_id "$line")
 case "$line" in
  *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id";;
  *'"method":"model/list"'*) saved=$id; count=$((count + 1));;
  *'"method":"thread/unsubscribe"'*) [ -z "$saved" ] || printf '{"id":%s,"result":{"models":[],"receivedCount":%s}}\n' "$saved" "$count";;
 esac
done
"#;
        let launch = ProviderLaunchSpec::new(shell_command(script), child_environment(&root.0), root.0.clone());
        let factory: Arc<dyn ProviderTaskFactory> = Arc::new(studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(root.identity()));
        let mut opts = options(); opts.launch=launch.clone();
        let mut cfg=config(); cfg.timeout_ms=HANG_GUARD.as_millis() as u64;
        let mut first = SessionHandle::open(factory.clone(),ProviderHandle::for_account("test"),opts,cfg).await.unwrap();
        status(&mut first, InitializationStatus::Ready).await;
        supervisor.own_child(first.status.borrow().attachment.info.as_ref().unwrap().pid);
        let initial = first.events.recv().await.unwrap().journal.unwrap();
        first.control.settled(initial.cursor).await.unwrap();
        loop { if first.status.borrow().attachment.info.as_ref().unwrap().acknowledged == initial.cursor { break; } first.status.changed().await.unwrap(); }
        let prepared = first.control.prepare(spec(77,HANG_GUARD.as_millis() as u64)).await.unwrap();
        let submitted = prepared.recovery_record();
        assert_eq!(submitted.deadline_ms, prepared.deadline_ms());
        let ticket = first.control.submit(prepared).await.unwrap();
        let recovery = loop { let e=first.events.recv().await.unwrap(); if let Some(r) = e.outcomes.into_iter().find_map(|o| match o { SessionOutcome::Write { outcome: WriteOutcome::Written, recovery, .. } => recovery, _ => None }) { break r; } };
        assert!(recovery.remote_id.is_some());
        first.checked_shutdown().await;
        let mut opts=options(); opts.launch=launch; opts.expected_generation=Some(Generation(1));
        let mut cfg=config(); cfg.timeout_ms=HANG_GUARD.as_millis() as u64; cfg.initialize_operation=OperationId("second:init".into()); cfg.request_id_prefix=Some("second".into()); cfg.recovered=vec![submitted];
        let mut second=SessionHandle::open(factory,ProviderHandle::for_account("test"),opts,cfg).await.unwrap();
        status(&mut second,InitializationStatus::Ready).await;
        let release=Frame::Notification(studio_runtime_providers::NotificationFrame { method: Method::ThreadUnsubscribe,params:None,fields:Default::default() });
        second.control.submit_frame(second.control.prepare_frame(OperationId("release".into()), &release).unwrap()).await.unwrap();
        let event=loop { let e=second.events.recv().await.unwrap(); if e.journal.is_some() {break e;} };
        let resolved=event.outcomes.iter().any(|o|matches!(o, SessionOutcome::Resolved { ticket:t,outcome:RequestOutcome::Reply {..} } if *t==ticket));
        let JournalPayload::Frame(Frame::Response { body: ResponseBody::Result(value), .. }) = &event.journal.as_ref().unwrap().payload else { panic!("retained orphan envelope"); };
        assert_eq!(value["receivedCount"], 1, "no new child write during recovery");
        let exit = second.checked_shutdown().await;
        assert!(!resolved, "missing remote mapping must not be guessed");
        assert!(exit.outcomes.iter().any(|o| matches!(o, SessionOutcome::Resolved { ticket:t, outcome:RequestOutcome::Unknown } if *t==ticket)));

    }).await.expect("disposable recovery probe watchdog");
}

/// Recovery restores every durable request, and new preparations do not
/// wait for the recovered ones.
#[tokio::test(start_paused = true)]
async fn recovery_restores_every_request_without_holding_back_preparation() {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.recovered = (2..602)
        .map(|id| {
            let mut r = recovered(RequestId::Number(id), Method::ModelList, 1000);
            r.operation_id = OperationId(format!("recovered:{id}"));
            r
        })
        .collect();
    let mut session =
        SessionHandle::open(factory, ProviderHandle::for_account("test"), options(), cfg)
            .await
            .expect("every recovered request is restored");
    ready(&mut session, &mut peer).await;
    session.control.prepare(spec(1000, 1000)).await.unwrap();
    let exit = session.checked_shutdown().await;
    let mut restored = exit
        .outcomes
        .iter()
        .chain(exit.undelivered.iter().flat_map(|event| &event.outcomes))
        .filter_map(|outcome| match outcome {
            SessionOutcome::Resolved {
                ticket,
                outcome: RequestOutcome::Unknown,
            } => Some(ticket.id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    restored.sort_by_key(|id| match id {
        RequestId::Number(id) => *id,
        _ => panic!("numeric recovered identity"),
    });
    assert_eq!(
        restored,
        (2..602).map(RequestId::Number).collect::<Vec<_>>()
    );
}

#[tokio::test(start_paused = true)]
async fn closure_recovered_thread_remains_serialized_until_reply() {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.wire = studio_runtime_providers::ProviderKind::Claude.wire();
    let mut r = recovered(RequestId::Number(42), Method::TurnStart, 1000);
    r.thread_id = Some("thread".into());
    cfg.recovered.push(r);
    let mut session =
        SessionHandle::open(factory, ProviderHandle::for_account("test"), options(), cfg)
            .await
            .unwrap();
    ready(&mut session, &mut peer).await;
    session
        .control
        .submit(session.control.prepare(serialized(43, 1000)).await.unwrap())
        .await
        .unwrap();
    pending(peer.commands.recv()).await;
    peer.events
        .send(entry(
            1,
            1,
            JournalPayload::Frame(reply(RequestId::Number(42))),
        ))
        .await
        .unwrap();
    session.events.recv().await.unwrap();
    written(write(&mut peer).await);
    session.checked_shutdown().await;
}
#[tokio::test(start_paused = true)]
async fn closure_ack_prefix_after_positive_final_receipt_and_gap() {
    for settle_middle in [false, true] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        for cursor in 1..=3 {
            peer.events
                .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
                .await
                .unwrap();
            session.events.recv().await.unwrap();
        }
        session.control.settled(EventCursor(3)).await.unwrap();
        session.control.settled(EventCursor(1)).await.unwrap();
        let Observed::Ack(cursor, receipt) = peer.commands.recv().await.unwrap() else {
            panic!("ack");
        };
        assert_eq!(cursor, EventCursor(1));
        if settle_middle {
            session.control.settled(EventCursor(2)).await.unwrap();
        }
        receipt
            .send(CommandReceipt::Acknowledged(cursor))
            .ok()
            .unwrap();
        let exit = session.checked_shutdown().await;
        assert_eq!(
            exit.pending_acknowledgement,
            if settle_middle {
                Some(EventCursor(3))
            } else {
                None
            }
        );
    }
}
#[tokio::test(start_paused = true)]
async fn closure_recovery_with_deadline_already_elapsed_reports_unknown_once() {
    let clock = SessionClock::default();
    tokio::time::advance(Duration::from_millis(20)).await;
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.recovered
        .push(recovered(RequestId::Number(42), Method::ModelList, 5));
    let mut opts = options();
    opts.clock = clock;
    let mut session = SessionHandle::open(factory, ProviderHandle::for_account("test"), opts, cfg)
        .await
        .unwrap();
    ready(&mut session, &mut peer).await;
    let event = session.events.recv().await.unwrap();
    assert_eq!(event.outcomes.iter().filter(|o|matches!(o,SessionOutcome::Resolved{ticket,outcome:RequestOutcome::Unknown} if ticket.id==RequestId::Number(42))).count(),1);
    let exit = session.checked_shutdown().await;
    assert!(!exit.outcomes.iter().any(|o|matches!(o,SessionOutcome::Resolved{ticket,outcome:RequestOutcome::Unknown} if ticket.id==RequestId::Number(42))));
}

#[path = "support/session_outcomes.rs"]
mod outcome_checks;
use outcome_checks::CheckedExit;

#[tokio::test(start_paused = true)]
async fn prepared_deadline_is_durable_before_submit_and_never_restarted() {
    for elapsed in [5, 20] {
        let (mut session, mut peer) = open().await;
        ready(&mut session, &mut peer).await;
        let prepared = session.control.prepare(spec(2, 10)).await.unwrap();
        let durable = prepared.recovery_record();
        assert_eq!(prepared.deadline_ms(), 10);
        assert_eq!(durable.deadline_ms, 10);
        assert_eq!(durable.remote_id, None);
        tokio::time::advance(Duration::from_millis(elapsed)).await;
        session.control.submit(prepared).await.unwrap();
        if elapsed < 10 {
            written(write(&mut peer).await);
        }
        let event = session.events.recv().await.unwrap();
        assert!(event.outcomes.iter().any(|o| match o {
            SessionOutcome::Write {
                outcome: WriteOutcome::Written,
                recovery: Some(r),
                ..
            } => elapsed < 10 && r.deadline_ms == durable.deadline_ms,
            SessionOutcome::Write {
                outcome: WriteOutcome::NotSent,
                ..
            } => elapsed >= 10,
            _ => false,
        }));
        pending(peer.commands.recv()).await;
        session.checked_shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn recovered_lightweight_request_does_not_reserve_serialized_thread() {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.wire = studio_runtime_providers::ProviderKind::Claude.wire();
    let mut r = recovered(RequestId::Number(42), Method::TurnStart, 1000);
    r.thread_id = Some("thread".into());
    r.lightweight = true;
    cfg.recovered.push(r);
    let mut session =
        SessionHandle::open(factory, ProviderHandle::for_account("test"), options(), cfg)
            .await
            .unwrap();
    ready(&mut session, &mut peer).await;
    session
        .control
        .submit(session.control.prepare(serialized(43, 1000)).await.unwrap())
        .await
        .unwrap();
    written(write(&mut peer).await);
    session.checked_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn active_barrier_holds_new_requests_but_not_callback_answers_or_interrupts() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    let control = session.control.clone();
    // An unread event consumer keeps a journal prefix unconsumed.
    for cursor in 1..=(LANE_CAPACITY * 2 + 1) as u64 {
        peer.events
            .send(entry(cursor, 0, JournalPayload::Stderr(json!({}))))
            .await
            .unwrap();
    }
    let barrier = tokio::spawn({
        let control = control.clone();
        async move { control.journal_barrier().await }
    });
    let held = tokio::spawn({
        let control = control.clone();
        async move { control.prepare(spec(7, 1000)).await }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(!barrier.is_finished());
    assert!(!held.is_finished(), "a new request is held by the barrier");

    let answer = control
        .prepare_frame(
            OperationId("callback:9".into()),
            &reply(RequestId::Number(9)),
        )
        .unwrap();
    control.submit_frame(answer).await.unwrap();
    let interrupt = control
        .prepare(RequestSpec {
            id: RequestId::Number(8),
            operation_id: OperationId("request:8".into()),
            method: Method::TurnInterrupt,
            params: json!({"threadId":"t","turnId":"u"}),
            thread_id: Some("t".into()),
            lightweight: true,
            timeout_ms: 1000,
        })
        .await
        .unwrap();
    control.submit(interrupt).await.unwrap();
    written(write(&mut peer).await);
    assert!(!barrier.is_finished() && !held.is_finished());

    // Consuming the journal prefix completes the barrier and releases the request.
    while !(barrier.is_finished() && held.is_finished()) {
        session.events.recv().await.unwrap();
        tokio::task::yield_now().await;
    }
    barrier.await.unwrap().unwrap();
    held.await.unwrap().unwrap();
    written(write(&mut peer).await);
    session.checked_shutdown().await;
}
