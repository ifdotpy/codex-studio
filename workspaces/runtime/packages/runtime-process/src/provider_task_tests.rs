#![allow(clippy::unwrap_used)]
use super::*;
#[cfg(not(target_os = "linux"))]
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
        clock: SessionClock::default(),
        handle: ProviderHandle::for_account("unit"),
        open: OpenRequest {
            handle: SupervisorHandle("account:unit".into()),
            command: vec![],
            env: Default::default(),
            cwd: "/".into(),
            extra: Default::default(),
        },
        identity: BackendIdentity {
            backend_id: BackendId("unit".into()),
            state_dir: "/unused".into(),
        },
        endpoint: "/unused/socket".into(),
        #[cfg(target_os = "linux")]
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
    };
    (actor, commands, journal)
}

async fn read_request<S: AsyncRead + Unpin>(server: &mut BufReader<S>) -> Value {
    let mut line = String::new();
    assert_ne!(server.read_line(&mut line).await.unwrap(), 0);
    serde_json::from_str(&line).unwrap()
}
async fn empty_reply(server: &mut BufReader<tokio::io::DuplexStream>, request: Value) {
    let response = serde_json::json!({"requestId":request["requestId"],"result":{"event":null,"returnCode":null,"backpressure":false,"stdoutReaderAlive":true,"stderrReaderAlive":true}});
    server
        .get_mut()
        .write_all(format!("{response}\n").as_bytes())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn sole_next_pause_is_exact_and_sticky_stop_never_waits_for_it() {
    let shutdown = Shutdown::default();
    let (mut actor, commands, _journal) = actor(&shutdown);
    let (socket, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(socket),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        actor.serve(&mut rpc, EventCursor(0)).await
    });
    let first = read_request(&mut server).await;
    let (ack, barrier) = AdapterCommand::ack(EventCursor(0));
    commands
        .send(ack)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    empty_reply(&mut server, first).await;
    assert!(matches!(
        barrier.receive().await,
        CommandReceipt::Acknowledged(_)
    ));
    tokio::time::advance(NEXT_QUERY_PAUSE - Duration::from_nanos(1)).await;
    let mut read = Box::pin(read_request(&mut server));
    assert!(std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx).is_pending())).await);
    drop(read);
    tokio::time::advance(Duration::from_nanos(1)).await;
    let second = read_request(&mut server).await;
    let (ack, barrier) = AdapterCommand::ack(EventCursor(0));
    commands
        .send(ack)
        .await
        .unwrap_or_else(|_| panic!("closed"));
    empty_reply(&mut server, second).await;
    barrier.receive().await;
    let before_stop = Instant::now();
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
    assert_eq!(Instant::now(), before_stop);
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
async fn journal_arrival_clock_is_sampled_before_consumer_admission() {
    let shutdown = Shutdown::default();
    let (mut actor, _commands, mut journal) = actor(&shutdown);
    let (socket, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(socket),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        actor.serve(&mut rpc, EventCursor(0)).await
    });
    let request = read_request(&mut server).await;
    tokio::time::advance(Duration::from_millis(7)).await;
    let response = serde_json::json!({"requestId":request["requestId"],"result":{
        "event":{"sequence":1,"generation":1,"kind":"stderr","payload":"{\"data\":\"output\"}"},
        "returnCode":null,"backpressure":false,"stdoutReaderAlive":true,"stderrReaderAlive":true}});
    server
        .get_mut()
        .write_all(format!("{response}\n").as_bytes())
        .await
        .unwrap();
    read_request(&mut server).await; // proves previous response is queued
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(journal.recv().await.unwrap().received_at_ms, 7);
    shutdown.cancel();
    assert!(matches!(task.await.unwrap(), Err(AdapterError::Cancelled)));
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

#[cfg(target_os = "linux")]
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
        let first = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        entered.await.unwrap();
        let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = executed.clone();
        let queued = tokio::task::spawn_blocking(move || {
            observed.store(true, std::sync::atomic::Ordering::SeqCst)
        });
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
        let running = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        entered.await.unwrap();
        let mut receiver = stop.subscribe();
        let mut join = Box::pin(reap_environment_job(running, &mut receiver));
        assert!(std::future::poll_fn(|cx| Poll::Ready(join.as_mut().poll(cx).is_pending())).await);
        release.send(()).unwrap();
        assert!(matches!(join.await, Err(AdapterError::Cancelled)));
    });
}

#[tokio::test(start_paused = true)]
async fn response_mapping_capacity_refuses_before_send_and_prunes_all_settled_entries() {
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
        for id in 1..=MAX_PENDING_RESPONSES as u64 {
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
        assert_eq!(actor.response_ids.len(), MAX_PENDING_RESPONSES);
        let rejected = actor
            .write(
                &mut rpc,
                PreparedWrite {
                    operation_id: OperationId("overflow".into()),
                    message: serde_json::json!({"id":99,"method":"model/list"}),
                },
            )
            .await
            .unwrap();
        assert!(matches!(rejected, CommandReceipt::NotSent));
        for id in 1..=MAX_PENDING_RESPONSES as u64 {
            let event = SupervisorEvent {
                sequence: EventCursor(id),
                generation: Generation(1),
                kind: EventKind::Stdout,
                payload: serde_json::json!({"id":id,"result":{}}),
                extra: Default::default(),
            };
            let frame = actor.journal_entry(event, 0).unwrap();
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
    for remote in 1..=MAX_PENDING_RESPONSES as u64 + 1 {
        let request = read_request(&mut server).await;
        assert_ne!(request["operationId"], "overflow");
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
        peer.get_mut().write_all(b"{\"ok\":true}\n").await.unwrap();
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
