//! A thread request whose `submitted` commit returned but which was never
//! written must not hold its agent forever, and must never be sent again.
use super::*;

/// Unresolved thread request receipts of the account.
async fn unresolved_thread_receipts(state: &State) -> Vec<String> {
    state
        .bridge
        .run(|store| async move {
            let connection = store.test_reader()?;
            let mut statement = connection.prepare(
                "SELECT r.id FROM runtime_operation_receipts r WHERE r.id GLOB 'account-worker:account:default:*:thread/*:submitted' \
                 AND NOT EXISTS(SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':resolved')",
            )?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .await
        .unwrap()
}

/// Commit the agent's thread request `submitted` receipt and stop the worker
/// at the commit's return, before the worker can write the request.
async fn commit_thread_request_without_writing(state: &State, worker: &AccountHandle) {
    *state.bridge.faults.pause_after_operation.lock().unwrap() = Some("submitted".into());
    state.wake(worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        state.bridge.faults.paused_after.notified(),
    )
    .await
    .expect("the thread request's submitted commit returns");
    *state.bridge.faults.pause_after_operation.lock().unwrap() = None;
    assert_eq!(unresolved_thread_receipts(state).await.len(), 1);
}

/// What a send does: wake the agent and queue a message of `kind`.
async fn send_message(state: &State, id: &str, kind: &'static str) {
    let agent_id = state.agent.clone();
    let id = id.to_owned();
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                let mut agent =
                    AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.unwrap();
                agent.extra.insert("autoWake".into(), json!(true));
                agent.extra.insert("error".into(), Value::Null);
                agent.extra.remove("nativeFailureHold");
                AgentRepository::put(tx, &agent)?;
                crate::event_write::enqueue_event(tx, &agent, id, kind, "continue".into(), now())
                    .unwrap();
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn shutdown_settles_a_committed_thread_request_it_never_wrote() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    commit_thread_request_without_writing(&state, &worker).await;
    let faults = state.bridge.faults.clone();
    let release = tokio::spawn(async move {
        while faults.current_shutdown_phase() == "serving" {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        faults.release_after.notify_one();
    });
    worker.shutdown().await.unwrap();
    release.await.unwrap();
    assert!(peer.writes.try_recv().is_err(), "nothing was written");
    assert!(
        unresolved_thread_receipts(&state).await.is_empty(),
        "the unwritten request is settled as not sent"
    );
    assert_eq!(state.event_status("input").await, "pending");

    let mut next = Peer::new();
    let mut replacement = next.worker(state.bridge.clone()).await;
    state.wake(&replacement).await;
    let start = next.write_turn_start().await;
    next.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker_barrier(&replacement).await;
    assert!(next.writes.try_recv().is_err(), "the input is sent once");
    replacement.shutdown().await.unwrap();
}

#[derive(Clone, Copy, PartialEq)]
enum Orphan {
    /// A stale disconnect record names the agent's last completed turn.
    StaleRecord,
    /// An earlier build also restored that turn's attempt as the live one.
    RestoredAttempt,
    /// As `RestoredAttempt`, and a message came before the restart.
    MessageBeforeRestart,
}

/// The live shapes of agents ecd0da59 and d786b33f: a crash left a thread
/// receipt without its write. After a restart the request is never sent again;
/// a message sent after it began continues the agent.
async fn unwritten_thread_receipt_after_restart(shape: Orphan) {
    let state = State::new().await;
    // A finished turn whose attempt the stale disconnect record names.
    let mut first = Peer::new();
    let mut worker = first.worker(state.bridge.clone()).await;
    let start = state.start(&mut first, &worker).await;
    first.accepted(&start).await;
    let finished = state.record().await["startAttempt"].clone();
    assert_eq!(finished["turnId"], "turn", "{finished}");
    first.completed().await;
    assert_eq!(state.record().await["lastCompletedTurn"], "turn");
    worker.shutdown().await.unwrap();

    state.enqueue("second").await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    commit_thread_request_without_writing(&state, &worker).await;
    worker.task.abort();
    assert!(worker.completion().await.is_err());
    let orphan = unresolved_thread_receipts(&state).await;
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                let mut agent =
                    AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.unwrap();
                agent.extra.insert(
                    "disconnectRecovery".into(),
                    json!({"accountKey":"default","epoch":epoch,"autoWake":true,
                        "threadId":"fake-thread-0","turnId":"turn",
                        "startAttempt":finished.clone()}),
                );
                if shape != Orphan::StaleRecord {
                    // As the earlier restart left ecd0da59: the unsent claim's
                    // input back in the queue, and the finished attempt live.
                    tx.execute(
                        "UPDATE runtime_events SET status='pending',turn_id=NULL \
                         WHERE agent=?1 AND status='reserved'",
                        [agent_id.as_str()],
                    )?;
                    agent.extra.insert("startAttempt".into(), finished);
                    agent.status = studio_runtime_store::AgentStatus::Queued;
                    agent.extra.insert("inFlight".into(), json!(false));
                }
                AgentRepository::put(tx, &agent)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
    if shape == Orphan::MessageBeforeRestart {
        send_message(&state, "lead-followup", "followup").await;
    }

    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut next = Peer::new();
    let mut replacement = next.worker(bridge).await;
    worker_barrier(&replacement).await;
    if shape == Orphan::MessageBeforeRestart {
        assert!(unresolved_thread_receipts(&state).await.is_empty());
        assert!(state.record().await["startOutcomeHold"].is_null());
    } else {
        let held = state.record().await;
        assert_eq!(
            held["startOutcomeHold"]["reason"],
            store::RECEIPT_PENDING_HOLD,
            "{held}"
        );
        assert_eq!(held["error"], store::RECEIPT_PENDING_HOLD);
        // A wake without a new message keeps the unknown request held.
        state.wake(&replacement).await;
        worker_barrier(&replacement).await;
        assert!(
            next.writes.try_recv().is_err(),
            "nothing is sent while held"
        );
        assert_eq!(unresolved_thread_receipts(&state).await, orphan);
        send_message(&state, "continue-message", "user").await;
    }

    state.wake(&replacement).await;
    let resume = next.write("thread/resume").await;
    let resume_id = resume["id"].as_str().unwrap();
    assert!(
        !orphan.iter().any(|receipt| receipt.contains(resume_id)),
        "the unknown request is never sent again: {resume_id}"
    );
    next.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let start = next.write("turn/start").await;
    assert!(
        start["params"]["input"].to_string().contains("second"),
        "{start}"
    );
    next.reply(&start, json!({"turn":{"id":"second-turn"}}))
        .await;
    assert_eq!(state.event_status("second").await, "delivered");
    worker_barrier(&replacement).await;
    // A later message may follow in its own request; nothing is sent twice.
    while let Ok(wire) = next.writes.try_recv() {
        let id = wire.value["id"].as_str().unwrap_or_default().to_owned();
        assert!(
            !orphan.iter().any(|receipt| receipt.contains(&id))
                && id != resume_id
                && id != start["id"],
            "{}",
            wire.value
        );
        assert!(!wire.value["params"]["input"].to_string().contains("second"));
    }
    assert!(unresolved_thread_receipts(&state).await.is_empty());
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn unwritten_thread_receipt_with_a_stale_record_holds_until_a_message() {
    unwritten_thread_receipt_after_restart(Orphan::StaleRecord).await;
}

#[tokio::test]
async fn unwritten_thread_receipt_with_a_restored_attempt_holds_until_a_message() {
    unwritten_thread_receipt_after_restart(Orphan::RestoredAttempt).await;
}

#[tokio::test]
async fn unwritten_thread_receipt_continues_after_a_message_sent_before_restart() {
    unwritten_thread_receipt_after_restart(Orphan::MessageBeforeRestart).await;
}

/// Another thread/start would create a second thread, so a message does not
/// continue past an unknown one: the agent stays held and nothing is sent.
#[tokio::test]
async fn unwritten_thread_start_stays_held_after_a_message() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut agent =
                        AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.unwrap();
                    agent.extra.remove("threadId");
                    AgentRepository::put(tx, &agent)?;
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    commit_thread_request_without_writing(&state, &worker).await;
    worker.task.abort();
    assert!(worker.completion().await.is_err());
    let orphan = unresolved_thread_receipts(&state).await;
    assert!(orphan[0].ends_with(":thread/start:submitted"), "{orphan:?}");

    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut next = Peer::new();
    let mut replacement = next.worker(bridge).await;
    worker_barrier(&replacement).await;
    send_message(&state, "continue-message", "user").await;
    state.wake(&replacement).await;
    worker_barrier(&replacement).await;
    assert!(next.writes.try_recv().is_err(), "nothing is sent");
    assert_eq!(unresolved_thread_receipts(&state).await, orphan);
    replacement.shutdown().await.unwrap();
}
