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

fn agent_at_epoch(epoch: u64) -> Value {
    let mut agent = rate_agent();
    agent["epoch"] = json!(epoch);
    agent
}

fn analytics_effect(cursor: u64, method: &str, params: Value) -> NotificationPostCommit {
    analytics_effect_for(rate_agent(), cursor, method, params)
}

fn analytics_effect_for(
    agent: Value,
    cursor: u64,
    method: &str,
    params: Value,
) -> NotificationPostCommit {
    NotificationPostCommit::Analytics {
        observation: NotificationObservation {
            agent: Some(SharedJson::new(agent)),
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
        params: SharedJson::new(
            json!({"threadId":"thread","turnId":"turn","item":{"id":"item","type":"commandExecution"}}),
        ),
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

async fn seed_analytics_agent(store: &RuntimeStore) {
    let agent = rate_agent();
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                rusqlite::params![agent["id"].as_str(), agent.to_string()],
            )?;
            Ok::<(), WorkerError>(())
        })
        .await
        .expect("seed analytics agent identity");
}

async fn advance_agent_epoch(store: &RuntimeStore, epoch: u64) {
    let agent = agent_at_epoch(epoch);
    store
        .transaction_with(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                rusqlite::params![agent.to_string(), agent["id"].as_str()],
            )?;
            Ok::<(), WorkerError>(())
        })
        .await
        .expect("advance authoritative agent epoch");
}

fn production_sink() -> (
    Arc<crate::change_notifications::ChangeNotificationHub>,
    RuntimeNotificationPostCommitSink,
) {
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let sink = RuntimeNotificationPostCommitSink {
        token_rates: Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
            Arc::clone(&changes),
        )),
        resources: Arc::new(crate::resource_notify::ResourceNotifications::new(
            Arc::clone(&changes),
        )),
    };
    (changes, sink)
}

fn root() -> PathBuf {
    test_root().join(format!(
        "publication-drain-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

async fn test_store() -> (PathBuf, Arc<RuntimeStore>) {
    let root = root();
    seed_fixture_root(&root).await;
    let store = RuntimeStore::open(Some(&root)).await.expect("open store");
    (root, Arc::new(store))
}

async fn stage(store: &RuntimeStore, sequence: u64, index: usize, value: Value) {
    store
        .transaction_with(move |tx| {
            let operation = format!("notification-publication:default:{sequence}:{index}");
            store::save(tx, &operation, "pending", value)
        })
        .await
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
        Box::pin(async move {
            let cursor = match effect {
                NotificationPostCommit::TokenRateStream(observation)
                | NotificationPostCommit::TokenRateAssociation(observation)
                | NotificationPostCommit::TokenRateUsage(observation)
                | NotificationPostCommit::AnalyticsCaptured(observation)
                | NotificationPostCommit::AnalyticsStreamCaptured(observation)
                | NotificationPostCommit::Analytics { observation, .. }
                | NotificationPostCommit::AnalyticsStream { observation, .. } => {
                    Some(observation.delivery.cursor)
                }
                // Recovered effects have a stable identity but no provider cursor;
                // the cursor failure injector does not apply to that lane.
                NotificationPostCommit::Recovered { .. } => None,
            };
            if cursor.is_some_and(|cursor| {
                *self.fail_cursor.lock().expect("failure state") == Some(cursor)
            }) {
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
        })
    }
}

#[tokio::test]
async fn committed_receipt_is_delivered_then_removed() {
    let (root, store) = test_store().await;
    let expected = publication_effect(1);
    stage(&store, 1, 0, value(1, 0, expected.clone())).await;
    assert_eq!(pending_count(&store), 1);

    let sink = Arc::new(RecordingSink::default());
    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .expect("drain"),
        1
    );
    assert_eq!(sink.seen.lock().expect("sink").as_slice(), &[expected]);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn restart_delivers_a_receipt_that_was_pending() {
    let (root, store) = test_store().await;
    let expected = publication_effect(2);
    {
        stage(&store, 1, 0, value(1, 0, expected.clone())).await;
    }
    drop(store);
    let store = Arc::new(RuntimeStore::open(Some(&root)).await.expect("reopen store"));
    let sink = Arc::new(RecordingSink::default());
    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .expect("drain after restart"),
        1
    );
    assert_eq!(sink.seen.lock().expect("sink").as_slice(), &[expected]);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn sequence_and_index_order_survive_a_sink_failure() {
    let (root, store) = test_store().await;
    for cursor in 1..=3 {
        stage(
            &store,
            cursor,
            0,
            value(cursor, 0, publication_effect(cursor)),
        )
        .await;
    }
    let sink = Arc::new(RecordingSink::default());
    *sink.fail_cursor.lock().expect("failure state") = Some(2);
    assert!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .is_err()
    );
    {
        let delivered = sink.seen.lock().expect("sink");
        assert_eq!(delivered.len(), 1);
        assert!(
            matches!(&delivered[0], NotificationPostCommit::TokenRateStream(value) if value.delivery.cursor == 1)
        );
    }
    assert_eq!(pending_count(&store), 2);

    *sink.fail_cursor.lock().expect("failure state") = None;
    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .expect("retry"),
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

#[tokio::test]
async fn crash_before_delete_keeps_the_receipt_for_idempotent_replay() {
    let (root, store) = test_store().await;
    let sink = Arc::new(RecordingSink::default());
    {
        stage(&store, 1, 0, value(1, 0, publication_effect(4))).await;
        assert!(
            store::drain_publications_with_hook(
                Arc::clone(&store),
                "default".into(),
                sink.clone(),
                || { Err(WorkerError::TaskFailed) }
            )
            .await
            .is_err()
        );
        assert_eq!(pending_count(&store), 1);
        assert_eq!(sink.seen.lock().expect("sink").len(), 1);
    }
    drop(store);
    let store = Arc::new(
        RuntimeStore::open(Some(&root))
            .await
            .expect("reopen after delete failpoint"),
    );
    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .expect("restart replay"),
        1
    );
    assert_eq!(sink.seen.lock().expect("sink").len(), 1);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn long_stream_keeps_the_publication_table_bounded() {
    let (root, store) = test_store().await;
    let sink = Arc::new(RecordingSink::default());
    let mut maximum = 0;
    for sequence in 1..=256 {
        stage(
            &store,
            sequence,
            0,
            value(sequence, 0, publication_effect(sequence)),
        )
        .await;
        maximum = maximum.max(pending_count(&store));
        assert_eq!(
            store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
                .await
                .expect("drain stream"),
            1
        );
    }
    assert_eq!(maximum, 1);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn complete_analytics_usage_persists_beyond_the_publication_bound() {
    let (root, store) = test_store().await;
    seed_analytics_agent(&store).await;
    let backlog = NotificationPublicationBatch {
        sequence: 1,
        effects: (1..=store::MAX_PUBLICATION_RECEIPTS as u64)
            .map(publication_effect)
            .collect(),
        analytics_overflow: 0,
    };
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &backlog))
        .await
        .expect("fill token-rate receipts to the bound");
    let usage = NotificationPublicationBatch {
        sequence: 2,
        effects: vec![analytics_effect(
            300,
            "thread/tokenUsage/updated",
            json!({"turnId":"turn","tokenUsage":{
                "total":{"inputTokens":20,"outputTokens":100,"totalTokens":120},
                "last":{"inputTokens":2,"outputTokens":10,"totalTokens":12}
            }}),
        )],
        analytics_overflow: 0,
    };
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &usage))
        .await
        .expect("complete usage persists under token-rate pressure");
    assert_eq!(
        pending_count(&store),
        store::MAX_PUBLICATION_RECEIPTS as i64 + 1
    );
    let (durable_usage, shed): (i64, i64) = store
        .read()
        .expect("read durable usage")
        .query_row(
            "SELECT \
             count(CASE WHEN json_extract(result,'$.effect.AnalyticsCaptured.delivery.cursor')=300 THEN 1 END), \
             count(CASE WHEN json_extract(result,'$.analyticsOverflow')>0 THEN 1 END) \
             FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .expect("count durable usage and shed captures");
    assert_eq!((durable_usage, shed), (1, 0));
    let sink = Arc::new(RecordingSink::default());
    while store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
        .await
        .expect("drain publications")
        > 0
    {}
    assert_eq!(pending_count(&store), 0);
    assert!(
        sink.seen
            .lock()
            .expect("recording state")
            .iter()
            .any(|effect| matches!(
                effect,
                NotificationPostCommit::AnalyticsCaptured(observation)
                    if observation.delivery.cursor == 300
            ))
    );
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn durable_usage_survives_a_failed_drain_and_replays_once() {
    let (root, store) = test_store().await;
    seed_analytics_agent(&store).await;
    let usage = NotificationPublicationBatch {
        sequence: 1,
        effects: vec![analytics_effect(
            7,
            "thread/tokenUsage/updated",
            json!({"turnId":"turn","tokenUsage":{
                "total":{"inputTokens":20,"outputTokens":100,"totalTokens":120},
                "last":{"inputTokens":2,"outputTokens":10,"totalTokens":12}
            }}),
        )],
        analytics_overflow: 0,
    };
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &usage))
        .await
        .expect("persist durable usage");
    let sink = Arc::new(RecordingSink::default());
    *sink.fail_cursor.lock().expect("failure state") = Some(7);
    assert!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .is_err()
    );
    assert_eq!(pending_count(&store), 1);
    *sink.fail_cursor.lock().expect("failure state") = None;
    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .expect("replay durable usage"),
        1
    );
    assert_eq!(pending_count(&store), 0);
    let delivered = sink
        .seen
        .lock()
        .expect("recording state")
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                NotificationPostCommit::AnalyticsCaptured(observation)
                    if observation.delivery.cursor == 7
            )
        })
        .count();
    assert_eq!(delivered, 1);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn publication_overflow_receipt_records_only_newly_attached_counts() {
    let (root, store) = test_store().await;
    crate::analytics_capture::record_publication_overflow(&store, "receipt:overflow".into(), 1, 0)
        .await
        .expect("record initial overflow");
    crate::analytics_capture::record_publication_overflow(&store, "receipt:overflow".into(), 3, 0)
        .await
        .expect("record increased overflow count");
    crate::analytics_capture::record_publication_overflow(&store, "receipt:overflow".into(), 3, 0)
        .await
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
        effects: (1..=store::MAX_PUBLICATION_RECEIPTS as u64)
            .map(publication_effect)
            .collect(),
        analytics_overflow: 0,
    };
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    store::persist_publication_batch(tx, "default", &backlog)
                })
                .await
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
        .run(move |store| async move {
            let connection = store.read()?;
            Ok(connection.query_row(
                "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .expect("count bounded receipts");
    // Complete usage persists beyond the best-effort bound (at most the three
    // effects of this one frame) until the off-path drain removes receipts.
    assert!(pending <= store::MAX_PUBLICATION_RECEIPTS as i64 + 3);
    // The drain task delivers one bounded batch per pass and re-arms itself while
    // a full batch remains; later journal commits delete what it delivered.
    let backlog_prefix = format!(
        "account-worker:{}:%",
        store::publication_operation("default", 10_000, 0)
            .rsplit_once(':')
            .expect("sequence prefix")
            .0
    );
    let pending = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let mut usage = 11;
        loop {
            let prefix = backlog_prefix.clone();
            let pending = state
                .bridge
                .run(move |store| async move {
                    let connection = store.read()?;
                    Ok(connection.query_row(
                        "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE ?1",
                        [prefix],
                        |row| row.get::<_, i64>(0),
                    )?)
                })
                .await
                .expect("count receipts after drains");
            if pending == 0 {
                return pending;
            }
            usage += 1;
            let cursor = peer
                .frame(json!({"method":"thread/tokenUsage/updated","params":{
                    "threadId":"fake-thread-0","turnId":"turn","tokenUsage":{
                        "total":{"inputTokens":usage,"outputTokens":1,"totalTokens":usage + 1},
                        "last":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}}}))
                .await;
            peer.acknowledged_or_worker_exits(cursor, &mut worker).await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("backlog drained");
    assert_eq!(pending, 0);
    worker.shutdown().await.expect("stop worker");
}

#[tokio::test]
async fn publication_bound_keeps_state_invalidation_and_token_rate_usage() {
    let (root, store) = test_store().await;
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let client = changes.subscribe();
    let token_rates = Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
        Arc::clone(&changes),
    ));
    let resources = Arc::new(crate::resource_notify::ResourceNotifications::new(
        Arc::clone(&changes),
    ));
    let sink = Arc::new(RuntimeNotificationPostCommitSink {
        token_rates: Arc::clone(&token_rates),
        resources: Arc::clone(&resources),
    });
    seed_analytics_agent(&store).await;
    let backlog = NotificationPublicationBatch {
        sequence: 1,
        effects: std::iter::once(token_rate_started_effect(1))
            .chain((2..=store::MAX_PUBLICATION_RECEIPTS as u64).map(publication_effect))
            .collect(),
        analytics_overflow: 0,
    };
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &backlog))
        .await
        .expect("fill publication receipts to the bound");
    let pressured = NotificationPublicationBatch {
        sequence: 2,
        effects: vec![
            token_rate_usage_effect(300),
            token_rate_association_effect(300),
        ],
        // A stream capture the buffer shed raises the state invalidation below.
        analytics_overflow: 1,
    };
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &pressured))
        .await
        .expect("preserve token-rate observation under analytics pressure");
    assert_eq!(
        pending_count(&store),
        store::MAX_PUBLICATION_RECEIPTS as i64
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

    while store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
        .await
        .expect("drain publications")
        > 0
    {}

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
    token_rates.settled().await;
    let snapshot = token_rates.snapshot().to_value();
    assert_eq!(snapshot["rates"]["agent"]["turnId"], "turn");
    assert_eq!(snapshot["rates"]["agent"]["outputTokens"], 10.0);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn saturated_publication_coalescing_has_a_hard_effect_bound_and_never_stops_the_account() {
    let (root, store) = test_store().await;
    let backlog = NotificationPublicationBatch {
        sequence: 1,
        effects: std::iter::once(token_rate_started_effect(1))
            .chain((2..=store::MAX_PUBLICATION_RECEIPTS as u64).map(publication_effect))
            .collect(),
        analytics_overflow: 0,
    };
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &backlog))
        .await
        .expect("fill publication receipts to the bound");
    let bounded = NotificationPublicationBatch {
        sequence: 2,
        effects: (300..300 + store::MAX_COALESCED_TOKEN_RATE_EFFECTS as u64 + 1)
            .map(publication_effect)
            .collect(),
        analytics_overflow: 0,
    };
    // Live cutover 4: this refusal ended the account worker on every turn
    // ("account evidence capacity exhausted; recovery required") while the
    // publication head could not drain. Python never stops an account for its
    // token-rate observer; the extra observation is counted as overflow.
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &bounded))
        .await
        .expect("a saturated token-rate receipt counts overflow instead of failing");
    assert_eq!(
        pending_count(&store),
        store::MAX_PUBLICATION_RECEIPTS as i64
    );
    let (coalesced, overflow): (i64, i64) = store
        .read()
        .expect("read saturated receipt")
        .query_row(
            "SELECT COALESCE(max(json_array_length(result,'$.coalescedEffects')),0), \
             COALESCE(sum(json_extract(result,'$.publicationOverflow')),0) \
             FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("count coalesced payloads and overflow");
    assert_eq!(
        coalesced,
        store::MAX_COALESCED_TOKEN_RATE_EFFECTS as i64,
        "the coalesced receipt keeps its hard effect bound"
    );
    assert_eq!(overflow, 1, "the observation beyond the bound is counted");
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn drained_token_rate_effect_uses_its_stream_without_resetting_state() {
    let (root, store) = test_store().await;
    let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
    let token_rates = Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
        Arc::clone(&changes),
    ));
    let mut snapshots = token_rates.subscribe();
    let resources = Arc::new(crate::resource_notify::ResourceNotifications::new(
        changes.clone(),
    ));
    let sink = Arc::new(RuntimeNotificationPostCommitSink {
        token_rates,
        resources: Arc::clone(&resources),
    });
    seed_analytics_agent(&store).await;
    let started = analytics_effect(8, "turn/started", json!({"turnId":"turn"}));
    let delta = publication_effect(9);
    stage(&store, 1, 0, value(1, 0, started)).await;
    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .expect("drain analytics to hubs"),
        1
    );
    let state_resource = [json!({"kind":"state"})];
    assert!(
        resources
            .changes_since(&state_resource, Some(0))
            .expect("analytics state invalidation")
            .entity_reset
    );
    sink.token_rates.settled().await;
    snapshots.borrow_and_update();
    let before_delta = changes.snapshot().revision;

    stage(&store, 1, 1, value(1, 1, delta)).await;
    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), sink.clone())
            .await
            .expect("drain volatile token update"),
        1
    );
    sink.token_rates.settled().await;
    assert!(snapshots.has_changed().expect("token-rate hub stays open"));
    let value = snapshots.borrow_and_update();
    assert_eq!(value.to_value()["rates"]["agent"]["active"], true);
    assert!(
        value.to_value()["rates"]["agent"]["outputTokens"]
            .as_f64()
            .is_some_and(|tokens| tokens > 0.0)
    );
    assert!(changes.snapshot().revision > before_delta);
    let resource_changes = resources
        .changes_since(&state_resource, Some(before_delta))
        .expect("read unchanged durable resource cursor");
    assert!(resource_changes.resources.is_empty());
    assert!(!resource_changes.entity_reset);
    assert_eq!(pending_count(&store), 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

/// Persist one analytics effect through the journal's publication path.
async fn persist_one(store: &RuntimeStore, sequence: u64, effect: NotificationPostCommit) {
    let batch = NotificationPublicationBatch {
        sequence,
        effects: vec![effect],
        analytics_overflow: 0,
    };
    store
        .transaction_with(move |tx| store::persist_publication_batch(tx, "default", &batch))
        .await
        .expect("persist analytics effect");
}

fn projection_log_count(store: &RuntimeStore) -> i64 {
    store
        .read()
        .expect("read store")
        .query_row(
            "SELECT count(*) FROM runtime_analytics_projection",
            [],
            |row| row.get(0),
        )
        .expect("count projection log rows")
}

#[tokio::test]
async fn stale_analytics_observation_is_neither_logged_nor_published() {
    let (root, store) = test_store().await;
    seed_analytics_agent(&store).await;
    advance_agent_epoch(&store, 1).await;
    persist_one(
        &store,
        1,
        analytics_effect_for(
            agent_at_epoch(0),
            50,
            "thread/tokenUsage/updated",
            json!({"turnId":"turn","tokenUsage":{
                "total":{"inputTokens":20,"outputTokens":100,"totalTokens":120},
                "last":{"inputTokens":2,"outputTokens":10,"totalTokens":12}
            }}),
        ),
    )
    .await;
    assert_eq!(
        (projection_log_count(&store), pending_count(&store)),
        (0, 0)
    );

    persist_one(
        &store,
        2,
        analytics_effect_for(
            agent_at_epoch(1),
            51,
            "turn/started",
            json!({"turnId":"turn"}),
        ),
    )
    .await;
    assert_eq!(
        (projection_log_count(&store), pending_count(&store)),
        (1, 1)
    );
    let before = analytics_counts(&root);
    crate::analytics_projection::project_pending(&store)
        .await
        .expect("project the current observation");
    assert_eq!(analytics_counts(&root).1, before.1 + 1);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn deleted_agent_observation_does_not_recreate_identity() {
    let (root, store) = test_store().await;
    seed_analytics_agent(&store).await;
    store
        .transaction_with(move |tx| {
            tx.execute("DELETE FROM runtime_agents WHERE id='agent'", [])?;
            Ok::<(), WorkerError>(())
        })
        .await
        .expect("delete authoritative agent");
    persist_one(
        &store,
        1,
        analytics_effect(50, "turn/started", json!({"turnId":"turn"})),
    )
    .await;
    assert_eq!(
        (projection_log_count(&store), pending_count(&store)),
        (0, 0)
    );
    let agents: i64 = store
        .read()
        .expect("read store")
        .query_row(
            "SELECT count(*) FROM runtime_agents WHERE id='agent'",
            [],
            |row| row.get(0),
        )
        .expect("count authoritative agents");
    assert_eq!(agents, 0);
    std::fs::remove_dir_all(root).expect("remove test state");
}

#[tokio::test]
async fn legacy_uncaptured_analytics_receipts_drain_without_capture() {
    // Receipts written before the projection log still drain in order; the sink
    // publishes their token rate and state, never analytics rows.
    for first in [
        analytics_effect_for(
            agent_at_epoch(0),
            50,
            "turn/started",
            json!({"turnId":"turn"}),
        ),
        analytics_effect_for(
            agent_at_epoch(1),
            50,
            "item/started",
            json!(["not an object"]),
        ),
    ] {
        assert_drains_before_current_receipt(first).await;
    }
}

#[tokio::test]
async fn unmapped_token_rate_receipts_do_not_block_later_observations() {
    for mut effect in [
        publication_effect(50),
        token_rate_association_effect(50),
        token_rate_usage_effect(50),
    ] {
        match &mut effect {
            NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation)
            | NotificationPostCommit::TokenRateUsage(observation) => observation.agent = None,
            _ => panic!("the fixture must be a token-rate observation"),
        }
        assert_drains_before_current_receipt(effect).await;
    }
}

async fn assert_drains_before_current_receipt(first: NotificationPostCommit) {
    let (root, store) = test_store().await;
    seed_analytics_agent(&store).await;
    advance_agent_epoch(&store, 1).await;
    let (_changes, sink) = production_sink();
    stage(&store, 1, 0, value(1, 0, first)).await;
    stage(
        &store,
        2,
        0,
        value(
            2,
            0,
            analytics_effect_for(
                agent_at_epoch(1),
                51,
                "turn/started",
                json!({"turnId":"turn"}),
            ),
        ),
    )
    .await;
    let before = analytics_counts(&root);

    assert_eq!(
        store::drain_publications(Arc::clone(&store), "default".into(), Arc::new(sink))
            .await
            .expect("drain superseded then current receipt"),
        2
    );
    assert_eq!(pending_count(&store), 0);
    assert_eq!(analytics_counts(&root), before);
    drop(store);
    std::fs::remove_dir_all(root).expect("remove test state");
}

fn analytics_counts(root: &std::path::Path) -> (i64, i64) {
    let db = rusqlite::Connection::open(root.join("analytics.sqlite3")).expect("analytics DB");
    let notifications = db
        .query_row("SELECT count(*) FROM analytics_notifications", [], |row| {
            row.get(0)
        })
        .expect("notification rows");
    let turns = db
        .query_row("SELECT count(*) FROM analytics_turns", [], |row| row.get(0))
        .expect("turn rows");
    (notifications, turns)
}
