//! Invalid stored data of one agent must not end its account worker. The
//! failing frame, reply or effect is rolled back for that agent, the agent is
//! held visibly, and every other agent of the account keeps working.
use super::*;

/// Bounds a wait for progress that a stalled journal would never make;
/// unit: wall time.
const PROGRESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a running worker must stay up to count as not ended; unit: wall time.
const STILL_RUNNING: std::time::Duration = std::time::Duration::from_millis(300);
/// Provider frames sent behind an unsettled one: more than the session's
/// unsettled bound, so a held cursor would stop the journal intake.
const FRAMES_PAST_THE_UNSETTLED_BOUND: usize = crate::provider_session_task::MAX_UNSETTLED + 8;

/// A second agent of the fixture account on its own thread, with queued input.
async fn second_agent(state: &State, id: &str, thread: &str, input: &str) {
    let first = state.agent.clone();
    let (id_row, thread) = (id.to_owned(), thread.to_owned());
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(first.as_str()))?.unwrap();
                row.id = AgentId::from(id_row.as_str());
                row.root_id = AgentId::from(id_row.as_str());
                row.extra.insert("threadId".into(), json!(thread));
                AgentRepository::put(tx, &row)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
    state.enqueue_for(id, input).await;
}

async fn record_of(state: &State, id: &str) -> Value {
    let id = id.to_owned();
    state
        .bridge
        .run(move |store| async move {
            let raw: String = store.test_reader()?.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            Ok(serde_json::from_str(&raw).unwrap())
        })
        .await
        .unwrap()
}

/// Change one agent row with raw SQL, as a corrupt or stale writer would.
async fn corrupt(state: &State, id: &str, sql: &'static str) {
    let id = id.to_owned();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    assert_eq!(tx.execute(sql, [id])?, 1);
                    Ok::<_, StoreError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn assert_running(worker: &mut AccountHandle) {
    assert!(
        tokio::time::timeout(STILL_RUNNING, worker.completion())
            .await
            .is_err(),
        "the account worker keeps running"
    );
}

fn assert_held(record: &Value, cause: &str) {
    assert_eq!(record["status"], "failed", "{record}");
    assert_eq!(record["nativeFailureHold"], true, "{record}");
    assert_eq!(record["autoWake"], false, "{record}");
    assert_eq!(record["inFlight"], false, "{record}");
    let error = record["error"].as_str().unwrap_or_default();
    assert!(error.contains(cause), "{error}");
}

/// Wake `agent` and return its accepted-ready turn/start; its thread is resumed first.
async fn start_agent(
    state: &State,
    peer: &mut Peer,
    worker: &AccountHandle,
    agent: &str,
    thread: &str,
) -> Value {
    worker
        .send(AccountCommand::Wake {
            agent_id: agent.to_owned(),
            epoch: state.epoch,
        })
        .await
        .unwrap();
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":thread}})).await;
    peer.write("turn/start").await
}

#[tokio::test]
async fn invalid_record_of_one_agent_in_a_journal_frame_keeps_the_other_agent_working() {
    let state = State::new().await;
    let a = state.agent.clone();
    second_agent(&state, "agent-b", "fake-thread-1", "b-input").await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let a_start = start_agent(&state, &mut peer, &worker, &a, "fake-thread-0").await;
    peer.reply(&a_start, json!({"turn":{"id":"turn-a"}})).await;
    let b_start = start_agent(&state, &mut peer, &worker, "agent-b", "fake-thread-1").await;

    // Agent A's submitted attempt loses its input list: its next turn notice
    // cannot load the attempt (the 2026-10-20 review attempt failure).
    corrupt(
        &state,
        &a,
        "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.events',json('[]')) WHERE id=?1",
    )
    .await;
    let started = peer
        .frame(json!({"method":"turn/started","params":{"threadId":"fake-thread-0","turn":{"id":"turn-a"}}}))
        .await;
    peer.acknowledged_or_worker_exits(started, &mut worker)
        .await;
    assert_held(&record_of(&state, &a).await, &format!("runtime_agents/{a}"));

    // Agent B's later reply is still read and applied on the same connection.
    let reply = peer
        .frame(json!({"id":b_start["id"],"result":{"turn":{"id":"turn-b"}}}))
        .await;
    peer.acknowledged_or_worker_exits(reply, &mut worker).await;
    assert_eq!(state.event_status("b-input").await, "delivered");
    let completed = peer
        .frame(json!({"method":"turn/completed","params":{"threadId":"fake-thread-1","turn":{"id":"turn-b","status":"completed"}}}))
        .await;
    peer.acknowledged_or_worker_exits(completed, &mut worker)
        .await;
    let b = record_of(&state, "agent-b").await;
    assert_eq!(b["lastCompletedTurn"], "turn-b", "{b}");
    assert_ne!(b["status"], "failed", "{b}");

    assert_running(&mut worker).await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err(), "nothing is sent again");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_record_of_one_agent_in_a_provider_reply_retires_the_request_and_keeps_the_worker()
{
    let state = State::new().await;
    let a = state.agent.clone();
    second_agent(&state, "agent-b", "fake-thread-1", "b-input").await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let a_start = start_agent(&state, &mut peer, &worker, &a, "fake-thread-0").await;
    corrupt(
        &state,
        &a,
        "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.events',json('[]')) WHERE id=?1",
    )
    .await;
    let reply = peer
        .frame(json!({"id":a_start["id"],"result":{"turn":{"id":"turn-a"}}}))
        .await;
    peer.acknowledged_or_worker_exits(reply, &mut worker).await;
    assert_held(&record_of(&state, &a).await, &format!("runtime_agents/{a}"));

    let b_start = start_agent(&state, &mut peer, &worker, "agent-b", "fake-thread-1").await;
    peer.reply(&b_start, json!({"turn":{"id":"turn-b"}})).await;
    assert_eq!(state.event_status("b-input").await, "delivered");
    assert_running(&mut worker).await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err(), "nothing is sent again");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn poisoned_durable_effect_is_parked_once_and_does_not_end_the_worker() {
    let state = State::new().await;
    let a = state.agent.clone();
    // A malformed thread request receipt of agent A: every claim of A reads it.
    let receipt_agent = a.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                EventWriteRepository::put_receipt(
                    tx,
                    "account-worker:account:other:broken-attempt:thread/resume:submitted",
                    "submitted",
                    &json!({"context":{"intent":{"agent_id":receipt_agent},"method":"thread/resume"}})
                        .to_string(),
                )?;
                store::persist_effect(tx, "default", &Effect::Wake(receipt_agent, epoch))?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();

    let effects = || async {
        state
            .bridge
            .run(|store| async move {
                Ok(store.test_reader()?.query_row(
                    "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:effect:default:%'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap()
    };
    assert_eq!(effects().await, 1);

    // The worker runs the recovered effect at start and settles it.
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let settled = async {
        while effects().await != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        result = worker.completion() => panic!("the account worker ended: {result:?}"),
        settled = tokio::time::timeout(PROGRESS_DEADLINE, settled) => {
            settled.expect("the poisoned effect is settled");
        }
    }
    worker_barrier(&worker).await;
    assert_running(&mut worker).await;
    let held = record_of(&state, &a).await;
    assert_held(&held, &format!("account_worker/{a}"));
    worker.shutdown().await.unwrap();

    // A restarted worker has nothing to run again and stays up.
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut next = Peer::new();
    let mut replacement = next.worker(bridge).await;
    worker_barrier(&replacement).await;
    assert_running(&mut replacement).await;
    assert_eq!(effects().await, 0);
    assert_eq!(record_of(&state, &a).await, held, "the hold is placed once");
    assert!(next.writes.try_recv().is_err(), "nothing is sent");
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn journal_keeps_reading_behind_a_completion_no_request_can_bind() {
    let state = State::new().await;
    let a = state.agent.clone();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    // The attempt loses its bound turn while no request on its thread is
    // outstanding: its completion can never bind (the 2026-10-20 review
    // turn whose acceptance did not bind it).
    corrupt(
        &state,
        &a,
        "UPDATE runtime_agents SET record=json_remove(record,'$.startAttempt.turnId') WHERE id=?1",
    )
    .await;
    let progress = async {
        let completion = peer
            .frame(json!({"method":"turn/completed","params":{"threadId":"fake-thread-0","turn":{"id":"turn","status":"completed"}}}))
            .await;
        peer.acknowledged_at_least(completion).await;
        let mut last = completion;
        for _ in 0..FRAMES_PAST_THE_UNSETTLED_BOUND {
            last = peer
                .frame(json!({"method":"test/barrier","params":{}}))
                .await;
        }
        peer.acknowledged_at_least(last).await;
    };
    tokio::time::timeout(PROGRESS_DEADLINE, progress)
        .await
        .expect("the journal keeps reading and settling provider frames");
    assert_held(&record_of(&state, &a).await, "has no bound attempt");
    assert_running(&mut worker).await;
    worker.shutdown().await.unwrap();
}

/// Publication receipts of the account, by receipt id.
async fn publications(state: &State) -> Vec<String> {
    state
        .bridge
        .run(|store| async move {
            let reader = store.test_reader()?;
            let mut statement = reader.prepare(
                "SELECT id FROM runtime_operation_receipts \
                 WHERE id LIKE 'account-worker:notification-publication:default:%' ORDER BY id",
            )?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .await
        .unwrap()
}

/// A frame that faults after another agent's frame in the same journal
/// transaction rolls back only itself: the earlier frame's state and its
/// publication receipts commit.
#[tokio::test]
async fn a_faulting_frame_keeps_the_earlier_frames_of_its_batch() {
    let state = State::new().await;
    let a = state.agent.clone();
    second_agent(&state, "agent-b", "fake-thread-1", "b-input").await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let a_start = start_agent(&state, &mut peer, &worker, &a, "fake-thread-0").await;
    peer.reply(&a_start, json!({"turn":{"id":"turn-a"}})).await;
    let b_start = start_agent(&state, &mut peer, &worker, "agent-b", "fake-thread-1").await;
    peer.reply(&b_start, json!({"turn":{"id":"turn-b"}})).await;
    worker_barrier(&worker).await;
    worker.shutdown().await.unwrap();
    corrupt(
        &state,
        &a,
        "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.events',json('[]')) WHERE id=?1",
    )
    .await;
    let before = publications(&state).await;

    // One transaction: agent B's answer delta applies, then agent A's turn
    // notice faults.
    let clock = SessionClock::default();
    let frame = |cursor: u64, value: Value| journal_frame(&clock, Generation(1), cursor, &value);
    let batch = store::journal_batch(
        Arc::clone(&state.bridge.store),
        "default",
        "default",
        &NotificationStreamBuffer::default(),
        vec![
            frame(
                10_000,
                json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-1","turnId":"turn-b","itemId":"answer","delta":"b"}}),
            ),
            frame(
                10_001,
                json!({"method":"turn/started","params":{"threadId":"fake-thread-0","turn":{"id":"turn-a"}}}),
            ),
        ],
        &Value::Null,
        None,
        None,
        Vec::new(),
        None,
        false,
    )
    .await
    .expect("the batch commits");
    assert_eq!(
        batch
            .committed
            .iter()
            .map(|(entry, _)| entry.cursor.0)
            .collect::<Vec<_>>(),
        [10_000]
    );
    assert_eq!(
        batch
            .parked
            .iter()
            .map(|entry| entry.cursor.0)
            .collect::<Vec<_>>(),
        [10_001]
    );
    assert_eq!(batch.faults.len(), 1);
    assert_eq!(batch.faults[0].agent_id, a);

    let after = publications(&state).await;

    assert!(
        after.len() > before.len() && before.iter().all(|id| after.contains(id)),
        "agent B's publication commits: {before:?} -> {after:?}"
    );
    let b = record_of(&state, "agent-b").await;
    assert_ne!(b["status"], "failed", "{b}");
    assert_held(&record_of(&state, &a).await, &format!("runtime_agents/{a}"));
}

/// A fault after the claim returns the unsubmitted input to the queue, so a
/// later message starts the agent and sends that input once.
#[tokio::test]
async fn held_agent_keeps_its_unsent_input_and_a_message_starts_it_once() {
    let state = State::new().await;
    let a = state.agent.clone();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    assert_eq!(state.event_status("input").await, "reserved");
    // The claimed attempt already holds a conflicting thread receipt, as the
    // 2026-10-20 stale start effect did.
    let attempt = record_of(&state, &a).await["startAttempt"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let changed = tx.execute(
                        "UPDATE runtime_execution_attempts SET record=json_set(record,'$.receipts.threadPrepared',json('{\"input\":{\"thread\":\"elsewhere\"},\"result\":false}')) WHERE id=?1",
                        [attempt],
                    )?;
                    assert_eq!(changed, 1);
                    Ok::<_, StoreError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let reply = peer
        .frame(json!({"id":resume["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    peer.acknowledged_or_worker_exits(reply, &mut worker).await;
    let held = record_of(&state, &a).await;
    assert_held(&held, "runtime_execution_attempts/");
    assert!(
        held.get("startAttempt").is_none_or(Value::is_null),
        "{held}"
    );
    assert_eq!(state.event_status("input").await, "pending");

    // A manual message clears the hold (as `coordination` does) and is the
    // user's decision to continue.
    let agent_id = a.clone();
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
    state.wake(&worker).await;
    let start = peer.write_turn_start().await;
    let input = start["params"]["input"].to_string();
    assert!(
        input.contains("input") && input.contains("again"),
        "{start}"
    );
    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert_eq!(state.event_status("again").await, "delivered");
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err(), "the input is sent once");
    assert_running(&mut worker).await;
    worker.shutdown().await.unwrap();
}
