//! A queued native review child reaches `review/start` through the account
//! worker wake. Before this path, the wake refused it with
//! `thread-preparation/NativeReview` (agent 5b9d7fe2, 19 October 2026).
use super::*;
use crate::native_review_turn::tests::{Reply, Session};

#[tokio::test]
async fn a_worker_wake_runs_the_pending_native_review_before_queued_input() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                for (key, value) in [
                    ("threadId", Value::Null),
                    ("turnId", Value::Null),
                    ("status", json!("queued")),
                    ("autoWake", json!(true)),
                    ("inFlight", json!(false)),
                    ("role", json!("reviewer")),
                    ("yoloMode", json!(false)),
                    ("worktree", json!(false)),
                    ("nativeEffort", json!("medium")),
                    (
                        "nativeReview",
                        json!({"target":{"type":"custom","instructions":"Check the change"},
                            "requestId":"lead:review:r1","actorId":"lead","args":{},
                            "status":"pending","response":{}}),
                    ),
                ] {
                    row.extra.insert(key.into(), value);
                }
                row.extra.remove("startAttempt");
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    let session = Session::new(Reply::Accept, Reply::Accept);
    let mut peer = Peer::new();
    let resources = test_resources()
        .unwrap()
        .with_account_session(Some(Arc::clone(&session) as _));
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    state.wake(&worker).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    // The review/start receipt commits after the provider reply.
    let record = loop {
        let record = state.record().await;
        if record["turnId"].is_string() {
            break record;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "review requests: {:?}",
            session.methods()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        session.methods(),
        ["thread/start", "thread/settings/update", "review/start"]
    );
    assert_eq!(record["turnId"], "review-turn", "{record}");
    assert_eq!(record["status"], "running");
    assert_eq!(record["nativeReview"]["status"], "started");
    assert!(record["error"].is_null(), "{record}");
    // The queued input waits for the review turn; it is not a turn/start.
    assert_eq!(state.event_status("input").await, "pending");
    assert!(peer.writes.try_recv().is_err(), "no ordinary turn request");
    worker.shutdown().await.unwrap();
}
