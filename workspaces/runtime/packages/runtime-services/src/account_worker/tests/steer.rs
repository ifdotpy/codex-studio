//! Account-worker witnesses for Claude busy-turn input (`turn/steer`). Each
//! test starts from a real accepted turn and drives one fail point through the
//! worker: write, reply, acknowledgement, turn end and restart.
use super::*;

const INPUT: &str = "steer-input";
const TEXT: &str = "Please also check the tests";

/// An accepted, running turn whose agent is a Claude agent with queued input.
async fn running_claude_turn() -> (State, Peer, AccountHandle) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let (agent, epoch) = (state.agent.clone(), state.epoch);
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut record = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?
                        .expect("agent");
                    assert_eq!(record.status, studio_runtime_store::AgentStatus::Running);
                    record.extra.insert("provider".into(), json!("claude"));
                    AgentRepository::put(tx, &record)?;
                    tx.execute(
                        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                         VALUES(?1,?2,'user',?3,'pending',?4,?5,NULL,NULL)",
                        rusqlite::params![INPUT, agent, TEXT, now(), epoch],
                    )?;
                    Ok::<_, StoreError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    (state, peer, worker)
}

async fn set_provider(state: &State, provider: &'static str) {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut record =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.expect("agent");
                    record.extra.insert("provider".into(), json!(provider));
                    AgentRepository::put(tx, &record)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

/// End the turn and serve the ordinary next turn. That start path is not part
/// of steering; the Codex fixture provider serves it here.
async fn next_turn(state: &State, peer: &mut Peer, completed: Value) -> Value {
    set_provider(state, "codex").await;
    frame(peer, completed).await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    peer.write("turn/start").await
}

async fn input(state: &State) -> (String, Option<String>, Option<String>) {
    state
        .bridge
        .run(move |store| async move {
            Ok(store.read()?.query_row(
                "SELECT status,turn_id,error FROM runtime_events WHERE id=?1",
                [INPUT],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .unwrap()
}

async fn transcript(state: &State) -> i64 {
    let id = format!("{}:{INPUT}", state.agent);
    state
        .bridge
        .run(move |store| async move {
            Ok(store.read()?.query_row(
                "SELECT COUNT(*) FROM runtime_items WHERE id=?1 AND json_extract(record,'$.role')='user'",
                [id],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap()
}

/// Wake the agent and return the steer request after proving that its durable
/// submission committed before the transport write.
async fn steer(state: &State, peer: &mut Peer, worker: &AccountHandle) -> Value {
    state.wake(worker).await;
    let wire = peer.next_write().await;
    assert_eq!(wire.value["method"], "turn/steer");
    assert_eq!(input(state).await.0, "dispatching");
    let operation = wire.value["id"].as_str().unwrap().to_owned();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    assert!(store::receipt(tx, &operation, "submitted")?.is_some());
                    Ok::<_, WorkerError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let _ignored = wire.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    wire.value
}

async fn frame(peer: &mut Peer, value: Value) {
    let cursor = peer.frame(value).await;
    peer.acknowledged(cursor).await;
}

/// A journal barrier: everything before it, including effects, was consumed.
async fn no_write(peer: &mut Peer) {
    frame(peer, json!({"method":"test/barrier","params":{}})).await;
    assert!(peer.writes.try_recv().is_err(), "nothing was written");
}

fn acknowledgement() -> Value {
    json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn",
        "item":{"id":INPUT,"type":"userMessage","clientId":INPUT,"delivery":"steer",
                "content":[{"type":"text","text":TEXT}]}}})
}

fn finished(unapplied: &[&str]) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
        "turn":{"id":"turn","status":"completed","unappliedSteers":unapplied}}})
}

#[tokio::test]
async fn acknowledged_steer_is_delivered_to_the_running_turn_once() {
    let (state, mut peer, mut worker) = running_claude_turn().await;
    let request = steer(&state, &mut peer, &worker).await;
    assert_eq!(request["params"]["expectedTurnId"], "turn");
    assert_eq!(request["params"]["threadId"], "fake-thread-0");
    assert_eq!(request["params"]["clientUserMessageId"], INPUT);
    assert_eq!(
        request["params"]["input"],
        json!([{"type":"text","text":TEXT}])
    );
    peer.reply(&request, json!({"turnId":"turn"})).await;
    assert_eq!(
        input(&state).await.0,
        "dispatching",
        "a reply is not delivery"
    );
    assert_eq!(transcript(&state).await, 0);
    frame(&mut peer, acknowledgement()).await;
    assert_eq!(
        input(&state).await,
        ("delivered".into(), Some("turn".into()), None)
    );
    assert_eq!(transcript(&state).await, 1);
    frame(&mut peer, acknowledgement()).await;
    no_write(&mut peer).await;
    peer.completed().await;
    assert_eq!(input(&state).await.0, "delivered");
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_steer_waits_for_the_next_turn_and_is_sent_once() {
    let (state, mut peer, mut worker) = running_claude_turn().await;
    let request = steer(&state, &mut peer, &worker).await;
    frame(
        &mut peer,
        json!({"id":request["id"],"error":{"code":-32000,"message":"The steer belongs to a different Claude turn"}}),
    )
    .await;
    assert_eq!(input(&state).await.0, "pending");
    assert_eq!(state.record().await["steerRejectedTurnId"], "turn");
    state.wake(&worker).await;
    no_write(&mut peer).await;
    let next = next_turn(&state, &mut peer, finished(&[])).await;
    assert!(
        next.to_string().contains(TEXT),
        "the next turn carries the input"
    );
    assert_eq!(input(&state).await.0, "dispatching");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn turn_end_reporting_unread_input_requeues_it_once() {
    let (state, mut peer, mut worker) = running_claude_turn().await;
    let request = steer(&state, &mut peer, &worker).await;
    peer.reply(&request, json!({"turnId":"turn"})).await;
    let next = next_turn(&state, &mut peer, finished(&[INPUT])).await;
    assert!(
        next.to_string().contains(TEXT),
        "the next turn carries the input"
    );
    assert_eq!(
        transcript(&state).await,
        1,
        "only the delivering start wrote a transcript item"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn turn_end_without_acknowledgement_is_unconfirmed_and_never_resent() {
    let (state, mut peer, mut worker) = running_claude_turn().await;
    let request = steer(&state, &mut peer, &worker).await;
    peer.reply(&request, json!({"turnId":"turn"})).await;
    frame(&mut peer, finished(&[])).await;
    let (status, _, error) = input(&state).await;
    assert_eq!(status, "failed");
    assert_eq!(
        error.as_deref(),
        Some(studio_runtime_store::STEER_UNCONFIRMED_ERROR)
    );
    state.wake(&worker).await;
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn transport_close_before_reply_is_unconfirmed_and_never_resent() {
    let (state, mut peer, mut worker) = running_claude_turn().await;
    let _request = steer(&state, &mut peer, &worker).await;
    // The closed generation can no longer acknowledge the written input.
    worker.shutdown().await.unwrap();
    assert_eq!(input(&state).await.0, "failed");
    let mut peer = Peer::identity("default", 2, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    no_write(&mut peer).await;
    assert_eq!(input(&state).await.0, "failed");
    worker.shutdown().await.unwrap();
}

/// A process restart skips the closing worker's `GenerationClosed` path: the
/// next worker's `recover()` must decide the steer before reconciling the old
/// turn, which turns `dispatching` rows `uncertain`.
#[tokio::test]
async fn restart_recovery_unconfirms_a_written_steer_and_keeps_its_receipt() {
    let (state, mut peer, mut worker) = running_claude_turn().await;
    let _request = steer(&state, &mut peer, &worker).await;
    let stage = state
        .bridge
        .run(move |store| async move {
            store::recover(&store, "default", Generation(2)).await?;
            Ok(store.read()?.query_row(
                "SELECT json_extract(result,'$.stage') FROM runtime_operation_receipts \
                 WHERE signature='claude-steer'",
                [],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(stage, "unconfirmed", "a late acknowledgement can repair it");
    let (status, _, error) = input(&state).await;
    assert_eq!(status, "failed");
    assert_eq!(
        error.as_deref(),
        Some(studio_runtime_store::STEER_UNCONFIRMED_ERROR)
    );
    worker.shutdown().await.unwrap();
}

/// A worker that starts closing while the `submitted` boundary commits never
/// writes the request, so the input returns to the queue instead of becoming
/// unconfirmed.
#[tokio::test]
async fn closing_during_submission_returns_the_input_unsent() {
    let (state, mut peer, mut worker) = running_claude_turn().await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("steer_submitted".into());
    state.wake(&worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("the steer reaches its submission boundary");
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    worker.cancel();
    state.bridge.faults.release.notify_one();
    worker.completion().await.unwrap();
    assert!(peer.writes.try_recv().is_err(), "nothing was written");
    assert_eq!(input(&state).await, ("pending".into(), None, None));
}
