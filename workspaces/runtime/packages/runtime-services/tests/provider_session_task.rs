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
    EventCursor, Generation, OperationId, ProviderHandle, ProviderLaunchSpec, provider_seam::*,
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
            let needs_observation = matches!(
                result,
                CommandReceipt::Written { .. } | CommandReceipt::CachedInitialize(_)
            );
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
            recovery_needed: (!initialized).then_some(InitializationRecovery {
                handle,
                generation: Some(Generation(1)),
                phase: InitializationStatus::AwaitingReply,
            }),
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
        claude: false,
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
    let exit = session.shutdown().await;
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
    session.shutdown().await;
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
    session.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn recovered_deadline_emits_without_any_further_input() {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.timeout_ms = 100;
    cfg.recovered
        .push((RequestId::Number(42), Method::ModelList, None));
    let mut session =
        SessionHandle::open(factory, ProviderHandle::for_account("test"), options(), cfg)
            .await
            .unwrap();
    ready(&mut session, &mut peer).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    let event = session.events.recv().await.unwrap();
    assert!(event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { ticket, outcome: RequestOutcome::Unknown } if ticket.id == RequestId::Number(42))));
    session.shutdown().await;
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
    let exit = session.shutdown().await;
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
    for arrived in [9, 11] {
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
        assert!(event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { outcome: RequestOutcome::Reply { late, .. }, .. } if *late == (arrived >= 10))));
        session.shutdown().await;
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
            cfg.initialize_params = json!({"large":"x".repeat(1024*1024)});
        } else if case == 1 {
            cfg.recovered = vec![(RequestId::Number(8), Method::ModelList, None); 2];
        } else if case == 2 {
            cfg.recovered = vec![(RequestId::Number(8), Method::Initialize, None)];
        } else if case == 3 {
            cfg.recovered = vec![(RequestId::String("x".repeat(1025)), Method::ModelList, None)];
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
    let exit = session.shutdown().await;
    peer.gone.await.unwrap();
    assert_eq!(exit.undelivered.len(), 1);
    assert_eq!(
        exit.undelivered[0].journal.as_ref().unwrap().cursor,
        EventCursor(17)
    );
    assert!(exit.adapter.unwrap().recovery_needed.is_none());
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
            session.shutdown().await
        } else {
            session.completion().await
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
    session.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn incomplete_initialization_shutdown_surfaces_recovery_after_adapter_join() {
    let (mut session, mut peer) = open().await;
    let _unanswered = write(&mut peer).await;
    let exit = session.shutdown().await;
    peer.gone.await.unwrap();
    let recovery = exit.adapter.unwrap().recovery_needed.unwrap();
    assert_eq!(recovery.handle, ProviderHandle::for_account("test"));
    assert_eq!(recovery.generation, Some(Generation(1)));
}

#[tokio::test(start_paused = true)]
async fn before_at_and_after_deadline_arrivals_keep_classification_under_full_backlog() {
    for arrived in [9, 10, 11] {
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
        assert!(response.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::Resolved { outcome: RequestOutcome::Reply { late, .. }, .. } if *late == (arrived >= 10))));
        session.shutdown().await;
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
    session.shutdown().await;
}

#[cfg(unix)]
#[path = "../../runtime-process/tests/support/mod.rs"]
mod support;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn disposable_supervisor_session_preserves_replay_and_settlement_across_reattach() {
    tokio::time::timeout(Duration::from_secs(15), async {
        use support::*;
        let root = DisposableState::new();
        let python = python_executable();
        let mut supervisor = SupervisorProcess::start(&root, &python);
        let script = r#"
import json,sys
for line in sys.stdin:
 m=json.loads(line)
 if m.get('method')=='initialize': print(json.dumps({'id':m['id'],'result':{}}),flush=True)
 elif m.get('method')=='initialized':
  print('raw stdout',flush=True)
  print('stderr entry',file=sys.stderr,flush=True)
  print(json.dumps({'method':'observed/initialized'}),flush=True)
 elif m.get('method')=='model/list': print(json.dumps({'id':m['id'],'result':{'models':[]}}),flush=True)
"#;
        let launch = ProviderLaunchSpec::new(vec![python, "-u".into(), "-c".into(), script.into()], BTreeMap::from([("PYTHONUNBUFFERED".into(), "1".into())]), root.0.clone());
        let factory: Arc<dyn ProviderTaskFactory> = Arc::new(studio_runtime_process::provider_task::SupervisorProviderTaskFactory::new(root.identity()));
        let mut opts = options();
        opts.launch = launch.clone();
        let mut cfg = config();
        cfg.timeout_ms = 10_000;
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
        let ticket = first.control.submit(first.control.prepare(spec(2, 10_000)).await.unwrap()).await.unwrap();
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
        assert!(first.shutdown().await.adapter.unwrap().recovery_needed.is_none());
        let mut opts = options();
        opts.launch = launch;
        opts.launch.handled_cursor = 1;
        opts.expected_generation = Some(Generation(1));
        let mut cfg = config();
        cfg.request_id_prefix = Some("attachment-2".into());
        cfg.initialize_operation = OperationId("init:attachment-2".into());
        cfg.timeout_ms = 10_000;
        let mut second = SessionHandle::open(factory, ProviderHandle::for_account("test"), opts, cfg).await.unwrap();
        status(&mut second, InitializationStatus::Ready).await;
        assert_eq!(second.status.borrow().attachment.info.as_ref().unwrap().pid, pid);
        let replay = second.events.recv().await.unwrap().journal.unwrap();
        assert_eq!(Some(replay.cursor), replay_cursor);
        assert_eq!(replay.generation, Generation(1));
        assert!(second.shutdown().await.adapter.unwrap().recovery_needed.is_none());
    }).await.expect("disposable supervisor session exceeded local watchdog");
}

async fn claude_ready() -> (SessionHandle, Peer) {
    let (factory, mut peer) = fake();
    let mut cfg = config();
    cfg.claude = true;
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
    session.shutdown().await;
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
    session.shutdown().await;
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
    session.shutdown().await;
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
    session.shutdown().await;
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
    let exit = session.shutdown().await;
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
    first.shutdown().await;
    second.shutdown().await;
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
    let exit = session.shutdown().await;
    cleaned.await.unwrap();
    assert!(exit.result.is_ok());
    assert!(exit.adapter.is_none());
}

#[tokio::test(start_paused = true)]
async fn unresolved_request_limit_includes_unknown_until_positive_reply() {
    let (mut session, mut peer) = open().await;
    ready(&mut session, &mut peer).await;
    for id in 2..MAX_PENDING_RESPONSES as u64 + 2 {
        let request = session.control.prepare(spec(id, 100)).await.unwrap();
        session.control.submit(request).await.unwrap();
        written(write(&mut peer).await);
        session.events.recv().await.unwrap();
    }
    assert!(matches!(
        session.control.prepare(spec(100, 100)).await,
        Err(SessionTaskError::Capacity)
    ));
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(
        session.events.recv().await.unwrap().outcomes.len(),
        MAX_PENDING_RESPONSES
    );
    assert!(matches!(
        session.control.prepare(spec(100, 100)).await,
        Err(SessionTaskError::Capacity)
    ));
    peer.events
        .send(entry(
            1,
            100,
            JournalPayload::Frame(reply(RequestId::Number(2))),
        ))
        .await
        .unwrap();
    session.events.recv().await.unwrap();
    assert!(session.control.prepare(spec(100, 100)).await.is_ok());
    session.shutdown().await;
}
