//! Account-worker witnesses for busy-turn input (`turn/steer`). Each test
//! starts from a real accepted turn and drives one fail point through the
//! worker: write, reply, acknowledgement, turn end and restart. Scenarios the
//! two providers share run once per provider through `per_provider!`.
use super::*;

const INPUT: &str = "steer-input";
const TEXT: &str = "Please also check the tests";

/// Generate one test per provider from `async fn name(provider)`.
macro_rules! per_provider {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            #[tokio::test]
            async fn codex() {
                super::$name("codex").await;
            }
            #[tokio::test]
            async fn claude() {
                super::$name("claude").await;
            }
        }
    )*};
}

per_provider!(
    acknowledged_steer_is_delivered_to_the_running_turn_once,
    rejected_steer_waits_for_the_next_turn_and_is_sent_once,
    turn_end_without_acknowledgement_is_unconfirmed_and_never_resent,
    transport_close_before_reply_is_unconfirmed_and_never_resent,
    turn_ending_before_a_rejection_starts_the_next_turn_once,
);

/// An accepted, running turn of a `provider` agent with queued input.
async fn running_turn(provider: &'static str) -> (State, Peer, AccountHandle) {
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
                    record.extra.insert("provider".into(), json!(provider));
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
    peer.write_turn_start().await
}

async fn input(state: &State) -> (String, Option<String>, Option<String>) {
    state
        .bridge
        .run(move |store| async move {
            Ok(store.test_reader()?.query_row(
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
            Ok(store.test_reader()?.query_row(
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

async fn acknowledged_steer_is_delivered_to_the_running_turn_once(provider: &'static str) {
    let (state, mut peer, mut worker) = running_turn(provider).await;
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

fn rejection(request: &Value) -> Value {
    json!({"id":request["id"],"error":{"code":-32600,"message":"expected active turn id `turn` but found none"}})
}

async fn rejected_steer_waits_for_the_next_turn_and_is_sent_once(provider: &'static str) {
    let (state, mut peer, mut worker) = running_turn(provider).await;
    let request = steer(&state, &mut peer, &worker).await;
    frame(&mut peer, rejection(&request)).await;
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

/// The turn ends while the steer is in flight; the provider then rejects it.
/// The input is unconfirmed until the rejection proves it unread, and the
/// worker starts the next turn with it exactly once.
async fn turn_ending_before_a_rejection_starts_the_next_turn_once(provider: &'static str) {
    let (state, mut peer, mut worker) = running_turn(provider).await;
    let request = steer(&state, &mut peer, &worker).await;
    set_provider(&state, "codex").await;
    frame(&mut peer, finished(&[])).await;
    assert_eq!(input(&state).await.0, "failed");
    no_write(&mut peer).await;
    frame(&mut peer, rejection(&request)).await;
    let next = peer.write_turn_start().await;
    assert!(
        next.to_string().contains(TEXT),
        "the next turn carries the input"
    );
    assert_eq!(input(&state).await.0, "dispatching");
    frame(&mut peer, rejection(&request)).await;
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

/// Claude bridge only: Codex app-server does not report unread steers.
#[tokio::test]
async fn turn_end_reporting_unread_input_requeues_it_once() {
    let (state, mut peer, mut worker) = running_turn("claude").await;
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

async fn turn_end_without_acknowledgement_is_unconfirmed_and_never_resent(provider: &'static str) {
    let (state, mut peer, mut worker) = running_turn(provider).await;
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

async fn transport_close_before_reply_is_unconfirmed_and_never_resent(provider: &'static str) {
    let (state, mut peer, mut worker) = running_turn(provider).await;
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
    let (state, mut peer, mut worker) = running_turn("claude").await;
    let _request = steer(&state, &mut peer, &worker).await;
    let stage = state
        .bridge
        .run(move |store| async move {
            store::recover(&store, "default", Generation(2), true).await?;
            Ok(store.test_reader()?.query_row(
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
    let (state, mut peer, mut worker) = running_turn("claude").await;
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

/// Mixed busy-turn input uses the real preparation/compiler and durable worker
/// path. The deferred prefix stays in the same queue for the following turn.
async fn mixed_inputs(state: &State) {
    let agent = state.agent.clone();
    let epoch = state.epoch;
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            let mut sender = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
            sender.id = "mixed-child".into();
            sender.status = studio_runtime_store::AgentStatus::Completed;
            sender.extra.insert("parentId".into(), json!(agent));
            sender.extra.insert("isLead".into(), json!(false));
            sender.extra.insert("threadId".into(), json!("child-thread"));
            AgentRepository::put(tx, &sender)?;
            tx.execute("INSERT INTO runtime_rooms(id,record) VALUES('mixed-room',?1)",
                [json!({"id":"mixed-room","kind":"private","members":[agent,"mixed-child"]}).to_string()])?;
            tx.execute("UPDATE runtime_events SET created=50 WHERE id=?1", [INPUT])?;
            let rows = [
                ("deferred", "user", "for the next turn".to_owned()),
                ("available", "work_available", json!({"task":"task-1","title":"Review work"}).to_string()),
                ("review", "work_review", json!({"task":"task-1","result":{"id":"result-1","text":"checks passed","checks":"cargo test","files":["src/lib.rs"]}}).to_string()),
                ("child", "child_result", json!({"agent":"mixed-child","name":"Child","status":"completed","result":"child evidence ".repeat(2000)}).to_string()),
                ("message", "agent_message", json!({"sender":"mixed-child","room":"mixed-room","text":"inspect the evidence","importance":"message"}).to_string()),
                ("followup", "followup", "Child asks for another check".to_owned()),
                ("decision", "work_decision", "Accepted with the reported checks".to_owned()),
            ];
            for (index, (id, kind, text)) in rows.into_iter().enumerate() {
                tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,?3,?4,'pending',?5,?6)",
                    rusqlite::params![id,agent,kind,text,index as f64+1.0,epoch])?;
                let metadata = match id {
                    "deferred" => json!({"delivery":"next_turn"}),
                    "followup" => json!({"delivery":"current_turn","senderId":"mixed-child"}),
                    _ => json!({"delivery":"current_turn","source":"mixed-test"}),
                };
                tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2)",rusqlite::params![id,metadata.to_string()])?;
            }
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
}

async fn mixed_statuses(state: &State) -> Vec<(String, String)> {
    let agent = state.agent.clone();
    state.bridge.run(move |store| async move {
 store.read_async("test/account_worker_steer/mixed_statuses", move |db| {

        let mut statement=db.prepare("SELECT id,status FROM runtime_events WHERE agent=?1 AND id<>'annotation:input' ORDER BY created,rowid")?;
        Ok(statement.query_map([agent], |row| Ok((row.get(0)?,row.get(1)?)))?.collect::<Result<Vec<_>,_>>()?)

 }).await
}).await.unwrap()
}

per_provider!(mixed_batch_is_compiled_acknowledged_once_and_deferred_input_waits);

async fn mixed_batch_is_compiled_acknowledged_once_and_deferred_input_waits(
    provider: &'static str,
) {
    let (state, mut peer, mut worker) = running_turn(provider).await;
    mixed_inputs(&state).await;
    let request = steer(&state, &mut peer, &worker).await;
    assert_eq!(request["params"]["clientUserMessageId"], "available");
    assert_eq!(request["params"]["expectedTurnId"], "turn");
    let text = request["params"]["input"][0]["text"].as_str().unwrap();
    for kind in [
        "work_available",
        "work_review",
        "child_result",
        "agent_message",
        "followup",
        "work_decision",
    ] {
        assert!(
            text.contains(&format!("[Orchestration event: {kind}]")),
            "{provider}: missing {kind}"
        );
    }
    assert!(text.contains(TEXT));
    assert!(!text.contains("for the next turn"));
    assert!(
        text.contains("resultFile"),
        "large child evidence is safely referenced"
    );
    assert!(
        !text.contains(&"child evidence ".repeat(2000)),
        "compiler bounds synthetic evidence"
    );
    assert_eq!(
        mixed_statuses(&state)
            .await
            .iter()
            .filter(|(_, status)| status == "dispatching")
            .count(),
        7
    );
    if provider == "codex" {
        peer.reply(&request, json!({"turnId":"turn"})).await;
        let operation = request["id"].as_str().unwrap().to_owned();
        state
            .bridge
            .run(move |store| async move {
                store
                    .transaction_with(move |tx| {
                        assert!(
                            store::receipt(tx, &operation, "submitted")?.is_none(),
                            "ordinary request receipt was retired before input acknowledgement"
                        );
                        Ok::<_, WorkerError>(())
                    })
                    .await
            })
            .await
            .unwrap();
    }
    // Claude loses its response; Codex already retired the transport receipt.
    // Both must settle the same frozen compiler projection exactly once.
    let ack = json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn",
        "item":{"id":"available","type":"userMessage","clientId":"available","delivery":"steer",
            "content":request["params"]["input"]}}});
    frame(&mut peer, ack.clone()).await;
    frame(&mut peer, ack).await;
    no_write(&mut peer).await;
    let agent = state.agent.clone();
    let transcript = state
        .bridge
        .run(move |store| async move {
 store.read_async("test/account_worker_steer/mixed_batch_is_compiled_acknowledged_once_and_deferred_input_waits", move |db| {

            let raw: String = db.query_row(
                "SELECT record FROM runtime_items WHERE id=?1",
                [format!("{agent}:available")],
                |r| r.get(0),
            )?;
            let record: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(record["inputs"].as_array().unwrap().len(), 7);
            assert_eq!(record["inputs"][0]["kind"], "work_available");
            assert!(
                record["text"]
                    .as_str()
                    .unwrap()
                    .contains("[Orchestration event: work_available]")
            );
            assert!(record["text"].as_str().unwrap().contains("resultFile"));
            assert_eq!(record["inputs"][6]["id"], INPUT);
            let meta: String = db.query_row(
                "SELECT record FROM runtime_event_meta WHERE id='followup'",
                [],
                |r| r.get(0),
            )?;
            let meta: Value = serde_json::from_str(&meta).unwrap();
            assert_eq!(meta["senderId"], "mixed-child");
            let count: i64 = db.query_row(
                "SELECT count(*) FROM runtime_items WHERE id=?1",
                [format!("{agent}:available")],
                |r| r.get(0),
            )?;
            Ok(count)

 }).await
})
        .await
        .unwrap();
    assert_eq!(transcript, 1);
    for (id, status) in mixed_statuses(&state).await {
        assert_eq!(
            status,
            if id == "deferred" {
                "pending"
            } else {
                "delivered"
            },
            "{provider}: {id}"
        );
    }
    let next = next_turn(&state, &mut peer, finished(&[])).await;
    assert!(
        next["params"]["input"]
            .to_string()
            .contains("for the next turn")
    );
    assert!(!next["params"]["input"].to_string().contains(TEXT));
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn foreign_notification_fails_visibly_while_valid_current_input_is_sent() {
    let (state, mut peer, mut worker) = running_turn("codex").await;
    mixed_inputs(&state).await;
    state.bridge.run(|store| async move {
        store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','foreign') WHERE id='mixed-child'",[])?;
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
    let request = steer(&state, &mut peer, &worker).await;
    assert_safe_user_batch(&state, &request).await;
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

async fn thousand_inputs(state: &State) {
    let (agent, epoch) = (state.agent.clone(), state.epoch);
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            tx.execute("UPDATE runtime_events SET created=1000 WHERE agent=?1 AND kind='user'",[&agent])?;
            for i in 0..999 {
                tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,'work_decision','x','pending',?3,?4)",
                    rusqlite::params![format!("small-{i:04}"),agent,i as f64,epoch])?;
            }
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
}

#[tokio::test]
async fn one_thousand_small_events_share_one_compiled_busy_submission() {
    let (state, mut peer, mut worker) = running_turn("codex").await;
    thousand_inputs(&state).await;
    let request = steer(&state, &mut peer, &worker).await;
    assert_eq!(request["params"]["clientUserMessageId"], "small-0000");
    let text = request["params"]["input"][0]["text"].as_str().unwrap();
    assert_eq!(
        text.matches("[Orchestration event: work_decision]").count(),
        999
    );
    assert!(text.contains(TEXT));
    frame(&mut peer,json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn",
        "item":{"id":"small-0000","type":"userMessage","clientId":"small-0000","delivery":"steer","content":request["params"]["input"]}}})).await;
    let agent = state.agent.clone();
    state.bridge.run(move |store| async move {
 store.read_async("test/account_worker_steer/one_thousand_small_events_share_one_compiled_busy_submission", move |db| {

        let delivered:i64=db.query_row("SELECT count(*) FROM runtime_events WHERE agent=?1 AND status='delivered' AND id<>'annotation:input'",[&agent],|r|r.get(0))?;
        assert_eq!(delivered,1000);
        let count:i64=db.query_row("SELECT json_array_length(record,'$.inputs') FROM runtime_items WHERE id=?1",[format!("{agent}:small-0000")],|r|r.get(0))?;
        assert_eq!(count,1000);
        Ok(())

 }).await
}).await.unwrap();
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_thousand_small_events_share_one_idle_turn_submission() {
    let state = State::new().await;
    thousand_inputs(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let turn = state.start(&mut peer, &worker).await;
    let text = turn["params"]["input"][0]["text"].as_str().unwrap();
    assert_eq!(
        text.matches("[Orchestration event: work_decision]").count(),
        999
    );
    peer.accepted(&turn).await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
 store.read_async("test/account_worker_steer/one_thousand_small_events_share_one_idle_turn_submission", move |db| {

            let count: i64 = db.query_row(
                "SELECT count(*) FROM runtime_events WHERE agent=?1 AND status='delivered'",
                [agent],
                |r| r.get(0),
            )?;
            assert_eq!(count, 1000);
            Ok(())

 }).await
})
        .await
        .unwrap();
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_authority_releases_the_original_batch_and_only_sends_safe_input() {
    let (state, mut peer, mut worker) = running_turn("codex").await;
    mixed_inputs(&state).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("steer_submitted".into());
    state.wake(&worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("compiled steer reaches commit");
    state.bridge.run(|store| async move {
        store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','foreign') WHERE id='mixed-child'",[])?;
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_one();
    let request = steer(&state, &mut peer, &worker).await;
    assert_safe_user_batch(&state, &request).await;
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

async fn assert_safe_user_batch(state: &State, request: &Value) {
    let text = request["params"]["input"][0]["text"].as_str().unwrap();
    assert!(text.contains(TEXT));
    assert!(!text.contains("inspect the evidence"));
    assert!(!text.contains("Child asks for another check"));
    let statuses = mixed_statuses(state).await;
    for id in ["message", "followup"] {
        assert_eq!(
            statuses.iter().find(|(event, _)| event == id).unwrap().1,
            "failed"
        );
    }
    state.bridge.run(|store| async move {
 store.read_async("test/account_worker_steer/assert_safe_user_batch", move |db| {

        let errors:i64=db.query_row("SELECT count(*) FROM runtime_events WHERE id IN ('message','followup') AND status='failed' AND error IS NOT NULL",[],|r|r.get(0))?;
        assert_eq!(errors,2,"unauthorized notifications have visible queue errors");
        Ok(())

 }).await
}).await.unwrap();
    assert_eq!(
        input(state).await.0,
        "dispatching",
        "valid user input is not withheld"
    );
    assert!(
        state.record().await["error"]
            .as_str()
            .unwrap()
            .starts_with("Queued input could not be delivered:"),
        "recipient's existing visible error field exposes notification failure"
    );
}

#[tokio::test]
async fn malformed_notification_evidence_fails_visibly_without_stranding_user_input() {
    let (state, mut peer, mut worker) = running_turn("codex").await;
    mixed_inputs(&state).await;
    state.bridge.run(|store| async move {
        store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_events SET text=json_set(text,'$.result.checks',7) WHERE id='review'",[])?;
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
    let request = steer(&state, &mut peer, &worker).await;
    let text = request["params"]["input"][0]["text"].as_str().unwrap();
    assert!(text.contains(TEXT));
    assert!(!text.contains("[Orchestration event: work_review]"));
    state
        .bridge
        .run(|store| async move {
 store.read_async("test/account_worker_steer/malformed_notification_evidence_fails_visibly_without_stranding_user_input", move |db| {

            let row: (String, String) = db.query_row(
                "SELECT status,error FROM runtime_events WHERE id='review'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(
                row,
                (
                    "failed".into(),
                    "Invalid stored event evidence metadata".into()
                )
            );
            Ok(())

 }).await
})
        .await
        .unwrap();
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn acknowledged_mixed_input_uses_frozen_authority_after_sender_moves() {
    let (state, mut peer, mut worker) = running_turn("codex").await;
    mixed_inputs(&state).await;
    let request = steer(&state, &mut peer, &worker).await;
    peer.reply(&request, json!({"turnId":"turn"})).await;
    state.bridge.run(|store| async move {
        store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','foreign') WHERE id='mixed-child'",[])?;
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
    let ack = json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn",
        "item":{"id":"available","type":"userMessage","clientId":"available","delivery":"steer","content":request["params"]["input"]}}});
    frame(&mut peer, ack.clone()).await;
    frame(&mut peer, ack).await;
    assert_eq!(input(&state).await.0, "delivered");
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
 store.read_async("test/account_worker_steer/acknowledged_mixed_input_uses_frozen_authority_after_sender_moves", move |db| {

            let raw: String = db.query_row(
                "SELECT record FROM runtime_items WHERE id=?1",
                [format!("{agent}:available")],
                |r| r.get(0),
            )?;
            let record: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(record["inputs"].as_array().unwrap().len(), 7);
            assert!(
                record["text"]
                    .as_str()
                    .unwrap()
                    .contains("[Orchestration event: agent_message]")
            );
            Ok(())

 }).await
})
        .await
        .unwrap();
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_user_input_remains_intact_with_visible_transport_error() {
    let (state, mut peer, mut worker) = running_turn("codex").await;
    let text = "x".repeat(studio_runtime_providers::MAX_FRAME_BYTES);
    let expected_bytes = text.len();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    tx.execute(
                        "UPDATE runtime_events SET text=?1 WHERE id=?2",
                        rusqlite::params![text, INPUT],
                    )?;
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
    let mut changes = state.bridge.agent_changes();
    state.wake(&worker).await;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if state.record().await["error"]
                .as_str()
                .is_some_and(|error| error.contains("Full input remains queued"))
            {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("oversized input refusal becomes visible");
    no_write(&mut peer).await;
    state
        .bridge
        .run(move |store| async move {
 store.read_async("test/account_worker_steer/oversized_user_input_remains_intact_with_visible_transport_error", move |db| {

            let row: (String, String, i64) = db.query_row(
                "SELECT status,error,length(CAST(text AS BLOB)) FROM runtime_events WHERE id=?1",
                [INPUT],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(row.0, "pending");
            assert!(
                row.1
                    .contains(&studio_runtime_providers::MAX_FRAME_BYTES.to_string())
            );
            assert_eq!(
                row.2,
                i64::try_from(expected_bytes).unwrap(),
                "no user bytes were dropped to fit transport"
            );
            Ok(())

 }).await
})
        .await
        .unwrap();
    assert_eq!(state.record().await["status"], "running");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_mixed_batches_respect_the_shared_attachment_admission_budget() {
    let state = State::new().await;
    let file = state.root.join("delivery-attachment.txt");
    std::fs::write(&file, "fixture attachment").unwrap();
    let (agent, epoch) = (state.agent.clone(), state.epoch);
    let per_message = studio_runtime_schema::shared_values::MAX_TURN_ATTACHMENTS / 2 + 1;
    state.bridge.run(move |store| async move {
        store.transaction_with(move |tx| {
            tx.execute("UPDATE runtime_events SET created=0 WHERE agent=?1",[&agent])?;
            for (index,id) in ["assets-first","assets-second"].into_iter().enumerate() {
                let assets=(0..per_message).map(|asset|format!("{id}-{asset}")).collect::<Vec<_>>();
                for asset in &assets {
                    tx.execute("INSERT INTO runtime_assets(id,record) VALUES(?1,?2)",rusqlite::params![asset,
                        json!({"id":asset,"agentId":agent,"image":false,"path":file,"name":"fixture.txt","size":18}).to_string()])?;
                }
                tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,'user',?1,'pending',?3,?4)",
                    rusqlite::params![id,agent,index as f64+1.0,epoch])?;
                tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2)",rusqlite::params![id,json!({"assets":assets,"delivery":"next_turn"}).to_string()])?;
            }
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let first = state.start(&mut peer, &worker).await;
    assert_eq!(
        first["params"]["input"].as_array().unwrap().len(),
        per_message + 1
    );
    let text = first["params"]["input"][0]["text"].as_str().unwrap();
    assert!(text.contains("assets-first"));
    assert!(!text.contains("assets-second"));
    peer.accepted(&first).await;
    peer.completed().await;
    let second = peer.write_turn_start().await;
    assert_eq!(
        second["params"]["input"].as_array().unwrap().len(),
        per_message + 1
    );
    assert!(
        second["params"]["input"][0]["text"]
            .as_str()
            .unwrap()
            .contains("assets-second")
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_bad_notifications_are_isolated_without_failing_valid_user_input() {
    let state = State::new().await;
    mixed_inputs(&state).await;
    state.bridge.run(|store|async move {
        store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','foreign') WHERE id='mixed-child'",[])?;
            tx.execute("UPDATE runtime_events SET text=json_set(text,'$.result.checks',7) WHERE id='review'",[])?;
            Ok::<_,WorkerError>(())
        }).await
    }).await.unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let request = state.start(&mut peer, &worker).await;
    let text = request["params"]["input"].to_string();
    assert!(!text.contains("inspect the evidence"));
    assert!(!text.contains("Child asks for another check"));
    assert!(!text.contains("[Orchestration event: work_review]"));
    assert!(text.contains("for the next turn"));
    for id in ["message", "followup", "review"] {
        assert_eq!(
            mixed_statuses(&state)
                .await
                .into_iter()
                .find(|(event, _)| event == id)
                .unwrap()
                .1,
            "failed"
        );
    }
    peer.accepted(&request).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert_ne!(state.record().await["status"], "failed");
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_authority_change_at_commit_reclaims_safe_input_with_a_fresh_identity() {
    let state = State::new().await;
    mixed_inputs(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("submitted".into());
    state.wake(&worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("idle prepared commit pauses");
    let before = state.record().await["startAttempt"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    state.bridge.run(|store|async move {store.transaction_with(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','foreign') WHERE id='mixed-child'",[])?;
        Ok::<_,WorkerError>(())
    }).await}).await.unwrap();
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_one();
    let request = state.start(&mut peer, &worker).await;
    assert_ne!(state.record().await["startAttempt"]["id"], before);
    assert!(
        !request["params"]["input"]
            .to_string()
            .contains("inspect the evidence")
    );
    peer.accepted(&request).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert_ne!(state.record().await["status"], "failed");
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

/// The follow-up a lead sends to its busy worker.
const FOLLOWUP: &str = "lead-followup";
const FOLLOWUP_TEXT: &str = "Push with 360 s, not 600 s";

/// An accepted, running turn of a `provider` agent with one queued follow-up
/// whose stored sender is `sender`. A sender other than the agent is added as
/// the root of another team.
async fn running_turn_with_followup(
    provider: &'static str,
    sender: &'static str,
) -> (State, Peer, AccountHandle) {
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
                    record.extra.insert("provider".into(), json!(provider));
                    AgentRepository::put(tx, &record)?;
                    if sender != agent {
                        let mut outsider = record.clone();
                        outsider.id = AgentId::from(sender);
                        outsider.root_id = AgentId::from(sender);
                        outsider.status = studio_runtime_store::AgentStatus::Completed;
                        outsider
                            .extra
                            .insert("threadId".into(), json!("outsider-thread"));
                        outsider.extra.insert("inFlight".into(), json!(false));
                        AgentRepository::put(tx, &outsider)?;
                    }
                    tx.execute(
                        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                         VALUES(?1,?2,'followup',?3,'pending',?4,?5,NULL,NULL)",
                        rusqlite::params![FOLLOWUP, agent, FOLLOWUP_TEXT, now(), epoch],
                    )?;
                    tx.execute(
                        "INSERT INTO runtime_event_meta(id,record) VALUES(?1,?2)",
                        rusqlite::params![FOLLOWUP, json!({"senderId":sender}).to_string()],
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

async fn followup_status(state: &State) -> (String, Option<String>) {
    state
        .bridge
        .run(move |store| async move {
            Ok(store.test_reader()?.query_row(
                "SELECT status,turn_id FROM runtime_events WHERE id=?1",
                [FOLLOWUP],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap()
}

per_provider!(
    a_team_followup_reaches_the_running_turn,
    a_followup_from_another_team_is_quarantined,
);

/// A lead's follow-up reaches its worker's running turn at once, in the
/// orchestration event form of a turn start. It used to wait for the turn to
/// end (Lumina trackwork-parity, 20 October 2026: median 810 s).
async fn a_team_followup_reaches_the_running_turn(provider: &'static str) {
    let (state, mut peer, mut worker) =
        running_turn_with_followup(provider, FIXTURE_AGENT_ID).await;
    state.wake(&worker).await;
    let wire = peer.next_write().await;
    assert_eq!(wire.value["method"], "turn/steer", "{}", wire.value);
    let request = wire.value.clone();
    let _ignored = wire.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    assert_eq!(request["params"]["expectedTurnId"], "turn");
    assert_eq!(request["params"]["clientUserMessageId"], FOLLOWUP);
    assert_eq!(
        request["params"]["input"],
        json!([{"type":"text","text":format!("[Orchestration event: followup]\n{FOLLOWUP_TEXT}")}])
    );
    assert_eq!(followup_status(&state).await.0, "dispatching");
    peer.reply(&request, json!({"turnId":"turn"})).await;
    frame(
        &mut peer,
        json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn",
            "item":{"id":FOLLOWUP,"type":"userMessage","clientId":FOLLOWUP,"delivery":"steer",
                    "content":[{"type":"text","text":FOLLOWUP_TEXT}]}}}),
    )
    .await;
    assert_eq!(
        followup_status(&state).await,
        ("delivered".into(), Some("turn".into()))
    );
    peer.completed().await;
    no_write(&mut peer).await;
    worker.shutdown().await.unwrap();
}

/// The team-isolation gate also guards the busy-turn path: a follow-up whose
/// sender is outside the recipient's team is quarantined by full preparation.
async fn a_followup_from_another_team_is_quarantined(provider: &'static str) {
    let (state, mut peer, mut worker) = running_turn_with_followup(provider, "outsider").await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    no_write(&mut peer).await;
    assert_eq!(followup_status(&state).await, ("failed".into(), None));
    worker.shutdown().await.unwrap();
}
