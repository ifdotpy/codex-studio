//! A provider request whose generation has exited can never be answered. It
//! settles without a reply, so its cursor cannot hold the journal intake in
//! front of the new generation's initialize reply, and no answer meant for
//! the exited process reaches its successor.
use super::*;

/// The provider generation that sent the request and then exited.
const EXITED: u64 = 5;
/// The provider generation the worker serves.
const CURRENT: u64 = 6;
/// The exited generation's request id; the new process never issued it.
const EXITED_REQUEST_ID: u64 = 260;
/// Bounds a wait for progress that a stalled journal would never make;
/// unit: wall time.
const PROGRESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Send one frame of the exited generation at the peer's next cursor. Read
/// ACKs meanwhile into `acked`, so the fake provider never waits on them.
async fn exited_frame(peer: &mut Peer, acked: &mut u64, value: Value) -> u64 {
    peer.cursor += 1;
    let entry = journal_frame(&peer.clock, Generation(EXITED), peer.cursor, &value);
    let slot = tokio::time::timeout(PROGRESS_DEADLINE, async {
        loop {
            tokio::select! {
                slot = peer.entries.reserve() => break slot.unwrap(),
                ack = peer.acks.recv() => *acked = (*acked).max(ack.unwrap().0),
            }
        }
    })
    .await
    .expect("the session reads the exited generation's frames");
    slot.send(entry);
    peer.cursor
}

/// Answer `initialize` through the journal and wait for the session to be
/// ready, reading ACKs into `acked` meanwhile. Return the reply's cursor.
async fn initialize_reply(peer: &mut Peer, acked: &mut u64, initialize: &Value) -> u64 {
    let reply = peer.frame(json!({"id":initialize["id"],"result":{}})).await;
    let mut ready = peer.ready.clone();
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        while !*ready.borrow_and_update() {
            tokio::select! {
                changed = ready.changed() => changed.unwrap(),
                ack = peer.acks.recv() => *acked = (*acked).max(ack.unwrap().0),
            }
        }
    })
    .await
    .expect("the new generation becomes ready behind the exited generation's frames");
    reply
}

/// Wait until the contiguous ACK reaches `cursor`.
async fn acknowledged_through(peer: &mut Peer, acked: &mut u64, cursor: u64) {
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        while *acked < cursor {
            *acked = (*acked).max(peer.acks.recv().await.unwrap().0);
        }
    })
    .await
    .expect("the journal is acknowledged past the exited generation's request");
}

/// No provider write answers the exited generation's request.
fn assert_no_exited_reply(peer: &mut Peer) {
    while let Ok(wire) = peer.writes.try_recv() {
        assert!(
            wire.value.get("method").is_some() || wire.value["id"] != json!(EXITED_REQUEST_ID),
            "an exited generation's request was answered to its successor: {}",
            wire.value
        );
    }
}

/// The exited generation's journal receipt is ready, so its checkpoint covers it.
async fn assert_exited_checkpoint(state: &State, cursor: u64) {
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    assert!(
                        store::checkpoint(tx, "default", EXITED)? >= cursor,
                        "the exited generation's checkpoint passes its request"
                    );
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn an_exited_generation_request_settles_unanswered_and_the_new_generation_becomes_ready() {
    let state = State::new().await;
    let mut peer = Peer::initialized_by_journal("default", CURRENT);
    let mut worker = peer.open_worker(state.bridge.clone()).await;
    let initialize = peer.write("initialize").await;
    let mut acked = 0;
    let request = exited_frame(
        &mut peer,
        &mut acked,
        json!({"id":EXITED_REQUEST_ID,"method":"item/tool/call","params":{
            "threadId":"exited-thread","turnId":"exited-turn","callId":"call",
            "tool":"orchestration_spawn","arguments":{}
        }}),
    )
    .await;
    // With the request, one full unsettled bound of the exited generation.
    for _ in 1..crate::provider_session_task::MAX_UNSETTLED {
        exited_frame(
            &mut peer,
            &mut acked,
            json!({"method":"thread/tokenUsage/updated","params":{}}),
        )
        .await;
    }
    initialize_reply(&mut peer, &mut acked, &initialize).await;
    worker_barrier(&worker).await;
    assert_no_exited_reply(&mut peer);
    assert_exited_checkpoint(&state, request).await;
    // The journal reads on past the exited generation's frames.
    let later = peer
        .frame(json!({"method":"thread/tokenUsage/updated","params":{}}))
        .await;
    acknowledged_through(&mut peer, &mut acked, later).await;
    assert_no_exited_reply(&mut peer);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reply_to_an_exited_generation_request_is_refused_at_the_reply_fence() {
    let state = State::new().await;
    // Reach the reply fence: journal the exited request as a current one.
    state
        .bridge
        .faults
        .journal_exited_requests_as_current
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut peer = Peer::initialized_by_journal("default", CURRENT);
    let mut worker = peer.open_worker(state.bridge.clone()).await;
    let initialize = peer.write("initialize").await;
    let mut acked = 0;
    let request = exited_frame(
        &mut peer,
        &mut acked,
        json!({"id":EXITED_REQUEST_ID,"method":"item/commandExecution/requestApproval","params":{}}),
    )
    .await;
    let reply = initialize_reply(&mut peer, &mut acked, &initialize).await;
    // The refused request settles, so the ACK passes it.
    acknowledged_through(&mut peer, &mut acked, reply).await;
    worker_barrier(&worker).await;
    assert_no_exited_reply(&mut peer);
    assert_exited_checkpoint(&state, request).await;
    worker.shutdown().await.unwrap();
}
