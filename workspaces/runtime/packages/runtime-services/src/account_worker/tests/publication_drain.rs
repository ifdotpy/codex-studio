use super::*;
use crate::notification_dispatch::{
    NotificationObservation, NotificationPostCommit, NotificationPublicationBatch,
    ProviderDeliveryIdentity, SharedJson,
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
        agent: Some(SharedJson::new(rate_agent())),
        method: "item/agentMessage/delta".into(),
        params: SharedJson::new(json!({"threadId":"thread","turnId":"turn","delta":"sample"})),
        delivery: ProviderDeliveryIdentity {
            provider_handle: "account:default".into(),
            generation: 7,
            cursor,
        },
        observed_at: 10.0 + cursor as f64,
        provider_at: 10.0 + cursor as f64,
    })
}

fn rate_agent() -> Value {
    json!({"id":"agent","name":"Agent","rootId":"root","accountKey":"default",
        "accountHistory":[],"provider":"codex","threadId":"thread","turnId":"turn",
        "model":"gpt-5","effort":"high","fastMode":false,"daybreakEnabled":false,
        "cyberAccessProgram":false,"cwd":"/disposable","deletedAt":null,"epoch":0,"status":"running"})
}

fn analytics_effect(cursor: u64, method: &str, params: Value) -> NotificationPostCommit {
    NotificationPostCommit::Analytics {
        observation: NotificationObservation {
            agent: Some(SharedJson::new(rate_agent())),
            method: method.into(),
            params: SharedJson::new(params),
            delivery: ProviderDeliveryIdentity {
                provider_handle: "account:default".into(),
                generation: 7,
                cursor,
            },
            observed_at: 10.0 + cursor as f64,
            provider_at: 10.0 + cursor as f64,
        },
        budget_capture_value: None,
    }
}

fn token_rate_usage_effect(cursor: u64) -> NotificationPostCommit {
    NotificationPostCommit::TokenRateUsage(NotificationObservation {
        agent: Some(SharedJson::new(rate_agent())),
        method: "thread/tokenUsage/updated".into(),
        params: SharedJson::new(json!({"turnId":"turn","tokenUsage":{
            "total":{"inputTokens":20,"outputTokens":100,"totalTokens":120},
            "last":{"inputTokens":2,"outputTokens":10,"totalTokens":12}
        }})),
        delivery: ProviderDeliveryIdentity {
            provider_handle: "account:default".into(),
            generation: 7,
            cursor,
        },
        observed_at: 10.0 + cursor as f64,
        provider_at: 10.0 + cursor as f64,
    })
}

fn token_rate_association_effect(cursor: u64) -> NotificationPostCommit {
    NotificationPostCommit::TokenRateAssociation(NotificationObservation {
        agent: Some(SharedJson::new(rate_agent())),
        method: "item/started".into(),
        params: SharedJson::new(json!({"threadId":"thread","turnId":"turn","item":{"id":"item","type":"commandExecution"}})),
        delivery: ProviderDeliveryIdentity {
            provider_handle: "account:default".into(),
            generation: 7,
            cursor,
        },
        observed_at: 10.0 + cursor as f64,
        provider_at: 10.0 + cursor as f64,
    })
}

fn token_rate_started_effect(cursor: u64) -> NotificationPostCommit {
    NotificationPostCommit::TokenRateUsage(NotificationObservation {
        agent: Some(SharedJson::new(rate_agent())),
        method: "turn/started".into(),
        params: SharedJson::new(json!({"turnId":"turn"})),
        delivery: ProviderDeliveryIdentity {
            provider_handle: "account:default".into(),
            generation: 7,
            cursor,
        },
        observed_at: 10.0 + cursor as f64,
        provider_at: 10.0 + cursor as f64,
    })
}

fn seed_analytics_agent(store: &RuntimeStore) {
    let agent = rate_agent();
    store
        .transaction_with(|tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                rusqlite::params![agent["id"].as_str(), agent.to_string()],
            )?;
            Ok::<(), WorkerError>(())
        })
        .expect("seed analytics agent identity");
}

fn root() -> PathBuf {
    test_root().join(format!(
        "publication-drain-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

fn test_store() -> (PathBuf, Arc<RuntimeStore>) {
    let (source, _) = TEMPLATE.get_or_init(template).clone();
    let root = root();
    copy_template(&source, &root);
    let store = RuntimeStore::open(Some(&root)).expect("open store");
    (root, Arc::new(store))
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
            | NotificationPostCommit::TokenRateUsage(observation)
            | NotificationPostCommit::Analytics { observation, .. }
            | NotificationPostCommit::AnalyticsStream { observation, .. } => {
                Some(observation.delivery.cursor)
            }
            // Recovered effects have a stable identity but no provider cursor;
            // the cursor failure injector does not apply to that lane.
            NotificationPostCommit::Recovered { .. } => None,
        };
        if cursor
            .is_some_and(|cursor| *self.fail_cursor.lock().expect("failure state") == Some(cursor))
        {
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
fn analytics_backlog_at_receipt_bound_drops_capture_without_failing_write() {
    let (root, store) = test_store();
    let first = NotificationPublicationBatch {
        sequence: 1,
        effects: (1..=store::MAX_PENDING_PUBLICATION_RECEIPTS as u64)
            .map(|cursor| analytics_effect(cursor, "turn/started", json!({"turnId":"turn"})))
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
        effects: vec![analytics_effect(
            300,
            "turn/started",
            json!({"turnId":"turn"}),
        )],
        analytics_overflow: 0,
    };
    store
        .transaction_with(|tx| store::persist_publication_batch(tx, "default", &extra))
        .expect("publication pressure must not fail the provider write");
    assert_eq!(
        pending_count(&store),
        store::MAX_PENDING_PUBLICATION_RECEIPTS as i64
    );
    let overflow: i64 = store
        .read()
        .expect("read overflow").query_row(
            "SELECT json_extract(result,'$.analyticsOverflow') FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:1:%' ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("record dropped analytics capture");
    assert_eq!(overflow, 1);
    let sink = RecordingSink::default();
    let mut before_delete_calls = 0;
    for _ in 0..(store::MAX_PENDING_PUBLICATION_RECEIPTS / store::PUBLICATION_DRAIN_LIMIT) - 1 {
        assert_eq!(
            store::drain_publications(&store, "default", &sink).expect("drain bounded batch"),
            store::PUBLICATION_DRAIN_LIMIT
        );
        before_delete_calls += store::PUBLICATION_DRAIN_LIMIT;
    }
    assert!(
        store::drain_publications_with_hook(&store, "default", &sink, || {
            before_delete_calls += 1;
            if before_delete_calls == store::MAX_PENDING_PUBLICATION_RECEIPTS {
                Err(WorkerError::TaskFailed)
            } else {
                Ok(())
            }
        })
        .is_err()
    );
    assert_eq!(pending_count(&store), 1);
    let analytics =
        rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("open analytics store");
    let errors: String = analytics
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='captureErrors'",
            [],
            |row| row.get(0),
        )
        .expect("record overflow in analytics coverage");
    assert_eq!(
        serde_json::from_str::<Value>(&errors).expect("decode errors")["count"],
        1
    );
    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("retry overflow receipt"),
        1
    );
    let repeated: String = analytics
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='captureErrors'",
            [],
            |row| row.get(0),
        )
        .expect("overflow metadata remains available");
    assert_eq!(
        serde_json::from_str::<Value>(&repeated).expect("decode repeated errors")["count"],
        1
    );
    drop(store);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn publication_overflow_receipt_records_only_newly_attached_counts() {
    let (root, store) = test_store();
    crate::analytics_capture::record_publication_overflow(&store, "receipt:overflow", 1, 0)
        .expect("record initial overflow");
    crate::analytics_capture::record_publication_overflow(&store, "receipt:overflow", 3, 0)
        .expect("record increased overflow count");
    crate::analytics_capture::record_publication_overflow(&store, "receipt:overflow", 3, 0)
        .expect("replay overflow count");
    let analytics =
        rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("open analytics DB");
    let errors: String = analytics
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='captureErrors'",
            [],
            |row| row.get(0),
        )
        .expect("read coverage error count");
    assert_eq!(
        serde_json::from_str::<Value>(&errors).expect("decode coverage errors")["count"],
        3
    );
    drop(store);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn provider_notification_is_acknowledged_at_the_publication_bound() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.start(&mut peer, &worker).await;

    let backlog = NotificationPublicationBatch {
        sequence: 10_000,
        effects: (1..=store::MAX_PENDING_PUBLICATION_RECEIPTS as u64)
            .map(publication_effect)
            .collect(),
        analytics_overflow: 0,
    };
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| store::persist_publication_batch(tx, "default", &backlog))
        })
        .await
        .expect("fill publication backlog");

    let cursor = peer
        .frame(json!({
            "method":"thread/tokenUsage/updated",
            "params":{
                "threadId":"fake-thread-0",
                "turnId":"turn",
                "tokenUsage":{
                    "total":{"inputTokens":10,"outputTokens":1,"totalTokens":11},
                    "last":{"inputTokens":10,"outputTokens":1,"totalTokens":11}
                }
            }
        }))
        .await;
    peer.acknowledged_or_worker_exits(cursor, &mut worker).await;

    let pending = state
        .bridge
        .run(|store| {
            let connection = store.read()?;
            Ok(connection.query_row(
                "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .expect("count bounded receipts");
    assert!(pending <= store::MAX_PENDING_PUBLICATION_RECEIPTS as i64);
    // Every acknowledged journal turn drains one bounded batch. These no-op
    // provider frames are gates for the three remaining drain passes.
    for index in 0..3 {
        let cursor = peer
            .frame(json!({
                "method":"hook/started",
                "params":{"threadId":"fake-thread-0","hookId":format!("drain-{index}")}
            }))
            .await;
        peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
    }
    let pending = state
        .bridge
        .run(|store| {
            let connection = store.read()?;
            Ok(connection.query_row(
                "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .expect("count receipts after gated drains");
    assert_eq!(pending, 0);
    worker.shutdown().await.expect("stop worker");
}

#[test]
fn publication_bound_keeps_state_invalidation_and_token_rate_usage() {
    let (root, store) = test_store();
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let client = changes.subscribe();
    let token_rates = Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
        Arc::clone(&changes),
    ));
    let resources = Arc::new(crate::resource_notify::ResourceNotifications::new(
        Arc::clone(&changes),
    ));
    let sink = RuntimeNotificationPostCommitSink {
        token_rates: Arc::clone(&token_rates),
        resources: Arc::clone(&resources),
        analytics_store: Arc::clone(&store),
    };
    seed_analytics_agent(&store);
    let backlog = NotificationPublicationBatch {
        sequence: 1,
        effects: std::iter::once(token_rate_started_effect(1))
            .chain((2..=store::MAX_PENDING_PUBLICATION_RECEIPTS as u64).map(publication_effect))
            .collect(),
        analytics_overflow: 0,
    };
    store
        .transaction_with(|tx| store::persist_publication_batch(tx, "default", &backlog))
        .expect("fill publication receipts to the bound");
    let pressured = NotificationPublicationBatch {
        sequence: 2,
        effects: vec![
            analytics_effect(
                300,
                "thread/tokenUsage/updated",
                json!({"turnId":"turn","tokenUsage":{
                    "total":{"inputTokens":20,"outputTokens":100,"totalTokens":120},
                    "last":{"inputTokens":2,"outputTokens":10,"totalTokens":12}
                }}),
            ),
            token_rate_usage_effect(300),
            token_rate_association_effect(300),
        ],
        analytics_overflow: 0,
    };
    store
        .transaction_with(|tx| store::persist_publication_batch(tx, "default", &pressured))
        .expect("preserve token-rate observation under analytics pressure");
    assert_eq!(
        pending_count(&store),
        store::MAX_PENDING_PUBLICATION_RECEIPTS as i64
    );
    let coalesced_effects: String = store
        .read()
        .expect("read coalesced effects")
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%' AND json_extract(result,'$.tokenRateCoalesced')=1 ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("saturated receipt retains token-rate effects");
    let coalesced: Value = serde_json::from_str(&coalesced_effects).expect("coalesced JSON");
    let effects = std::iter::once(&coalesced["effect"])
        .chain(coalesced["coalescedEffects"].as_array().unwrap().iter())
        .collect::<Vec<_>>();
    assert!(
        effects
            .iter()
            .any(|effect| effect["TokenRateUsage"].is_object())
    );
    assert!(
        effects
            .iter()
            .any(|effect| effect["TokenRateAssociation"].is_object())
    );
    assert_eq!(effects[0]["TokenRateStream"]["delivery"]["cursor"], 256);
    assert_eq!(effects[1]["TokenRateUsage"]["delivery"]["cursor"], 300);
    assert_eq!(
        effects[2]["TokenRateAssociation"]["delivery"]["cursor"],
        300
    );
    let invalidations: i64 = store
        .read()
        .expect("read publication marker")
        .query_row(
            "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%' AND json_extract(result,'$.stateInvalidation')=1",
            [],
            |row| row.get(0),
        )
        .expect("count coalesced invalidation markers");
    assert_eq!(invalidations, 1);

    while store::drain_publications(&store, "default", &sink).expect("drain publications") > 0 {}

    assert!(
        client
            .has_changed()
            .expect("connected client remains subscribed")
    );
    let changes_since = resources
        .changes_since(&[json!({"kind":"state"})], Some(0))
        .expect("read state invalidation");
    assert!(changes_since.entity_reset);
    assert_eq!(changes_since.resources, vec![json!({"kind":"state"})]);
    let snapshot = token_rates.snapshot().to_value();
    assert_eq!(snapshot["rates"]["agent"]["turnId"], "turn");
    assert_eq!(snapshot["rates"]["agent"]["outputTokens"], 10.0);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn saturated_publication_coalescing_has_a_hard_effect_bound() {
    let (root, store) = test_store();
    let backlog = NotificationPublicationBatch {
        sequence: 1,
        effects: std::iter::once(token_rate_started_effect(1))
            .chain((2..=store::MAX_PENDING_PUBLICATION_RECEIPTS as u64).map(publication_effect))
            .collect(),
        analytics_overflow: 0,
    };
    store
        .transaction_with(|tx| store::persist_publication_batch(tx, "default", &backlog))
        .expect("fill publication receipts to the bound");
    let bounded = NotificationPublicationBatch {
        sequence: 2,
        effects: (300..300 + store::MAX_COALESCED_TOKEN_RATE_EFFECTS as u64 + 1)
            .map(publication_effect)
            .collect(),
        analytics_overflow: 0,
    };
    assert!(
        store
            .transaction_with(|tx| store::persist_publication_batch(tx, "default", &bounded))
            .is_err()
    );
    assert_eq!(
        pending_count(&store),
        store::MAX_PENDING_PUBLICATION_RECEIPTS as i64
    );
    let coalesced: i64 = store
        .read()
        .expect("read saturated receipt")
        .query_row(
            "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%' AND json_type(result,'$.coalescedEffects') IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count committed coalesced payloads");
    assert_eq!(
        coalesced, 0,
        "failed batch must roll back all coalesced payloads"
    );
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn drained_effect_reaches_token_rate_and_resource_hubs() {
    let (root, store) = test_store();
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let token_rates = Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
        Arc::clone(&changes),
    ));
    let mut snapshots = token_rates.subscribe();
    let resources = Arc::new(crate::resource_notify::ResourceNotifications::new(
        changes.clone(),
    ));
    let sink = RuntimeNotificationPostCommitSink {
        token_rates,
        resources,
        analytics_store: Arc::clone(&store),
    };
    seed_analytics_agent(&store);
    let started = analytics_effect(8, "turn/started", json!({"turnId":"turn"}));
    let delta = publication_effect(9);
    stage(&store, 1, 0, value(1, 0, started));
    stage(&store, 1, 1, value(1, 1, delta));

    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("drain to hubs"),
        2
    );
    snapshots.changed().await.expect("token-rate snapshot");
    let value = snapshots.borrow_and_update();
    assert_eq!(value.to_value()["rates"]["agent"]["active"], true);
    assert!(
        value.to_value()["rates"]["agent"]["outputTokens"]
            .as_f64()
            .is_some_and(|tokens| tokens > 0.0)
    );
    assert!(changes.snapshot().revision >= 3);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn production_sink_replay_keeps_analytics_rows_unchanged() {
    let (root, store) = test_store();
    seed_analytics_agent(&store);
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let token_rates = Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
        Arc::clone(&changes),
    ));
    let resources = Arc::new(crate::resource_notify::ResourceNotifications::new(
        Arc::clone(&changes),
    ));
    let sink = RuntimeNotificationPostCommitSink {
        token_rates,
        resources,
        analytics_store: Arc::clone(&store),
    };
    let effect = analytics_effect(50, "turn/started", json!({"turnId":"turn"}));
    stage(&store, 1, 0, value(1, 0, effect));

    assert!(
        store::drain_publications_with_hook(&store, "default", &sink, || {
            Err(WorkerError::TaskFailed)
        })
        .is_err()
    );
    let before = analytics_counts(&root);
    let revision = changes.snapshot().revision;
    let snapshot = sink.token_rates.snapshot();
    assert_eq!(pending_count(&store), 1);

    assert_eq!(
        store::drain_publications(&store, "default", &sink).expect("replay same receipt"),
        1
    );
    assert_eq!(analytics_counts(&root), before);
    assert_eq!(changes.snapshot().revision, revision);
    assert_eq!(sink.token_rates.snapshot(), snapshot);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn analytics_receipt_upgrade_preserves_legacy_delivery_watermark() {
    let (root, store) = test_store();
    seed_analytics_agent(&store);
    let agent = rate_agent();
    let prepared = crate::analytics_capture::PreparedCapture::event(
        &agent,
        "turn/started",
        &json!({"turnId":"turn"}),
        10.0,
        10.0,
        crate::analytics_capture::CaptureSource::Live,
    )
    .expect("prepare legacy capture replay");
    let analytics =
        rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("open analytics DB");
    analytics
        .execute_batch(
            "CREATE TABLE analytics_capture_receipts (provider TEXT PRIMARY KEY,generation INTEGER NOT NULL,cursor INTEGER NOT NULL,effects INTEGER NOT NULL);\
             INSERT INTO analytics_capture_receipts VALUES ('account:default',7,50,1);",
        )
        .expect("seed the prior receipt schema");

    assert!(
        !crate::analytics_capture::capture_notification_effect(
            &store,
            "account:default",
            7,
            50,
            "analytics",
            &prepared,
            None,
        )
        .expect("deduplicate a legacy receipt")
    );
    assert!(
        crate::analytics_capture::capture_notification_effect(
            &store,
            "account:default",
            7,
            51,
            "analytics",
            &prepared,
            None,
        )
        .expect("capture after the legacy watermark")
    );

    let analytics =
        rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("reopen analytics DB");
    let migrated: (i64, i64, i64) = analytics
        .query_row(
            "SELECT generation,cursor,effects FROM analytics_capture_legacy_watermarks WHERE provider='account:default'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read migrated watermark");
    assert_eq!(migrated, (7, 50, 1));
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[test]
fn delayed_analytics_capture_is_not_rejected_by_a_later_cursor() {
    let (root, store) = test_store();
    let agent = rate_agent();
    seed_analytics_agent(&store);
    let prepared = crate::analytics_capture::PreparedCapture::event(
        &agent,
        "turn/started",
        &json!({"turnId":"turn"}),
        10.0,
        10.0,
        crate::analytics_capture::CaptureSource::Live,
    )
    .expect("prepare turn capture");
    let deliver = |cursor| {
        crate::analytics_capture::capture_notification_effect(
            &store,
            "account:default",
            7,
            cursor,
            "analytics",
            &prepared,
            None,
        )
        .expect("capture analytics effect")
    };

    assert!(deliver(4));
    assert!(deliver(3), "delayed first delivery remains capturable");
    assert!(!deliver(3), "only an exact replay is deduplicated");
    let analytics =
        rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("open analytics DB");
    let receipts: i64 = analytics
        .query_row(
            "SELECT count(*) FROM analytics_capture_effect_receipts WHERE provider='account:default' AND generation=7 AND effect='analytics'",
            [],
            |row| row.get(0),
        )
        .expect("count exact delivery receipts");
    assert_eq!(receipts, 2);
    std::fs::remove_dir_all(root).expect("remove test state");
}

fn analytics_counts(root: &std::path::Path) -> (i64, i64, i64) {
    let db = rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("analytics DB");
    let notifications = db
        .query_row("SELECT count(*) FROM analytics_notifications", [], |row| {
            row.get(0)
        })
        .expect("notification rows");
    let turns = db
        .query_row("SELECT count(*) FROM analytics_turns", [], |row| row.get(0))
        .expect("turn rows");
    let captures = db
        .query_row(
            "SELECT count(*) FROM analytics_capture_effect_receipts",
            [],
            |row| row.get(0),
        )
        .expect("capture idempotency rows");
    (notifications, turns, captures)
}

#[test]
fn streamed_turn_analytics_rows_match_python_capture_oracle() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = crate::agents_create::prepared_python(&source);
    let oracle =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/analytics_capture/stream_oracle.py");
    let output = Command::new(python)
        .arg(oracle)
        .output()
        .expect("run Python stream analytics oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");

    let (root, store) = test_store();
    seed_analytics_agent(&store);
    let agent = rate_agent();
    for (method, params, at) in [
        ("turn/started", json!({"turnId":"turn"}), 10.0),
        (
            "item/started",
            json!({"threadId":"thread","turnId":"turn","item":{"id":"item","type":"agentMessage"}}),
            11.0,
        ),
    ] {
        let prepared = crate::analytics_capture::PreparedCapture::event(
            &agent,
            method,
            &params,
            at,
            12.0,
            crate::analytics_capture::CaptureSource::Live,
        )
        .expect("prepare pre-stream capture");
        crate::analytics_capture::capture(&store, &prepared).expect("capture pre-stream rows");
    }
    let first_params = json!({"threadId":"thread","turnId":"turn","itemId":"item","delta":"hello"});
    let second_params =
        json!({"threadId":"thread","turnId":"turn","itemId":"item","delta":" world\n"});
    let mut combined = first_params.clone();
    combined["delta"] = json!("hello world\n");
    let encoded_bytes = studio_runtime_store::python_json_compact(&first_params).len() as u64
        + studio_runtime_store::python_json_compact(&second_params).len() as u64;
    let stream = crate::notification_dispatch::NotificationStreamMetrics {
        hours: vec![crate::notification_dispatch::NotificationHourMetrics {
            hour: 0,
            count: 2,
            encoded_bytes,
        }],
        bytes: 12,
        characters: 12,
        lines: 1,
    };
    let effect = NotificationPostCommit::AnalyticsStream {
        observation: NotificationObservation {
            agent: Some(SharedJson::new(agent)),
            method: "item/agentMessage/delta".into(),
            params: SharedJson::new(combined),
            delivery: ProviderDeliveryIdentity {
                provider_handle: "account:default".into(),
                generation: 7,
                cursor: 2,
            },
            observed_at: 12.0,
            provider_at: 12.0,
        },
        metrics: stream,
    };
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let sink = RuntimeNotificationPostCommitSink {
        token_rates: Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
            Arc::clone(&changes),
        )),
        resources: Arc::new(crate::resource_notify::ResourceNotifications::new(changes)),
        analytics_store: Arc::clone(&store),
    };
    sink.publish(&effect).expect("publish streamed analytics");

    let actual_db =
        rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("open analytics output");
    let mut actual = json!({});
    for table in [
        "analytics_agents",
        "analytics_notifications",
        "analytics_turns",
        "analytics_items",
        "analytics_usage",
        "analytics_usage_roots",
        "analytics_meta",
        "analytics_limits",
    ] {
        let mut statement = actual_db
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .expect("prepare table read");
        let columns = statement
            .column_names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        let rows = statement
            .query_map([], |row| {
                let mut value = json!({});
                for (index, column) in columns.iter().enumerate() {
                    let cell = row.get_ref(index)?;
                    value[column] = match cell {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(value) => json!(value),
                        rusqlite::types::ValueRef::Real(value) => json!(value),
                        rusqlite::types::ValueRef::Text(value) => {
                            let text = String::from_utf8_lossy(value).into_owned();
                            serde_json::from_str(&text).unwrap_or_else(|_| json!(text))
                        }
                        rusqlite::types::ValueRef::Blob(_) => panic!("unexpected blob"),
                    };
                }
                Ok(value)
            })
            .expect("query rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode rows");
        actual[table] = json!(rows);
    }
    // Tracking-since is store initialization metadata, independent of captures.
    let values = actual["analytics_meta"]
        .as_array_mut()
        .expect("metadata rows");
    values.retain(|row| row["key"] != "trackingSince");
    let mut expected = expected;
    let values = expected["analytics_meta"]
        .as_array_mut()
        .expect("oracle metadata");
    values.retain(|row| row["key"] != "trackingSince");
    for table in [
        "analytics_agents",
        "analytics_turns",
        "analytics_items",
        "analytics_usage",
        "analytics_limits",
    ] {
        for row in expected[table].as_array_mut().expect("oracle table") {
            if let Some(raw) = row["record"].as_str() {
                row["record"] = serde_json::from_str(raw).expect("oracle record JSON");
            }
        }
    }
    for row in expected["analytics_notifications"]
        .as_array_mut()
        .expect("oracle notifications")
    {
        if let Some(raw) = row["id"].as_str() {
            row["id"] = serde_json::from_str(raw).expect("notification identity JSON");
        }
    }
    for row in expected["analytics_meta"]
        .as_array_mut()
        .expect("oracle metadata")
    {
        if let Some(raw) = row["value"].as_str()
            && let Ok(value) = serde_json::from_str(raw)
        {
            row["value"] = value;
        }
    }
    assert_eq!(
        actual, expected,
        "stream analytics rows diverged from Python"
    );
    std::fs::remove_dir_all(root).expect("remove test state");
}
