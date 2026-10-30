//! A provider text delta reaches the live path at once, not after the stream cadence.
use super::*;
use crate::notification_dispatch::NotificationPostCommit;
use crate::notification_runtime_effects::NotificationPostCommitSink;
use crate::resource_notify::LiveDelta;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct LiveSink {
    live: Mutex<Vec<LiveDelta>>,
    arrived: tokio::sync::Notify,
}

impl LiveSink {
    async fn wait_for(&self, count: usize) {
        loop {
            let arrived = self.arrived.notified();
            if self.live.lock().expect("live").len() >= count {
                return;
            }
            arrived.await;
        }
    }
}

impl NotificationPostCommitSink for LiveSink {
    fn publish<'a>(
        &'a self,
        _effect: &'a NotificationPostCommit,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), crate::notification_dispatch::NotificationDispatchError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(()) })
    }

    fn live_delta(&self, delta: LiveDelta) {
        self.live.lock().expect("live").push(delta);
        self.arrived.notify_waiters();
    }
}

fn delta_frame(text: &str) -> serde_json::Value {
    json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn",
        "itemId":"answer","delta":text}})
}

#[tokio::test]
async fn a_text_delta_is_live_before_the_stream_cadence_elapses() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let sink = Arc::new(LiveSink::default());
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

    tokio::time::pause();
    let began = tokio::time::Instant::now();
    peer.frame(delta_frame("Hel")).await;
    sink.wait_for(1).await;
    peer.frame(delta_frame("lo")).await;
    sink.wait_for(2).await;

    let live = sink.live.lock().expect("live").clone();
    assert!(
        began.elapsed() < STREAM_CADENCE,
        "no timer stands between the delta and the live path"
    );
    let seen: Vec<_> = live
        .iter()
        .map(|delta| (delta.item_id.as_str(), delta.offset, delta.text.as_str()))
        .collect();
    let id = format!("{}:answer", live[0].agent_id);
    assert_eq!(
        seen,
        vec![(id.as_str(), 0, "Hel"), (id.as_str(), 3, "lo")],
        "each delta carries the text length that precedes it"
    );
    tokio::time::resume();
    worker.shutdown().await.unwrap();
}

