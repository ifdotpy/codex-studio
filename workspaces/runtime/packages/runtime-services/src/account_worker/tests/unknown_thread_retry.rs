//! A thread request carries no input; only turn/start does. When its outcome is
//! unknown while the provider session stays open (its reply deadline passed),
//! the input returns to the queue once and the agent starts again. A second
//! unknown outcome holds the input. Before, the first unknown outcome held the
//! input until the owner sent a new message.
use super::*;
use studio_runtime_schema::shared_values::PROVIDER_RESUME_UNKNOWN_ERROR as UNKNOWN_HOLD;

/// Pass the 60 s reply deadline, then prove the worker consumed that outcome.
async fn pass_reply_deadline(peer: &mut Peer) {
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    tokio::time::resume();
    let cursor = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    peer.acknowledged(cursor).await;
}

#[tokio::test]
async fn an_unknown_thread_resume_retries_once_and_sends_the_input_once() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let first = peer.write("thread/resume").await;
    pass_reply_deadline(&mut peer).await;

    let retry = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        peer.write("thread/resume"),
    )
    .await
    .expect("the unknown request is retried once without a new message");
    assert_ne!(retry["id"], first["id"], "{retry}");
    // The first request's late reply is stale: it neither prepares nor starts a turn.
    let late = peer
        .frame(json!({"id":first["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    peer.acknowledged_or_worker_exits(late, &mut worker).await;

    peer.reply(&retry, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let start = peer.write("turn/start").await;
    assert!(
        start["params"]["input"].to_string().contains("input"),
        "the original input is submitted: {start}"
    );
    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert!(state.record().await["error"].is_null());
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "no request and no input is sent twice"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_unknown_thread_resume_holds_the_input() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    peer.write("thread/resume").await;
    pass_reply_deadline(&mut peer).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        peer.write("thread/resume"),
    )
    .await
    .expect("the first unknown request is retried");
    pass_reply_deadline(&mut peer).await;

    let held = state.record().await;
    assert_eq!(held["error"], UNKNOWN_HOLD, "{held}");
    assert_eq!(state.event_status("input").await, "pending");
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the second unknown request is not retried"
    );
    worker.shutdown().await.unwrap();
}
