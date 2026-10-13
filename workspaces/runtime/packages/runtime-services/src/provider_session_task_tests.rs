#![allow(clippy::unwrap_used)]
use super::*;
use std::cell::Cell;
use tokio::sync::oneshot;
thread_local! { static FAULT: Cell<u8> = const { Cell::new(0) }; }
pub(super) fn panic_at(point: u8) {
    FAULT.with(|fault| {
        if fault.get() == point {
            fault.set(0);
            panic!("injected session transition panic {point}");
        }
    });
}
struct Factory(std::sync::Mutex<Option<ProviderAttachment>>);
impl ProviderTaskFactory for Factory {
    fn open(
        &self,
        _: ProviderHandle,
        _: OpenOptions,
        _: Shutdown,
    ) -> studio_runtime_process::provider_seam::OpenFuture<'_> {
        let attachment = self.0.lock().unwrap().take().unwrap();
        Box::pin(async { Ok(attachment) })
    }
}
#[tokio::test(start_paused = true)]
async fn panic_after_take_after_mutation_and_with_outstanding_receipt_retains_evidence() {
    for point in [1, 2, 3, 4] {
        let stop = Shutdown::default();
        let mut stopped = stop.subscribe();
        let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(16);
        let (entries, journal) = mpsc::channel(16);
        let handle = ProviderHandle::for_account("panic");
        let (status, observed) = watch::channel(AttachmentStatus {
            handle: handle.clone(),
            open_attempted: true,
            info: None,
            initialization: InitializationStatus::AwaitingReply,
        });
        let (blocked, waiting) = oneshot::channel();
        let (gone, joined) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _status = status;
            for index in 0.. {
                let command = tokio::select! { _ = cancelled(&mut stopped) => break, command = inbox.recv() => command.unwrap() };
                if let AdapterCommand::Write {
                    reply, observed, ..
                } = command
                {
                    if index == 2 {
                        let _ignored = blocked.send(());
                        cancelled(&mut stopped).await;
                        let _ignored = reply.send(CommandReceipt::Unknown);
                        break;
                    }
                    let result = if index == 0 {
                        CommandReceipt::CachedInitialize(serde_json::json!({}))
                    } else {
                        CommandReceipt::Written {
                            remote_id: None,
                            duplicate: false,
                        }
                    };
                    let _ignored = reply.send(result);
                    let _ignored = observed.await;
                }
            }
            let _ignored = gone.send(());
            TaskExit {
                result: Ok(()),
                recovery_needed: None,
            }
        });
        let factory = Arc::new(Factory(std::sync::Mutex::new(Some(
            ProviderAttachment::from_task(commands, journal, observed, stop, task),
        ))));
        let launch = studio_runtime_process::ProviderLaunchSpec::new(
            vec!["fake".into()],
            BTreeMap::new(),
            "/var/tmp".into(),
        );
        let mut session = SessionHandle::open(factory, handle, OpenOptions { clock: SessionClock::default(), launch, expected_generation: None }, SessionTaskConfig { initialize_params: serde_json::json!({"clientInfo":{"name":"test","version":"1"},"capabilities":{"experimentalApi":true}}), generation: Generation(1), claude: false, timeout_ms: 100, initialize_operation: OperationId("init".into()), request_id_prefix: None, recovered: vec![] }).await.unwrap();
        while session.status.borrow().initialization != InitializationStatus::Ready {
            session.status.changed().await.unwrap();
        }
        let prepared = session
            .control
            .prepare(RequestSpec {
                id: RequestId::Number(2),
                operation_id: OperationId("request".into()),
                method: Method::ModelList,
                params: serde_json::json!({}),
                thread_id: None,
                lightweight: false,
                timeout_ms: 100,
            })
            .await
            .unwrap();
        let ticket = session.control.submit(prepared).await.unwrap();
        waiting.await.unwrap();
        if point == 4 {
            let queued = session
                .control
                .prepare(RequestSpec {
                    id: RequestId::Number(3),
                    operation_id: OperationId("unsent".into()),
                    method: Method::ModelList,
                    params: serde_json::json!({}),
                    thread_id: None,
                    lightweight: false,
                    timeout_ms: 100,
                })
                .await
                .unwrap();
            session.control.submit(queued).await.unwrap();
        }
        FAULT.with(|fault| fault.set(point));
        entries
            .send(JournalEntry {
                received_at_ms: if point == 4 { 200 } else { 1 },
                cursor: EventCursor(1),
                generation: Generation(1),
                payload: JournalPayload::Frame(Frame::Response {
                    id: ticket.id.clone(),
                    body: ResponseBody::Result(serde_json::json!({})),
                    fields: Map::new(),
                }),
            })
            .await
            .unwrap();
        let exit = session.checked_completion().await;
        joined.await.unwrap();
        assert!(matches!(exit.result, Err(SessionTaskError::TaskFailed)));
        assert_eq!(exit.undelivered.len(), 1);
        assert_eq!(
            exit.undelivered[0].journal.as_ref().unwrap().cursor,
            EventCursor(1)
        );
        if point == 4 {
            assert!(exit.undelivered[0].outcomes.iter().any(|o| matches!(o, SessionOutcome::Write { ticket: Some(ticket), outcome: WriteOutcome::NotSent, .. } if ticket.id == RequestId::Number(3))));
        }
        if point == 2 {
            assert!(exit.undelivered[0].outcomes.iter().any(|o| matches!(o, SessionOutcome::Resolved { ticket: t, outcome: RequestOutcome::Reply { late: false, .. } } if *t == ticket)));
        }
        assert!(exit.outcomes.iter().any(|o| matches!(o, SessionOutcome::Write { ticket: Some(t), outcome: WriteOutcome::Unknown, .. } if *t == ticket)));
    }
}
#[tokio::test(start_paused = true)]
async fn closure_panic_unknown_batch_does_not_return_unknown_twice() {
    let shutdown = Shutdown::default();
    let mut stopped = shutdown.subscribe();
    let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(16);
    let (entries, journal) = mpsc::channel(16);
    let handle = ProviderHandle::for_account("panic-unknown");
    let (status, observed) = watch::channel(AttachmentStatus {
        handle: handle.clone(),
        open_attempted: true,
        info: None,
        initialization: InitializationStatus::AwaitingReply,
    });
    let (sent, waiting) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _status = status;
        let command = inbox.recv().await.unwrap();
        if let AdapterCommand::Write {
            reply, observed, ..
        } = command
        {
            reply
                .send(CommandReceipt::Written {
                    remote_id: None,
                    duplicate: false,
                })
                .ok()
                .unwrap();
            observed.await.unwrap();
        }
        sent.send(()).unwrap();
        cancelled(&mut stopped).await;
        TaskExit {
            result: Ok(()),
            recovery_needed: None,
        }
    });
    let factory = Arc::new(Factory(std::sync::Mutex::new(Some(
        ProviderAttachment::from_task(commands, journal, observed, shutdown, task),
    ))));
    let recovered_ticket = RequestTicket {
        generation: 1,
        id: RequestId::Number(44),
        method: Method::ModelList,
    };
    let launch = studio_runtime_process::ProviderLaunchSpec::new(
        vec!["fake".into()],
        BTreeMap::new(),
        "/var/tmp".into(),
    );
    let mut session = SessionHandle::open(
        factory,
        handle,
        OpenOptions {
            clock: SessionClock::default(),
            launch,
            expected_generation: None,
        },
        SessionTaskConfig {
            initialize_params: serde_json::json!({}),
            generation: Generation(1),
            claude: false,
            timeout_ms: 1000,
            initialize_operation: OperationId("init".into()),
            request_id_prefix: None,
            recovered: vec![RecoveredRequest {
                ticket: recovered_ticket.clone(),
                operation_id: OperationId("r44".into()),
                thread_id: None,
                lightweight: false,
                deadline_ms: 1000,
                unknown: false,
                remote_id: None,
            }],
        },
    )
    .await
    .unwrap();
    waiting.await.unwrap();
    FAULT.with(|f| f.set(2));
    entries
        .send(JournalEntry {
            cursor: EventCursor(1),
            generation: Generation(1),
            received_at_ms: 1,
            payload: JournalPayload::Frame(Frame::Response {
                id: RequestId::Number(1),
                body: ResponseBody::Error(serde_json::json!({"code":-1,"message":"refused"})),
                fields: Map::new(),
            }),
        })
        .await
        .unwrap();
    let exit = session.checked_completion().await;
    assert!(matches!(exit.result, Err(SessionTaskError::TaskFailed)));
    let mut outcomes = exit.outcomes;
    for e in exit.undelivered {
        outcomes.extend(e.outcomes);
    }
    while let Ok(e) = session.events.try_recv() {
        outcomes.extend(e.outcomes);
    }
    let count=outcomes.iter().filter(|o|matches!(o,SessionOutcome::Resolved {ticket,outcome:RequestOutcome::Unknown} if ticket==&recovered_ticket)).count();
    assert_eq!(
        count, 1,
        "one retained Unknown is emitted by action cleanup and again by final_outcomes"
    );
}

#[path = "../tests/support/session_outcomes.rs"]
mod outcome_checks;
use outcome_checks::CheckedExit;
