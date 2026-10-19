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
    /// Every delivered effect by cursor and kind; one frame can carry several.
    effects: Mutex<Vec<(u64, String)>>,
}

impl GatedSink {
    fn new(open: bool) -> Arc<Self> {
        Arc::new(Self {
            gate: tokio::sync::Semaphore::new(if open { usize::MAX >> 4 } else { 0 }),
            delivered: Mutex::new(Vec::new()),
            effects: Mutex::new(Vec::new()),
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
            | NotificationPostCommit::AnalyticsCaptured(observation)
            | NotificationPostCommit::AnalyticsStreamCaptured(observation)
            | NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation)
            | NotificationPostCommit::TokenRateUsage(observation) => observation.delivery.cursor,
            NotificationPostCommit::Recovered { .. } => 0,
        };
        let kind = format!("{effect:?}")
            .split(['(', ' ', '{'])
            .next()
            .unwrap_or_default()
            .to_owned();
        Box::pin(async move {
            drop(self.gate.acquire().await.expect("gate"));
            self.delivered.lock().expect("delivered").push(cursor);
            self.effects.lock().expect("effects").push((cursor, kind));
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

static WRITER_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn count_writer_job(event: studio_runtime_store::StoreDiagnosticsEvent) {
    if matches!(
        event,
        studio_runtime_store::StoreDiagnosticsEvent::WriterTransaction { .. }
    ) {
        WRITER_JOBS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn analytics_rows(state: &State) -> i64 {
    rusqlite::Connection::open(state.root.join("analytics.sqlite3"))
        .expect("analytics DB")
        .query_row("SELECT count(*) FROM analytics_notifications", [], |row| {
            row.get(0)
        })
        .expect("notification rows")
}

/// Notification analytics are captured inside the journal commit: the rows exist
/// when the frame is acknowledged, with the sink still shut, and each frame costs
/// one writer transaction (the journal batch) instead of one for the journal, one
/// for its capture and one for its receipt cleanup.
#[tokio::test]
async fn analytics_are_captured_in_the_journal_commit_with_one_writer_job() {
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
    let before = analytics_rows(&state);
    state
        .bridge
        .store
        .set_diagnostics_callback(Some(count_writer_job))
        .await
        .expect("install writer job counter");
    let jobs_before = WRITER_JOBS.load(std::sync::atomic::Ordering::Relaxed);
    let cursor = peer
        .frame(json!({"method":"turn/started","params":{"threadId":"fake-thread-0","turn":{"id":"turn"}}}))
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
    let jobs = WRITER_JOBS.load(std::sync::atomic::Ordering::Relaxed) - jobs_before;
    assert!(
        analytics_rows(&state) > before,
        "captured at acknowledgement"
    );
    assert!(sink.delivered().is_empty(), "the sink is still shut");
    assert_eq!(jobs, 1, "one writer job per journaled frame");
    worker.shutdown().await.expect("stop worker");
}

fn usage_frame(index: usize) -> Value {
    json!({"method":"thread/tokenUsage/updated","params":{"threadId":"fake-thread-0","turnId":"turn",
        "tokenUsage":{"total":{"inputTokens":index,"outputTokens":1,"totalTokens":index + 1},
            "last":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}}})
}

/// Pending publication receipts that carry provider `cursor`.
fn receipts_for(state: &State, cursor: u64) -> i64 {
    state
        .bridge
        .store
        .read()
        .expect("reader")
        .query_row(
            "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:%' \
             AND EXISTS(SELECT 1 FROM json_tree(result) WHERE key='cursor' AND value=?1)",
            [cursor as i64],
            |row| row.get(0),
        )
        .expect("publication receipts")
}

/// The drain writes nothing: the worker's next journal transaction deletes the
/// receipts it delivered, so every frame costs one writer transaction, and no
/// receipt is delivered twice while it waits for that deletion.
#[tokio::test]
async fn delivered_receipts_are_deleted_by_the_next_journal_commit() {
    const FRAME_LIMIT: usize = 64;
    let state = State::new().await;
    let mut peer = Peer::new();
    let sink = GatedSink::new(true);
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
    worker_barrier(&worker).await;
    state
        .bridge
        .store
        .set_diagnostics_callback(Some(count_writer_job))
        .await
        .expect("install writer job counter");
    let jobs_before = WRITER_JOBS.load(std::sync::atomic::Ordering::Relaxed);
    let first = peer.frame(usage_frame(0)).await;
    peer.acknowledged_or_worker_exits(first, &mut worker).await;
    assert!(receipts_for(&state, first) > 0, "committed with the frame");
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while !sink.delivered().contains(&first) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("sink delivery deadline");
    let mut frames = 1;
    while receipts_for(&state, first) > 0 {
        assert!(
            frames < FRAME_LIMIT,
            "a later journal commit deletes the receipt"
        );
        worker_barrier(&worker).await;
        let cursor = peer.frame(usage_frame(frames)).await;
        peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
        frames += 1;
    }
    let jobs = WRITER_JOBS.load(std::sync::atomic::Ordering::Relaxed) - jobs_before;
    assert_eq!(
        jobs, frames,
        "one writer job per journaled frame, none for the drain"
    );
    let effects = sink.effects.lock().expect("effects").clone();
    let mut unique = effects.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(effects.len(), unique.len(), "no receipt delivered twice");
    worker.shutdown().await.expect("stop worker");
}
