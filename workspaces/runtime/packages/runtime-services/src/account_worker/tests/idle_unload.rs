//! An idle thread is unsubscribed, the idle session stops, and the next wake
//! resumes the same thread on a new session. Sweeps run on an injected clock.
use super::*;
use std::time::Duration;
use studio_runtime_store::{IDLE_SESSION_UNLOAD_SETTING, OrchestrationSettingsRepository};

const THREAD: &str = "fake-thread-0";
/// Longer than the longest accepted unload delay.
const BEYOND_ANY_DELAY: Duration = Duration::from_secs((IDLE_SESSION_UNLOAD_SETTING.max + 1) * 60);

async fn sweep(worker: &AccountHandle, at: tokio::time::Instant) {
    worker.send(AccountCommand::IdleSweep(at)).await.unwrap();
}

async fn answer(peer: &mut Peer, method: &str, params: Value, result: Value) {
    let request = peer.write(method).await;
    assert_eq!(request["params"], params, "{method}");
    peer.reply(&request, result).await;
}

async fn loaded(peer: &mut Peer) {
    answer(
        peer,
        "thread/loaded/list",
        json!({"limit": 100}),
        json!({"data": [THREAD], "nextCursor": null}),
    )
    .await;
}

async fn idle_checks(peer: &mut Peer) {
    answer(
        peer,
        "thread/read",
        json!({"threadId": THREAD, "includeTurns": false}),
        json!({"thread": {"id": THREAD, "status": {"type": "idle"}}}),
    )
    .await;
    answer(
        peer,
        "thread/backgroundTerminals/list",
        json!({"threadId": THREAD, "limit": 100}),
        json!({"data": [], "nextCursor": null}),
    )
    .await;
    answer(
        peer,
        "thread/queue/list",
        json!({"threadId": THREAD}),
        json!({"data": [], "nextCursor": null}),
    )
    .await;
}

async fn set_delay(state: &State, minutes: u64) {
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    OrchestrationSettingsRepository::set(tx, IDLE_SESSION_UNLOAD_SETTING, minutes)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn idle_thread_unloads_then_the_session_stops_and_a_new_turn_resumes_it() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let base = tokio::time::Instant::now();

    // A running turn keeps its thread loaded however long it runs.
    sweep(&worker, base).await;
    loaded(&mut peer).await;
    sweep(&worker, base + BEYOND_ANY_DELAY).await;
    loaded(&mut peer).await;
    peer.completed().await;

    // The idle clock starts at the first sweep that finds the thread unused.
    sweep(&worker, base + BEYOND_ANY_DELAY * 2).await;
    loaded(&mut peer).await;
    sweep(&worker, base + BEYOND_ANY_DELAY * 3).await;
    loaded(&mut peer).await;
    idle_checks(&mut peer).await;
    // A failed unsubscribe proves nothing; the thread stays loaded.
    let unsubscribe = peer.write("thread/unsubscribe").await;
    let cursor = peer
        .frame(json!({"id": unsubscribe["id"], "error": {"code": -32000, "message": "busy"}}))
        .await;
    peer.acknowledged(cursor).await;
    worker_barrier(&worker).await;

    sweep(&worker, base + BEYOND_ANY_DELAY * 4).await;
    loaded(&mut peer).await;
    idle_checks(&mut peer).await;
    let unsubscribe = peer.write("thread/unsubscribe").await;
    assert_eq!(unsubscribe["params"], json!({"threadId": THREAD}));
    // The provider reports the unload; an idle thread's unload is no failure.
    for notification in [
        json!({"method": "thread/status/changed",
            "params": {"threadId": THREAD, "status": {"type": "notLoaded"}}}),
        json!({"method": "thread/closed", "params": {"threadId": THREAD}}),
    ] {
        let cursor = peer.frame(notification).await;
        peer.acknowledged(cursor).await;
    }
    peer.reply(&unsubscribe, json!({"status": "unsubscribed"}))
        .await;
    // With no loaded thread left in use, the worker closes its session.
    worker.completion().await.unwrap();

    // A message that arrives while no session runs queues the agent; the new
    // provider generation must not treat the completed turn as unresolved.
    state.enqueue("again").await;
    let mut peer = Peer::identity("default", 2, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    assert_eq!(state.record().await["status"], "queued");
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    assert_eq!(
        resume["params"]["threadId"], THREAD,
        "the same thread reloads"
    );
    peer.reply(&resume, json!({"thread": {"id": THREAD}})).await;
    let next = peer.write("turn/start").await;
    assert_eq!(next["params"]["threadId"], THREAD);
    assert!(
        next["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("again")
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn zero_minutes_never_unloads() {
    let state = State::new().await;
    set_delay(&state, 0).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    peer.completed().await;
    let base = tokio::time::Instant::now();
    for step in 1..=3 {
        sweep(&worker, base + BEYOND_ANY_DELAY * step).await;
    }
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the sweep sends nothing when unloading is off"
    );
    worker.shutdown().await.unwrap();
}
