//! Account notifications have no thread ID. Their durable effects survive replay.
use super::*;
use std::time::Duration;
use studio_runtime_providers::{DecodedJournalPayload, decode_journal_value};

#[tokio::test]
async fn live_limits_journal_pins_both_providers_and_suppresses_replay() {
    for provider in ["codex", "claude"] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        let payload = json!({"method":"account/rateLimits/updated","params":{
            "rateLimits":{"limitId":provider,"primary":{"usedPercent":100.0,"resetsAt":9000.0}}
        }});
        let source = crate::notification_dispatch::NativeProcessSource {
            account: "default".into(),
            connection: "connection".into(),
            generation: Some(7),
            provider: provider.into(),
        };
        let pin = json!({"provider":provider});
        let mut streams = NotificationStreamBuffer::default();
        for replay in [false, true] {
            let DecodedJournalPayload::Frame(frame) = decode_journal_value(1, 7, &payload).payload
            else {
                panic!("notification");
            };
            let entry = JournalEntry {
                received_at_ms: 1,
                cursor: EventCursor(1),
                generation: Generation(7),
                payload: JournalPayload::Frame(frame),
            };
            let result = store::journal_batch(
                store.clone(),
                "default",
                "default",
                &streams,
                vec![entry],
                &pin,
                None,
                Some(source.clone()),
                Vec::new(),
                None,
                false,
            )
            .await
            .unwrap();
            assert!(result.deferred.is_empty());
            assert_eq!(result.committed.len(), 1);
            if replay {
                assert!(result.committed[0].1.is_none());
            } else {
                let effect = result.committed[0]
                    .1
                    .as_ref()
                    .expect("account notification creates an effect");
                let value = serde_json::to_value(effect).unwrap();
                assert_eq!(
                    value["LimitsUpdated"]["scope"],
                    json!({"account":"default","connection":"connection","generation":7})
                );
                assert_eq!(value["LimitsUpdated"]["cursor"], 1);
                assert_eq!(value["LimitsUpdated"]["data"], payload["params"]);
            }
            streams = result.streams;
        }
        let effects = store::recover_effects(&store.test_reader().unwrap(), "default").unwrap();
        assert_eq!(
            effects.len(),
            1,
            "one durable effect remains until ingestion commits"
        );
    }
}

struct CapturedLimits {
    updates: tokio::sync::mpsc::UnboundedSender<crate::account_session_runtime::LimitsUpdate>,
}
impl crate::account_session_runtime::AccountSessionPort for CapturedLimits {
    fn ingest_limits<'a>(
        &'a self,
        update: crate::account_session_runtime::LimitsUpdate,
    ) -> crate::account_session_runtime::AccountSessionFuture<'a, bool> {
        Box::pin(async move {
            self.updates.send(update).unwrap();
            Ok(true)
        })
    }
}

#[tokio::test]
async fn live_limits_worker_delivers_account_notification_to_session_owner() {
    let state = State::new().await;
    let (updates, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut resources = test_resources().unwrap();
    resources.account_session = Some(Arc::new(CapturedLimits { updates }));
    let mut peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let cursor=peer.frame(json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":100.0}}}})).await;
    peer.acknowledged(cursor).await;
    let update = tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(update.scope.account, "default");
    assert_eq!(update.scope.generation, peer.generation.0);
    assert!(!update.scope.connection.is_empty());
    assert_eq!(update.cursor, cursor);
    assert_eq!(update.data["rateLimits"]["primary"]["usedPercent"], 100.0);
    worker.shutdown().await.unwrap();
    assert!(received.try_recv().is_err());
}
