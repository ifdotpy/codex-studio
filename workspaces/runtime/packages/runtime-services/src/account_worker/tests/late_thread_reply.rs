//! A thread/resume reply that arrives after its deadline settled the attempt
//! held must not end the account worker or send any request twice. The first
//! unknown resume is retried once (see `unknown_thread_retry`), so the attempt
//! is held at the deadline of that retry.
use super::*;

/// Pass one 60 s reply deadline, then prove the worker consumed that outcome.
async fn pass_reply_deadline(peer: &mut Peer) {
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    tokio::time::resume();
    // A journal barrier proves the deadline outcome was consumed.
    let cursor = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    peer.acknowledged(cursor).await;
}

/// Pass the deadline of the first resume and of its single retry, so the
/// attempt is held. Returns the retry, the request that the hold names.
async fn hold_after_retry(peer: &mut Peer) -> Value {
    pass_reply_deadline(peer).await;
    let retry = peer.write("thread/resume").await;
    pass_reply_deadline(peer).await;
    retry
}

#[tokio::test]
async fn late_thread_resume_reply_after_held_settlement_keeps_the_worker() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    peer.write("thread/resume").await;
    let resume = hold_after_retry(&mut peer).await;
    let held = state.record().await;
    assert_eq!(
        held["error"], "Provider thread resume outcome is unknown; input remains pending.",
        "{held}"
    );
    assert_ne!(state.event_status("input").await, "delivered");

    let reply = peer
        .frame(json!({"id":resume["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    peer.acknowledged_or_worker_exits(reply, &mut worker).await;
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the late reply sends nothing on its own"
    );
    worker.shutdown().await.unwrap();
}

/// The state an earlier build left: the late reply prepared the held attempt
/// and saved a durable turn/start effect for it, then the agent's attempt was
/// cleared. A restart runs that effect; it must settle as stale, and a new
/// message must start the agent with its input sent once.
#[tokio::test]
async fn saved_start_effect_of_a_held_attempt_does_not_end_the_restarted_worker() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    peer.write("thread/resume").await;
    let resume = hold_after_retry(&mut peer).await;
    worker.shutdown().await.unwrap();

    let operation = resume["id"].as_str().unwrap().to_owned();
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                let saved = store::receipt(tx, &operation, "submitted")?
                    .expect("the held thread request keeps its receipt");
                let pending: Pending = serde_json::from_value(saved["context"].clone()).unwrap();
                tx.execute(
                    "DELETE FROM runtime_operation_receipts WHERE id LIKE ?1",
                    [format!("account-worker:{operation}:%")],
                )?;
                store::persist_effect(tx, "default", &Effect::Submit(pending.intent))?;
                let mut agent =
                    AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.unwrap();
                agent.extra.remove("startAttempt");
                agent.extra.insert("autoWake".into(), json!(false));
                agent.status = studio_runtime_store::AgentStatus::Interrupted;
                AgentRepository::put(tx, &agent)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();

    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut next = Peer::new();
    let mut replacement = next.worker(bridge).await;
    worker_barrier(&replacement).await;
    assert!(
        next.writes.try_recv().is_err(),
        "the stale effect sends nothing"
    );

    // A new message is the user's decision to continue.
    let agent_id = state.agent.clone();
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
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
    state.enqueue("again").await;
    state.wake(&replacement).await;
    let start = next.write_turn_start().await;
    assert!(
        start["params"]["input"].to_string().contains("input"),
        "{start}"
    );
    next.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker_barrier(&replacement).await;
    assert!(next.writes.try_recv().is_err(), "the input is sent once");
    replacement.shutdown().await.unwrap();
}
