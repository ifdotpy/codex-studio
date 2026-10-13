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
    cancelled(&mut receiver).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_partial_response_without_restart() {
    let (client, mut server) = tokio::io::duplex(128);
    let shutdown = Shutdown::default();
    let mut stop = shutdown.subscribe();
    let task = tokio::spawn(async move {
        let mut rpc = Rpc {
            stream: BufReader::new(client),
            sequence: 0,
            started: false,
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
        info: None,
        initialization: InitializationStatus::AwaitingReply,
    };
    let actor = AdapterTask {
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

async fn read_request(server: &mut BufReader<tokio::io::DuplexStream>) -> Value {
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
