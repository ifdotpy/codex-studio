//! An Unknown thread request remains held and is never retried automatically.
use super::*;

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
async fn an_unknown_thread_resume_holds_input_without_retry() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    peer.write("thread/resume").await;
    pass_reply_deadline(&mut peer).await;
    worker_barrier(&worker).await;
    assert_eq!(state.event_status("input").await, "pending");
    assert!(
        peer.writes.try_recv().is_err(),
        "Unknown is never retried automatically"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unknown_thread_resume_is_not_retried_after_restart() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    peer.write("thread/resume").await;
    pass_reply_deadline(&mut peer).await;
    worker.shutdown().await.unwrap();
    let mut next = Peer::identity("default", 2, None);
    let mut replacement = next.worker(state.bridge.clone()).await;
    state.wake(&replacement).await;
    worker_barrier(&replacement).await;
    assert_eq!(state.event_status("input").await, "pending");
    assert!(
        next.writes.try_recv().is_err(),
        "restart does not make Unknown safe to resend"
    );
    replacement.shutdown().await.unwrap();
}
