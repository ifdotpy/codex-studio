//! A finished stream keeps its closed key only while its turn runs.
use super::*;

/// Bounds a wait for the persisted stream buffer to change; unit: wall time.
const PROGRESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The closed keys and pending bytes of the account's persisted stream buffer.
async fn persisted_streams(state: &State) -> (usize, usize) {
    state
        .bridge
        .run(|store| async move {
            store
                .transaction_with(|tx| {
                    let streams = store::load_streams(tx, "default")?;
                    Ok::<_, WorkerError>((streams.closed_len(), streams.pending_bytes()))
                })
                .await
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_settled_turn_drops_its_closed_keys_from_the_persisted_buffer() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let streamed = peer
        .frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn",
            "itemId":"answer","delta":"do"}}))
        .await;
    peer.acknowledged_at_least(streamed).await;
    let finished = peer
        .frame(
            json!({"method":"item/completed","params":{"threadId":"fake-thread-0","turnId":"turn",
            "item":{"id":"answer","type":"agentMessage","text":"done"}}}),
        )
        .await;
    peer.acknowledged_at_least(finished).await;
    worker_barrier(&worker).await;
    assert_eq!(persisted_streams(&state).await.0, 1);
    let late = peer
        .frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn",
            "itemId":"answer","delta":"late"}}))
        .await;
    peer.acknowledged_at_least(late).await;
    peer.completed().await;
    tokio::time::timeout(PROGRESS_DEADLINE, worker_barrier(&worker))
        .await
        .expect("the worker settles");
    assert_eq!(persisted_streams(&state).await.0, 0);
    // A delta of the settled turn is dropped by the turn: it opens no stream.
    let after = peer
        .frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn",
            "itemId":"answer","delta":"after"}}))
        .await;
    peer.acknowledged_at_least(after).await;
    worker_barrier(&worker).await;
    assert_eq!(
        persisted_streams(&state).await,
        (0, 0),
        "no stream reopened"
    );
    worker.shutdown().await.unwrap();
}
