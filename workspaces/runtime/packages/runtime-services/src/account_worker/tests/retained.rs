use super::*;
use crate::account_worker::retained::{
    PortError, RequestActor, RequestResult, RetainedSessionPort, SessionRequest,
};
fn request(state: &State, port: &RetainedSessionPort, operation: &str) -> SessionRequest {
    SessionRequest {
        scope: port.scope().clone(),
        operation: operation.into(),
        actor: RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        method: Method::ThreadRead,
        params: json!({"threadId":"fake-thread-0","includeTurns":false}),
        deadline: (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std(),
    }
}
#[tokio::test(start_paused = true)]
async fn retained_reply_replays_without_resending_and_rejects_changed_payload() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "history-1");
    let sender = port.clone();
    let first = call.clone();
    let task = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    peer.reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let receipt = task.await.unwrap().unwrap();
    assert_eq!(
        receipt.result,
        RequestResult::Reply(json!({"thread":{"id":"fake-thread-0"}}))
    );
    assert_eq!(
        port.request(call.clone()).await.unwrap().result,
        receipt.result
    );
    let mut altered = call;
    altered.params["limit"] = json!(2);
    assert_eq!(
        port.request(altered).await.unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_lost_waiter_preserves_unknown_until_exact_late_reply() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "lost-control");
    let sender = port.clone();
    let first = call.clone();
    let task = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    task.abort();
    let _ignored = task.await;
    assert_eq!(
        port.request(call.clone()).await.unwrap().result,
        RequestResult::Unknown
    );
    peer.reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    port.barrier(port.scope(), call.deadline).await.unwrap();
    assert!(matches!(
        port.request(call).await.unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_scope_epoch_and_expired_queue_refuse_before_write() {
    let state = State::new().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let mut call = request(&state, &port, "scope");
    call.scope.generation += 1;
    assert_eq!(port.request(call).await.unwrap_err(), PortError::Identity);
    let mut call = request(&state, &port, "epoch");
    call.actor.epoch += 1;
    assert_eq!(port.request(call).await.unwrap_err(), PortError::Identity);
    let mut call = request(&state, &port, "expired");
    call.deadline = tokio::time::Instant::now().into_std();
    assert_eq!(port.request(call).await.unwrap_err(), PortError::NotSent);
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_authorization_refuses_deleted_agent() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id=?1",
                    [agent],
                )?;
                Ok(())
            })?;
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        port.request(request(&state, &port, "deleted-agent"))
            .await
            .unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn repeated_retained_history_reads_observe_fresh_provider_state() {
    use crate::{
        account_worker::retained::consumers::HistoryPort,
        recovery_history::{HistoryMethod, HistoryReceipt},
        recovery_runtime::RetainedHistory,
    };

    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let retained = worker.retained_session();
    let scope = retained.scope().clone();
    let first_history = HistoryPort::new(
        retained.clone(),
        RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        tokio::runtime::Handle::current(),
    );
    let first_deadline =
        (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let params = json!({"threadId":"fake-thread-0","includeTurns":false});

    let first_scope = scope.clone();
    let first_params = params.clone();
    let first = tokio::task::spawn_blocking(move || {
        let mut history = first_history;
        history.request(
            &first_scope,
            HistoryMethod::Thread,
            first_params,
            first_deadline,
        )
    });
    let first_wire = peer.write("thread/read").await;
    let first_cursor = peer
        .frame(json!({
            "id":first_wire["id"],
            "result":{"thread":{"id":"fake-thread-0","revision":1}}
        }))
        .await;
    peer.acknowledged_or_worker_exits(first_cursor, &mut worker)
        .await;
    let first_result: HistoryReceipt = first.await.unwrap().unwrap();

    let second_scope = scope.clone();
    let second_params = params.clone();
    let second_deadline =
        (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let second_history = HistoryPort::new(
        retained,
        RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        tokio::runtime::Handle::current(),
    );
    let second = tokio::task::spawn_blocking(move || {
        let mut history = second_history;
        history.request(
            &second_scope,
            HistoryMethod::Thread,
            second_params,
            second_deadline,
        )
    });
    let mut second = second;
    let second_wire = tokio::select! {
        wire = peer.write("thread/read") => wire,
        _result = &mut second => panic!("repeated history read reused an earlier receipt"),
    };
    assert_eq!(second_wire["method"], "thread/read");
    assert_ne!(first_wire["id"], second_wire["id"]);
    let second_cursor = peer
        .frame(json!({
            "id":second_wire["id"],
            "result":{"thread":{"id":"fake-thread-0","revision":2}}
        }))
        .await;
    peer.acknowledged_or_worker_exits(second_cursor, &mut worker)
        .await;
    let second_result = second.await.unwrap().unwrap();
    assert_eq!(first_result.body["thread"]["revision"], 1);
    assert_eq!(second_result.body["thread"]["revision"], 2);
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_barrier_waits_for_committed_journal_without_owning_bridge() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("journal".into());
    let cursor = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    state.bridge.faults.paused.notified().await;
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let waiter = tokio::spawn(async move { port.barrier(port.scope(), deadline).await });
    // The same permit remains available while the journal/barrier is paused.
    state
        .bridge
        .run(|s| {
            let _: i64 = s.read()?.query_row("SELECT 1", [], |r| r.get(0))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!waiter.is_finished());
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_waiters();
    waiter.await.unwrap().unwrap();
    peer.acknowledged(cursor).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn recovery_reconcile_wait_does_not_hold_store_bridge_permit() {
    use crate::{
        recovery::{RecoveryError, RecoveryOutcome},
        recovery_absence::RecoveryObservation,
        recovery_history::ConnectionScope,
        recovery_policy::RecoverySnapshot,
        recovery_runtime::{RecoveryJob, RecoveryRuntime, RetainedRecoveryReader},
        recovery_store::CompletionWriter,
        task_recovery_store::TaskCompletionWriter,
    };

    struct Completion;
    impl CompletionWriter for Completion {
        fn start_held(
            &mut self,
            _: &studio_runtime_store::Transaction<'_>,
            _: &Value,
            _: &str,
            _: &str,
        ) -> Result<(), RecoveryError> {
            Ok(())
        }
        fn complete(
            &mut self,
            _: &studio_runtime_store::Transaction<'_>,
            _: &RecoverySnapshot,
            _: &ConnectionScope,
            _: &crate::recovery::RecoveryAction,
        ) -> Result<RecoveryOutcome, RecoveryError> {
            Ok(RecoveryOutcome::Unconfirmed)
        }
    }
    struct TaskCompletion;
    impl TaskCompletionWriter for TaskCompletion {
        fn resolve_receipt(&mut self, receipt: &Value) -> Result<Value, RecoveryError> {
            Ok(receipt.clone())
        }
        fn complete(
            &mut self,
            _: &crate::task_recovery::TaskRecoverySnapshot,
            _: &ConnectionScope,
            _: Value,
        ) -> Result<bool, RecoveryError> {
            Ok(false)
        }
    }

    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let mut record = state.record().await;
    record["inFlight"] = json!(true);
    record["status"] = json!("running");
    if let Some(agent) = record.as_object_mut() {
        agent.remove("startAttempt");
        agent.remove("turnId");
    }
    let snapshot = RecoverySnapshot::from_record(record).unwrap();
    let session = worker.retained_session();
    let scope = session.scope().clone();
    let observation = RecoveryObservation {
        scope: scope.clone(),
        at: 100.0,
        supervisor: None,
        quiet: false,
        journal_drained: false,
        account_reserved: true,
        tools_refreshing: false,
        preparation_pending: false,
        completed_turn: None,
    };
    let observation_scope = scope.clone();
    let observation_for_reader = observation.clone();
    let actor = RequestActor {
        agent: state.agent.clone(),
        epoch: state.epoch,
    };
    let store = state.bridge.store.clone();
    let history_port = crate::account_worker::retained::consumers::HistoryPort::new(
        session,
        actor,
        tokio::runtime::Handle::current(),
    );
    let reconcile = tokio::task::spawn_blocking(move || {
        let observation = move |current: &ConnectionScope| {
            (current == &observation_scope).then(|| observation_for_reader.clone())
        };
        let source: &crate::recovery_runtime::ObservationSource<'_> = &observation;
        let mut completion = Completion;
        let mut task_completion = TaskCompletion;
        let mut history = history_port;
        let mut reader = RetainedRecoveryReader::new(
            scope.clone(),
            "fake-thread-0".into(),
            &mut history,
            source,
        )
        .unwrap();
        let mut runtime = RecoveryRuntime {
            store: &store,
            observation: source,
            completion: &mut completion,
            task_completion: &mut task_completion,
        };
        runtime.reconcile(&RecoveryJob::Turn(snapshot), scope, &mut reader)
    });
    let request = peer.next_write().await;
    assert_eq!(request.value["method"], "thread/read");
    assert!(
        request
            .receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    state
        .bridge
        .run(|store| {
            let _: i64 = store.read()?.query_row("SELECT 1", [], |row| row.get(0))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!reconcile.is_finished());
    peer.reply(
        &request.value,
        json!({"thread":{"id":"fake-thread-0","status":{"type":"active"}}}),
    )
    .await;
    assert!(matches!(
        reconcile.await.unwrap().unwrap(),
        RecoveryOutcome::Unconfirmed
    ));
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_reply_failure_cannot_publish_a_positive_receipt() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "failed-journal");
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    state.bridge.run(|s| s.transaction_with(|tx| { tx.execute_batch("CREATE TRIGGER fail_journal BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'account-worker:journal:%' BEGIN SELECT RAISE(ABORT,'journal failed'); END;")?; Ok(()) })).await.unwrap();
    peer.frame(json!({"id":wire["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    assert_eq!(waiter.await.unwrap().unwrap_err(), PortError::Unknown);
    assert!(worker.completion().await.is_err());
    state
        .bridge
        .run(|s| {
            s.transaction_with(|tx| {
                tx.execute_batch("DROP TRIGGER fail_journal")?;
                Ok(())
            })
        })
        .await
        .unwrap();
    // Same generation replay has the same operation receipt but no committed cursor.
    let peer = Peer::new();
    let mut replacement = peer.worker(state.bridge.clone()).await;
    assert_eq!(
        replacement
            .retained_session()
            .request(call)
            .await
            .unwrap()
            .result,
        RequestResult::Unknown
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_committed_callback_keeps_exact_response_and_sends_once() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let frame = Frame::Response {
        id: RequestId::String("native-callback".into()),
        body: ResponseBody::Result(json!({"answer":"approved"})),
        fields: Default::default(),
    };
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let sender = port.clone();
    let first = frame.clone();
    let task = tokio::spawn(async move {
        sender
            .submit_committed_frame(
                sender.scope(),
                "canonical-operation".into(),
                first,
                deadline,
            )
            .await
    });
    let wire = peer.next_write().await;
    assert_eq!(wire.value["id"], "native-callback");
    assert_eq!(wire.value["result"], json!({"answer":"approved"}));
    assert!(
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: Some(42),
                duplicate: false,
            })
            .is_ok()
    );
    assert_eq!(task.await.unwrap().unwrap(), WriteOutcome::Written);
    assert_eq!(
        port.submit_committed_frame(port.scope(), "canonical-operation".into(), frame, deadline)
            .await
            .unwrap(),
        WriteOutcome::Written
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_deadline_during_intent_never_writes_after_expiry() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("retained_submitted".into());
    let mut call = request(&state, &port, "expires-in-queue");
    call.deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(2)).into_std();
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    state.bridge.faults.paused.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert_eq!(waiter.await.unwrap().unwrap_err(), PortError::Unknown);
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_waiters();
    worker_barrier(&worker).await;
    call.deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    assert_eq!(
        port.request(call).await.unwrap().result,
        RequestResult::NotSent
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_response_before_write_receipt_stays_positive() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "reply-before-write");
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.next_write().await;
    let cursor = peer
        .frame(json!({"id":wire.value["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    assert!(
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false
            })
            .is_ok()
    );
    peer.acknowledged(cursor).await;
    assert!(matches!(
        waiter.await.unwrap().unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(matches!(
        port.request(call).await.unwrap().result,
        RequestResult::Reply(_)
    ));
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn claude_adapter_uses_retained_scoped_control_and_invalidates_on_stop() {
    use crate::claude_controls::service::{ControlScope, ControlSession};
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let session = std::sync::Arc::new(worker.claude_session().unwrap());
    let scope = ControlScope {
        agent: state.agent.clone(),
        epoch: state.epoch.try_into().unwrap(),
        request_id: Some("stop-task-request".into()),
    };
    let copy = session.clone();
    let call_scope = scope.clone();
    let task = tokio::spawn(async move {
        copy.call_scoped(
            call_scope,
            "claude/stopTask",
            json!({"threadId":"fake-thread-0","taskId":"task"}),
        )
        .await
    });
    let request = peer.write("claude/stopTask").await;
    peer.reply(&request, json!({"status":"stopped"})).await;
    assert_eq!(task.await.unwrap().unwrap(), json!({"status":"stopped"}));
    assert_eq!(
        session
            .call_scoped(
                scope.clone(),
                "claude/stopTask",
                json!({"threadId":"fake-thread-0","taskId":"task"})
            )
            .await
            .unwrap(),
        json!({"status":"stopped"})
    );
    worker.shutdown().await.unwrap();
    assert!(
        session
            .call_scoped(scope, "claude/state", json!({"threadId":"fake-thread-0"}))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn claude_adapter_distinguishes_not_sent_from_unknown() {
    use crate::claude_controls::service::ControlSession;
    use crate::claude_controls::{ControlError, service::ControlScope};
    use crate::claude_runtime::RetainedClaudeSession;

    for (request_id, receipt, expected) in [
        ("not-sent", CommandReceipt::NotSent, "not-sent"),
        ("unknown", CommandReceipt::Unknown, "unknown"),
    ] {
        let state = State::new().await;
        let mut peer = Peer::new();
        peer.claude = true;
        let mut worker = peer.worker(state.bridge.clone()).await;
        let session = RetainedClaudeSession::new(std::sync::Arc::new(
            crate::account_worker::retained::consumers::ClaudePort::new(worker.retained_session())
                .unwrap(),
        ));
        let scope = ControlScope {
            agent: state.agent.clone(),
            epoch: state.epoch.try_into().unwrap(),
            request_id: Some(request_id.into()),
        };
        let task = tokio::spawn(async move {
            session
                .call_scoped(
                    scope,
                    "claude/stopTask",
                    json!({"threadId":"fake-thread-0","taskId":"task"}),
                )
                .await
        });
        let wire = peer.next_write().await;
        assert_eq!(wire.value["method"], "claude/stopTask");
        let _ignored = wire.receipt.send(receipt);
        match expected {
            "not-sent" => assert!(matches!(task.await.unwrap(), Err(ControlError::NotSent))),
            _ => assert!(matches!(task.await.unwrap(), Err(ControlError::Unknown))),
        }
        worker.shutdown().await.unwrap();
    }
}
