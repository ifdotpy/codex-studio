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

/// Re-evaluate the idle clocks, then expire them; see [`expire_idle_clocks`].
async fn sweep(worker: &AccountHandle) {
    worker
        .send(AccountCommand::IdleStateChange(tokio::time::Instant::now()))
        .await
        .unwrap();
    expire_idle_clocks(worker).await;
}

/// Make every idle clock of a quiescent worker due. A thread's own clock is
/// zero ([`THREAD_UNLOAD_DELAY`]), so an unused loaded thread starts its
/// checks without any clock advance; call this only when the worker has no
/// provider request in flight, because advancing the clock also expires a
/// pending request's timeout and a barrier would wait behind that request.
async fn expire_idle_clocks(worker: &AccountHandle) {
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

async fn set_agent_in_use(state: &State, thread: &str, in_use: bool) {
    let agent_id = state.agent.clone();
    let thread = thread.to_owned();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("test agent exists");
                    agent.extra.insert("threadId".into(), json!(thread));
                    agent.extra.insert("inFlight".into(), json!(in_use));
                    agent
                        .extra
                        .insert("turnId".into(), json!(if in_use { "busy" } else { "" }));
                    agent.status = if in_use {
                        AgentStatus::Running
                    } else {
                        // Preserve the completed attempt's authoritative terminal status.
                        AgentStatus::Completed
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

    // Completion starts the already-known loaded thread's unload at once.
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

    // An unused loaded thread starts its checks at once.
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
    set_agent_in_use(&state, THREAD, true).await;
    enable_unload(&state, &mut peer).await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err());

    set_agent_in_use(&state, THREAD, false).await;
    worker
        .send(AccountCommand::IdleStateChange(tokio::time::Instant::now()))
        .await
        .unwrap();
    // An unused loaded thread starts its checks at once.
    idle_checks(&mut peer).await;
    let unsubscribe = progress(peer.write("thread/unsubscribe")).await;
    assert_eq!(unsubscribe["params"], json!({"threadId": THREAD}));
    progress(peer.reply(&unsubscribe, json!({"status": "unsubscribed"}))).await;
    expire_idle_clocks(&worker).await;
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
    set_agent_in_use(&state, "unloaded-thread", true).await;

    idle_checks(&mut peer).await;
    let unsubscribe = progress(peer.write("thread/unsubscribe")).await;
    assert_eq!(unsubscribe["params"], json!({"threadId": THREAD}));
    progress(peer.reply(&unsubscribe, json!({"status": "unsubscribed"}))).await;
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the account-wide durable owner keeps the process alive after its only loaded thread is released"
    );

    set_agent_in_use(&state, "unloaded-thread", false).await;
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
    peer.wire = studio_runtime_providers::ProviderKind::Claude.wire();
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

/// A restarted backend can attach behind more provider frames than the event
/// lane holds. Live 2026-10-19: the idle snapshot's `thread/loaded/list`
/// preparation waited for the session, the session waited to hand the next
/// backlog frame to the worker, and the account never acknowledged a frame.
#[tokio::test]
async fn idle_snapshot_is_admitted_while_a_full_backlog_waits_for_the_worker() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("idle_unload_delay".into());
    set_delay(&state, IDLE_SESSION_UNLOAD_SETTING.default).await;
    tokio::time::timeout(PROGRESS_GUARD, state.bridge.faults.paused.notified())
        .await
        .expect("the enabled setting's delay read is held");
    *state.bridge.faults.pause_operation.lock().unwrap() = None;

    // Drain acknowledgements beside the protocol so the fake attachment never
    // waits on its own acknowledgement buffer.
    let mut acks = std::mem::replace(&mut peer.acks, mpsc::channel(1).1);
    let (acknowledged, mut acknowledged_rx) = watch::channel(0u64);
    let drain = tokio::spawn(async move {
        while let Some(cursor) = acks.recv().await {
            acknowledged.send_modify(|seen| *seen = (*seen).max(cursor.0));
        }
    });

    // Fill the worker's event lane, the session's one held frame, and the
    // attachment buffer, so the session can take no command until the worker reads.
    let backlog = LANE_CAPACITY + 1 + PEER_JOURNAL_CAPACITY;
    let entries = peer.entries.clone();
    let clock = peer.clock.clone();
    let generation = peer.generation;
    let start = peer.cursor;
    tokio::time::timeout(PROGRESS_GUARD, async move {
        for cursor in start + 1..=start + backlog as u64 {
            let entry = JournalEntry {
                received_at_ms: clock.now_ms(),
                cursor: EventCursor(cursor),
                generation,
                payload: JournalPayload::Stderr(json!({"data": format!("backlog {cursor}\n")})),
            };
            entries.send(entry).await.unwrap();
        }
    })
    .await
    .expect("the backlog fills the lane while the worker is held");
    peer.cursor = start + backlog as u64;
    state.bridge.faults.release.notify_one();

    let wire = tokio::time::timeout(PROGRESS_GUARD, peer.writes.recv())
        .await
        .expect("the snapshot request is admitted behind the held backlog")
        .expect("provider session remains open");
    assert_eq!(wire.value["method"], "thread/loaded/list");
    let _ignored = wire.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    let reply = peer
        .frame(json!({"id": wire.value["id"], "result": {"data": [THREAD], "nextCursor": null}}))
        .await;
    tokio::time::timeout(
        PROGRESS_GUARD,
        acknowledged_rx.wait_for(|seen| *seen >= reply),
    )
    .await
    .expect("the backlog and the snapshot reply are acknowledged")
    .unwrap();
    worker_barrier(&worker).await;
    drain.abort();
}

/// Input that arrives while a settled turn's thread is being checked for
/// unload keeps the thread from being unsubscribed; the new turn starts once,
/// after a resume that proves the thread is loaded again.
async fn scenario_input_at_turn_completion_is_not_raced_by_unload(
    provider: studio_runtime_providers::ProviderKind,
) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = provider_worker(&state, &mut peer, provider).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    enable_unload(&state, &mut peer).await;

    // The settled turn starts the unload checks at once, with no idle wait.
    progress(peer.completed()).await;
    let read = progress(peer.write("thread/read")).await;
    assert_eq!(
        read["params"],
        json!({"threadId": THREAD, "includeTurns": false})
    );
    state.enqueue("arrived-at-completion").await;
    progress(peer.reply(
        &read,
        json!({"thread": {"id": THREAD, "status": {"type": "idle"}}}),
    ))
    .await;
    answer(
        &mut peer,
        "thread/backgroundTerminals/list",
        json!({"threadId": THREAD, "limit": 100}),
        json!({"data": [], "nextCursor": null}),
    )
    .await;
    answer(
        &mut peer,
        "thread/queue/list",
        json!({"threadId": THREAD}),
        json!({"data": [], "nextCursor": null}),
    )
    .await;
    state.wake(&worker).await;
    // `write_turn_start` fails on any write other than resume or turn/start.
    let next = progress(peer.write_turn_start()).await;
    let input = next["params"]["input"].to_string();
    assert_eq!(input.matches("arrived-at-completion").count(), 1, "{input}");
    progress(worker.shutdown()).await.unwrap();
}

#[tokio::test]
async fn codex_input_at_turn_completion_is_not_raced_by_unload() {
    scenario_input_at_turn_completion_is_not_raced_by_unload(
        studio_runtime_providers::ProviderKind::Codex,
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn claude_input_at_turn_completion_is_not_raced_by_unload() {
    scenario_input_at_turn_completion_is_not_raced_by_unload(
        studio_runtime_providers::ProviderKind::Claude,
    )
    .await;
}
