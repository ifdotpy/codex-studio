#[path = "support/prepared_python.rs"]
mod prepared_python;
use serde_json::{Value, json};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, OnceLock, atomic::AtomicBool};
use studio_runtime_services::analytics_history::{
    self as history, BatchLimits, HistoryBudgetSink, HistoryError, HistorySnapshot, RolloutPath,
};
use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository as Repo, CaptureTable};
use studio_runtime_store::{AnalyticsRepository, RuntimeStore, StoreError, TranscriptRepository};

struct Budget;
impl HistoryBudgetSink for Budget {
    fn capture_page<'a>(
        &'a self,
        _: Vec<(Arc<Value>, Value, f64)>,
        _: f64,
    ) -> Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}
// Allow the fixed mutation runner to keep SQLite fixtures on caller-owned tmpfs.
fn fixture_directory() -> tempfile::TempDir {
    std::env::var_os("STUDIO_ANALYTICS_TEST_TMPDIR")
        .map(tempfile::tempdir_in)
        .unwrap_or_else(tempfile::tempdir)
        .expect("disposable fixture directory")
}
fn fixture() -> (tempfile::TempDir, Value) {
    static TEMPLATE: OnceLock<(tempfile::TempDir, Value)> = OnceLock::new();
    let (root, agent) = TEMPLATE.get_or_init(|| {
        let root = fixture_directory();
        let out = Command::new(prepared_python::prepared_python(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        ))
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/analytics_capture/state_fixture.py"),
        )
        .arg(root.path())
        .output()
        .expect("Python writer");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (root, serde_json::from_slice(&out.stdout).expect("agent"))
    });
    let copy = fixture_directory();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        fs::copy(root.path().join(name), copy.path().join(name)).expect("clone template");
    }
    (copy, agent.clone())
}
fn file(root: &Path, lines: &[Value], suffix: &[u8]) -> PathBuf {
    let path = root.join("rollout-fixture-thread.jsonl");
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend(serde_json::to_vec(line).expect("JSON"));
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(suffix);
    fs::write(&path, bytes).expect("synthetic history");
    path
}
fn records() -> Vec<Value> {
    vec![
        json!({"type":"session_meta","timestamp":1,"payload":{"id":"thread"}}),
        json!({"type":"event_msg","timestamp":2,"payload":{"type":"task_started","turn_id":"turn"}}),
        json!({"type":"event_msg","timestamp":3,"payload":{"type":"task_complete","turn_id":"turn","status":"completed"}}),
    ]
}
fn checkpoint(store: &RuntimeStore) -> Option<Value> {
    let db = AnalyticsRepository::new(store.root())
        .open_read_only()
        .expect("read");
    Repo::read(&db, CaptureTable::History, "a:default:thread").expect("checkpoint")
}
#[tokio::test]
async fn restart_after_ack_loss_retains_exact_effects_and_cursor() {
    let (root, agent) = fixture();
    let path = file(root.path(), &records(), b"");
    let cancel = AtomicBool::new(false);
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let first = history::prepare(
        HistorySnapshot::from_records(agent.clone(), None).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path.clone()),
        BatchLimits::default(),
        200.,
        &cancel,
    )
    .expect("batch");
    let stale = history::prepare(
        HistorySnapshot::from_records(agent.clone(), None).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path.clone()),
        BatchLimits::default(),
        200.,
        &cancel,
    )
    .expect("competing batch");
    first
        .commit(&store, &Budget, 200.)
        .await
        .expect("commit before response lost");
    let committed = checkpoint(&store).expect("checkpoint");
    assert_eq!(
        committed["offset"],
        fs::metadata(&path).expect("stat").len()
    );
    assert!(matches!(
        stale.commit(&store, &Budget, 201.).await,
        Err(HistoryError::StaleCheckpoint)
    ));
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    let batch = history::prepare(
        HistorySnapshot::from_records(agent, checkpoint(&store)).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path),
        BatchLimits::default(),
        202.,
        &cancel,
    )
    .expect("resume");
    assert!(!batch.advanced());
    batch
        .commit(&store, &Budget, 202.)
        .await
        .expect("idle commit");
    assert_eq!(checkpoint(&store), Some(committed));
    let db = AnalyticsRepository::new(store.root())
        .open_read_only()
        .expect("reader");
    let turn = Repo::read(&db, CaptureTable::Turns, "a:thread:turn")
        .expect("turn")
        .expect("row");
    assert_eq!(turn["status"], "completed");
    assert_eq!(turn["durationMs"], 1000.);
}

#[tokio::test]
async fn committed_read_snapshot_finishes_while_store_writer_is_held() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (root, _) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let (writer_started, started) = mpsc::channel();
    let (release_writer, release) = mpsc::channel();
    let (reader_done, read_done) = mpsc::channel();
    let writer_store = store.clone();
    let writer = tokio::spawn(async move {
        writer_store
            .transaction(move |tx| {
                tx.execute("UPDATE runtime_agents SET record=record WHERE id='a'", [])?;
                writer_started.send(()).expect("writer gate");
                release.recv().expect("writer release");
                Ok(())
            })
            .await
            .expect("writer commit");
    });
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("writer holds transaction");
    let reader_store = store.clone();
    tokio::spawn(async move {
        reader_done
            .send(reader_store.read_transaction(|tx| {
                Ok(
                    tx.query_row("SELECT id FROM runtime_agents WHERE id='a'", [], |r| {
                        r.get::<_, String>(0)
                    })?,
                )
            }))
            .expect("reader result receiver");
    });
    let result = read_done.recv_timeout(Duration::from_secs(5));
    release_writer.send(()).expect("release writer");
    writer.await.expect("writer task");
    assert_eq!(
        result
            .expect("reader completed before writer release")
            .expect("read"),
        "a"
    );
}

#[tokio::test]
async fn unchanged_transcript_projection_performs_no_sql_writes() {
    let (root, _) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let payload = json!({
        "agent":{"id":"a"},
        "items":[{"id":"item-1","role":"assistant","text":"stable"}]
    });
    let initial_payload = payload.clone();
    let initial = store
        .transaction(move |tx| {
            let before = tx.total_changes();
            TranscriptRepository::pull_payload(
                tx,
                "transcript:a",
                0,
                &initial_payload,
                false,
                Some(1),
            )
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            Ok(tx.total_changes() - before)
        })
        .await;
    let duplicate_payload = payload;
    let duplicate = store
        .transaction(move |tx| {
            let before = tx.total_changes();
            TranscriptRepository::pull_payload(
                tx,
                "transcript:a",
                0,
                &duplicate_payload,
                false,
                Some(1),
            )
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
            Ok(tx.total_changes() - before)
        })
        .await;
    assert!(initial.expect("initial projection") > 0);
    assert_eq!(duplicate.expect("same projection"), 0);
}

#[tokio::test]
async fn main_store_write_runs_between_separately_scheduled_history_batches() {
    let (root, agent) = fixture();
    let path = file(root.path(), &records(), b"");
    let cancel = AtomicBool::new(false);
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let first = history::prepare(
        HistorySnapshot::from_records(agent.clone(), checkpoint(&store)).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path.clone()),
        BatchLimits::new(1024 * 1024, 1).expect("batch limits"),
        10.0,
        &cancel,
    )
    .expect("first batch prepares");

    let (schedule_write, write_scheduled) = tokio::sync::oneshot::channel();
    let (write_committed, committed) = tokio::sync::oneshot::channel();
    let agent_id = agent["id"].as_str().expect("agent id").to_owned();
    let writer_store = store.clone();
    let task = tokio::spawn(async move {
        write_scheduled.await.expect("queued write is scheduled");
        writer_store
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=record WHERE id=?",
                    [&agent_id],
                )?;
                Ok(())
            })
            .await
            .expect("queued main-store write commits");
        write_committed
            .send(())
            .expect("commit receipt is received");
    });

    first
        .commit(&store, &Budget, 10.0)
        .await
        .expect("first batch commits");
    schedule_write.send(()).expect("main write is queued");
    committed
        .await
        .expect("main write commits before next batch");

    let second = history::prepare(
        HistorySnapshot::from_records(agent, checkpoint(&store)).expect("next snapshot"),
        root.path(),
        RolloutPath::Found(path),
        BatchLimits::new(1024 * 1024, 1).expect("batch limits"),
        11.0,
        &cancel,
    )
    .expect("second batch prepares");
    second
        .commit(&store, &Budget, 11.0)
        .await
        .expect("second batch commits");
    task.await.expect("queued writer exits");

    let stored = checkpoint(&store).expect("history checkpoint persists");
    assert_eq!(stored["status"], "catchingUp");
}
#[test]
fn partial_tail_and_changed_anchor_never_acknowledge_unread_bytes() {
    let (root, agent) = fixture();
    let path = file(root.path(), &records()[..2], b"{\"type\":");
    let cancel = AtomicBool::new(false);
    let snapshot = HistorySnapshot::from_records(agent.clone(), None).expect("snapshot");
    let batch = history::prepare(
        snapshot,
        root.path(),
        RolloutPath::Found(path.clone()),
        BatchLimits::default(),
        200.,
        &cancel,
    )
    .expect("tail");
    assert_eq!(batch.checkpoint()["status"], "partialLine");
    assert!(
        batch.checkpoint()["offset"].as_u64().expect("offset")
            < fs::metadata(&path).expect("stat").len()
    );
    let previous = batch.checkpoint().clone();
    let mut bytes = fs::read(&path).expect("bytes");
    bytes[10] = b'x';
    fs::write(&path, bytes).expect("change checkpoint byte");
    let refused = history::prepare(
        HistorySnapshot::from_records(agent, Some(previous.clone())).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path),
        BatchLimits::default(),
        201.,
        &cancel,
    )
    .expect("refusal");
    assert_eq!(refused.checkpoint()["status"], "identityChanged");
    assert_eq!(refused.checkpoint()["offset"], previous["offset"]);
    assert!(!refused.advanced());
}
#[tokio::test]
async fn store_failure_rolls_back_each_effect_and_the_checkpoint() {
    for timing in ["BEFORE", "AFTER"] {
        for table in [
            "analytics_agents",
            "analytics_turns",
            "analytics_items",
            "analytics_usage",
            "analytics_usage_roots",
            "analytics_meta",
            "analytics_history",
        ] {
            let (root, agent) = fixture();
            let mut lines = records();
            lines.push(json!({"type":"response_item","timestamp":4,"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"synthetic"}]}}));
            lines.push(json!({"type":"token_usage_record","timestamp":5,"payload":{"thread_id":"thread","response_id":"r","usage":{"total_tokens":2},"thread_token_usage":{"total_tokens":2}}}));
            let path = file(root.path(), &lines, b"");
            let store = RuntimeStore::open(Some(root.path())).expect("store");
            let db = rusqlite::Connection::open(root.path().join("analytics.sqlite3")).expect("db");
            db.execute("DELETE FROM analytics_agents", [])
                .expect("exercise insert");
            db.execute_batch(&format!("CREATE TRIGGER abort_capture {timing} INSERT ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;")).expect("failpoint");
            drop(db);
            let batch = history::prepare(
                HistorySnapshot::from_records(agent.clone(), None).expect("snapshot"),
                root.path(),
                RolloutPath::Found(path.clone()),
                BatchLimits::default(),
                200.,
                &AtomicBool::new(false),
            )
            .expect("batch");
            assert!(
                batch.commit(&store, &Budget, 200.).await.is_err(),
                "failpoint {timing} {table} must fire"
            );
            assert!(checkpoint(&store).is_none());
            let db = rusqlite::Connection::open(root.path().join("analytics.sqlite3")).expect("db");
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM analytics_turns", [], |r| r
                    .get::<_, i64>(0))
                    .expect("count"),
                0
            );
            db.execute_batch("DROP TRIGGER abort_capture")
                .expect("clear failure");
            drop(db);
            drop(store);
            let store = RuntimeStore::open(Some(root.path())).expect("restart");
            let batch = history::prepare(
                HistorySnapshot::from_records(agent, None).expect("snapshot"),
                root.path(),
                RolloutPath::Found(path),
                BatchLimits::default(),
                201.,
                &AtomicBool::new(false),
            )
            .expect("retry");
            batch
                .commit(&store, &Budget, 201.)
                .await
                .expect("single commit");
            assert_eq!(checkpoint(&store).expect("saved")["importedRecords"], 5);
        }
    }
}
#[tokio::test]
async fn stale_agent_budget_failure_and_cancellation_write_nothing() {
    let (root, agent) = fixture();
    let mut lines = records();
    lines.push(json!({"type":"token_usage_record","timestamp":4,"payload":{"thread_id":"thread","response_id":"r","usage":{"total_tokens":2},"thread_token_usage":{"total_tokens":2}}}));
    let path = file(root.path(), &lines, b"");
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let batch = history::prepare(
        HistorySnapshot::from_records(agent.clone(), None).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path.clone()),
        BatchLimits::default(),
        200.,
        &AtomicBool::new(false),
    )
    .expect("batch");
    assert!(matches!(
        batch
            .commit(&store, &history::BudgetUnavailable, 200.)
            .await,
        Err(HistoryError::BudgetUnavailable)
    ));
    assert!(checkpoint(&store).is_none());
    assert!(matches!(
        history::prepare(
            HistorySnapshot::from_records(agent.clone(), None).expect("snapshot"),
            root.path(),
            RolloutPath::Found(path.clone()),
            BatchLimits::default(),
            200.,
            &AtomicBool::new(true)
        ),
        Err(HistoryError::Cancelled)
    ));
    let batch = history::prepare(
        HistorySnapshot::from_records(agent, None).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path),
        BatchLimits::default(),
        200.,
        &AtomicBool::new(false),
    )
    .expect("batch");
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.epoch',1) WHERE id='a'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("epoch advance");
    assert!(batch.commit(&store, &Budget, 200.).await.is_err());
    assert!(checkpoint(&store).is_none());
}

#[tokio::test]
async fn history_usage_is_admitted_once_per_bounded_page() {
    use std::sync::Mutex;
    #[derive(Default)]
    struct Pages(Mutex<Vec<Vec<String>>>);
    impl HistoryBudgetSink for Pages {
        fn capture_page<'a>(
            &'a self,
            captures: Vec<(Arc<Value>, Value, f64)>,
            _: f64,
        ) -> Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>> {
            Box::pin(async move {
                self.0.lock().expect("page counts").push(
                    captures
                        .iter()
                        .map(|(_, params, _)| {
                            params["responseId"]
                                .as_str()
                                .expect("captured history page includes response ID")
                                .to_owned()
                        })
                        .collect(),
                );
                Ok(())
            })
        }
    }
    let (root, agent) = fixture();
    let mut lines = records();
    for response in ["r1", "r2"] {
        lines.push(json!({"type":"token_usage_record","timestamp":5,"payload":{
            "thread_id":"thread","response_id":response,"usage":{"total_tokens":2},
            "thread_token_usage":{"total_tokens":2}
        }}));
    }
    let path = file(root.path(), &lines, b"");
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let batch = history::prepare(
        HistorySnapshot::from_records(agent, None).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path),
        BatchLimits::new(1024 * 1024, 16).expect("page limit"),
        200.,
        &AtomicBool::new(false),
    )
    .expect("batch");
    let pages = Pages::default();
    batch.commit(&store, &pages, 200.).await.expect("commit");
    assert_eq!(
        *pages.0.lock().expect("page counts"),
        vec![vec!["r1".to_owned(), "r2".to_owned()]],
        "two usage events must reach one budget page call"
    );
}

#[tokio::test]
async fn store_history_budget_sink_commits_page_once_and_replays_exactly() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use studio_runtime_services::analytics_history::StoreHistoryBudgetSink;
    let (root, agent) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let commits = Arc::new(AtomicUsize::new(0));
    let observed = commits.clone();
    store.set_commit_notifier(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    let captures = ["r1", "r2"].map(|response| {
        (
            Arc::new(agent.clone()),
            json!({
                "threadId":"thread", "turnId":"turn", "responseId":response,
                "rawTokenUsageRecord":{"response_id":response},
                "requestUsage":{"totalTokens":2},
                "tokenUsage":{"total":{"totalTokens":2},"last":{"totalTokens":2}}
            }),
            5.0,
        )
    });
    let sink = StoreHistoryBudgetSink::new(&store);
    sink.capture_page(captures.to_vec(), 200.)
        .await
        .expect("budget page");
    assert_eq!(
        commits.load(Ordering::SeqCst),
        1,
        "one writer commit for the page"
    );
    sink.capture_page(captures.to_vec(), 200.)
        .await
        .expect("idempotent replay");
    assert_eq!(
        commits.load(Ordering::SeqCst),
        1,
        "replay must not rewrite budget rows"
    );
    let (rows, tokens) = store
        .read_transaction(|tx| {
            Ok((
                tx.query_row("SELECT count(*) FROM runtime_budget_usage", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT COALESCE(sum(tokens),0) FROM runtime_budget_usage",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
            ))
        })
        .expect("read budget receipts");
    assert_eq!((rows, tokens), (2, 4));
}

#[tokio::test]
async fn collector_tick_imports_rollout_usage_and_replay_is_exactly_once() {
    use studio_runtime_services::analytics_history::{
        HistoryClock, HistoryCollector, StoreHistoryBudgetSink,
    };
    let (root, agent) = fixture();
    let sessions = root.path().join("sessions");
    fs::create_dir_all(&sessions).expect("fixture sessions");
    let path = sessions.join("rollout-fixture-thread.jsonl");
    let mut lines = records();
    lines.push(json!({"type":"token_usage_record","timestamp":4,"payload":{
        "thread_id":"thread","response_id":"history-response",
        "usage":{"total_tokens":23},"thread_token_usage":{"total_tokens":23}
    }}));
    let mut bytes = Vec::new();
    for line in &lines {
        bytes.extend(serde_json::to_vec(line).expect("rollout line"));
        bytes.push(b'\n');
    }
    fs::write(&path, bytes).expect("fixture rollout");
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let cancel = Arc::new(AtomicBool::new(false));
    let mut collector = HistoryCollector::default();
    let budget = StoreHistoryBudgetSink::new(&store);
    let roster = [agent];
    let profile_path = root.path().to_path_buf();

    assert!(
        collector
            .step(
                &store,
                &roster,
                move |_| Ok(profile_path),
                HistoryClock {
                    observed_at: 200.0,
                    monotonic: 10.0
                },
                Arc::clone(&cancel),
                &budget,
            )
            .await
            .expect("first import tick")
    );
    let read_budget = || {
        store
            .read_transaction(|tx| {
                Ok((
                    tx.query_row("SELECT count(*) FROM runtime_budget_usage", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                    tx.query_row(
                        "SELECT COALESCE(sum(tokens),0) FROM runtime_budget_usage",
                        [],
                        |row| row.get::<_, i64>(0),
                    )?,
                ))
            })
            .expect("budget receipts")
    };
    assert_eq!(read_budget(), (1, 23));
    let analytics = rusqlite::Connection::open(root.path().join("analytics.sqlite3"))
        .expect("analytics database");
    let read_analytics = || {
        analytics
            .query_row(
                "SELECT count(*) FROM analytics_usage WHERE json_extract(record,'$.responseId')='history-response'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("analytics usage")
    };
    assert_eq!(read_analytics(), 1);
    assert_eq!(
        checkpoint(&store).expect("history checkpoint")["importedRecords"],
        4
    );

    let _replay_advanced = collector
        .step(
            &store,
            &roster,
            |_| panic!("idle poll must not rediscover files"),
            HistoryClock {
                observed_at: 205.0,
                monotonic: 15.0,
            },
            Arc::clone(&cancel),
            &budget,
        )
        .await
        .expect("replay tick");

    assert_eq!(read_budget(), (1, 23));
    assert_eq!(read_analytics(), 1);
    assert_eq!(
        checkpoint(&store).expect("history checkpoint")["importedRecords"],
        4
    );
}

#[tokio::test]
async fn historical_budget_capture_accepts_prior_turn_notices_and_keeps_other_identity_strict() {
    use studio_runtime_services::analytics_history::{
        HistoryClock, HistoryCollector, StoreHistoryBudgetSink,
    };
    use studio_runtime_services::budget::{BudgetError, CaptureSource};
    use studio_runtime_services::budget_service;

    let (root, mut agent) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    agent["turnId"] = json!("current-turn");
    let agent_id = agent["id"].as_str().expect("agent id").to_owned();
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.turnId',?1) WHERE id=?2",
                rusqlite::params!["current-turn", agent_id],
            )?;
            Ok(())
        })
        .await
        .expect("persist current turn");

    let usage = json!({
        "input_tokens": 10,
        "cached_input_tokens": 0,
        "output_tokens": 5,
        "reasoning_output_tokens": 0,
        "total_tokens": 15
    });
    let lines = vec![
        json!({"type":"session_meta","timestamp":1,"payload":{"id":"thread"}}),
        json!({"type":"event_msg","timestamp":2,"payload":{"type":"task_started","turn_id":"earlier-turn"}}),
        json!({"type":"event_msg","timestamp":3,"payload":{"type":"token_count","info":{"total_token_usage":usage,"last_token_usage":usage}}}),
        json!({"type":"event_msg","timestamp":4,"payload":{"type":"task_complete","turn_id":"earlier-turn","status":"completed"}}),
        json!({"type":"event_msg","timestamp":5,"payload":{"type":"task_started","turn_id":"current-turn"}}),
        json!({"type":"event_msg","timestamp":6,"payload":{"type":"token_count","info":{"total_token_usage":usage,"last_token_usage":usage}}}),
        json!({"type":"token_usage_record","timestamp":6.5,"payload":{"thread_id":"thread","turn_id":"earlier-turn","response_id":"delayed-earlier-response","usage":{"total_tokens":4},"thread_token_usage":{"total_tokens":19}}}),
        json!({"type":"event_msg","timestamp":7,"payload":{"type":"task_complete","turn_id":"current-turn","status":"completed"}}),
    ];
    let sessions = root.path().join("sessions");
    fs::create_dir_all(&sessions).expect("fixture sessions");
    let path = file(&sessions, &lines, b"");
    let cancel = Arc::new(AtomicBool::new(false));
    let budget = StoreHistoryBudgetSink::new(&store);
    let mut collector = HistoryCollector::default();
    let profile_path = root.path().to_path_buf();
    assert!(
        collector
            .step(
                &store,
                std::slice::from_ref(&agent),
                move |_| Ok(profile_path),
                HistoryClock {
                    observed_at: 200.0,
                    monotonic: 10.0,
                },
                Arc::clone(&cancel),
                &budget,
            )
            .await
            .expect("import notices and delayed prior-turn response")
    );

    let read_budget = || {
        store
            .read_transaction(|tx| {
                Ok((
                    tx.query_row("SELECT count(*) FROM runtime_budget_usage", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                    tx.query_row(
                        "SELECT COALESCE(sum(tokens),0) FROM runtime_budget_usage",
                        [],
                        |row| row.get::<_, i64>(0),
                    )?,
                ))
            })
            .expect("budget receipts")
    };
    assert_eq!(read_budget(), (3, 34));
    let stored_checkpoint = checkpoint(&store).expect("history checkpoint");
    assert_eq!(
        stored_checkpoint["offset"],
        fs::metadata(&path).expect("stat").len()
    );
    assert_eq!(stored_checkpoint["importedRecords"], lines.len() as u64);

    let replay = history::prepare(
        HistorySnapshot::from_records(agent.clone(), Some(stored_checkpoint.clone()))
            .expect("checkpoint snapshot"),
        root.path(),
        RolloutPath::Found(path),
        BatchLimits::default(),
        201.0,
        &cancel,
    )
    .expect("replay historical turns");
    assert!(!replay.advanced());
    replay
        .commit(&store, &budget, 201.0)
        .await
        .expect("idempotent historical replay");
    assert_eq!(checkpoint(&store), Some(stored_checkpoint));
    assert_eq!(read_budget(), (3, 34));

    let params = json!({
        "threadId":"thread",
        "turnId":"earlier-turn",
        "responseId":"live-wrong-turn",
        "rawTokenUsageRecord":{"response_id":"live-wrong-turn"},
        "requestUsage":{"totalTokens":5},
        "tokenUsage":{"total":{"totalTokens":5},"last":{"totalTokens":5}}
    });
    let mut wrong_live_agent = agent.clone();
    wrong_live_agent["turnId"] = json!("earlier-turn");
    let result = store
        .transaction_with::<_, BudgetError>(|tx| {
            budget_service::capture(
                tx,
                &mut wrong_live_agent,
                &params,
                202.0,
                CaptureSource::Live,
                202.0,
            )
            .map(|_| ())
        })
        .await;
    assert!(matches!(
        result,
        Err(BudgetError::Invalid(location)) if location == "runtime_agents/turnId/identity"
    ));

    for (field, value) in [
        ("rootId", json!("foreign-root")),
        ("accountKey", json!("foreign-account")),
        ("threadId", json!("foreign-thread")),
        ("epoch", json!(9_999)),
    ] {
        let mut wrong_history_agent = agent.clone();
        wrong_history_agent["turnId"] = json!("earlier-turn");
        wrong_history_agent[field] = value;
        let result = store
            .transaction_with::<_, BudgetError>(|tx| {
                budget_service::capture(
                    tx,
                    &mut wrong_history_agent,
                    &params,
                    203.0,
                    CaptureSource::History,
                    203.0,
                )
                .map(|_| ())
            })
            .await;
        assert!(matches!(
            result,
            Err(BudgetError::Invalid(location)) if location == format!("runtime_agents/{field}/identity")
        ));
    }
}

#[test]
fn active_import_quota_notification_waits_for_event() {
    use studio_runtime_services::analytics_history_schedule::HistorySchedule;
    let (_, mut active) = fixture();
    active["status"] = json!("running");
    let mut archive = active.clone();
    archive["id"] = json!("archive");
    archive["status"] = json!("completed");
    archive["deletedAt"] = json!(10.);
    let actors = vec![active.clone(), archive.clone()];
    let mut schedule = HistorySchedule::default();
    let mut checkpoint = json!({"id":"a:default:thread","agent":"a","accountKey":"default","threadId":"thread","offset":0,"importedRecords":0,"malformedLines":0,"context":{"threadId":"thread"},"status":"catchingUp"});
    for _ in 0..8 {
        assert_eq!(
            schedule
                .next(&actors, 10.)
                .expect("selection")
                .expect("active")["id"],
            "a"
        );
        schedule.completed(&active, &checkpoint, 10.).expect("poll");
    }
    assert_eq!(
        schedule
            .next(&actors, 10.)
            .expect("selection")
            .expect("archive fair turn")["id"],
        "archive"
    );
    checkpoint["status"] = json!("current");
    schedule
        .completed(&archive, &checkpoint, 10.)
        .expect("archive complete");
    schedule
        .completed(&active, &checkpoint, 10.)
        .expect("watch");
    assert!(schedule.next(&actors, 11.).expect("selection").is_none());
    schedule.notify("a").expect("notification");
    assert_eq!(
        schedule
            .next(&actors, 11.)
            .expect("selection")
            .expect("dirty")["id"],
        "a"
    );
    assert!(schedule.has_pending());
    schedule
        .completed(&active, &checkpoint, 11.)
        .expect("notified actor complete");
    assert!(!schedule.has_pending());
}

#[tokio::test]
async fn background_consumer_imports_selected_roster_actor() {
    let (root, agent) = fixture();
    let session = root.path().join("sessions");
    fs::create_dir(&session).expect("sessions");
    file(&session, &records(), b"");
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let mut collector = history::HistoryCollector::default();
    assert!(
        collector
            .step(
                &store,
                std::slice::from_ref(&agent),
                {
                    let profile_path = root.path().to_path_buf();
                    move |_| Ok(profile_path)
                },
                history::HistoryClock {
                    observed_at: 200.,
                    monotonic: 10.
                },
                Arc::new(AtomicBool::new(false)),
                &Budget
            )
            .await
            .expect("background step")
    );
    assert_eq!(checkpoint(&store).expect("checkpoint")["status"], "current");
    assert!(
        !collector
            .step(
                &store,
                &[agent],
                |_| panic!("idle poll must not rediscover files"),
                history::HistoryClock {
                    observed_at: 201.,
                    monotonic: 11.
                },
                Arc::new(AtomicBool::new(false)),
                &Budget
            )
            .await
            .expect("idle")
    );
}

#[tokio::test]
async fn terminal_error_repair_matches_python_exact_and_ambiguous_notices() {
    use studio_runtime_services::analytics_capture::{CaptureSource, PreparedCapture, capture};
    use studio_runtime_services::analytics_history_repair::repair_terminal_errors;
    for thread in [Some("thread"), None, Some("foreign")] {
        let (root, agent) = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        capture(
            &store,
            PreparedCapture::event(
                &agent,
                "turn/started",
                &json!({}),
                1.,
                200.,
                CaptureSource::Live,
            )
            .expect("prepared"),
        )
        .await
        .expect("capture");
        let notice = json!({"id":"a:native-notice:error:turn","agent":"a","turnId":"turn","threadId":thread,"nativeError":{"message":"native failure"},"at":3});
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_items(id,agent,record,created) VALUES (?,?,?,?)",
                    rusqlite::params![notice["id"].as_str(), "a", notice.to_string(), 3],
                )?;
                Ok(())
            })
            .await
            .expect("saved notice");
        if thread.is_none() {
            store
                .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                    let mut row =
                        Repo::read(tx, CaptureTable::Turns, "a:thread:turn")?.expect("turn");
                    row["id"] = json!("a:other:turn");
                    row["threadId"] = json!("other");
                    Repo::write(tx, CaptureTable::Turns, &row)
                })
                .await
                .expect("ambiguous second turn");
        }
        drop(store);
        let reference = tempfile::tempdir().expect("oracle clone");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            fs::copy(root.path().join(name), reference.path().join(name)).expect("clone");
        }
        let out = Command::new(prepared_python::prepared_python(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        ))
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/analytics_capture/repair_oracle.py"),
        )
        .arg(reference.path())
        .output()
        .expect("oracle");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let expected: Value = serde_json::from_slice(&out.stdout).expect("oracle result");
        let store = RuntimeStore::open(Some(root.path())).expect("restart");
        repair_terminal_errors(&store).await.expect("Rust repair");
        let db = AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("db");
        let turns = db
            .prepare("SELECT record FROM analytics_turns ORDER BY id")
            .expect("query")
            .query_map([], |r| r.get::<_, String>(0))
            .expect("rows")
            .map(|v| serde_json::from_str::<Value>(&v.expect("row")).expect("JSON"))
            .collect::<Vec<_>>();
        let state: String = db
            .query_row(
                "SELECT value FROM analytics_meta WHERE key='terminalErrorRepairV1'",
                [],
                |r| r.get(0),
            )
            .expect("state");
        assert_eq!(
            json!({"turns":turns,"state":serde_json::from_str::<Value>(&state).expect("state JSON")}),
            expected
        );
    }
}

#[tokio::test]
async fn budget_migration_reconciles_lost_cursor_ack_without_double_charging() {
    use std::sync::Mutex;
    use studio_runtime_services::analytics_capture::{CaptureSource, PreparedCapture, capture};
    use studio_runtime_services::analytics_history_repair::migrate_budget;
    #[derive(Default)]
    struct Receipts {
        ids: Mutex<std::collections::BTreeSet<String>>,
        page_sizes: Mutex<Vec<usize>>,
    }
    impl HistoryBudgetSink for Receipts {
        fn capture_page<'a>(
            &'a self,
            captures: Vec<(Arc<Value>, Value, f64)>,
            _: f64,
        ) -> Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>> {
            Box::pin(async move {
                self.page_sizes
                    .lock()
                    .expect("page sizes")
                    .push(captures.len());
                let mut receipts = self.ids.lock().expect("receipts");
                for (_, params, _) in captures {
                    receipts.insert(params["responseId"].as_str().expect("response").into());
                }
                Ok(())
            })
        }
    }
    let (root, agent) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let params = json!({"responseId":"r0","tokenUsage":{"total":{"totalTokens":2},"last":{"totalTokens":2}},"rawTokenUsageRecord":{"response_id":"r0"}});
    capture(
        &store,
        PreparedCapture::event(
            &agent,
            "thread/tokenUsage/updated",
            &params,
            1.,
            200.,
            CaptureSource::Rollout,
        )
        .expect("input"),
    )
    .await
    .expect("capture");
    store.analytics_transaction::<_,studio_runtime_store::StoreError>(|tx,_|{let raw:String=tx.query_row("SELECT record FROM analytics_usage LIMIT 1",[],|r|r.get(0))?;let base:Value=serde_json::from_str(&raw)?;for index in 1..65{let mut record=base.clone();record["id"]=json!(format!("usage-{index}"));record["responseId"]=json!(format!("r{index}"));Repo::write(tx,CaptureTable::Usage,&record)?;}tx.execute_batch("CREATE TRIGGER abort_migration BEFORE INSERT ON analytics_meta WHEN NEW.key LIKE 'budgetUsageMigrationV1:%' BEGIN SELECT RAISE(ABORT,'injected'); END;")?;Ok(())}).await.expect("65 saved responses and failpoint");
    let receipts = Receipts::default();
    assert!(
        migrate_budget(&store, agent.clone(), &receipts, 200.)
            .await
            .is_err()
    );
    assert_eq!(receipts.ids.lock().expect("receipts").len(), 64);
    assert_eq!(*receipts.page_sizes.lock().expect("page sizes"), vec![64]);
    store
        .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
            tx.execute_batch("DROP TRIGGER abort_migration")?;
            Ok(())
        })
        .await
        .expect("remove failpoint");
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    assert!(
        migrate_budget(&store, agent.clone(), &receipts, 200.)
            .await
            .expect("idempotent replay")
    );
    assert_eq!(receipts.ids.lock().expect("receipts").len(), 64);
    assert_eq!(
        *receipts.page_sizes.lock().expect("page sizes"),
        vec![64, 64]
    );
    assert!(
        migrate_budget(&store, agent.clone(), &receipts, 200.)
            .await
            .expect("last page")
    );
    assert_eq!(receipts.ids.lock().expect("receipts").len(), 65);
    assert_eq!(
        *receipts.page_sizes.lock().expect("page sizes"),
        vec![64, 64, 1]
    );
    assert!(
        !migrate_budget(&store, agent, &receipts, 200.)
            .await
            .expect("complete")
    );
}

#[path = "analytics_capture/common.rs"]
mod common;

#[tokio::test]
async fn full_tail_import_matches_real_python_rows_and_checkpoint() {
    for suffix in [b"".as_slice(), b"{\"partial\":", b"malformed\n"] {
        let (root, agent) = fixture();
        let oracle_root = tempfile::tempdir().expect("oracle clone");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            fs::copy(root.path().join(name), oracle_root.path().join(name)).expect("clone");
        }
        let mut lines = records();
        lines.push(json!({"type":"response_item","timestamp":4,"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"tail \u{03bb}"}]}}));
        lines.push(json!({"type":"token_usage_record","timestamp":5,"payload":{"thread_id":"thread","response_id":"r","usage":{"total_tokens":2},"thread_token_usage":{"total_tokens":2}}}));
        let path = file(root.path(), &lines, suffix);
        let output = Command::new(prepared_python::prepared_python(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        ))
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/analytics_capture/history_oracle.py"),
        )
        .arg(oracle_root.path())
        .arg(&path)
        .output()
        .expect("Python importer");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle rows");
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        history::prepare(
            HistorySnapshot::from_records(agent, None).expect("snapshot"),
            root.path(),
            RolloutPath::Found(path),
            BatchLimits::default(),
            200.,
            &AtomicBool::new(false),
        )
        .expect("read")
        .commit(&store, &Budget, 200.)
        .await
        .expect("commit");
        let db = AnalyticsRepository::new(root.path())
            .open_read_only()
            .expect("read");
        common::compare(&common::actual(&db, &expected), &expected, "tail");
    }
}

#[tokio::test]
async fn notification_batch_and_account_limit_consumers_commit_or_refuse_atomically() {
    use studio_runtime_services::analytics_capture::{
        CaptureSource, PreparedCapture, capture_batch, capture_limit,
    };
    let (root, agent) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let prepared = PreparedCapture::event(
        &agent,
        "turn/started",
        &json!({}),
        1.,
        200.,
        CaptureSource::Live,
    )
    .expect("prepare");
    let mut stale = agent.clone();
    stale["epoch"] = json!(77);
    let rejected = PreparedCapture::event(
        &stale,
        "turn/completed",
        &json!({}),
        2.,
        200.,
        CaptureSource::Live,
    )
    .expect("stale");
    assert!(
        capture_batch(&store, vec![prepared.clone(), rejected])
            .await
            .is_err()
    );
    let db = AnalyticsRepository::new(root.path())
        .open_read_only()
        .expect("reader");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM analytics_notifications", [], |r| r
            .get::<_, i64>(0))
            .expect("count"),
        0
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM analytics_turns", [], |r| r
            .get::<_, i64>(0))
            .expect("count"),
        0
    );
    capture_batch(&store, vec![prepared]).await.expect("commit");
    assert_eq!(
        db.query_row("SELECT SUM(count) FROM analytics_notifications", [], |r| {
            r.get::<_, i64>(0)
        })
        .expect("count"),
        1
    );
    let limits = json!({"rateLimits":{"primary":{"usedPercent":25}},"updated":1});
    capture_limit(&store, "default".into(), limits.clone(), 10.)
        .await
        .expect("limit");
    capture_limit(&store, "default".into(), limits.clone(), 11.)
        .await
        .expect("duplicate limit");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM analytics_limits", [], |r| r
            .get::<_, i64>(0))
            .expect("count"),
        1
    );
    assert!(
        capture_limit(&store, "default".into(), limits, f64::NAN)
            .await
            .is_err()
    );
}

#[test]
fn checked_history_identity_and_state_check_refuse_malformed_rows() {
    let (root, agent) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let base = json!({"id":"a:default:thread","agent":"a","accountKey":"default","threadId":"thread","offset":0,"importedRecords":0,"malformedLines":0,"context":{"threadId":"thread"}});
    for (field, value) in [
        ("offset", json!(-1)),
        ("context", json!([])),
        ("id", json!("foreign")),
        ("filesystemIdentity", json!([1])),
    ] {
        let mut invalid = base.clone();
        invalid[field] = value;
        assert!(HistorySnapshot::from_records(agent.clone(), Some(invalid)).is_err());
    }
    let db = rusqlite::Connection::open(root.path().join("analytics.sqlite3")).expect("db");
    db.execute("INSERT INTO analytics_history VALUES ('bad','a','{}')", [])
        .expect("corrupt fixture");
    let before: String = db
        .query_row(
            "SELECT record FROM analytics_history WHERE id='bad'",
            [],
            |r| r.get(0),
        )
        .expect("before");
    let report = studio_runtime_store::analytics_capture::check_state_readonly(root.path())
        .expect("state check");
    let history = report
        .iter()
        .find(|r| r.table == "analytics_history")
        .expect("registered");
    assert_eq!(history.rejected_count, 1);
    assert!(!history.failures.is_empty());
    assert_eq!(
        db.query_row(
            "SELECT record FROM analytics_history WHERE id='bad'",
            [],
            |r| r.get::<_, String>(0)
        )
        .expect("after"),
        before
    );
    drop(store);
}

#[test]
fn conflicting_header_alias_does_not_authorize_a_foreign_file() {
    let (root, agent) = fixture();
    let path = file(
        root.path(),
        &[json!({"type":"session_meta","payload":{"id":"foreign","session_id":"thread"}})],
        b"",
    );
    let batch = history::prepare(
        HistorySnapshot::from_records(agent, None).expect("snapshot"),
        root.path(),
        RolloutPath::Found(path),
        BatchLimits::default(),
        200.,
        &AtomicBool::new(false),
    )
    .expect("refusal");
    assert_eq!(batch.checkpoint()["status"], "wrongThread");
    assert_eq!(batch.checkpoint()["offset"], 0);
}

#[tokio::test]
async fn changed_consumed_range_is_refused_before_any_effect() {
    struct NoEffects;
    impl HistoryBudgetSink for NoEffects {
        fn capture_page<'a>(
            &'a self,
            _: Vec<(Arc<Value>, Value, f64)>,
            _: f64,
        ) -> Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>> {
            Box::pin(async { panic!("changed file must be refused before budget capture") })
        }
    }
    for change in ["rewrite", "truncate", "replace"] {
        let (root, agent) = fixture();
        let mut lines = records();
        lines.push(json!({"type":"token_usage_record","timestamp":4,"payload":{"thread_id":"thread","response_id":"r","usage":{"total_tokens":2},"thread_token_usage":{"total_tokens":2}}}));
        lines.push(json!({"type":"ignored","padding":"x".repeat(512)}));
        let path = file(root.path(), &lines, b"");
        let original = fs::read(&path).expect("original");
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        let batch = history::prepare(
            HistorySnapshot::from_records(agent, None).expect("snapshot"),
            root.path(),
            RolloutPath::Found(path.clone()),
            BatchLimits::default(),
            200.,
            &AtomicBool::new(false),
        )
        .expect("read batch");
        match change {
            "rewrite" => {
                let text = String::from_utf8(original.clone())
                    .expect("JSON")
                    .replace("task_started", "task_storted");
                assert_eq!(text.len(), original.len());
                assert_eq!(
                    &text.as_bytes()[text.len() - 256..],
                    &original[original.len() - 256..]
                );
                fs::write(&path, text).expect("rewrite consumed bytes, preserve final anchor");
            }
            "truncate" => fs::write(&path, b"").expect("truncate"),
            _ => {
                fs::rename(&path, root.path().join("old-file")).expect("replace inode");
                fs::write(&path, &original).expect("same bytes, new inode");
            }
        }
        assert!(
            matches!(
                batch.commit(&store, &NoEffects, 201.).await,
                Err(HistoryError::ChangedDuringRead)
            ),
            "{change}"
        );
        assert!(checkpoint(&store).is_none());
        let db = AnalyticsRepository::new(root.path())
            .open_read_only()
            .expect("read");
        for table in [
            "analytics_turns",
            "analytics_usage",
            "analytics_notifications",
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .expect("count"),
                0
            );
        }
    }
}

#[test]
fn discovery_round_reuses_profile_scan_and_enforces_filename_and_symlink_rules() {
    let root = tempfile::tempdir().expect("profile");
    let sessions = root.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let cancel = AtomicBool::new(false);
    let first = sessions.join("rollout-fixture-first.jsonl");
    let second = sessions.join("rollout-fixture-second.jsonl");
    fs::write(&first, "first").expect("first");
    fs::write(&second, "second").expect("second");
    fs::write(sessions.join("unrelated-third.jsonl"), "invalid filename").expect("foreign name");
    let outside = tempfile::tempdir().expect("outside profile");
    fs::write(
        outside.path().join("rollout-fixture-hidden.jsonl"),
        "hidden",
    )
    .expect("hidden");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), sessions.join("linked-directory")).expect("symlink");
    let mut round = history::DiscoveryRound::default();
    assert_eq!(
        round
            .resolve(root.path(), "first", &cancel)
            .expect("first scan"),
        RolloutPath::Found(first)
    );
    fs::write(sessions.join("rollout-duplicate-second.jsonl"), "duplicate")
        .expect("new file after round snapshot");
    assert_eq!(
        round
            .resolve(root.path(), "second", &cancel)
            .expect("shared scan"),
        RolloutPath::Found(second)
    );
    assert_eq!(
        round
            .resolve(root.path(), "third", &cancel)
            .expect("prefix"),
        RolloutPath::Missing
    );
    assert_eq!(
        round
            .resolve(root.path(), "hidden", &cancel)
            .expect("symlink directory"),
        RolloutPath::Missing
    );
    assert_eq!(
        history::discover(root.path(), "second", &cancel).expect("next round"),
        RolloutPath::Ambiguous
    );
}

#[tokio::test]
async fn collector_updates_shared_discovery_from_file_event_across_rounds() {
    let (root, agent) = fixture();
    let sessions = root.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let path = file(&sessions, &records(), b"");
    let mut second = agent.clone();
    second["id"] = json!("b");
    second["parentId"] = json!("a");
    let second_record = second.to_string();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('b',?)",
                [second_record],
            )?;
            Ok(())
        })
        .await
        .expect("second managed actor");
    let actors = [agent, second];
    let cancel = Arc::new(AtomicBool::new(false));
    let mut collector = history::HistoryCollector::default();
    for index in 0..2 {
        let profile_path = root.path().to_path_buf();
        let advanced = collector
            .step(
                &store,
                &actors,
                move |_| Ok(profile_path),
                history::HistoryClock {
                    observed_at: 200.,
                    monotonic: 10. + index as f64,
                },
                Arc::clone(&cancel),
                &Budget,
            )
            .await
            .expect("actor import");
        if index == 0 {
            assert!(advanced, "the first actor imports its rollout");
            let duplicate = sessions.join("rollout-duplicate-thread.jsonl");
            fs::copy(&path, &duplicate).expect("new file between actors");
            collector.file_changed(root.path(), &duplicate, false);
            collector
                .schedule
                .notify("a")
                .expect("notify first actor for new rollout");
            collector
                .schedule
                .notify("b")
                .expect("notify second actor for new rollout");
        } else {
            assert!(!advanced, "ambiguous rollout is not imported");
        }
    }
    let db = AnalyticsRepository::new(root.path())
        .open_read_only()
        .expect("read");
    assert_eq!(
        Repo::read(&db, CaptureTable::History, "b:default:thread")
            .expect("row")
            .expect("second checkpoint")["status"],
        "ambiguous"
    );
    drop(db);
    collector.begin_round();
    let profile_path = root.path().to_path_buf();
    assert!(
        !collector
            .step(
                &store,
                &actors,
                move |_| Ok(profile_path),
                history::HistoryClock {
                    observed_at: 201.,
                    monotonic: 20.,
                },
                Arc::clone(&cancel),
                &Budget,
            )
            .await
            .expect("next round"),
        "a watcher-updated ambiguous profile is refused without import"
    );
    assert_eq!(
        checkpoint(&store).expect("new discovery")["status"],
        "ambiguous"
    );
}
