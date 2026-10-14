use super::*;
use crate::notification_dispatch::{
    NotificationObservation, NotificationPostCommit, NotificationPublicationBatch,
    ProviderDeliveryIdentity,
};
use crate::notification_runtime_effects::{
    NotificationPostCommitSink, RuntimeNotificationPostCommitSink,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_store::RuntimeStore;

static NEXT_ROOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static TEMPLATE: std::sync::OnceLock<(PathBuf, Value)> = std::sync::OnceLock::new();

fn publication_effect(cursor: u64) -> NotificationPostCommit {
    NotificationPostCommit::TokenRateStream(NotificationObservation {
        agent: None,
        method: "item/agentMessage/delta".into(),
        params: json!({"threadId":"thread","delta":"sample"}),
        delivery: ProviderDeliveryIdentity {
            provider_handle: "account:default".into(),
            generation: 7,
            cursor,
        },
        observed_at: 10.0 + cursor as f64,
        provider_at: 10.0 + cursor as f64,
    })
}

fn root() -> PathBuf {
    test_root().join(format!(
        "publication-drain-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

fn test_store() -> (PathBuf, RuntimeStore) {
    let (source, _) = TEMPLATE.get_or_init(template).clone();
    let root = root();
    copy_template(&source, &root);
    let store = RuntimeStore::open(Some(&root)).expect("open store");
    (root, store)
}

fn stage(store: &RuntimeStore, sequence: u64, index: usize, value: Value) {
    store
        .transaction_with(|tx| {
            let operation = format!("notification-publication:default:{sequence}:{index}");
            store::save(tx, &operation, "pending", value)
        })
        .expect("commit publication receipt");
}

fn value(sequence: u64, index: usize, effect: NotificationPostCommit) -> Value {
    json!({"account":"default","sequence":sequence,"index":index,"effect":effect})
}

fn pending_count(store: &RuntimeStore) -> i64 {
    store
        .read()
        .expect("read store")
        .query_row(
            "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:%'",
            [],
            |row| row.get(0),
        )
        .expect("count publication receipts")
}

#[derive(Default)]
struct RecordingSink {
    seen: Mutex<Vec<NotificationPostCommit>>,
    fail_cursor: Mutex<Option<u64>>,
}

impl NotificationPostCommitSink for RecordingSink {
    fn publish(
        &self,
        effect: &NotificationPostCommit,
    ) -> Result<(), crate::notification_dispatch::NotificationDispatchError> {
        let cursor = match effect {
            NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation)
            | NotificationPostCommit::Analytics { observation, .. }
            | NotificationPostCommit::AnalyticsStream { observation, .. } => {
                observation.delivery.cursor
            }
        };
        if *self.fail_cursor.lock().expect("failure state") == Some(cursor) {
            return Err(
                crate::notification_dispatch::NotificationDispatchError::Dependency(
                    "test/publication-sink",
                ),
            );
        }
        let mut seen = self.seen.lock().expect("recording state");
        if !seen.contains(effect) {
            seen.push(effect.clone());
        }
        Ok(())
    }
}

#[test]
fn committed_receipt_is_delivered_then_removed() {
    let (root, store) = test_store();
    let expected = publication_effect(1);
    stage(&store, 1, 0, value(1, 0, expected.clone()));
    assert_eq!(pending_count(&store), 1);

    let sink = RecordingSink::default();
    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("drain"),
        1
    );
    assert_eq!(sink.seen.lock().expect("sink").as_slice(), &[expected]);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn restart_delivers_a_receipt_that_was_pending() {
    let (root, store) = test_store();
    let expected = publication_effect(2);
    {
        stage(&store, 1, 0, value(1, 0, expected.clone()));
    }
    drop(store);
    let store = RuntimeStore::open(Some(&root)).expect("reopen store");
    let sink = RecordingSink::default();
    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("drain after restart"),
        1
    );
    assert_eq!(sink.seen.lock().expect("sink").as_slice(), &[expected]);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn sequence_and_index_order_survive_a_sink_failure() {
    let (root, store) = test_store();
    for cursor in 1..=3 {
        stage(
            &store,
            cursor,
            0,
            value(cursor, 0, publication_effect(cursor)),
        );
    }
    let sink = RecordingSink::default();
    *sink.fail_cursor.lock().expect("failure state") = Some(2);
    assert!(store::drain_publications(&store, "default", &sink).is_err());
    let delivered = sink.seen.lock().expect("sink");
    assert_eq!(delivered.len(), 1);
    assert!(
        matches!(&delivered[0], NotificationPostCommit::TokenRateStream(value) if value.delivery.cursor == 1)
    );
    drop(delivered);
    assert_eq!(pending_count(&store), 2);

    *sink.fail_cursor.lock().expect("failure state") = None;
    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("retry"),
        2
    );
    let delivered = sink.seen.lock().expect("sink");
    let cursors = delivered
        .iter()
        .map(|effect| match effect {
            NotificationPostCommit::TokenRateStream(value) => value.delivery.cursor,
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    assert_eq!(cursors, [1, 2, 3]);
    assert_eq!(pending_count(&store), 0);
    drop(delivered);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn crash_before_delete_keeps_the_receipt_for_idempotent_replay() {
    let (root, store) = test_store();
    let sink = RecordingSink::default();
    {
        stage(&store, 1, 0, value(1, 0, publication_effect(4)));
        assert!(
            store::drain_publications_with_hook(&store, "default", &sink, || {
                Err(WorkerError::TaskFailed)
            })
            .is_err()
        );
        assert_eq!(pending_count(&store), 1);
        assert_eq!(sink.seen.lock().expect("sink").len(), 1);
    }
    drop(store);
    let store = RuntimeStore::open(Some(&root)).expect("reopen after delete failpoint");
    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("restart replay"),
        1
    );
    assert_eq!(sink.seen.lock().expect("sink").len(), 1);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn long_stream_keeps_the_publication_table_bounded() {
    let (root, store) = test_store();
    let sink = RecordingSink::default();
    let mut maximum = 0;
    for sequence in 1..=256 {
        stage(
            &store,
            sequence,
            0,
            value(sequence, 0, publication_effect(sequence)),
        );
        maximum = maximum.max(pending_count(&store));
        assert_eq!(
            store::drain_publications(&store, "default", &sink).expect("drain stream"),
            1
        );
    }
    assert_eq!(maximum, 1);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn failed_sink_backlog_stops_at_the_named_receipt_capacity() {
    let (root, store) = test_store();
    let first = NotificationPublicationBatch {
        sequence: 1,
        effects: (1..=store::MAX_PENDING_PUBLICATION_RECEIPTS as u64)
            .map(publication_effect)
            .collect(),
        analytics_overflow: 0,
    };
    store
        .transaction_with(|tx| store::persist_publication_batch(tx, "default", &first))
        .expect("fill bounded pending table");
    assert_eq!(
        pending_count(&store),
        store::MAX_PENDING_PUBLICATION_RECEIPTS as i64
    );
    let extra = NotificationPublicationBatch {
        sequence: 2,
        effects: vec![publication_effect(300)],
        analytics_overflow: 0,
    };
    assert!(matches!(
        store.transaction_with(|tx| store::persist_publication_batch(tx, "default", &extra)),
        Err(WorkerError::Capacity)
    ));
    assert_eq!(
        pending_count(&store),
        store::MAX_PENDING_PUBLICATION_RECEIPTS as i64
    );
    drop(store);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn drained_effect_reaches_token_rate_and_resource_hubs() {
    let (root, store) = test_store();
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let token_rates = Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
        Arc::clone(&changes),
    ));
    let mut observations = token_rates.subscribe_observations();
    let resources = Arc::new(crate::resource_notify::ResourceNotifications::new(
        changes.clone(),
    ));
    let sink = RuntimeNotificationPostCommitSink {
        token_rates,
        resources,
    };
    let expected = publication_effect(8);
    stage(&store, 1, 0, value(1, 0, expected.clone()));

    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("drain to hubs"),
        1
    );
    observations
        .changed()
        .await
        .expect("token-rate observation");
    assert_eq!(observations.borrow_and_update().as_ref(), Some(&expected));
    assert_eq!(changes.snapshot().revision, 2);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}
