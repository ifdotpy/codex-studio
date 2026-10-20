//! A disconnect receipt stays on an agent after its turn ends. A backend
//! restart must not restore that receipt's settled attempt as a live one: the
//! next provider generation then interrupted the finished agent with
//! "Codex disconnected" and told its parent again (live reviewers 8388eb70,
//! 6f209a16 and d0e0015f, and agent 50f9a796, 20 October 2026).
use super::*;

/// The state the restart finds.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// Idle after its turn ended, without `startAttempt`, with the old receipt.
    Idle,
    /// As an earlier build left it: the receipt's attempt restored as live and
    /// the agent interrupted for it by a later provider generation.
    Interrupted,
}

/// Change the fixture agent record in one transaction.
async fn edit(state: &State, change: impl FnOnce(&mut AgentRecord) + Send + 'static) {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                change(&mut row);
                AgentRepository::put(tx, &row)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
}

/// Leave the receipt that names `stale` on the finished agent, in `shape`.
async fn leave_stale_receipt(state: &State, shape: Shape, thread: &str, stale: Value) {
    let epoch = state.epoch;
    let thread = thread.to_owned();
    edit(state, move |row| {
        row.extra.insert(
            "disconnectRecovery".into(),
            json!({"accountKey":"default","epoch":epoch,"autoWake":true,"threadId":thread,
                "turnId":stale["turnId"],"connectionId":null,"supervisor":null,"at":1.0,
                "startAttempt":stale}),
        );
        row.extra.remove("startAttempt");
        if shape == Shape::Interrupted {
            row.status = studio_runtime_store::AgentStatus::Interrupted;
            row.extra.insert(
                "error".into(),
                json!(studio_runtime_store::PROVIDER_DISCONNECTED_TURN_ERROR),
            );
            row.extra.insert("startAttempt".into(), stale);
        }
    })
    .await;
}

/// A new backend process attaches to the same provider generation, as the
/// supervisor keeps it across a backend restart.
async fn restart(state: &State) -> (Peer, AccountHandle) {
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let peer = Peer::new();
    let worker = peer.worker(bridge).await;
    worker_barrier(&worker).await;
    (peer, worker)
}

/// The finished agent survives a restart, the end of that provider
/// generation, and one more restart without a change, a provider request, or
/// a parent notice.
async fn assert_finished_across_restarts(state: &State, parent: &str, answer: &Value) {
    let (mut peer, mut worker) = restart(state).await;
    let record = state.record().await;
    assert_eq!(record["status"], "completed", "{record}");
    assert!(record["error"].is_null(), "{record}");
    assert!(record["startAttempt"].is_null(), "{record}");
    assert_eq!(&record["lastAnswer"], answer, "{record}");
    provider_exits(&peer, &mut worker).await;
    let record = state.record().await;
    assert_eq!(record["status"], "completed", "{record}");
    assert!(record["error"].is_null(), "{record}");
    assert!(peer.writes.try_recv().is_err(), "nothing is sent");
    assert_eq!(child_stop_notices(state, parent).await, 0);

    let (mut next, mut replacement) = restart(state).await;
    let record = state.record().await;
    assert_eq!(record["status"], "completed", "{record}");
    assert!(record["error"].is_null(), "{record}");
    assert!(next.writes.try_recv().is_err(), "nothing is sent");
    assert_eq!(child_stop_notices(state, parent).await, 0);
    replacement.shutdown().await.unwrap();
}

/// Agent 50f9a796: the receipt names a turn that completed, and later turns
/// completed after it, so the attempt is not that of the last completed turn.
async fn ordinary_turn_older_than_the_last_completed_turn(shape: Shape) {
    let state = State::new().await;
    let parent = adopt_parent(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let stale = state.record().await["startAttempt"].clone();
    assert_eq!(stale["turnId"], "turn", "{stale}");
    peer.completed().await;
    state.enqueue("second").await;
    let start = state.start(&mut peer, &worker).await;
    peer.reply(&start, json!({"turn":{"id":"later-turn"}}))
        .await;
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
            "turn":{"id":"later-turn","status":"completed"}}}),
        )
        .await;
    peer.acknowledged(cursor).await;
    let done = state.record().await;
    assert_eq!(done["lastCompletedTurn"], "later-turn", "{done}");
    assert_eq!(done["status"], "completed", "{done}");
    worker.shutdown().await.unwrap();

    leave_stale_receipt(&state, shape, "fake-thread-0", stale).await;
    assert_finished_across_restarts(&state, &parent, &done["lastAnswer"]).await;
}

#[tokio::test]
async fn an_idle_agent_keeps_its_later_completed_turn_across_restarts() {
    ordinary_turn_older_than_the_last_completed_turn(Shape::Idle).await;
}

#[tokio::test]
async fn an_agent_interrupted_for_an_older_completed_turn_returns_to_completed() {
    ordinary_turn_older_than_the_last_completed_turn(Shape::Interrupted).await;
}

/// The live reviewers: the review attempt was saved before its acceptance
/// bound `startAttempt.turnId`, so only its execution receipt proves its end.
async fn native_review_attempt_without_a_turn(shape: Shape) {
    let state = State::new().await;
    let parent = adopt_parent(&state).await;
    let epoch = state.epoch;
    let attempt = json!({"id":"review-attempt","epoch":epoch,"events":[],"action":"review",
        "actionRequestId":"native-review:child:0","submitted":true,"accountKey":"default",
        "connectionGeneration":1,"connectionId":"connection","threadId":"review-thread",
        "requestTicket":"retained:default:review","created":1.0});
    let saved = attempt.clone();
    edit(&state, move |row| {
        for (key, value) in [
            ("threadId", json!("review-thread")),
            ("turnId", json!("review-turn")),
            ("status", json!("interrupted")),
            ("autoWake", json!(false)),
            ("inFlight", json!(false)),
            ("turnEpoch", json!(epoch)),
            (
                "error",
                json!(studio_runtime_store::PROVIDER_DISCONNECTED_TURN_ERROR),
            ),
            ("startAttempt", saved.clone()),
            (
                "disconnectRecovery",
                json!({"accountKey":"default","epoch":epoch,"autoWake":true,
                    "threadId":"review-thread","startAttempt":saved}),
            ),
            (
                "nativeReview",
                json!({"target":{"type":"custom","instructions":"Check"},
                    "requestId":"lead:review:r1","actorId":"lead","args":{},
                    "status":"started","response":{}}),
            ),
        ] {
            row.extra.insert(key.into(), value);
        }
    })
    .await;
    // The replayed review completion settles the cut-off review.
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer
        .frame(
            json!({"method":"item/completed","params":{"threadId":"review-thread",
            "turnId":"review-turn","item":{"id":"review-item","type":"exitedReviewMode",
            "review":"No defects found."}}}),
        )
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"review-thread",
            "turn":{"id":"review-turn","status":"completed"}}}),
        )
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
    let done = state.record().await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["lastCompletedTurn"], "review-turn", "{done}");
    assert!(done["turnId"].is_null(), "{done}");
    worker.shutdown().await.unwrap();

    leave_stale_receipt(&state, shape, "review-thread", attempt).await;
    assert_finished_across_restarts(&state, &parent, &done["lastAnswer"]).await;
}

#[tokio::test]
async fn an_idle_reviewer_stays_completed_across_restarts() {
    native_review_attempt_without_a_turn(Shape::Idle).await;
}

#[tokio::test]
async fn a_reviewer_interrupted_for_its_completed_review_returns_to_completed() {
    native_review_attempt_without_a_turn(Shape::Interrupted).await;
}

/// A turn that the disconnect really cut off keeps its recovery: the restart
/// restores its attempt, and the replayed completion settles it once.
#[tokio::test]
async fn a_turn_cut_off_by_a_disconnect_still_recovers_after_a_restart() {
    let state = State::new().await;
    let parent = adopt_parent(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    provider_exits(&peer, &mut worker).await;
    let cut = state.record().await;
    assert_eq!(cut["status"], "interrupted", "{cut}");
    assert!(cut["startAttempt"].is_null(), "{cut}");
    let attempt = cut["disconnectRecovery"]["startAttempt"]["id"].clone();
    assert!(attempt.is_string(), "{cut}");
    assert_eq!(child_stop_notices(&state, &parent).await, 1);

    let (mut next, mut replacement) = restart(&state).await;
    let restored = state.record().await;
    assert_eq!(restored["startAttempt"]["id"], attempt, "{restored}");
    assert_ne!(restored["status"], "completed", "{restored}");
    // The supervisor keeps the generation's journal, so its cursor continues.
    next.cursor = peer.cursor;
    let cursor = next
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
            "turn":{"id":"turn","status":"completed"}}}),
        )
        .await;
    next.acknowledged(cursor).await;
    let done = state.record().await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["lastCompletedTurn"], "turn", "{done}");
    assert_eq!(child_stop_notices(&state, &parent).await, 1);
    while let Ok(wire) = next.writes.try_recv() {
        assert_ne!(wire.value["method"], "turn/start", "{}", wire.value);
    }
    replacement.shutdown().await.unwrap();
}
