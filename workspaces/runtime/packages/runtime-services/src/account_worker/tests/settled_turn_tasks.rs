//! Live 2026-10-20: about 40 completed, idle or interrupted chats showed
//! "Waiting for 1 command". Their turns had ended, but a command or tool item
//! of the turn never reported its end, so its task stayed `running` (agent
//! 00399ad7, both Codex `commandExecution` items and Claude `Bash` tasks).
use super::*;

async fn task(state: &State, item: &str) -> Value {
    let id = format!("{}:{item}", state.agent);
    state
        .bridge
        .run(move |store| async move {
            let raw: String = store.read()?.query_row(
                "SELECT record FROM runtime_tasks WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            Ok(serde_json::from_str::<Value>(&raw).unwrap())
        })
        .await
        .unwrap()
}

/// A bridge for the next worker. The faults are shared, so the first
/// worker's finished restart pass is forgotten: the barrier then waits for the
/// next worker's pass.
async fn replacement_bridge(state: &State) -> StoreBridge {
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    bridge
        .faults
        .startup_recovery_done
        .store(false, std::sync::atomic::Ordering::SeqCst);
    bridge
}

async fn started(peer: &mut Peer, item: Value) {
    let cursor = peer
        .frame(
            json!({"method":"item/started","params":{"threadId":"fake-thread-0",
            "turnId":"turn","item":item}}),
        )
        .await;
    peer.acknowledged(cursor).await;
}

/// One plain command, one tool call and one provider background command.
async fn open_items(peer: &mut Peer) {
    started(
        peer,
        json!({"id":"cmd","type":"commandExecution","command":"sleep 60","status":"inProgress"}),
    )
    .await;
    started(
        peer,
        json!({"id":"tool","type":"mcpToolCall","server":"s","tool":"t","status":"inProgress"}),
    )
    .await;
    started(
        peer,
        json!({"id":"bg","type":"commandExecution","command":"serve","processId":"42",
            "status":"inProgress"}),
    )
    .await;
}

async fn assert_closed(state: &State, items: &[&str]) {
    for item in items {
        let task = task(state, item).await;
        assert_eq!(task["status"], "interrupted", "{task}");
        assert!(task["finished"].is_number(), "{task}");
    }
}

async fn turn_end_closes_its_open_items(status: &str) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    open_items(&mut peer).await;
    assert_eq!(task(&state, "cmd").await["status"], "running");
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
            "turn":{"id":"turn","status":status}}}),
        )
        .await;
    peer.acknowledged(cursor).await;
    assert_eq!(state.record().await["lastCompletedTurn"], "turn");
    assert_closed(&state, &["cmd", "tool"]).await;
    // The provider can still run it as a background terminal.
    assert_eq!(task(&state, "bg").await["status"], "running");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_completed_turn_closes_its_open_command_and_tool() {
    turn_end_closes_its_open_items("completed").await;
}

#[tokio::test]
async fn an_interrupted_turn_closes_its_open_command_and_tool() {
    turn_end_closes_its_open_items("interrupted").await;
}

/// A disconnect does not end the turn: its recovery can still continue it, so
/// its items stay open until the replayed completion settles the turn.
#[tokio::test]
async fn a_turn_cut_off_by_a_disconnect_closes_its_items_when_it_settles() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    open_items(&mut peer).await;
    provider_exits(&peer, &mut worker).await;
    assert_eq!(state.record().await["status"], "interrupted");
    assert_eq!(task(&state, "cmd").await["status"], "running");

    let mut next = Peer::new();
    let mut replacement = next.worker(replacement_bridge(&state).await).await;
    // The cut-off turn is live, so the new worker checks no command of it.
    worker_barrier(&replacement).await;
    assert_eq!(task(&state, "cmd").await["status"], "running");
    next.cursor = peer.cursor;
    let cursor = next
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
            "turn":{"id":"turn","status":"completed"}}}),
        )
        .await;
    next.acknowledged(cursor).await;
    assert_eq!(state.record().await["lastCompletedTurn"], "turn");
    assert_closed(&state, &["cmd", "tool"]).await;
    assert_eq!(task(&state, "bg").await["status"], "running");
    while let Ok(wire) = next.writes.try_recv() {
        assert_ne!(
            wire.value["method"], "thread/backgroundTerminals/list",
            "{}",
            wire.value
        );
    }
    replacement.shutdown().await.unwrap();
}

/// A new worker asks its provider which background commands of settled turns
/// still run. The listed command stays running; the other one is lost.
#[tokio::test]
async fn a_new_worker_closes_the_background_command_that_its_provider_no_longer_runs() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    for (item, process) in [("gone", "42"), ("kept", "43")] {
        started(
            &mut peer,
            json!({"id":item,"type":"commandExecution","command":"serve","processId":process,
                "status":"inProgress"}),
        )
        .await;
    }
    peer.completed().await;
    worker.shutdown().await.unwrap();
    assert_eq!(task(&state, "gone").await["status"], "running");

    let mut next = Peer::new();
    let mut replacement = next.worker(replacement_bridge(&state).await).await;
    let read = next.write("thread/read").await;
    assert_eq!(read["params"]["threadId"], "fake-thread-0", "{read}");
    next.reply(
        &read,
        json!({"thread":{"id":"fake-thread-0","status":{"type":"idle"}}}),
    )
    .await;
    let list = next.write("thread/backgroundTerminals/list").await;
    assert_eq!(list["params"]["threadId"], "fake-thread-0", "{list}");
    next.reply(
        &list,
        json!({"data":[{"itemId":"kept","processId":"43","threadId":"fake-thread-0"}],
            "nextCursor":null}),
    )
    .await;
    worker_barrier(&replacement).await;
    let lost = task(&state, "gone").await;
    assert_eq!(lost["status"], "lost", "{lost}");
    assert_eq!(
        lost["error"],
        crate::task_settlement::LOST_COMMAND,
        "{lost}"
    );
    assert!(lost["finished"].is_number(), "{lost}");
    assert_eq!(task(&state, "kept").await["status"], "running");
    replacement.shutdown().await.unwrap();
}
