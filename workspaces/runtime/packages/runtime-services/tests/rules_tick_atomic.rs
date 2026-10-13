use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use studio_runtime_services::rules_engine::{CheckResult, RuleCheck};
use studio_runtime_services::rules_tick::{
    CheckReconciliation, CheckSubmission, LocalRuleFiles, RuleEvent, RuleFiles, RuleMonitor,
    RulesTickError, UnavailableMonitor, event_in_transaction, finish, finish_in_transaction,
    recover_batch, submit_check, tick_batch,
};
use studio_runtime_store::{RuleRecord, RulesRepository, RuntimeStore, StoreError};
use tempfile::TempDir;

const NOW: f64 = 1_800_000_000.0;

fn temporary_state() -> TempDir {
    match std::env::var_os("RULES_TEST_TMPDIR") {
        Some(root) => tempfile::tempdir_in(root).expect("owned test state root"),
        None => tempfile::tempdir().expect("test state"),
    }
}

struct Template(TempDir);
impl Template {
    fn new() -> Self {
        let root = temporary_state();
        let python = studio_runtime_services::agents_create::prepared_python();
        let output = Command::new(python)
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/rules_engine/oracle.py"))
            .arg("--template")
            .arg(root.path())
            .output()
            .expect("Python writer");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(root)
    }
    fn clone_state(&self) -> TempDir {
        let state = temporary_state();
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            std::fs::copy(self.0.path().join(name), state.path().join(name))
                .expect("copy closed template");
        }
        state
    }
}
fn rule(store: &RuntimeStore) -> Value {
    RulesRepository::get(&store.read().expect("read"), "rule")
        .expect("rule read")
        .expect("rule exists")
        .value()
        .clone()
}
fn change(store: &RuntimeStore, fields: Value) {
    store
        .transaction(|tx| {
            let mut value = RulesRepository::get(tx, "rule")?
                .expect("rule exists")
                .value()
                .clone();
            for (field, value_new) in fields.as_object().expect("changes") {
                value[field] = value_new.clone();
            }
            RulesRepository::put(tx, &RuleRecord::from_value(value)?)
        })
        .expect("change rule");
}
fn events(store: &RuntimeStore) -> Vec<(String, String)> {
    let db = store.read().expect("events read");
    db.prepare("SELECT id,status FROM runtime_events ORDER BY id")
        .expect("query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("events")
}

struct CapturingMonitor<'a> {
    store: &'a RuntimeStore,
    calls: RefCell<Vec<RuleCheck>>,
    unknown: bool,
}
impl RuleMonitor for CapturingMonitor<'_> {
    fn available(&self) -> bool {
        true
    }
    fn launch(&self, check: &RuleCheck) -> CheckSubmission {
        // Independent read sees the reservation only if its transaction committed.
        assert_eq!(rule(self.store)["inFlight"], true);
        self.calls.borrow_mut().push(check.clone());
        if self.unknown {
            CheckSubmission::Unknown
        } else {
            CheckSubmission::Accepted
        }
    }
    fn cancel(&self, _monitor_id: &str) -> Result<(), RulesTickError> {
        Ok(())
    }
}
struct EditingFiles<'a> {
    store: &'a RuntimeStore,
    changes: Value,
}
impl RuleFiles for EditingFiles<'_> {
    fn fingerprint(&self, _path: &str) -> Result<Option<(i64, u64, u64)>, RulesTickError> {
        change(self.store, self.changes.clone());
        Ok(Some((200, 2, 3)))
    }
}

#[test]
fn tick_commit_restart_and_callback_failpoint_matrix() {
    let template = Template::new();
    // Fail before and after each durable table write in the no-command path.
    // Rollback must include canonical views, queue owner and wake; a restart retries
    // a rolled-back tick but never a committed one.
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let tables: &[&str] = if full {
        &[
            "sync_entities",
            "runtime_rules",
            "runtime_events",
            "runtime_agents",
            "runtime_event_meta",
        ]
    } else {
        &["runtime_events"]
    };
    let timings: &[&str] = if full {
        &["BEFORE", "AFTER"]
    } else {
        &["BEFORE"]
    };
    for table in tables {
        for timing in timings {
            let state = template.clone_state();
            let store = RuntimeStore::open(Some(state.path())).expect("Python database opens");
            let initial = rule(&store);
            store.transaction(|tx| {
                tx.execute_batch(&format!("CREATE TRIGGER rules_fail_insert {timing} INSERT ON {table} BEGIN SELECT RAISE(ABORT,'rules-failpoint'); END; CREATE TRIGGER rules_fail_update {timing} UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'rules-failpoint'); END;"))?;
                Ok(())
            }).expect("arm write failure");
            let result = tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor);
            assert!(
                result.is_err(),
                "failpoint {timing}/{table} was not reached"
            );
            assert_eq!(rule(&store), initial, "rule rollback {timing}/{table}");
            assert!(
                events(&store).is_empty(),
                "no wake on rollback {timing}/{table}"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch(
                        "DROP TRIGGER rules_fail_insert; DROP TRIGGER rules_fail_update",
                    )?;
                    Ok(())
                })
                .expect("remove failpoint");
            drop(store);
            let store = RuntimeStore::open(Some(state.path())).expect("reopen after rollback");
            recover_batch(&store, 0, NOW, &UnavailableMonitor).expect("recover");
            tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor)
                .expect("retry uncommitted tick");
            assert_eq!(
                events(&store),
                [("rule-wake:rule:1".to_owned(), "pending".to_owned())]
            );
            tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor)
                .expect("retry committed tick");
            assert_eq!(events(&store).len(), 1);
        }
    }
    // Crash after reservation / native submission but before an acknowledgement.
    for unknown in if full { vec![false, true] } else { vec![true] } {
        let state = template.clone_state();
        let store = RuntimeStore::open(Some(state.path())).expect("open");
        change(&store, json!({"command":"printf fixture"}));
        let monitor = CapturingMonitor {
            store: &store,
            calls: RefCell::new(Vec::new()),
            unknown,
        };
        tick_batch(&store, 0, NOW, &LocalRuleFiles, &monitor).expect("reserve and submit");
        tick_batch(&store, 0, NOW + 60.0, &LocalRuleFiles, &monitor).expect("in-flight tick");
        assert_eq!(monitor.calls.borrow().len(), 1);
        let check = monitor.calls.borrow()[0].clone();
        drop(monitor);
        drop(store);
        let store = RuntimeStore::open(Some(state.path())).expect("restart");
        recover_batch(&store, 0, NOW + 120.0, &UnavailableMonitor)
            .expect("recovery holds unknown check");
        assert_eq!(rule(&store)["status"], "paused");
        assert_eq!(rule(&store)["restartCheck"]["monitorId"], check.monitor_id);
        let monitor = CapturingMonitor {
            store: &store,
            calls: RefCell::new(Vec::new()),
            unknown,
        };
        tick_batch(&store, 0, NOW + 180.0, &LocalRuleFiles, &monitor)
            .expect("held checks do not resend");
        assert!(monitor.calls.borrow().is_empty());
        assert!(
            finish(
                &store,
                &check,
                CheckResult {
                    code: Some(0),
                    error: None,
                    output: "recovered"
                },
                NOW + 180.0
            )
            .expect("exact durable receipt")
        );
        assert!(
            !finish(
                &store,
                &check,
                CheckResult {
                    code: Some(0),
                    error: None,
                    output: "duplicate"
                },
                NOW + 180.0
            )
            .expect("duplicate receipt")
        );
        assert_eq!(events(&store).len(), 1);
    }
}

#[test]
fn stale_file_sample_and_foreign_or_repeated_completions_write_nothing() {
    let template = Template::new();
    let state = template.clone_state();
    let store = RuntimeStore::open(Some(state.path())).expect("open Python state");
    change(
        &store,
        json!({"kind":"file","path":"/disposable/watch","fingerprint":[100,2,3]}),
    );
    let files = EditingFiles {
        store: &store,
        changes: json!({"name":"changed during stat"}),
    };
    let report = tick_batch(&store, 0, NOW, &files, &UnavailableMonitor)
        .expect("stale file sample discarded");
    assert_eq!(report.changed, 0);
    assert_eq!(rule(&store)["checks"], 0);
    assert!(events(&store).is_empty());
    change(
        &store,
        json!({"kind":"interval","command":"printf fixture"}),
    );
    let before = rule(&store);
    assert!(matches!(
        tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor),
        Err(RulesTickError::MonitorUnavailable)
    ));
    assert_eq!(rule(&store), before);
    let monitor = CapturingMonitor {
        store: &store,
        calls: RefCell::new(Vec::new()),
        unknown: false,
    };
    tick_batch(&store, 0, NOW, &LocalRuleFiles, &monitor).expect("launch");
    let check = monitor.calls.borrow()[0].clone();
    let mut wrong_checks = Vec::new();
    let mut wrong = check.clone();
    wrong.epoch += 1;
    wrong_checks.push(wrong);
    let mut wrong = check.clone();
    wrong.check_number += 1;
    wrong_checks.push(wrong);
    let mut wrong = check.clone();
    wrong.agent_id = "foreign".into();
    wrong_checks.push(wrong);
    let mut wrong = check.clone();
    wrong.account_key = Some("foreign".into());
    wrong_checks.push(wrong);
    let mut wrong = check.clone();
    wrong.thread_id = Some("foreign".into());
    wrong_checks.push(wrong);
    let mut wrong = check.clone();
    wrong.monitor_id = "foreign".into();
    wrong_checks.push(wrong);
    let mut wrong = check.clone();
    wrong.command = "other command".into();
    wrong_checks.push(wrong);
    let before = rule(&store);
    for wrong in wrong_checks {
        assert!(
            !finish(
                &store,
                &wrong,
                CheckResult {
                    code: Some(0),
                    error: None,
                    output: "forged"
                },
                NOW
            )
            .expect("foreign receipt")
        );
        assert_eq!(rule(&store), before);
        assert!(events(&store).is_empty());
    }
    // A caller's transaction rollback also rolls back the rule's finish and wake.
    let rolled_back: Result<(), RulesTickError> = store.transaction_with(|tx| {
        assert!(finish_in_transaction(
            tx,
            &check,
            CheckResult {
                code: Some(0),
                error: None,
                output: "ok"
            },
            NOW
        )?);
        Err(StoreError::SchemaMismatch("injected monitor commit failure".into()).into())
    });
    assert!(rolled_back.is_err());
    assert_eq!(rule(&store), before);
    assert!(events(&store).is_empty());
    assert!(
        finish(
            &store,
            &check,
            CheckResult {
                code: Some(0),
                error: None,
                output: "ok"
            },
            NOW
        )
        .expect("finish")
    );
    assert!(
        !finish(
            &store,
            &check,
            CheckResult {
                code: Some(0),
                error: None,
                output: "again"
            },
            NOW
        )
        .expect("duplicate")
    );
    assert_eq!(events(&store).len(), 1);
    change(
        &store,
        json!({"kind":"event","event":"worker_completed","command":"","status":"active"}),
    );
    let event = || RuleEvent {
        agent_id: "owner",
        kind: "child_result",
        text: "child finished",
        key: "event-1",
        now: NOW,
    };
    // Event delivery consumes its identity in the same transaction as its wake.
    let result = store
        .transaction_with::<_, RulesTickError>(|tx| event_in_transaction(tx, event(), 0, false));
    let (report, checks) = result.expect("event hook");
    assert_eq!(report.changed, 1);
    assert!(checks.is_empty());
    let count = events(&store).len();
    let (report, _) = store
        .transaction_with::<_, RulesTickError>(|tx| event_in_transaction(tx, event(), 0, false))
        .expect("replay");
    assert_eq!(report.changed, 0);
    assert_eq!(events(&store).len(), count);
}

#[test]
fn local_metadata_reports_size_timestamp_and_missing_file() {
    let root = tempfile::tempdir().expect("file root");
    let path = root.path().join("watch");
    std::fs::write(&path, "abc").expect("file write");
    let observed = LocalRuleFiles
        .fingerprint(path.to_str().expect("path"))
        .expect("file metadata")
        .expect("file exists");
    assert_eq!(observed.1, 3);
    assert!(observed.0 > 0);
    assert_eq!(
        LocalRuleFiles
            .fingerprint(root.path().join("missing").to_str().expect("path"))
            .expect("missing file"),
        None
    );
}

struct CrashBeforeLaunch;
impl RuleMonitor for CrashBeforeLaunch {
    fn available(&self) -> bool {
        true
    }
    fn launch(&self, _check: &RuleCheck) -> CheckSubmission {
        // The adapter committed the intent, but no monitor or transport has run.
        std::process::exit(71)
    }
    fn cancel(&self, _id: &str) -> Result<(), RulesTickError> {
        Ok(())
    }
}

struct DurableMonitor<'a> {
    store: &'a RuntimeStore,
    unknown: bool,
    deliveries: std::cell::Cell<usize>,
}
impl RuleMonitor for DurableMonitor<'_> {
    fn available(&self) -> bool {
        true
    }
    fn reconcile(&self, check: &RuleCheck) -> CheckReconciliation {
        if self.unknown {
            return CheckReconciliation::Unknown;
        }
        let row = studio_runtime_store::MonitorsRepository::get(
            &self.store.read().expect("read monitor"),
            &check.monitor_id,
        )
        .expect("monitor row");
        match row {
            None => CheckReconciliation::NotSubmitted,
            Some(row)
                if row.value()["check"] == serde_json::to_value(check).expect("check JSON") =>
            {
                CheckReconciliation::Submitted
            }
            Some(_) => CheckReconciliation::Unknown,
        }
    }
    fn launch(&self, check: &RuleCheck) -> CheckSubmission {
        assert!(!self.unknown, "unknown outcome must never resend");
        let record = json!({"id":check.monitor_id,"agent":check.agent_id,"ruleId":check.rule_id,"status":"running","check":check});
        let inserted = self
            .store
            .transaction(|tx| {
                Ok(tx.execute(
                    "INSERT OR IGNORE INTO runtime_monitors(id,record) VALUES(?1,?2)",
                    rusqlite::params![check.monitor_id, record.to_string()],
                )?)
            })
            .expect("reserve durable fake monitor");
        self.deliveries.set(self.deliveries.get() + inserted);
        assert_eq!(self.reconcile(check), CheckReconciliation::Submitted);
        CheckSubmission::Accepted
    }
    fn cancel(&self, _id: &str) -> Result<(), RulesTickError> {
        Ok(())
    }
}

#[test]
fn committed_launch_intent_survives_process_exit_before_monitor_launch() {
    const CASE: &str = "committed_launch_intent_survives_process_exit_before_monitor_launch";
    if let Some(path) = std::env::var_os("RULES_CRASH_BEFORE_LAUNCH") {
        let store =
            RuntimeStore::open(Some(std::path::Path::new(&path))).expect("child opens state");
        if rule(&store)["kind"] == "event" {
            let (_, checks) = store
                .transaction_with::<_, RulesTickError>(|tx| {
                    event_in_transaction(
                        tx,
                        RuleEvent {
                            agent_id: "owner",
                            kind: "child_result",
                            text: "child finished",
                            key: "event-crash",
                            now: NOW,
                        },
                        0,
                        true,
                    )
                })
                .expect("child event transaction");
            submit_check(&store, &checks[0], NOW, &CrashBeforeLaunch).expect("child event launch");
        } else {
            tick_batch(&store, 0, NOW, &LocalRuleFiles, &CrashBeforeLaunch).expect("child tick");
        }
        panic!("crash boundary was not reached");
    }
    let template = Template::new();
    for kind in ["interval", "event"] {
        for state_kind in [
            CheckReconciliation::NotSubmitted,
            CheckReconciliation::Submitted,
            CheckReconciliation::Unknown,
        ] {
            let state = template.clone_state();
            {
                let store = RuntimeStore::open(Some(state.path())).expect("seed");
                change(
                    &store,
                    json!({"kind":kind,"event":"worker_completed","command":"printf fixture"}),
                );
            }
            let child = Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", CASE, "--nocapture"])
                .env("RULES_CRASH_BEFORE_LAUNCH", state.path())
                .output()
                .expect("crash process");
            assert_eq!(
                child.status.code(),
                Some(71),
                "{}",
                String::from_utf8_lossy(&child.stderr)
            );
            let store = RuntimeStore::open(Some(state.path())).expect("reopen committed WAL");
            assert_eq!(rule(&store)["inFlight"], true);
            let check: RuleCheck = serde_json::from_value(rule(&store)["launchIntent"].clone())
                .expect("exact intent committed before launch");
            assert!(events(&store).is_empty());
            let monitor = DurableMonitor {
                store: &store,
                unknown: state_kind == CheckReconciliation::Unknown,
                deliveries: std::cell::Cell::new(0),
            };
            if state_kind == CheckReconciliation::Submitted {
                monitor.launch(&check);
            }
            let report = recover_batch(&store, 0, NOW + 1.0, &monitor).expect("reconcile startup");
            assert_eq!(
                report.launched,
                usize::from(state_kind == CheckReconciliation::NotSubmitted)
            );
            if state_kind == CheckReconciliation::Unknown {
                assert_eq!(rule(&store)["status"], "paused");
                assert!(
                    rule(&store)["error"]
                        .as_str()
                        .expect("visible hold")
                        .contains("Outcome unknown")
                );
                assert_eq!(monitor.deliveries.get(), 0);
            } else {
                assert_eq!(rule(&store)["status"], "active");
                assert_eq!(monitor.deliveries.get(), 1);
                assert_eq!(
                    recover_batch(&store, 0, NOW + 2.0, &monitor)
                        .expect("repeat recovery")
                        .launched,
                    0
                );
                // Even a duplicate postcommit caller cannot send an already reserved identity twice.
                assert!(
                    submit_check(&store, &check, NOW + 2.0, &monitor)
                        .expect("idempotent monitor boundary")
                );
                assert_eq!(monitor.deliveries.get(), 1);
            }
            assert!(
                finish(
                    &store,
                    &check,
                    CheckResult {
                        code: Some(0),
                        error: None,
                        output: "done"
                    },
                    NOW + 3.0
                )
                .expect("exact completion")
            );
            assert!(rule(&store).get("launchIntent").is_none());
            assert_eq!(events(&store).len(), 1);
            assert_eq!(
                recover_batch(&store, 0, NOW + 4.0, &monitor)
                    .expect("completed intent absent")
                    .launched,
                0
            );
            assert!(
                !submit_check(&store, &check, NOW + 4.0, &monitor)
                    .expect("stale postcommit caller")
            );
        }
    }
}
