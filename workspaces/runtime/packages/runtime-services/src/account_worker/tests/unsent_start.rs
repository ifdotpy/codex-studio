//! A start request whose reply deadline elapsed before Studio wrote it never
//! reached the provider. Its input returns to the queue and the agent starts
//! again; the stored reason names the request and the cause (agents 83e59e82
//! and d0c9c794, 22 October 2026: both showed "Provider did not send
//! turn/start" and waited for a new message).
use super::*;

const DEADLINE_REASON: &str = "Provider did not send thread/resume: its reply deadline elapsed before Studio wrote it. The input is kept.";

/// Hold the next `submitted` commit, which runs after the request deadline is
/// fixed, until that deadline has elapsed; then release it.
async fn expire_held_submission(state: &State) -> String {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("the submitted commit is held");
    let attempt = state.record().await["startAttempt"]["id"]
        .as_str()
        .expect("a claimed attempt")
        .to_owned();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    tokio::time::resume();
    state.bridge.faults.release.notify_one();
    attempt
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
async fn an_expired_unwritten_thread_request_returns_its_input_and_starts_again() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("submitted".into());
    state.wake(&worker).await;
    let first = expire_held_submission(&state).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = None;

    // The expired request is never written; a new attempt resumes the thread.
    let resume = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        peer.write("thread/resume"),
    )
    .await
    .expect("the returned input starts again without a new message");
    assert!(
        !resume["id"].as_str().unwrap().contains(&first),
        "the expired request is not written: {resume}"
    );
    let saved = attempt_record(&state, &first).await;
    assert_eq!(saved["error"], DEADLINE_REASON, "{saved}");
    assert_eq!(saved["startReturned"], true, "{saved}");

    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let start = peer.write("turn/start").await;
    assert!(
        start["params"]["input"].to_string().contains("input"),
        "the original input is submitted once: {start}"
    );
    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert!(state.record().await["error"].is_null());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_start_that_stays_unsent_fails_with_its_cause_and_keeps_its_input() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("submitted".into());
    state.wake(&worker).await;
    // Each return starts a new attempt; the one after the limit fails.
    for _ in 0..=studio_runtime_store::START_RETURN_LIMIT {
        expire_held_submission(&state).await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while state.record().await["status"] != "failed" {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the start fails after its returns are used up");
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    let record = state.record().await;
    assert_eq!(record["error"], DEADLINE_REASON, "{record}");
    assert_eq!(state.event_status("input").await, "pending");
    assert!(
        peer.writes.try_recv().is_err(),
        "an expired request is never written"
    );
    worker.shutdown().await.unwrap();
}
