use super::*;
use crate::notification_dispatch::NotificationPostCommit;
use crate::notification_runtime_effects::NotificationPostCommitSink;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Records delivered cursors; delivery waits for a permit, so a test can hold
/// the sink shut and later open it.
struct GatedSink {
    gate: tokio::sync::Semaphore,
    delivered: Mutex<Vec<u64>>,
}

impl GatedSink {
    fn new(open: bool) -> Arc<Self> {
        Arc::new(Self {
            gate: tokio::sync::Semaphore::new(if open { usize::MAX >> 4 } else { 0 }),
            delivered: Mutex::new(Vec::new()),
        })
    }
    fn open(&self) {
        self.gate.add_permits(usize::MAX >> 5);
    }
    fn delivered(&self) -> Vec<u64> {
        self.delivered.lock().expect("delivered").clone()
    }
}

impl NotificationPostCommitSink for GatedSink {
    fn publish<'a>(
        &'a self,
        effect: &'a NotificationPostCommit,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), crate::notification_dispatch::NotificationDispatchError>,
                > + Send
                + 'a,
        >,
    > {
        let cursor = match effect {
            NotificationPostCommit::Analytics { observation, .. }
            | NotificationPostCommit::AnalyticsStream { observation, .. }
            | NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation)
            | NotificationPostCommit::TokenRateUsage(observation) => observation.delivery.cursor,
            NotificationPostCommit::Recovered { .. } => 0,
        };
        Box::pin(async move {
            drop(self.gate.acquire().await.expect("gate"));
            self.delivered.lock().expect("delivered").push(cursor);
            Ok(())
        })
    }
}

async fn wait_delivered(sink: &GatedSink, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while sink.delivered().len() < count {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("sink delivery deadline");
}

fn delta_frame(index: usize) -> Value {
    json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0",
        "turnId":"turn","itemId":"drain-off-path","delta":format!("part-{index};")}})
}

/// A sink that never answers must not stall the account worker: later frames
/// are journaled and acknowledged, commands are served, and once the sink opens
/// every receipt arrives in journal order, each exactly once.
#[tokio::test]
async fn blocked_sink_does_not_delay_the_worker_and_order_is_kept() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let sink = GatedSink::new(false);
    let mut worker = peer
        .open_worker_with_resources_and_sink(
            state.bridge.clone(),
            test_resources().unwrap(),
            sink.clone(),
        )
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let mut cursors = Vec::new();
    for index in 0..4 {
        let cursor = peer.frame(delta_frame(index)).await;
        peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
        cursors.push(cursor);
    }
    worker_barrier(&worker).await;
    assert!(sink.delivered().is_empty(), "the sink is still shut");
    sink.open();
    wait_delivered(&sink, cursors.len()).await;
    let delivered = sink.delivered();
    let mut sorted = delivered.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(delivered, sorted, "journal order, no duplicates");
    worker.shutdown().await.expect("stop worker");
}

/// Receipts that were committed but undelivered when the worker stopped are
/// replayed by the next worker, once.
#[tokio::test]
async fn restart_replays_undelivered_publications_once() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let blocked = GatedSink::new(false);
    let mut worker = peer
        .open_worker_with_resources_and_sink(
            state.bridge.clone(),
            test_resources().unwrap(),
            blocked.clone(),
        )
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let mut cursors = Vec::new();
    for index in 0..3 {
        let cursor = peer.frame(delta_frame(index)).await;
        peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
        cursors.push(cursor);
    }
    worker.shutdown().await.expect("stop worker");
    assert!(blocked.delivered().is_empty());

    let peer = Peer::new();
    let open = GatedSink::new(true);
    let mut worker = peer
        .open_worker_with_resources_and_sink(
            state.bridge.clone(),
            test_resources().unwrap(),
            open.clone(),
        )
        .await;
    wait_delivered(&open, 1).await;
    worker_barrier(&worker).await;
    let delivered = open.delivered();
    let mut sorted = delivered.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(delivered, sorted, "replayed in order, each once");
    assert!(delivered.len() >= cursors.len());
    worker.shutdown().await.expect("stop worker");
}
