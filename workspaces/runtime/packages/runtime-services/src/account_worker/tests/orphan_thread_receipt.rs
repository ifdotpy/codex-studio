//! A thread request whose `submitted` commit returned but which was never
//! written must not hold its agent forever, and must never be sent again.
use super::*;
use crate::account_worker::retained::storage::TransportRequestExt;
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus};

/// Outstanding account thread requests, selected from the pending provider outbox.
async fn unresolved_thread_obligations(state: &State) -> Vec<String> {
    state.bridge.run(|store|async move {
        let db=store.test_reader()?;
        let mut query=db.prepare("SELECT id,payload FROM runtime_outbox WHERE account=?1 AND kind=?2 AND status IN (?3,?4,?5) ORDER BY id")?;
        let rows=query.query_map(rusqlite::params!["default",OutboxKind::ProviderCall,OutboxStatus::Pending,OutboxStatus::Submitted,OutboxStatus::Unknown],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
        let mut operations=Vec::new();
        for (id,raw) in rows {
            let payload:store::ProviderCall=serde_json::from_str(&raw).map_err(StoreError::from)?;
            if matches!(payload,store::ProviderCall::Account(call) if matches!(call.context.method,Method::ThreadStart|Method::ThreadResume)) {operations.push(id);}
        }
        Ok(operations)
    }).await.unwrap()
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
    assert_eq!(unresolved_thread_obligations(state).await.len(), 1);
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
                agent.auto_wake = Some(true);
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
            faults.shutdown_phase_changed.notified().await;
        }
        faults.release_after.notify_one();
    });
    worker.shutdown().await.unwrap();
    release.await.unwrap();
    assert!(peer.writes.try_recv().is_err(), "nothing was written");
    assert!(
        unresolved_thread_obligations(&state).await.is_empty(),
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
    let orphan = unresolved_thread_obligations(&state).await;
    let old_call =
        store::account_submission(&state.bridge.store.test_reader().unwrap(), &orphan[0])
            .unwrap()
            .unwrap();
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
                    agent.in_flight = Some(false);
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
    if shape != Orphan::MessageBeforeRestart {
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
        assert_eq!(unresolved_thread_obligations(&state).await, orphan);
        let db = state.bridge.store.test_reader().unwrap();
        for id in &orphan {
            let count: i64 = db
                .query_row(
                    "SELECT count(*) FROM request_results WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "restart alone must not abandon the request");
        }
        drop(db);
        send_message(&state, "continue-message", "user").await;
    }

    state.wake(&replacement).await;
    let resume = next.write("thread/resume").await;
    let resume_id = resume["id"].as_str().unwrap();
    assert!(
        unresolved_thread_obligations(&state)
            .await
            .iter()
            .all(|id| !orphan.contains(id))
    );
    let db = state.bridge.store.test_reader().unwrap();
    for id in &orphan {
        let raw: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        let saved: studio_runtime_store::request_results::ResultEnvelope<
            studio_runtime_store::request_results::AbandonedResult,
        > = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            saved.reply,
            studio_runtime_store::request_results::AbandonedResult::Abandoned {
                outcome: studio_runtime_store::request_results::AbandonedOutcome::Unknown
            }
        );
    }
    drop(db);
    assert!(
        !orphan.iter().any(|receipt| receipt.contains(resume_id)),
        "the unknown request is never sent again: {resume_id}"
    );
    if shape == Orphan::MessageBeforeRestart {
        let before = state.record().await["startAttempt"].clone();
        let body = json!({"thread":{"id":"old-thread"}});
        let (settled, fault) = store::outcome(
            state.bridge.store.clone(),
            "default".into(),
            Arc::new(test_resources().unwrap()),
            Some(Arc::new(old_call.context.clone())),
            SessionOutcome::Resolved {
                ticket: old_call.request.recovered().ticket,
                outcome: RequestOutcome::Reply {
                    body: ResponseBody::Result(body.clone()),
                    late: true,
                },
            },
            false,
        )
        .await
        .unwrap();
        assert!(settled.retired && settled.effect.is_none() && fault.is_none());
        assert_eq!(
            state.record().await["startAttempt"],
            before,
            "late reply cannot change the new attempt"
        );
        let db = state.bridge.store.test_reader().unwrap();
        let (_, source) =
            store::execution_admission(&db, &old_call.context.intent.attempt_id).unwrap();
        let signature = json!({"Result":body}).to_string();
        let token = source.derive_result(
            "account-call-late",
            &[&old_call.context.operation],
            &signature,
        );
        assert!(
            studio_runtime_store::request_results::RequestResults::admitted_result(&db, &token)
                .unwrap()
                .is_some(),
            "exact late result is retained"
        );
    }
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
    assert!(unresolved_thread_obligations(&state).await.is_empty());
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

/// An explicit later message supersedes an unknown restarted thread/start.
#[tokio::test]
async fn unwritten_thread_start_continues_after_a_message() {
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
    let orphan = unresolved_thread_obligations(&state).await;
    assert!(orphan[0].ends_with(":thread/start"), "{orphan:?}");

    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut next = Peer::new();
    let mut replacement = next.worker(bridge).await;
    worker_barrier(&replacement).await;
    send_message(&state, "continue-message", "user").await;
    state.wake(&replacement).await;
    let start = next.write("thread/start").await;
    assert!(
        !orphan
            .iter()
            .any(|id| id.contains(start["id"].as_str().unwrap()))
    );
    assert!(
        unresolved_thread_obligations(&state)
            .await
            .iter()
            .all(|id| !orphan.contains(id))
    );
    next.reply(&start, json!({"thread":{"id":"new-thread"}}))
        .await;
    replacement.shutdown().await.unwrap();
}
