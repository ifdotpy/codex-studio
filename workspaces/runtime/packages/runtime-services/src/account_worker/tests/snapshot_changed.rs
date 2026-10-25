//! A lead's turn whose state changes between its preparation and its commit
//! (a child finishes) was refused at commit and the lead failed. Nothing was
//! sent, so its input returns to the queue and the lead prepares the turn
//! again from the current state (lead 85bc2231, 22 October 2026: failed with
//! "turn-preparation/snapshot-changed" while two child results stayed pending).
use super::*;

/// A child whose result is the lead's queued input.
const FINISHED_CHILD: &str = "finished-child";
/// A child that finishes while the lead's turn waits for its commit.
const LATE_CHILD: &str = "late-child";

fn child_result(child: &str) -> String {
    json!({"agent":child,"name":child,"status":"completed","result":format!("{child} result")})
        .to_string()
}

/// Add a child of the fixture lead with `status`. The child never runs here.
fn put_child(
    tx: &rusqlite::Transaction<'_>,
    lead: &str,
    child: &str,
    status: studio_runtime_store::AgentStatus,
) -> Result<(), WorkerError> {
    let mut row = AgentRepository::get(tx, &AgentId::from(lead))?.expect("lead");
    row.id = child.into();
    row.status = status;
    row.extra.insert("parentId".into(), json!(lead));
    row.extra.insert("isLead".into(), json!(false));
    row.extra.insert("autoWake".into(), json!(false));
    row.extra
        .insert("threadId".into(), json!(format!("{child}-thread")));
    AgentRepository::put(tx, &row)?;
    Ok(())
}

/// The fixture lead with one finished child, whose result is the queued
/// input, and one running child. The child-result hold is off, so the result
/// starts a turn while its sibling runs (as after the hold has elapsed).
async fn lead_with_children(state: &State) {
    let lead = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    use studio_runtime_store::AgentStatus;
                    studio_runtime_store::OrchestrationSettingsRepository::set_child_result_coalesce(
                        tx, 0,
                    )?;
                    put_child(tx, &lead, FINISHED_CHILD, AgentStatus::Completed)?;
                    put_child(tx, &lead, LATE_CHILD, AgentStatus::Running)?;
                    tx.execute(
                        "UPDATE runtime_events SET kind='child_result',text=?1 WHERE id='annotation:input'",
                        [child_result(FINISHED_CHILD)],
                    )?;
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
}

/// The running child finishes and its result reaches the lead's queue.
async fn late_child_finishes(state: &State) {
    let (lead, epoch) = (state.agent.clone(), state.epoch);
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut child =
                        AgentRepository::get(tx, &AgentId::from(LATE_CHILD))?.expect("child");
                    child.status = studio_runtime_store::AgentStatus::Completed;
                    AgentRepository::put(tx, &child)?;
                    tx.execute(
                        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                         VALUES(?1,?2,'child_result',?3,'pending',?4,?5)",
                        rusqlite::params![
                            format!("annotation:{LATE_CHILD}"),
                            lead,
                            child_result(LATE_CHILD),
                            now(),
                            epoch
                        ],
                    )?;
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
}

/// Wait until a `submitted` commit is held; return the held attempt.
async fn held(state: &State) -> String {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("the submitted commit is held");
    state.record().await["startAttempt"]["id"]
        .as_str()
        .expect("a claimed attempt")
        .to_owned()
}

/// Hold every `submitted` commit, wake the lead, and let its thread resume
/// pass. Return the attempt once its prepared turn/start waits for its commit.
async fn hold_turn_commit(state: &State, peer: &mut Peer, worker: &AccountHandle) -> String {
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("submitted".into());
    state.wake(worker).await;
    held(state).await;
    state.bridge.faults.release.notify_one();
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    held(state).await
}

fn release_last(state: &State) {
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_one();
}

async fn attempt_record(state: &State, attempt: &str) -> Value {
    let attempt = attempt.to_owned();
    state
        .bridge
        .run(move |store| async move {
            let raw: String = store.test_reader()?.query_row(
                "SELECT record FROM runtime_execution_attempts WHERE id=?1",
                [attempt],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str(&raw).expect("attempt record"))
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_child_finishing_before_commit_prepares_the_lead_turn_again() {
    let state = State::new().await;
    lead_with_children(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let first = hold_turn_commit(&state, &mut peer, &worker).await;
    late_child_finishes(&state).await;
    release_last(&state);

    let Ok(start) =
        tokio::time::timeout(std::time::Duration::from_secs(20), peer.write("turn/start")).await
    else {
        let record = state.record().await;
        panic!(
            "the lead prepares its turn again instead of failing: status={} error={}",
            record["status"], record["error"]
        );
    };
    let text = start["params"]["input"].to_string();
    assert!(
        text.contains(&format!("{LATE_CHILD} result")),
        "the next preparation includes the new child result: {text}"
    );
    assert!(
        text.contains("2 finished") && !text.contains("1 running"),
        "the turn carries the current team state, never the stale one: {text}"
    );
    let saved = attempt_record(&state, &first).await;
    assert_eq!(saved["startReturned"], true, "{saved}");
    assert!(
        saved["error"]
            .as_str()
            .is_some_and(|error| error.contains("turn-preparation/snapshot-changed: workers")),
        "the returned attempt names the changed part: {saved}"
    );

    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert_eq!(state.event_status(LATE_CHILD).await, "delivered");
    assert!(state.record().await["error"].is_null());
    assert!(
        peer.writes.try_recv().is_err(),
        "the input is submitted once"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lead_whose_state_changes_before_every_commit_fails_naming_the_change() {
    let state = State::new().await;
    lead_with_children(&state).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    hold_turn_commit(&state, &mut peer, &worker).await;
    // Each held commit sees one more running child; the commit after the
    // last allowed return fails.
    for index in 0..=studio_runtime_store::START_RETURN_LIMIT {
        if index > 0 {
            held(&state).await;
        }
        let lead = state.agent.clone();
        state
            .bridge
            .run(move |store| async move {
                store
                    .transaction_with(move |tx| {
                        put_child(
                            tx,
                            &lead,
                            &format!("new-child-{index}"),
                            studio_runtime_store::AgentStatus::Running,
                        )
                    })
                    .await
            })
            .await
            .unwrap();
        if index == studio_runtime_store::START_RETURN_LIMIT {
            release_last(&state);
        } else {
            state.bridge.faults.release.notify_one();
        }
    }
    state
        .wait_record(|record| record["status"] == "failed")
        .await;
    let record = state.record().await;
    assert_eq!(
        record["error"],
        "Provider did not send turn/start: the agent state changed while Studio prepared it \
         (turn-preparation/snapshot-changed: workers). The input is kept.",
        "{record}"
    );
    assert_eq!(state.event_status("input").await, "pending");
    assert!(
        peer.writes.try_recv().is_err(),
        "a stale compiled turn is never written"
    );
    worker.shutdown().await.unwrap();
}
