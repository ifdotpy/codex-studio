//! A thread/resume whose saved Claude session file is gone cannot succeed, so
//! holding its input as pending waits forever. The input was never sent: it
//! settles as not sent, so its owner can send it again, and the chat says why.
//! The transcript is kept.
use super::*;
use studio_runtime_schema::shared_values::PROVIDER_SESSION_MISSING_ERROR as SESSION_MISSING_ERROR;

#[tokio::test]
async fn a_missing_saved_session_settles_the_input_as_not_sent() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    let reply = peer
        .frame(json!({"id":resume["id"],"error":{
            "code":-32000,
            "message":"The saved Claude session fake-thread-0 is missing.",
            "data":{"sessionMissing":true}
        }}))
        .await;
    peer.acknowledged_or_worker_exits(reply, &mut worker).await;

    let record = state.record().await;
    assert_eq!(record["error"], SESSION_MISSING_ERROR, "{record}");
    assert_eq!(record["status"], "failed", "{record}");
    assert_eq!(state.event_status("input").await, "failed");
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "nothing is sent: no retry and no turn/start"
    );
    worker.shutdown().await.unwrap();
}
