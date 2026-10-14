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
        agent: Some(rate_agent()),
        method: "item/agentMessage/delta".into(),
        params: json!({"threadId":"thread","turnId":"turn","delta":"sample"}),
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
            agent: Some(rate_agent()),
            method: method.into(),
            params,
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

fn start_rate_process(store: &RuntimeStore) -> String {
    let process_id = "publication-drain-test-process".to_owned();
    crate::analytics_capture::start_token_rate_process(store, &process_id)
        .expect("start token-rate delivery epoch");
    process_id
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
    stage(
        &store,
        3,
        0,
        json!({"account":"default","sequence":3,"index":0,"analyticsOverflow":overflow}),
    );
    let sink = RecordingSink::default();
    while store::drain_publications(&store, "default", &sink).expect("drain overflow diagnostic")
        > 0
    {}
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
            .map(|cursor| analytics_effect(cursor, "turn/started", json!({"turnId":"turn"})))
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
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
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
                .expect("count receipts while worker drains");
            if pending == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker drains bounded backlog");
    worker.shutdown().await.expect("stop worker");
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
        process_id: start_rate_process(&store),
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
        process_id: start_rate_process(&store),
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
            "SELECT count(*) FROM analytics_capture_receipts",
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
            agent: Some(agent),
            method: "item/agentMessage/delta".into(),
            params: combined,
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
        process_id: start_rate_process(&store),
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
    for rows in ["analytics_meta"] {
        let values = actual[rows].as_array_mut().expect("metadata rows");
        values.retain(|row| row["key"] != "trackingSince");
    }
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
