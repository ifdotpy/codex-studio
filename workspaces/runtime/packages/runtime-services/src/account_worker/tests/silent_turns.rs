//! A running Codex turn whose provider sent nothing for 10 minutes while no
//! tool runs (liveness `turn_stalled`). The worker reads native history with
//! the existing turn recovery: an ended turn settles once, a running one is
//! left alone, and no input or turn request is ever sent.
use super::*;
use crate::account_worker::silent_turns;

/// Make the chat's newest provider event `seconds` old.
async fn silence(state: &State, seconds: f64) {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let at = crate::event_write::unix_time().unwrap() - seconds;
                    let stamp = chrono::DateTime::from_timestamp(at as i64, 0)
                        .unwrap()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    tx.execute(
                        "UPDATE runtime_agent_volatile SET activity=?1,lastEvent=?2,activeTools='[]' WHERE id=?3",
                        rusqlite::params![
                            json!({"phase":"thinking","at":at}).to_string(),
                            json!(stamp).to_string(),
                            agent
                        ],
                    )?;
                    Ok::<_, studio_runtime_store::StoreError>(())
                })
                .await
                .map_err(WorkerError::from)
        })
        .await
        .unwrap();
}

fn pass(state: &State, worker: &AccountHandle) -> silent_turns::Pass {
    silent_turns::Pass {
        bridge: state.bridge.clone(),
        account: "default".into(),
        scope: worker.retained_session().scope().clone(),
        session: worker.retained_session(),
        commands: worker.commands.clone(),
    }
}

async fn running_turn() -> (State, Peer, AccountHandle) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["status"], "running", "{record}");
    assert_eq!(record["inFlight"], true, "{record}");
    assert_eq!(record["turnId"], "turn", "{record}");
    silence(&state, 700.0).await;
    (state, peer, worker)
}

async fn liveness_rows(state: &State) -> Vec<crate::liveness::Finding> {
    state
        .bridge
        .run(|store| async move {
            store
                .read_async("tests/silent_turns", |tx| {
                    crate::liveness::findings(tx, crate::event_write::unix_time().unwrap())
                })
                .await
                .map_err(WorkerError::from)
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_turn_that_native_history_ended_is_settled_once() {
    let (state, mut peer, mut worker) = running_turn().await;
    let check = tokio::spawn(silent_turns::check_account(pass(&state, &worker)));
    for method in ["thread/read", "thread/turns/list", "thread/read"] {
        let request = peer.write(method).await;
        let result = if method == "thread/read" {
            json!({"thread":{"id":"fake-thread-0","status":{"type":"idle"}}})
        } else {
            json!({"data":[{"id":"turn","threadId":"fake-thread-0","status":"completed",
                "items":[{"id":"answer","type":"agentMessage","text":"done"}]}]})
        };
        peer.reply(&request, result).await;
    }
    check.await.unwrap();
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["inFlight"], false, "the turn settled: {record}");
    assert_eq!(record["lastCompletedTurn"], "turn", "{record}");
    assert!(
        peer.writes.try_recv().is_err(),
        "the check sends no input or turn request"
    );
    assert!(
        liveness_rows(&state)
            .await
            .iter()
            .all(|finding| finding.agent != state.agent),
        "a settled turn is not stuck"
    );

    // The next pass finds no running turn and sends nothing.
    silent_turns::check_account(pass(&state, &worker)).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "a settled turn is checked once"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_turn_that_native_history_shows_running_is_left_alone() {
    let (state, mut peer, mut worker) = running_turn().await;
    let check = tokio::spawn(silent_turns::check_account(pass(&state, &worker)));
    let request = peer.write("thread/read").await;
    peer.reply(
        &request,
        json!({"thread":{"id":"fake-thread-0","status":{"type":"active"}}}),
    )
    .await;
    check.await.unwrap();
    let record = state.record().await;
    assert_eq!(record["status"], "running", "{record}");
    assert_eq!(record["inFlight"], true, "{record}");
    assert!(
        liveness_rows(&state)
            .await
            .iter()
            .all(|finding| finding.agent != state.agent),
        "a turn that runs natively is not shown as stuck"
    );
    // The same silence step is not read again.
    silent_turns::check_account(pass(&state, &worker)).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "one check per silence step"
    );
    worker.shutdown().await.unwrap();
}
