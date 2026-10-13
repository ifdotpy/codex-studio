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
