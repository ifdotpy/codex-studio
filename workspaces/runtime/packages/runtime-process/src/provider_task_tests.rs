#![allow(clippy::unwrap_used)]
use super::*;
use std::io;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncReadExt, ReadBuf},
    sync::oneshot,
};

/// Observe the actual Pending poll, not a signal immediately before the wait.
struct PendingIo {
    entered: Option<oneshot::Sender<()>>,
}
impl AsyncRead for PendingIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}
impl AsyncWrite for PendingIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
        }
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_pending_socket_write() {
    let (entered, waiting) = oneshot::channel();
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(PendingIo {
                entered: Some(entered),
            }),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        rpc.call::<_, WriteResult>(
            SupervisorAction::Write,
            serde_json::json!({"handle":"account:test"}),
            &mut stop,
        )
        .await
    });
    waiting.await.unwrap();
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
}

#[tokio::test(start_paused = true)]
async fn rpc_absolute_deadline_expires_without_new_input() {
    let (entered, waiting) = oneshot::channel();
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(PendingIo {
                entered: Some(entered),
            }),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        rpc.call::<_, WriteResult>(
            SupervisorAction::Write,
            serde_json::json!({"handle":"account:test"}),
            &mut stop,
        )
        .await
    });
    waiting.await.unwrap();
    tokio::time::advance(RPC_TIMEOUT - Duration::from_nanos(1)).await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_nanos(1)).await;
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Timeout)));
}

#[tokio::test(start_paused = true)]
async fn persistent_next_batch_read_has_no_rpc_timeout_and_cancels_cleanly() {
    let (client, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(client),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        rpc.call_wait::<_, NextBatchResult>(
            SupervisorAction::NextBatch,
            NextBatchRequest {
                handle: SupervisorHandle("account:unit".into()),
                cursor: EventCursor(0),
                generation: Generation(1),
                limit: SUPERVISOR_NEXT_BATCH_MAX_EVENTS,
            },
            &mut stop,
        )
        .await
    });
    let request = read_request(&mut server).await;
    assert_eq!(request["action"], "nextBatch");
    assert_eq!(request["limit"], SUPERVISOR_NEXT_BATCH_MAX_EVENTS);
    tokio::time::advance(RPC_TIMEOUT * 2).await;
    assert!(!task.is_finished());
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
}

#[test]
fn v2_handshake_and_event_keep_typed_envelope_and_raw_payload() {
    let identity = BackendIdentity {
        backend_id: BackendId("backend:test".into()),
        state_dir: PathBuf::from("/tmp/state"),
    };
    let request = handshake_request_v2(&identity);
    let encoded = serde_json::to_value(request).unwrap();
    assert_eq!(encoded["protocol"], SUPERVISOR_PROTOCOL_V2);
    assert!(validate_handshake_v2(r#"{"ok":true}"#).is_err());
    assert!(validate_handshake_v2(r#"{"ok":true,"protocol":2}"#).is_ok());

    let event = SupervisorEventV2 {
        sequence: EventCursor(9),
        generation: Generation(2),
        kind: EventKind::Unknown("future".into()),
        payload: serde_json::value::RawValue::from_string(
            r#"{"nested":{"text":"escaped\\n雪"},"number":1e+02}"#.into(),
        )
        .unwrap(),
    };
    let frame = serde_json::to_string(&event).unwrap();
    assert!(frame.contains(r#""payload":{"nested":{"text":"escaped\\n雪"},"number":1e+02}"#));
    let decoded: SupervisorEventV2 = serde_json::from_str(&frame).unwrap();
    assert_eq!(decoded.payload.get(), event.payload.get());
}

#[tokio::test(start_paused = true)]
async fn sticky_shutdown_interrupts_connect_future_and_closed_sender() {
    let shutdown = Shutdown::default();
    shutdown.cancel();
    let mut stop = shutdown.subscribe();
    let mut polled = false;
    let result = interruptible(
        &mut stop,
        std::future::poll_fn(|_| {
            polled = true;
            Poll::<Result<(), AdapterError>>::Pending
        }),
    )
    .await;
    assert!(matches!(result, Err(AdapterError::Cancelled)));
    assert!(!polled);
    let (sender, mut receiver) = watch::channel(false);
    drop(sender);
    Shutdown::cancelled(&mut receiver).await;
}

struct FragmentProbe {
    inner: tokio::io::DuplexStream,
    received: usize,
    consumed: Option<oneshot::Sender<()>>,
}
impl AsyncRead for FragmentProbe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        self.received += buffer.filled().len() - before;
        // A second read poll after the fragment proves Rpc consumed its buffer.
        if result.is_pending()
            && self.received > 0
            && let Some(consumed) = self.consumed.take()
        {
            let _ignored = consumed.send(());
        }
        result
    }
}
impl AsyncWrite for FragmentProbe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_partial_response_without_restart() {
    let (client, mut server) = tokio::io::duplex(128);
    let (consumed, barrier) = oneshot::channel();
    let client = FragmentProbe {
        inner: client,
        received: 0,
        consumed: Some(consumed),
    };
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(client),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        rpc.call::<_, WriteResult>(
            SupervisorAction::Write,
            serde_json::json!({"handle":"account:test"}),
            &mut stop,
        )
        .await
    });
    let mut byte = [0];
    loop {
        server.read_exact(&mut byte).await.unwrap();
        if byte == *b"\n" {
            break;
        }
    }
    server.write_all(b"{\"requestId\":1,").await.unwrap();
    barrier.await.unwrap();
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
    assert_eq!(server.read(&mut byte).await.unwrap(), 0);
}

fn actor(
    shutdown: &Shutdown,
) -> (
    AdapterTask,
    mpsc::Sender<AdapterCommand>,
    mpsc::Receiver<JournalEntry>,
) {
    let (commands, inbox) = mpsc::channel(LANE_CAPACITY);
    let (events, journal) = mpsc::channel(LANE_CAPACITY);
    let status = AttachmentStatus {
        handle: ProviderHandle::for_account("unit"),
        open_attempted: false,
        info: None,
        initialization: InitializationStatus::AwaitingReply,
    };
    let actor = AdapterTask {
        activation: None,
        inherit_cwd: false,
        clock: SessionClock::default(),
        handle: ProviderHandle::for_account("unit"),
        open: OpenRequest {
            handle: SupervisorHandle("account:unit".into()),
            command: vec![],
            env: Default::default(),
            cwd: "/".into(),
            extra: Default::default(),
        },
        reattach_commands: Vec::new(),
        retained: RetainedLaunchValues::default(),
        identity: BackendIdentity {
            backend_id: BackendId("unit".into()),
            state_dir: "/unused".into(),
        },
        endpoint: "/unused/socket".into(),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        environment_reads: Arc::new(Semaphore::new(1)),
        expected_generation: None,
        handled_cursor: 0,
        response_ids: Default::default(),
        stop: shutdown.subscribe(),
        commands: inbox,
        events,
        status: watch::channel(status).0,
        info: None,
        initialization: InitializationStatus::AwaitingReply,
        initialize_id: None,
        cached_initialize: None,
        open_attempted: false,
        provider_exited: false,
        cleanup_error: None,
    };
    (actor, commands, journal)
}

async fn read_request<S: AsyncRead + Unpin>(server: &mut BufReader<S>) -> Value {
    let mut line = String::new();
    assert_ne!(server.read_line(&mut line).await.unwrap(), 0);
    serde_json::from_str(&line).unwrap()
}

async fn attachment_with_supervisor_peers() -> (
    tempfile::TempDir,
    ProviderAttachment,
    BufReader<UnixStream>,
    BufReader<UnixStream>,
) {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::UnixListener::bind(root.path().join("supervisor.sock")).unwrap();
    let factory = SupervisorProviderTaskFactory::new(BackendIdentity {
        backend_id: BackendId("detach-barrier".into()),
        state_dir: root.path().into(),
    });
    let attachment = factory
        .open(
            ProviderHandle::for_account("unit"),
            OpenOptions {
                launch: ProviderLaunchSpec::new(
                    vec!["fixture".into()],
                    Default::default(),
                    root.path().into(),
                ),
                clock: SessionClock::default(),
                expected_generation: None,
            },
            Shutdown::default(),
        )
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut control = BufReader::new(socket);
    let _handshake = read_request(&mut control).await;
    control
        .get_mut()
        .write_all(b"{\"ok\":true,\"protocol\":2}\n")
        .await
        .unwrap();
    let status = read_request(&mut control).await;
    control.get_mut().write_all(format!("{}\n", serde_json::json!({"requestId":status["requestId"],"error":"Unknown supervisor handle"})).as_bytes()).await.unwrap();
    let open = read_request(&mut control).await;
    control.get_mut().write_all(format!("{}\n", serde_json::json!({"requestId":open["requestId"],"result":{"pid":1,"generation":1,"resumed":false,"sequence":0,"acknowledged":0}})).as_bytes()).await.unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let mut events = BufReader::new(socket);
    let _handshake = read_request(&mut events).await;
    events
        .get_mut()
        .write_all(b"{\"ok\":true,\"protocol\":2}\n")
        .await
        .unwrap();
    assert_eq!(read_request(&mut events).await["action"], "nextBatch");
    (root, attachment, control, events)
}

#[tokio::test]
async fn shutdown_waits_for_supervisor_detach_on_both_connections() {
    tokio::time::timeout(RPC_TIMEOUT, async {
        let (_root, mut attachment, mut control, mut events) =
            attachment_with_supervisor_peers().await;
        let mut shutdown = Box::pin(attachment.shutdown());
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx).is_pending())).await
        );
        // The fake supervisor observes each FIN but withholds its EOF until
        // ownership release. Neither local socket drop nor one EOF is enough.
        assert_eq!(events.read(&mut [0]).await.unwrap(), 0);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx).is_pending())).await,
            "shutdown completed before the supervisor released the event connection"
        );
        events.get_mut().shutdown().await.unwrap();
        assert_eq!(control.read(&mut [0]).await.unwrap(), 0);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx).is_pending())).await,
            "shutdown completed before the supervisor released the control connection"
        );
        control.get_mut().shutdown().await.unwrap();
        let exit = shutdown.await;
        assert!(matches!(exit.result, Err(AdapterError::Cancelled)));
        assert!(exit.cleanup_error.is_none(), "{:?}", exit.cleanup_error);
        assert!(exit.provider_retained);
    })
    .await
    .expect("supervisor detach barrier hang guard");
}
#[tokio::test]
async fn shutdown_reports_control_detach_timeout() {
    let (_root, mut attachment, mut control, mut events) = attachment_with_supervisor_peers().await;
    let task = tokio::spawn(async move { attachment.shutdown().await });
    assert_eq!(events.read(&mut [0]).await.unwrap(), 0);
    events.get_mut().shutdown().await.unwrap();
    assert_eq!(control.read(&mut [0]).await.unwrap(), 0);
    tokio::time::pause();
    let exit = tokio::time::timeout(RPC_TIMEOUT * 2, task)
        .await
        .expect("supervisor detach must remain bounded")
        .unwrap();
    assert!(matches!(exit.result, Err(AdapterError::Cancelled)));
    assert!(
        exit.cleanup_error
            .unwrap()
            .contains(&AdapterError::Timeout.to_string())
    );
    assert!(exit.provider_retained);
}

#[tokio::test]
async fn shutdown_reports_both_detach_timeouts_within_one_budget() {
    let (_root, mut attachment, mut control, mut events) = attachment_with_supervisor_peers().await;
    let started = Instant::now();
    let task = tokio::spawn(async move { attachment.shutdown().await });
    assert_eq!(events.read(&mut [0]).await.unwrap(), 0);
    // Both FINs must arrive before either peer sends EOF or a timer fires.
    assert_eq!(control.read(&mut [0]).await.unwrap(), 0);
    tokio::time::pause();
    let exit = task.await.unwrap();
    assert!(Instant::now() - started < SUPERVISOR_DETACH_TIMEOUT * 2);
    assert!(matches!(exit.result, Err(AdapterError::Cancelled)));
    assert_eq!(
        exit.cleanup_error
            .unwrap()
            .matches(&AdapterError::Timeout.to_string())
            .count(),
        2
    );
    assert!(exit.provider_retained);
}

#[tokio::test]
async fn shutdown_reports_event_detach_timeout() {
    let (_root, mut attachment, mut control, mut events) = attachment_with_supervisor_peers().await;
    // The control peer will confirm detach; only the event peer withholds EOF.
    control.get_mut().shutdown().await.unwrap();
    let task = tokio::spawn(async move { attachment.shutdown().await });
    assert_eq!(events.read(&mut [0]).await.unwrap(), 0);
    tokio::time::pause();
    let exit = tokio::time::timeout(RPC_TIMEOUT * 2, task)
        .await
        .expect("supervisor detach must remain bounded")
        .unwrap();
    assert!(matches!(exit.result, Err(AdapterError::Cancelled)));
    assert!(
        exit.cleanup_error
            .unwrap()
            .contains(&AdapterError::Timeout.to_string())
    );
    assert!(exit.provider_retained);
}

#[tokio::test(start_paused = true)]
async fn idle_attachment_waits_for_events_without_periodic_next_requests() {
    let shutdown = Shutdown::default();
    let (mut actor, commands, _journal) = actor(&shutdown);
    let (socket, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let (_pump_sender, pump) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(socket),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        actor.serve(&mut rpc, pump, EventCursor(0)).await
    });
    let (ack, barrier) = AdapterCommand::ack(EventCursor(0));
    commands
        .send(ack)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    assert!(matches!(
        barrier.receive().await,
        CommandReceipt::Acknowledged(_)
    ));
    let mut read = Box::pin(read_request(&mut server));
    assert!(std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx).is_pending())).await);
    drop(read);
    let before_stop = Instant::now();
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
    assert_eq!(Instant::now(), before_stop);
}

#[tokio::test]
async fn full_journal_lane_keeps_ack_commands_live_while_next_event_waits() {
    let shutdown = Shutdown::default();
    let (mut actor, commands, mut journal) = actor(&shutdown);
    actor.info = Some(AttachmentInfo {
        handle: actor.handle.clone(),
        generation: Generation(1),
        pid: 1,
        acknowledged: EventCursor(0),
        resumed: false,
    });
    actor.initialize_id = Some(studio_runtime_providers::RequestId::String(
        "initialized".into(),
    ));
    let mut status = actor.status.subscribe();

    for sequence in 1..=LANE_CAPACITY as u64 {
        assert!(
            actor
                .events
                .try_send(JournalEntry {
                    received_at_ms: 0,
                    cursor: EventCursor(sequence),
                    generation: Generation(1),
                    payload: JournalPayload::Stderr(Value::Null),
                })
                .is_ok()
        );
    }

    let (pump_sender, pump) = mpsc::channel(1);
    let event_pump = tokio::spawn(async move {
        let (accepted, acceptance) = oneshot::channel();
        pump_sender
            .send(Ok(PumpMessage::Event {
                event: SupervisorEventV2 {
                    sequence: EventCursor(LANE_CAPACITY as u64 + 1),
                    generation: Generation(1),
                    kind: EventKind::Stdout,
                    payload: serde_json::value::RawValue::from_string(
                        r#"{"id":"initialized","result":{}}"#.into(),
                    )
                    .unwrap(),
                },
                received_at: Instant::now(),
                accepted,
            }))
            .await
            .map_err(|_| AdapterError::Cancelled)?;
        acceptance.await.map_err(|_| AdapterError::Cancelled)?;
        pump_sender
            .send(Ok(PumpMessage::Terminal))
            .await
            .map_err(|_| AdapterError::Cancelled)?;
        Ok::<(), AdapterError>(())
    });
    let (socket, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(socket),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        actor
            .serve(&mut rpc, pump, EventCursor(LANE_CAPACITY as u64))
            .await
    });

    status
        .wait_for(|status| status.initialization == InitializationStatus::InitializedWritePending)
        .await
        .unwrap();
    assert_eq!(journal.len(), LANE_CAPACITY);

    let (ack, barrier) = AdapterCommand::ack(EventCursor(LANE_CAPACITY as u64));
    commands.send(ack).await.unwrap();
    let request = tokio::time::timeout(Duration::from_secs(2), read_request(&mut server))
        .await
        .unwrap();
    assert_eq!(request["action"], "ack");
    assert_eq!(request["sequence"], LANE_CAPACITY as u64);
    let response = serde_json::json!({
        "requestId": request["requestId"],
        "result": {"acknowledged": LANE_CAPACITY as u64}
    });
    server
        .get_mut()
        .write_all(format!("{response}\n").as_bytes())
        .await
        .unwrap();
    assert!(matches!(
        barrier.receive().await,
        CommandReceipt::Acknowledged(EventCursor(cursor)) if cursor == LANE_CAPACITY as u64
    ));
    assert_eq!(journal.len(), LANE_CAPACITY);

    for expected in 1..=LANE_CAPACITY as u64 {
        assert_eq!(journal.recv().await.unwrap().cursor, EventCursor(expected));
    }
    assert_eq!(
        journal.recv().await.unwrap().cursor,
        EventCursor(LANE_CAPACITY as u64 + 1)
    );
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
    let _pump_result = event_pump.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cancellation_before_send_is_not_sent_and_missing_receipt_is_unknown() {
    let shutdown = Shutdown::default();
    let (mut actor, _commands, _journal) = actor(&shutdown);
    let (sender, receiver) = oneshot::channel();
    let (entered, _waiting) = oneshot::channel();
    let mut rpc = Rpc {
        stream: BufReader::new(PendingIo {
            entered: Some(entered),
        }),
        sequence: 0,
        started: false,
        received_at: Instant::now(),
    };
    shutdown.cancel();
    actor
        .command(
            &mut rpc,
            AdapterCommand::Write {
                write: PreparedWrite {
                    operation_id: OperationId("owned".into()),
                    message: serde_json::json!({"method":"notice"}),
                },
                reply: sender,
                observed: oneshot::channel().1,
            },
            EventCursor(0),
        )
        .await
        .unwrap();
    assert!(matches!(receiver.await.unwrap(), CommandReceipt::NotSent));
    assert!(!rpc.started);
    let (command, receipt) = AdapterCommand::ack(EventCursor(0));
    drop(command);
    assert!(matches!(receipt.receive().await, CommandReceipt::Unknown));
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_registered_connect_wait() {
    let (entered, waiting) = oneshot::channel();
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let task = tokio::spawn(async move {
        let mut entered = Some(entered);
        interruptible(
            &mut stop,
            std::future::poll_fn(move |_| {
                if let Some(entered) = entered.take() {
                    entered.send(()).unwrap();
                }
                Poll::<Result<(), AdapterError>>::Pending
            }),
        )
        .await
    });
    waiting.await.unwrap();
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
}

#[tokio::test(start_paused = true)]
async fn v2_journal_entry_uses_the_frame_arrival_clock() {
    let shutdown = Shutdown::default();
    let (mut actor, _commands, _journal) = actor(&shutdown);
    let received_at = Instant::now();
    tokio::time::advance(Duration::from_millis(7)).await;
    let event = SupervisorEventV2 {
        sequence: EventCursor(1),
        generation: Generation(1),
        kind: EventKind::Stderr,
        payload: serde_json::value::RawValue::from_string(r#"{"data":"output"}"#.into()).unwrap(),
    };
    let received_at_ms = actor.clock.millis_at(received_at);
    let entry = actor.journal_entry_v2(event, received_at_ms).unwrap();
    assert_eq!(entry.received_at_ms, 0);
    assert_eq!(actor.clock.now_ms(), 7);
}

#[tokio::test(start_paused = true)]
async fn abandoned_buffered_write_receipt_or_receive_future_fences_attachment() {
    for cancel_future in [false, true] {
        let shutdown = Shutdown::default();
        let (mut actor, _commands, _journal) = actor(&shutdown);
        let (socket, server) = tokio::io::duplex(4096);
        let mut server = BufReader::new(server);
        let (command, mut receipt) = AdapterCommand::write(PreparedWrite {
            operation_id: OperationId("once".into()),
            message: serde_json::json!({"method":"notice"}),
        });
        let task = tokio::spawn(async move {
            let mut rpc = Rpc {
                stream: BufReader::new(socket),
                sequence: 0,
                started: false,
                received_at: Instant::now(),
            };
            actor.command(&mut rpc, command, EventCursor(0)).await
        });
        let request = read_request(&mut server).await;
        let response = serde_json::json!({"requestId":request["requestId"],"result":{"accepted":true,"duplicate":false,"remoteId":null,"durableMs":0}});
        server
            .get_mut()
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        // Test-only peek consumes the buffered value but does not acknowledge
        // observation. The production owner still owns the observation guard.
        assert!(matches!(
            receipt.take_buffered_without_observing().await,
            CommandReceipt::Written { .. }
        ));
        if cancel_future {
            drop(Box::pin(receipt.receive()));
        } else {
            drop(receipt);
        }
        assert!(matches!(
            task.await.unwrap(),
            Err(AdapterError::Supervisor(SupervisorError::ConnectionLost))
        ));
        assert_eq!(server.read(&mut [0]).await.unwrap(), 0);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn shutdown_aborts_queued_environment_work_and_reaps_started_work() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (release, wait) = std::sync::mpsc::channel();
        let (started, entered) = oneshot::channel();
        let first = crate::blocking_pool::spawn_blocking(
            "runtime-process/provider_task_tests",
            move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            },
        );
        entered.await.unwrap();
        let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = executed.clone();
        let queued = crate::blocking_pool::spawn_blocking(
            "runtime-process/provider_task_tests",
            move || observed.store(true, std::sync::atomic::Ordering::SeqCst),
        );
        let stop = Shutdown::default();
        stop.cancel();
        let mut receiver = stop.subscribe();
        let mut cancelled_job = Box::pin(reap_environment_job(queued, &mut receiver));
        // Tokio abort prevents execution, but reaping waits until the occupied
        // blocking pool visits the cancelled job. Release the explicit barrier.
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(cancelled_job.as_mut().poll(cx).is_pending()))
                .await
        );
        release.send(()).unwrap();
        assert!(matches!(cancelled_job.await, Err(AdapterError::Cancelled)));
        first.await.unwrap();
        assert!(!executed.load(std::sync::atomic::Ordering::SeqCst));
        let (release, wait) = std::sync::mpsc::channel();
        let (started, entered) = oneshot::channel();
        let running = crate::blocking_pool::spawn_blocking(
            "runtime-process/provider_task_tests",
            move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            },
        );
        entered.await.unwrap();
        let mut receiver = stop.subscribe();
        let mut join = Box::pin(reap_environment_job(running, &mut receiver));
        assert!(std::future::poll_fn(|cx| Poll::Ready(join.as_mut().poll(cx).is_pending())).await);
        release.send(()).unwrap();
        assert!(matches!(join.await, Err(AdapterError::Cancelled)));
    });
}

#[tokio::test(start_paused = true)]
async fn response_mapping_prunes_all_settled_entries() {
    /// Requests the provider owes replies at once; the mapping has no window.
    const OWED: u64 = 40;
    let shutdown = Shutdown::default();
    let (mut actor, _commands, _journal) = actor(&shutdown);
    actor.info = Some(AttachmentInfo {
        handle: actor.handle.clone(),
        generation: Generation(1),
        pid: 1,
        acknowledged: EventCursor(0),
        resumed: false,
    });
    let (socket, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(socket),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        for id in 1..=OWED {
            let receipt = actor
                .write(
                    &mut rpc,
                    PreparedWrite {
                        operation_id: OperationId(format!("request:{id}")),
                        message: serde_json::json!({"id":id,"method":"model/list"}),
                    },
                )
                .await
                .unwrap();
            assert!(matches!(receipt, CommandReceipt::Written { .. }));
        }
        assert_eq!(actor.response_ids.len(), OWED as usize);
        for id in 1..=OWED {
            let event = SupervisorEventV2 {
                sequence: EventCursor(id),
                generation: Generation(1),
                kind: EventKind::Stdout,
                payload: serde_json::value::RawValue::from_string(format!(
                    r#"{{"id":{id},"result":{{}}}}"#
                ))
                .unwrap(),
            };
            let frame = actor.journal_entry_v2(event, 0).unwrap();
            assert!(
                matches!(frame.payload, JournalPayload::Frame(studio_runtime_providers::Frame::Response { id: studio_runtime_providers::RequestId::Number(local), .. }) if local == id)
            );
        }
        assert!(actor.response_ids.is_empty());
        assert!(matches!(
            actor
                .write(
                    &mut rpc,
                    PreparedWrite {
                        operation_id: OperationId("after-prune".into()),
                        message: serde_json::json!({"id":99,"method":"model/list"})
                    }
                )
                .await
                .unwrap(),
            CommandReceipt::Written { .. }
        ));
        assert_eq!(actor.response_ids.len(), 1);
    });
    for remote in 1..=OWED + 1 {
        let request = read_request(&mut server).await;
        let response = serde_json::json!({"requestId":request["requestId"],"result":{"accepted":true,"duplicate":false,"remoteId":remote,"durableMs":0}});
        server
            .get_mut()
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
    }
    task.await.unwrap();
}

thread_local! { static PANIC_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub(super) fn panic_on_open_write(bytes: &[u8]) {
    PANIC_OPEN.with(|armed| {
        if armed.get()
            && serde_json::from_slice::<Value>(bytes).is_ok_and(|v| v["action"] == "open")
        {
            armed.set(false);
            panic!("injected after production open socket write");
        }
    });
}

#[tokio::test]
async fn panic_during_uncertain_open_closes_socket_and_preserves_handle_recovery() {
    tokio::time::timeout(Duration::from_secs(15), async {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!("panic-open-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup { fn drop(&mut self) { let _ignored = std::fs::remove_dir_all(&self.0); } }
        let _cleanup = Cleanup(root.clone());
        let listener = tokio::net::UnixListener::bind(root.join("supervisor.sock")).unwrap();
        let identity = BackendIdentity { backend_id: BackendId("panic-test".into()), state_dir: root.clone() };
        let factory = SupervisorProviderTaskFactory::new(identity);
        let handle = ProviderHandle::for_account("panic-open");
        PANIC_OPEN.with(|armed| armed.set(true));
        let mut attachment = factory.open(handle.clone(), OpenOptions { clock: SessionClock::default(), launch: ProviderLaunchSpec::new(vec!["fake".into()], Default::default(), root), expected_generation: None }, Shutdown::default()).await.unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut peer = BufReader::new(socket);
        let _handshake = read_request(&mut peer).await;
        peer.get_mut()
            .write_all(b"{\"ok\":true,\"protocol\":2}\n")
            .await
            .unwrap();
        let status = read_request(&mut peer).await;
        assert_eq!(status["action"], "status");
        peer.get_mut().write_all(format!("{}\n", serde_json::json!({"requestId":status["requestId"], "error":"Unknown supervisor handle"})).as_bytes()).await.unwrap();
        let open = read_request(&mut peer).await;
        assert_eq!(open["action"], "open");
        let exit = attachment.completion().await;
        assert!(matches!(exit.result, Err(AdapterError::TaskFailed)));
        let recovery = exit.recovery_needed.unwrap();
        assert_eq!(recovery.handle, handle);
        assert_eq!(recovery.generation, None);
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
    }).await.unwrap();
}

#[test]
fn closed_pump_classification_preserves_sticky_shutdown_and_terminal_state() {
    assert!(matches!(
        pump_channel_closed(false, false),
        Some(AdapterError::TaskFailed)
    ));
    assert!(matches!(
        pump_channel_closed(true, false),
        Some(AdapterError::Cancelled)
    ));
    assert!(pump_channel_closed(true, true).is_none());
}

#[tokio::test]
async fn stdout_journal_remaps_only_numeric_replies_and_preserves_raw_bytes() {
    let shutdown = Shutdown::default();
    let (mut actor, _commands, _journal) = actor(&shutdown);
    actor.info = Some(AttachmentInfo {
        handle: actor.handle.clone(),
        generation: Generation(1),
        pid: 1,
        acknowledged: EventCursor(0),
        resumed: false,
    });
    actor.response_ids = BTreeMap::from([(42, RequestId::String("local:42".into()))]);
    // Codec shape/extension matrices belong to runtime-providers. This caller
    // owns numeric reply correlation and preserves every other ID and raw byte.
    for (payload, expected) in [
        (
            r#"{"id":42,"result":{"ok":true}}"#,
            r#"{"id":"local:42","result":{"ok":true}}"#,
        ),
        (
            r#"{"id":43,"result":{}}"#,
            r#"{"id":"supervisor-orphan-43","result":{}}"#,
        ),
        (
            r#"{"id":5,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}}"#,
            r#"{"id":5,"method":"item/commandExecution/requestApproval","params":{"threadId":"t"}}"#,
        ),
        (
            r#"{"id":"claude:42","result":{"ok":true}}"#,
            r#"{"id":"claude:42","result":{"ok":true}}"#,
        ),
        (
            r#"{"method":"claude/state","params":{"sessionId":"s","state":"idle"},"_studioSupervisorReceivedAt":1760000000456}"#,
            r#"{"method":"claude/state","params":{"sessionId":"s","state":"idle"},"_studioSupervisorReceivedAt":1760000000456}"#,
        ),
    ] {
        let entry = actor
            .journal_entry_v2(
                SupervisorEventV2 {
                    sequence: EventCursor(1),
                    generation: Generation(1),
                    kind: EventKind::Stdout,
                    payload: serde_json::value::RawValue::from_string(payload.into()).unwrap(),
                },
                0,
            )
            .unwrap();
        let JournalPayload::Frame(frame) = entry.payload else {
            panic!("protocol frame")
        };
        let actual: Value =
            serde_json::from_slice(&studio_runtime_providers::encode_line(&frame).unwrap())
                .unwrap();
        assert_eq!(actual, serde_json::from_str::<Value>(expected).unwrap());
    }
    assert!(actor.response_ids.is_empty());
    for (payload, expected) in [
        (
            r#"{"supervisorRaw":"provider diagnostic, not JSON\n"}"#,
            b"provider diagnostic, not JSON\n".as_slice(),
        ),
        (
            r#"{"extension":[1,2],"method":17}"#,
            br#"{"extension":[1,2],"method":17}"#.as_slice(),
        ),
    ] {
        let entry = actor
            .journal_entry_v2(
                SupervisorEventV2 {
                    sequence: EventCursor(2),
                    generation: Generation(1),
                    kind: EventKind::Stdout,
                    payload: serde_json::value::RawValue::from_string(payload.into()).unwrap(),
                },
                0,
            )
            .unwrap();
        let JournalPayload::RawStdout { bytes, .. } = entry.payload else {
            panic!("raw stdout")
        };
        assert_eq!(bytes, expected);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn launch_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOME".to_owned(), "/home/fixture".to_owned()),
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
    ])
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn retained_environment_ignores_only_the_backend_build_identity() {
    let mut requested = launch_environment();
    requested.insert("CODEX_BACKEND_BUILD".into(), "new-backend".into());
    requested.insert(
        "STUDIO_CLAUDE_CREDENTIAL_IDENTITY".into(),
        "claude:a@example.test".into(),
    );
    let clean = clean_environment(&requested);
    assert!(!clean.contains_key("CODEX_BACKEND_BUILD"));
    for previous_build in [None, Some("old-backend"), Some("new-backend")] {
        let mut original = requested.clone();
        original.remove("CODEX_BACKEND_BUILD");
        if let Some(build) = previous_build {
            original.insert("CODEX_BACKEND_BUILD".into(), build.into());
        }
        let (read, _) = scripted_records(vec![Ok(original.clone())]);
        let shutdown = Shutdown::default();
        let settled = settled_environment(
            &mut shutdown.subscribe(),
            &Arc::new(Semaphore::new(1)),
            &clean,
            &RetainedLaunchValues::default(),
            Duration::ZERO,
            read,
        )
        .await
        .unwrap();
        assert_eq!(
            settled, original,
            "the supervisor keeps its original launch"
        );
        for key in [
            "HOME",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "STUDIO_CLAUDE_CREDENTIAL_IDENTITY",
            "OPENAI_API_KEY",
            "UNCLASSIFIED_VARIABLE",
        ] {
            let mut foreign = original.clone();
            foreign.insert(key.into(), "changed".into());
            assert!(
                verify_environment(&foreign, &clean, &RetainedLaunchValues::default()).is_err(),
                "the backend build exception must not admit another {key}"
            );
        }
    }
}

/// A reader that returns `records` in order, then the last one again.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scripted_records(
    records: Vec<io::Result<BTreeMap<String, String>>>,
) -> (
    impl Fn() -> io::Result<BTreeMap<String, String>> + Clone + Send + 'static,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let records = Arc::new(
        records
            .into_iter()
            .map(|record| record.map_err(|error| error.kind()))
            .collect::<Vec<_>>(),
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    let read = move || {
        let index = counter.fetch_add(1, Ordering::SeqCst);
        records[index.min(records.len() - 1)]
            .clone()
            .map_err(io::Error::from)
    };
    (read, reads)
}

/// A just-started child shows a zeroed record during an exec and Apple
/// strings before libSystem startup. A reattach reads again until the record
/// equals the accepted launch, instead of refusing a correct child.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(start_paused = true)]
async fn retained_environment_settles_after_a_transient_launch_record() {
    use std::sync::atomic::Ordering;
    let clean = launch_environment();
    let mut with_apple_strings = clean.clone();
    with_apple_strings.insert("stack_guard".into(), "0x1".into());
    with_apple_strings.insert("pfz".into(), "0x2".into());
    let (read, reads) = scripted_records(vec![
        Err(io::ErrorKind::InvalidData.into()),
        Ok(with_apple_strings),
        Ok(clean.clone()),
    ]);
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let slots = Arc::new(Semaphore::new(1));
    let settled = settled_environment(
        &mut stop,
        &slots,
        &clean,
        &RetainedLaunchValues::default(),
        RETAINED_ENVIRONMENT_SETTLE,
        read,
    )
    .await
    .unwrap();
    assert_eq!(settled, clean);
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    assert_eq!(slots.available_permits(), 1);
}

/// A child whose launch record keeps differing is still refused.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(start_paused = true)]
async fn retained_environment_refuses_a_record_that_never_matches() {
    use std::sync::atomic::Ordering;
    let clean = launch_environment();
    let mut other = clean.clone();
    other.insert("HOME".into(), "/home/other".into());
    let (read, reads) = scripted_records(vec![Ok(other)]);
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let slots = Arc::new(Semaphore::new(1));
    let started = Instant::now();
    let result = settled_environment(
        &mut stop,
        &slots,
        &clean,
        &RetainedLaunchValues::default(),
        RETAINED_ENVIRONMENT_SETTLE,
        read,
    )
    .await;
    assert_eq!(started.elapsed(), RETAINED_ENVIRONMENT_SETTLE);
    assert!(matches!(result, Err(AdapterError::UnverifiableChild)));
    assert!(reads.load(Ordering::SeqCst) >= 2);
    assert_eq!(slots.available_permits(), 1);
}

/// Shutdown ends the wait between two reads.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(start_paused = true)]
async fn retained_environment_wait_ends_on_shutdown() {
    let clean = launch_environment();
    let (read, _reads) = scripted_records(vec![Err(io::ErrorKind::InvalidData.into())]);
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let slots = Arc::new(Semaphore::new(1));
    let cancel = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            shutdown.cancel();
        })
    };
    let result = settled_environment(
        &mut stop,
        &slots,
        &clean,
        &RetainedLaunchValues::default(),
        Duration::from_secs(60),
        read,
    )
    .await;
    assert!(matches!(result, Err(AdapterError::Cancelled)));
    cancel.await.unwrap();
}
