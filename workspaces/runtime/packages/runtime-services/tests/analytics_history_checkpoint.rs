use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{OnceLock, atomic::AtomicBool};
use studio_runtime_services::analytics_history::{
    self as history, BatchLimits, HistoryBudgetSink, HistoryError, HistorySnapshot, RolloutPath,
};
use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository as Repo, CaptureTable};
use studio_runtime_store::{AnalyticsRepository, RuntimeStore};

struct Budget;
impl HistoryBudgetSink for Budget {
    fn capture(&self, _: &Value, _: &Value, _: f64) -> Result<(), HistoryError> {
        Ok(())
    }
}
fn fixture() -> (tempfile::TempDir, Value) {
    static TEMPLATE: OnceLock<(tempfile::TempDir, Value)> = OnceLock::new();
    let (root, agent) = TEMPLATE.get_or_init(|| {
        let root = tempfile::tempdir().expect("template");
        let out = Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("isolated Python"))
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
    let copy = tempfile::tempdir().expect("case");
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
#[test]
fn restart_after_ack_loss_retains_exact_effects_and_cursor() {
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
        .expect("commit before response lost");
    let committed = checkpoint(&store).expect("checkpoint");
    assert_eq!(
        committed["offset"],
        fs::metadata(&path).expect("stat").len()
    );
    assert!(matches!(
        stale.commit(&store, &Budget, 201.),
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
    batch.commit(&store, &Budget, 202.).expect("idle commit");
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
#[test]
fn store_failure_rolls_back_each_effect_and_the_checkpoint() {
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
                batch.commit(&store, &Budget, 200.).is_err(),
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
            batch.commit(&store, &Budget, 201.).expect("single commit");
            assert_eq!(checkpoint(&store).expect("saved")["importedRecords"], 5);
        }
    }
}
#[test]
fn stale_agent_budget_failure_and_cancellation_write_nothing() {
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
        batch.commit(&store, &history::BudgetUnavailable, 200.),
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
        .expect("epoch advance");
    assert!(batch.commit(&store, &Budget, 200.).is_err());
    assert!(checkpoint(&store).is_none());
}

#[test]
fn active_import_quota_notification_and_timer_budget() {
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
        schedule
            .completed(&active, &checkpoint, 10., false, false)
            .expect("poll");
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
        .completed(&archive, &checkpoint, 10., false, false)
        .expect("archive complete");
    schedule
        .completed(&active, &checkpoint, 10., true, false)
        .expect("watch");
    assert!(schedule.next(&actors, 11.).expect("selection").is_none());
    schedule.notify("a");
    assert_eq!(
        schedule
            .next(&actors, 11.)
            .expect("selection")
            .expect("dirty")["id"],
        "a"
    );
    assert!((schedule.delay(11., 0., 0.15) - 1.).abs() < 0.00001);
}

#[test]
fn background_consumer_imports_selected_roster_actor() {
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
                |_| Ok(root.path().to_path_buf()),
                history::HistoryClock {
                    observed_at: 200.,
                    monotonic: 10.
                },
                &AtomicBool::new(false),
                &Budget
            )
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
                &AtomicBool::new(false),
                &Budget
            )
            .expect("idle")
    );
}

#[test]
fn terminal_error_repair_matches_python_exact_and_ambiguous_notices() {
    use studio_runtime_services::analytics_capture::{CaptureSource, PreparedCapture, capture};
    use studio_runtime_services::analytics_history_repair::repair_terminal_errors;
    for thread in [Some("thread"), None, Some("foreign")] {
        let (root, agent) = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        capture(
            &store,
            &PreparedCapture::event(
                &agent,
                "turn/started",
                &json!({}),
                1.,
                200.,
                CaptureSource::Live,
            )
            .expect("prepared"),
        )
        .expect("capture");
        let notice = json!({"id":"a:native-notice:error:turn","agent":"a","turnId":"turn","threadId":thread,"nativeError":{"message":"native failure"},"at":3});
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_items(id,agent,record,created) VALUES (?,?,?,?)",
                    rusqlite::params![notice["id"].as_str(), "a", notice.to_string(), 3],
                )?;
                Ok(())
            })
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
                .expect("ambiguous second turn");
        }
        drop(store);
        let reference = tempfile::tempdir().expect("oracle clone");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            fs::copy(root.path().join(name), reference.path().join(name)).expect("clone");
        }
        let out = Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("Python"))
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
        repair_terminal_errors(&store).expect("Rust repair");
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

#[test]
fn budget_migration_reconciles_lost_cursor_ack_without_double_charging() {
    use std::sync::Mutex;
    use studio_runtime_services::analytics_capture::{CaptureSource, PreparedCapture, capture};
    use studio_runtime_services::analytics_history_repair::migrate_budget;
    #[derive(Default)]
    struct Receipts(Mutex<std::collections::BTreeSet<String>>);
    impl HistoryBudgetSink for Receipts {
        fn capture(&self, _: &Value, p: &Value, _: f64) -> Result<(), HistoryError> {
            self.0
                .lock()
                .expect("receipts")
                .insert(p["responseId"].as_str().expect("response").into());
            Ok(())
        }
    }
    let (root, agent) = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let params = json!({"responseId":"r0","tokenUsage":{"total":{"totalTokens":2},"last":{"totalTokens":2}},"rawTokenUsageRecord":{"response_id":"r0"}});
    capture(
        &store,
        &PreparedCapture::event(
            &agent,
            "thread/tokenUsage/updated",
            &params,
            1.,
            200.,
            CaptureSource::Rollout,
        )
        .expect("input"),
    )
    .expect("capture");
    store.analytics_transaction::<_,studio_runtime_store::StoreError>(|tx,_|{let raw:String=tx.query_row("SELECT record FROM analytics_usage LIMIT 1",[],|r|r.get(0))?;let base:Value=serde_json::from_str(&raw)?;for index in 1..65{let mut record=base.clone();record["id"]=json!(format!("usage-{index}"));record["responseId"]=json!(format!("r{index}"));Repo::write(tx,CaptureTable::Usage,&record)?;}tx.execute_batch("CREATE TRIGGER abort_migration BEFORE INSERT ON analytics_meta WHEN NEW.key LIKE 'budgetUsageMigrationV1:%' BEGIN SELECT RAISE(ABORT,'injected'); END;")?;Ok(())}).expect("65 saved responses and failpoint");
    let receipts = Receipts::default();
    assert!(migrate_budget(&store, &agent, &receipts).is_err());
    assert_eq!(receipts.0.lock().expect("receipts").len(), 64);
    store
        .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
            tx.execute_batch("DROP TRIGGER abort_migration")?;
            Ok(())
        })
        .expect("remove failpoint");
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    assert!(migrate_budget(&store, &agent, &receipts).expect("idempotent replay"));
    assert_eq!(receipts.0.lock().expect("receipts").len(), 64);
    assert!(migrate_budget(&store, &agent, &receipts).expect("last page"));
    assert_eq!(receipts.0.lock().expect("receipts").len(), 65);
    assert!(!migrate_budget(&store, &agent, &receipts).expect("complete"));
}

#[path = "analytics_capture/common.rs"]
mod common;

#[test]
fn full_tail_import_matches_real_python_rows_and_checkpoint() {
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
        let output =
            Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("isolated Python"))
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
        .expect("commit");
        let db = AnalyticsRepository::new(root.path())
            .open_read_only()
            .expect("read");
        common::compare(&common::actual(&db, &expected), &expected, "tail");
    }
}

#[test]
fn notification_batch_and_account_limit_consumers_commit_or_refuse_atomically() {
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
    assert!(capture_batch(&store, &[prepared.clone(), rejected]).is_err());
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
    capture_batch(&store, &[prepared]).expect("commit");
    assert_eq!(
        db.query_row("SELECT SUM(count) FROM analytics_notifications", [], |r| {
            r.get::<_, i64>(0)
        })
        .expect("count"),
        1
    );
    let limits = json!({"rateLimits":{"primary":{"usedPercent":25}},"updated":1});
    capture_limit(&store, "default", &limits, 10.).expect("limit");
    capture_limit(&store, "default", &limits, 11.).expect("duplicate limit");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM analytics_limits", [], |r| r
            .get::<_, i64>(0))
            .expect("count"),
        1
    );
    assert!(capture_limit(&store, "default", &limits, f64::NAN).is_err());
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
