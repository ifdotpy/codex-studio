//! A queued native review child reaches `review/start` through the account
//! worker wake. Before this path, the wake refused it with
//! `thread-preparation/NativeReview` (agent 5b9d7fe2, 19 October 2026).
use super::*;
use crate::native_review_turn::tests::{Reply, Session};

/// The review child as `orchestration_review` saves it, queued with input.
async fn queue_review_child(state: &State) {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                let mut lead = row.value();
                lead["id"] = json!("review-lead");
                lead["threadId"] = json!("review-lead-thread");
                lead["turnId"] = Value::Null;
                lead["status"] = json!("completed");
                lead["autoWake"] = json!(false);
                lead["inFlight"] = json!(false);
                lead.as_object_mut().unwrap().remove("startAttempt");
                lead.as_object_mut().unwrap().remove("parentId");
                AgentRepository::put(
                    tx,
                    &AgentRecord::from_value(lead).map_err(StoreError::from)?,
                )?;
                row.extra.insert("parentId".into(), json!("review-lead"));
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
}

#[tokio::test]
async fn a_worker_wake_runs_the_pending_native_review_before_queued_input() {
    let state = State::new().await;
    queue_review_child(&state).await;
    let session = Session::new(Reply::Accept, Reply::Accept);
    let mut peer = Peer::new();
    let resources = test_resources()
        .unwrap()
        .with_account_session(Some(Arc::clone(&session) as _));
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    state.wake(&worker).await;
    // The review/start receipt commits after the provider reply.
    let record = state
        .wait_record(|record| record["turnId"].is_string())
        .await;
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

async fn lead_child_results(state: &State) -> Vec<String> {
    state
        .bridge
        .run(|store| async move {
            let connection = store.test_reader()?;
            let mut statement = connection.prepare(
                "SELECT text FROM runtime_events WHERE agent='review-lead' AND kind='child_result' ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

/// A review attempt has no input events. Its `turn/started` and
/// `turn/completed` must settle through the ordinary lifecycle. Before the
/// fix, `validated_events` refused the empty list and the account worker
/// ended with `invalid stored sync object runtime_agents/<child>`, which
/// interrupted every agent on the account (live 8388eb70, 20 October 2026).
#[tokio::test]
async fn a_native_review_turn_starts_completes_and_reports_to_the_parent() {
    let state = State::new().await;
    queue_review_child(&state).await;
    // No later input: the completed review is the child's final result.
    state
        .bridge
        .run(|store| async move {
            store
                .transaction(|tx| {
                    let deleted =
                        tx.execute("DELETE FROM runtime_events WHERE id='annotation:input'", [])?;
                    assert_eq!(deleted, 1, "the queued input row");
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let session = Session::new(Reply::Accept, Reply::Accept);
    // The fake session hands the review to connection generation 7.
    let mut peer = Peer::identity("default", 7, None);
    let resources = test_resources()
        .unwrap()
        .with_account_session(Some(Arc::clone(&session) as _));
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    state.wake(&worker).await;
    let running = state.wait_record(|r| r["turnId"].is_string()).await;
    assert_eq!(running["startAttempt"]["action"], "review");
    assert_eq!(running["startAttempt"]["events"], json!([]));
    // The review/start reply binds the attempt like an ordinary turn/start reply.
    assert_eq!(
        running["startAttempt"]["turnId"], "review-turn",
        "{running}"
    );

    let cursor = peer
        .frame(json!({"method":"turn/started","params":{"threadId":"review-thread","turn":{"id":"review-turn"}}}))
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
    let cursor = peer
        .frame(json!({"method":"item/completed","params":{"threadId":"review-thread","turnId":"review-turn",
            "item":{"id":"review-item","type":"exitedReviewMode","review":"No defects found."}}}))
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"review-thread",
            "turn":{"id":"review-turn","status":"completed"}}}),
        )
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;

    let done = state.wait_record(|r| r["status"] == "completed").await;
    assert_eq!(done["lastCompletedTurn"], "review-turn", "{done}");
    assert_eq!(done["lastAnswer"], "No defects found.", "{done}");
    assert_eq!(done["inFlight"], false);
    let results = lead_child_results(&state).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].contains("No defects found."), "{results:?}");
    worker.shutdown().await.unwrap();
}

/// Live reviewers saved before the fix have no `startAttempt.turnId`: the
/// review/start reply set only the agent turn, then the worker crash left them
/// interrupted with a disconnect receipt. Their `turn/completed` was deferred
/// for good, and 64 unsettled frames behind it stopped the account journal
/// (thread/resume replies included). The replayed completion now settles.
#[tokio::test]
async fn an_unbound_interrupted_review_completion_settles_instead_of_holding_the_journal() {
    let state = State::new().await;
    let agent = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                let attempt = json!({"id":"review-attempt","epoch":epoch,"events":[],"action":"review",
                    "actionRequestId":"native-review:child:0","submitted":true,"accountKey":"default",
                    "connectionGeneration":1,"connectionId":"connection","threadId":"review-thread",
                    "requestTicket":"retained:default:review","created":1.0});
                for (key, value) in [
                    ("threadId", json!("review-thread")),
                    ("turnId", json!("review-turn")),
                    ("status", json!("interrupted")),
                    ("autoWake", json!(false)),
                    ("inFlight", json!(false)),
                    ("turnEpoch", json!(epoch)),
                    ("error", json!(studio_runtime_store::PROVIDER_DISCONNECTED_TURN_ERROR)),
                    ("startAttempt", attempt.clone()),
                    (
                        "disconnectRecovery",
                        json!({"accountKey":"default","epoch":epoch,"autoWake":true,
                            "threadId":"review-thread","startAttempt":attempt}),
                    ),
                    (
                        "nativeReview",
                        json!({"target":{"type":"custom","instructions":"Check"},
                            "requestId":"lead:review:r1","actorId":"lead","args":{},
                            "status":"started","response":{}}),
                    ),
                ] {
                    row.extra.insert(key.into(), value);
                }
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"review-thread",
            "turn":{"id":"review-turn","status":"completed"}}}),
        )
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
    let done = state.wait_record(|r| r["status"] == "completed").await;
    assert_eq!(done["lastCompletedTurn"], "review-turn", "{done}");
    assert_eq!(done["startAttempt"]["turnId"], "review-turn", "{done}");
    worker.shutdown().await.unwrap();
}
