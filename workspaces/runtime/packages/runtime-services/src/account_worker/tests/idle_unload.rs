//! Idle threads unload at their deadline; state changes re-evaluate them and
//! disabled unloading leaves no scheduled scan.
use super::*;
use std::time::Duration;
use studio_runtime_store::{
    AgentId, AgentRepository, AgentStatus, IDLE_SESSION_UNLOAD_SETTING,
    OrchestrationSettingsRepository,
};

const THREAD: &str = "fake-thread-0";
/// A fixture protocol step must make progress without waiting for the production timeout.
const PROGRESS_GUARD: Duration = Duration::from_secs(5);

async fn progress<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(PROGRESS_GUARD, future)
        .await
        .expect("idle protocol progress deadline")
}
/// Longer than the longest accepted unload delay.
const BEYOND_ANY_DELAY: Duration = Duration::from_secs((IDLE_SESSION_UNLOAD_SETTING.max + 1) * 60);

async fn sweep(worker: &AccountHandle) {
    worker
        .send(AccountCommand::IdleStateChange(tokio::time::Instant::now()))
        .await
        .unwrap();
    progress(worker_barrier(worker)).await;
    // Advance the worker's actual clock, then resume before database work can
    // cause Tokio to auto-advance unrelated protocol deadlines.
    tokio::time::pause();
    tokio::time::advance(BEYOND_ANY_DELAY).await;
    tokio::time::resume();
}

async fn enable_unload(state: &State, peer: &mut Peer) {
    set_delay(state, IDLE_SESSION_UNLOAD_SETTING.default).await;
    loaded(peer).await;
}

async fn answer(peer: &mut Peer, method: &str, params: Value, result: Value) {
    let request = progress(peer.write(method)).await;
    assert_eq!(request["params"], params, "{method}");
    progress(peer.reply(&request, result)).await;
}

async fn loaded(peer: &mut Peer) {
    let wire = tokio::time::timeout(PROGRESS_GUARD, peer.writes.recv())
        .await
        .expect("loaded-thread snapshot is event-driven")
        .expect("provider session remains open");
    assert_eq!(wire.value["method"], "thread/loaded/list");
    assert_eq!(wire.value["params"], json!({"limit": 100}));
    let _ignored = wire.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    progress(peer.reply(&wire.value, json!({"data": [THREAD], "nextCursor": null}))).await;
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

async fn set_thread_in_use(state: &State, in_use: bool) {
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("test agent exists");
                    agent.extra.insert("threadId".into(), json!(THREAD));
                    agent.extra.insert("inFlight".into(), json!(in_use));
                    agent
                        .extra
                        .insert("turnId".into(), json!(if in_use { "busy" } else { "" }));
                    agent.status = if in_use {
                        AgentStatus::Running
                    } else {
                        AgentStatus::Idle
                    };
                    AgentRepository::put(tx, &agent)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn set_unloaded_agent_in_use(state: &State, in_use: bool) {
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("test agent exists");
                    agent
                        .extra
                        .insert("threadId".into(), json!("unloaded-thread"));
                    agent.extra.insert("inFlight".into(), json!(in_use));
                    agent
                        .extra
                        .insert("turnId".into(), json!(if in_use { "busy" } else { "" }));
                    agent.status = if in_use {
                        AgentStatus::Running
                    } else {
                        AgentStatus::Idle
                    };
                    AgentRepository::put(tx, &agent)
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
    // Acknowledge the single attach inventory before sending later provider events.
    enable_unload(&state, &mut peer).await;

    // In-flight work never starts a periodic inventory or an unload request.
    sweep(&worker).await;
    progress(worker_barrier(&worker)).await;
    assert!(peer.writes.try_recv().is_err());
    progress(peer.completed()).await;

    // Completion makes the already-known loaded thread eligible for its idle clock.
    sweep(&worker).await;
    idle_checks(&mut peer).await;
    // A failed unsubscribe proves nothing; the thread stays loaded.
    let unsubscribe = progress(peer.write("thread/unsubscribe")).await;
    let cursor = peer
        .frame(json!({"id": unsubscribe["id"], "error": {"code": -32000, "message": "busy"}}))
        .await;
    progress(peer.acknowledged(cursor)).await;
    worker_barrier(&worker).await;

    sweep(&worker).await;
    idle_checks(&mut peer).await;
    let unsubscribe = progress(peer.write("thread/unsubscribe")).await;
    assert_eq!(unsubscribe["params"], json!({"threadId": THREAD}));
    // The provider reports the unload; an idle thread's unload is no failure.
    for notification in [
        json!({"method": "thread/status/changed",
            "params": {"threadId": THREAD, "status": {"type": "notLoaded"}}}),
        json!({"method": "thread/closed", "params": {"threadId": THREAD}}),
    ] {
        let cursor = peer.frame(notification).await;
        progress(peer.acknowledged(cursor)).await;
    }
    progress(peer.reply(&unsubscribe, json!({"status": "unsubscribed"}))).await;
    // With no loaded thread left in use, the worker closes its session.
    progress(worker.completion()).await.unwrap();

    // A message that arrives while no session runs queues the agent; the new
    // provider generation must not treat the completed turn as unresolved.
    state.enqueue("again").await;
    let mut peer = Peer::identity("default", 2, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    answer(
        &mut peer,
        "thread/loaded/list",
        json!({"limit": 100}),
        json!({"data": [], "nextCursor": null}),
    )
    .await;
    progress(worker_barrier(&worker)).await;
    assert_eq!(state.record().await["status"], "queued");
    state.wake(&worker).await;
    let resume = progress(peer.write("thread/resume")).await;
    assert_eq!(
        resume["params"]["threadId"], THREAD,
        "the same thread reloads"
    );
    peer.reply(&resume, json!({"thread": {"id": THREAD}})).await;
    let next = progress(peer.write("turn/start")).await;
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
async fn input_arriving_during_native_checks_prevents_unsubscribe() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    progress(peer.completed()).await;
    enable_unload(&state, &mut peer).await;

    sweep(&worker).await;
    answer(
        &mut peer,
        "thread/read",
        json!({"threadId": THREAD, "includeTurns": false}),
        json!({"thread": {"id": THREAD, "status": {"type": "idle"}}}),
    )
    .await;
    answer(
        &mut peer,
        "thread/backgroundTerminals/list",
        json!({"threadId": THREAD, "limit": 100}),
        json!({"data": [], "nextCursor": null}),
    )
    .await;
    let queue = progress(peer.write("thread/queue/list")).await;
    state.enqueue("arrived-during-idle-checks").await;
    progress(peer.reply(&queue, json!({"data": [], "nextCursor": null}))).await;
    progress(worker_barrier(&worker)).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "new input keeps its thread loaded"
    );
    progress(worker.shutdown()).await.unwrap();
}

#[tokio::test]
async fn a_loaded_thread_blocked_when_enabled_gets_a_deadline_after_durable_completion() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    progress(peer.completed()).await;
    set_thread_in_use(&state, true).await;
    enable_unload(&state, &mut peer).await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err());

    set_thread_in_use(&state, false).await;
    worker
        .send(AccountCommand::IdleStateChange(tokio::time::Instant::now()))
        .await
        .unwrap();
    sweep(&worker).await;
    idle_checks(&mut peer).await;
    let unsubscribe = progress(peer.write("thread/unsubscribe")).await;
    assert_eq!(unsubscribe["params"], json!({"threadId": THREAD}));
    progress(peer.reply(&unsubscribe, json!({"status": "unsubscribed"}))).await;
    progress(worker.completion()).await.unwrap();
}

#[tokio::test]
async fn a_durable_account_blocker_wakes_the_released_process_clock_when_cleared() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    progress(peer.completed()).await;
    enable_unload(&state, &mut peer).await;
    set_unloaded_agent_in_use(&state, true).await;

    sweep(&worker).await;
    idle_checks(&mut peer).await;
    let unsubscribe = progress(peer.write("thread/unsubscribe")).await;
    assert_eq!(unsubscribe["params"], json!({"threadId": THREAD}));
    progress(peer.reply(&unsubscribe, json!({"status": "unsubscribed"}))).await;
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the account-wide durable owner keeps the process alive after its only loaded thread is released"
    );

    set_unloaded_agent_in_use(&state, false).await;
    worker
        .send(AccountCommand::IdleStateChange(tokio::time::Instant::now()))
        .await
        .unwrap();
    sweep(&worker).await;
    progress(worker.completion()).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn claude_idle_close_requires_a_loaded_snapshot_and_native_idle_confirmation() {
    let state = State::new().await;
    let mut peer = Peer::new();
    peer.claude = true;
    let resources = super::claude_preparation::claude_resources(&state);
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    progress(peer.completed()).await;
    enable_unload(&state, &mut peer).await;

    // Claude goes through the same authoritative loaded snapshot. A busy
    // native thread must keep the account session alive despite durable idle.
    sweep(&worker).await;
    answer(
        &mut peer,
        "thread/read",
        json!({"threadId": THREAD, "includeTurns": false}),
        json!({"thread": {"id": THREAD, "status": {"type": "running"}}}),
    )
    .await;
    assert!(
        peer.writes.try_recv().is_err(),
        "a busy Claude thread is not unsubscribed and its session is not closed"
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
    progress(peer.completed()).await;
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the sweep sends nothing when unloading is off"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_setting_and_agent_changes_wake_idle_reconciliation() {
    let state = State::new().await;
    let mut settings = state.bridge.idle_setting_changes();
    let mut changes = state.bridge.idle_state_changes();

    set_delay(&state, IDLE_SESSION_UNLOAD_SETTING.default).await;
    tokio::time::timeout(PROGRESS_GUARD, settings.changed())
        .await
        .expect("setting commit wakes idle workers")
        .expect("idle change watch remains open");

    state.enqueue("new input").await;
    tokio::time::timeout(PROGRESS_GUARD, changes.changed())
        .await
        .expect("agent and event commits wake idle workers")
        .expect("idle change watch remains open");
}
