#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

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
        let python = prepared_python_support::prepared_python(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
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
async fn change(store: &RuntimeStore, fields: Value) {
    store
        .transaction(move |tx| {
            let mut value = RulesRepository::get(tx, "rule")?
                .expect("rule exists")
                .value()
                .clone();
            for (field, value_new) in fields.as_object().expect("changes") {
                value[field] = value_new.clone();
            }
            RulesRepository::put(tx, &RuleRecord::from_value(value)?)
        })
        .await
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
    calls: Mutex<Vec<RuleCheck>>,
    unknown: bool,
}
impl RuleMonitor for CapturingMonitor<'_> {
    fn available(&self) -> bool {
        true
    }
    fn launch<'a>(
        &'a self,
        check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
        Box::pin(async move {
            // Independent read sees the reservation only if its transaction committed.
            assert_eq!(rule(self.store)["inFlight"], true);
            self.calls
                .lock()
                .expect("monitor calls")
                .push(check.clone());
            if self.unknown {
                CheckSubmission::Unknown
            } else {
                CheckSubmission::Accepted
            }
        })
    }
    fn cancel<'a>(
        &'a self,
        _monitor_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
        Box::pin(async { Ok(()) })
    }
}
struct EditingFiles<'a> {
    store: &'a RuntimeStore,
    changes: Value,
}
impl RuleFiles for EditingFiles<'_> {
    fn fingerprint<'a>(
        &'a self,
        _path: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<Option<(i64, u64, u64)>, RulesTickError>> {
        Box::pin(async move {
            change(self.store, self.changes.clone()).await;
            Ok(Some((200, 2, 3)))
        })
    }
}

#[tokio::test]
async fn tick_commit_restart_and_callback_failpoint_matrix() {
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
            store.transaction(move |tx| {
                tx.execute_batch(&format!("CREATE TRIGGER rules_fail_insert {timing} INSERT ON {table} BEGIN SELECT RAISE(ABORT,'rules-failpoint'); END; CREATE TRIGGER rules_fail_update {timing} UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'rules-failpoint'); END;"))?;
                Ok(())
            }).await.expect("arm write failure");
            let result = tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor).await;
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
                .transaction(move |tx| {
                    tx.execute_batch(
                        "DROP TRIGGER rules_fail_insert; DROP TRIGGER rules_fail_update",
                    )?;
                    Ok(())
                })
                .await
                .expect("remove failpoint");
            drop(store);
            let store = RuntimeStore::open(Some(state.path())).expect("reopen after rollback");
            recover_batch(&store, 0, NOW, &UnavailableMonitor)
                .await
                .expect("recover");
            tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor)
                .await
                .expect("retry uncommitted tick");
            assert_eq!(
                events(&store),
                [("rule-wake:rule:1".to_owned(), "pending".to_owned())]
            );
            tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor)
                .await
                .expect("retry committed tick");
            assert_eq!(events(&store).len(), 1);
        }
    }
    // Crash after reservation / native submission but before an acknowledgement.
    for unknown in if full { vec![false, true] } else { vec![true] } {
        let state = template.clone_state();
        let store = RuntimeStore::open(Some(state.path())).expect("open");
        change(&store, json!({"command":"printf fixture"})).await;
        let monitor = CapturingMonitor {
            store: &store,
            calls: Mutex::new(Vec::new()),
            unknown,
        };
        tick_batch(&store, 0, NOW, &LocalRuleFiles, &monitor)
            .await
            .expect("reserve and submit");
        tick_batch(&store, 0, NOW + 60.0, &LocalRuleFiles, &monitor)
            .await
            .expect("in-flight tick");
        assert_eq!(monitor.calls.lock().expect("monitor calls").len(), 1);
        let check = monitor.calls.lock().expect("monitor calls")[0].clone();
        drop(monitor);
        drop(store);
        let store = RuntimeStore::open(Some(state.path())).expect("restart");
        recover_batch(&store, 0, NOW + 120.0, &UnavailableMonitor)
            .await
            .expect("recovery holds unknown check");
        assert_eq!(rule(&store)["status"], "paused");
        assert_eq!(rule(&store)["restartCheck"]["monitorId"], check.monitor_id);
        let monitor = CapturingMonitor {
            store: &store,
            calls: Mutex::new(Vec::new()),
            unknown,
        };
        tick_batch(&store, 0, NOW + 180.0, &LocalRuleFiles, &monitor)
            .await
            .expect("held checks do not resend");
        assert!(monitor.calls.lock().expect("monitor calls").is_empty());
        assert!(
            finish(
                &store,
                &check,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: "recovered"
                },
                NOW + 180.0
            )
            .await
            .expect("exact durable receipt")
        );
        assert!(
            !finish(
                &store,
                &check,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: "duplicate"
                },
                NOW + 180.0
            )
            .await
            .expect("duplicate receipt")
        );
        assert_eq!(events(&store).len(), 1);
    }
}

#[tokio::test]
async fn stale_file_sample_and_foreign_or_repeated_completions_write_nothing() {
    let template = Template::new();
    let state = template.clone_state();
    let store = RuntimeStore::open(Some(state.path())).expect("open Python state");
    change(
        &store,
        json!({"kind":"file","path":"/disposable/watch","fingerprint":[100,2,3]}),
    )
    .await;
    let files = EditingFiles {
        store: &store,
        changes: json!({"name":"changed during stat"}),
    };
    let report = tick_batch(&store, 0, NOW, &files, &UnavailableMonitor)
        .await
        .expect("stale file sample discarded");
    assert_eq!(report.changed, 0);
    assert_eq!(rule(&store)["checks"], 0);
    assert!(events(&store).is_empty());
    change(
        &store,
        json!({"kind":"interval","command":"printf fixture"}),
    )
    .await;
    let before = rule(&store);
    assert!(matches!(
        tick_batch(&store, 0, NOW, &LocalRuleFiles, &UnavailableMonitor).await,
        Err(RulesTickError::MonitorUnavailable)
    ));
    assert_eq!(rule(&store), before);
    let monitor = CapturingMonitor {
        store: &store,
        calls: Mutex::new(Vec::new()),
        unknown: false,
    };
    tick_batch(&store, 0, NOW, &LocalRuleFiles, &monitor)
        .await
        .expect("launch");
    let check = monitor.calls.lock().expect("monitor calls")[0].clone();
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
                    cancelled: false,
                    output: "forged"
                },
                NOW
            )
            .await
            .expect("foreign receipt")
        );
        assert_eq!(rule(&store), before);
        assert!(events(&store).is_empty());
    }
    // A caller's transaction rollback also rolls back the rule's finish and wake.
    let check_for_rollback = check.clone();
    let rolled_back: Result<(), RulesTickError> = store
        .transaction_with(move |tx| {
            assert!(finish_in_transaction(
                tx,
                &check_for_rollback,
                CheckResult {
                    code: Some(0),
                    error: None,
                    cancelled: false,
                    output: "ok"
                },
                NOW
            )?);
            Err(StoreError::SchemaMismatch("injected monitor commit failure".into()).into())
        })
        .await;
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
                cancelled: false,
                output: "ok"
            },
            NOW
        )
        .await
        .expect("finish")
    );
    assert!(
        !finish(
            &store,
            &check,
            CheckResult {
                code: Some(0),
                error: None,
                cancelled: false,
                output: "again"
            },
            NOW
        )
        .await
        .expect("duplicate")
    );
    assert_eq!(events(&store).len(), 1);
    change(
        &store,
        json!({"kind":"event","event":"worker_completed","command":"","status":"active"}),
    )
    .await;
    let event = || RuleEvent {
        agent_id: "owner",
        kind: "child_result",
        text: "child finished",
        key: "event-1",
        now: NOW,
    };
    // Event delivery consumes its identity in the same transaction as its wake.
    let result = store
        .transaction_with::<_, RulesTickError>(move |tx| {
            event_in_transaction(tx, event(), 0, false)
        })
        .await;
    let (report, checks) = result.expect("event hook");
    assert_eq!(report.changed, 1);
    assert!(checks.is_empty());
    let count = events(&store).len();
    let (report, _) = store
        .transaction_with::<_, RulesTickError>(move |tx| {
            event_in_transaction(tx, event(), 0, false)
        })
        .await
        .expect("replay");
    assert_eq!(report.changed, 0);
    assert_eq!(events(&store).len(), count);
}

#[tokio::test]
async fn interval_check_uses_injected_clock_and_records_fake_runner_result() {
    let template = Template::new();
    let state = template.clone_state();
    let store = RuntimeStore::open(Some(state.path())).expect("open Python state");
    change(
        &store,
        json!({
            "kind":"interval",
            "status":"active",
            "inFlight":false,
            "checks":0,
            "intervalSeconds":60,
            "nextAt":NOW,
            "command":"printf fake-runner"
        }),
    )
    .await;
    let monitor = CapturingMonitor {
        store: &store,
        calls: Mutex::new(Vec::new()),
        unknown: false,
    };

    let report = tick_batch(&store, 0, NOW, &LocalRuleFiles, &monitor)
        .await
        .expect("schedule and submit the due check");
    assert_eq!(report.launched, 1);
    let check = monitor.calls.lock().expect("monitor calls")[0].clone();
    assert_eq!(check.command, "printf fake-runner");
    assert_eq!(check.check_number, 1);
    assert_eq!(rule(&store)["inFlight"], true);

    assert!(
        finish(
            &store,
            &check,
            CheckResult {
                code: Some(0),
                error: None,
                cancelled: false,
                output: "fake output",
            },
            NOW + 2.0,
        )
        .await
        .expect("persist fake runner result")
    );
    let completed = rule(&store);
    assert_eq!(completed["inFlight"], false);
    assert_eq!(completed["lastExitCode"], 0);
    assert_eq!(completed["lastOutput"], "fake output");
    assert_eq!(completed["lastFinished"], NOW + 2.0);
    assert_eq!(events(&store).len(), 1);
}

#[tokio::test]
async fn cancelled_paused_check_settles_its_durable_launch_intent() {
    let template = Template::new();
    let state = template.clone_state();
    let store = RuntimeStore::open(Some(state.path())).expect("open Python state");
    change(
        &store,
        json!({
            "kind":"interval",
            "status":"active",
            "inFlight":false,
            "checks":0,
            "intervalSeconds":60,
            "nextAt":NOW,
            "command":"printf fixture"
        }),
    )
    .await;
    let monitor = CapturingMonitor {
        store: &store,
        calls: Mutex::new(Vec::new()),
        unknown: false,
    };
    assert_eq!(
        tick_batch(&store, 0, NOW, &LocalRuleFiles, &monitor)
            .await
            .expect("reserve a rule check")
            .launched,
        1
    );

    // The monitor callback must consume its exact durable reservation even if the
    // current rule snapshot derives a different timeout from the persisted request.
    store
        .transaction(move |tx| {
            let mut value = RulesRepository::get(tx, "rule")?
                .expect("rule exists")
                .value()
                .clone();
            value["status"] = json!("paused");
            value["inFlight"] = json!(false);
            value["launchIntent"]["timeoutMs"] = json!(30_000);
            RulesRepository::put(tx, &RuleRecord::from_value(value)?)
        })
        .await
        .expect("pause with the pending intent retained");
    let intent = rule(&store)["launchIntent"].clone();
    let check: RuleCheck = serde_json::from_value(intent).expect("persisted monitor intent");
    assert!(
        finish(
            &store,
            &check,
            CheckResult {
                code: None,
                error: Some("Monitor cancelled"),
                cancelled: true,
                output: "",
            },
            NOW + 1.0,
        )
        .await
        .expect("settle the cancelled monitor")
    );
    let settled = rule(&store);
    assert_eq!(settled["status"], "paused");
    assert_eq!(settled["inFlight"], false);
    assert_eq!(settled["error"], "Monitor cancelled");
    assert!(settled.get("launchIntent").is_none());
}

#[tokio::test]
async fn local_metadata_reports_size_timestamp_and_missing_file() {
    let root = tempfile::tempdir().expect("file root");
    let path = root.path().join("watch");
    std::fs::write(&path, "abc").expect("file write");
    let observed = LocalRuleFiles
        .fingerprint(path.to_str().expect("path"))
        .await
        .expect("file metadata")
        .expect("file exists");
    assert_eq!(observed.1, 3);
    assert!(observed.0 > 0);
    assert_eq!(
        LocalRuleFiles
            .fingerprint(root.path().join("missing").to_str().expect("path"))
            .await
            .expect("missing file"),
        None
    );
}

struct CrashBeforeLaunch;
impl RuleMonitor for CrashBeforeLaunch {
    fn available(&self) -> bool {
        true
    }
    fn launch<'a>(
        &'a self,
        _check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
        Box::pin(async { std::process::exit(71) })
    }
    fn cancel<'a>(
        &'a self,
        _id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
        Box::pin(async { Ok(()) })
    }
}

struct DurableMonitor<'a> {
    store: &'a RuntimeStore,
    unknown: bool,
    deliveries: Mutex<usize>,
}
impl RuleMonitor for DurableMonitor<'_> {
    fn available(&self) -> bool {
        true
    }
    fn reconcile<'a>(
        &'a self,
        check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckReconciliation> {
        Box::pin(async move {
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
        })
    }
    fn launch<'a>(
        &'a self,
        check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
        Box::pin(async move {
            assert!(!self.unknown, "unknown outcome must never resend");
            let record = json!({"id":check.monitor_id,"agent":check.agent_id,"ruleId":check.rule_id,"status":"running","check":check});
            let monitor_id = check.monitor_id.clone();
            let inserted = self
                .store
                .transaction(move |tx| {
                    Ok(tx.execute(
                        "INSERT OR IGNORE INTO runtime_monitors(id,record) VALUES(?1,?2)",
                        rusqlite::params![monitor_id, record.to_string()],
                    )?)
                })
                .await
                .expect("reserve durable fake monitor");
            *self.deliveries.lock().expect("delivery count") += inserted;
            assert_eq!(self.reconcile(check).await, CheckReconciliation::Submitted);
            CheckSubmission::Accepted
        })
    }
    fn cancel<'a>(
        &'a self,
        _id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn committed_launch_intent_survives_process_exit_before_monitor_launch() {
    const CASE: &str = "committed_launch_intent_survives_process_exit_before_monitor_launch";
    if let Some(path) = std::env::var_os("RULES_CRASH_BEFORE_LAUNCH") {
        let store =
            RuntimeStore::open(Some(std::path::Path::new(&path))).expect("child opens state");
        if rule(&store)["kind"] == "event" {
            let (_, checks) = store
                .transaction_with::<_, RulesTickError>(move |tx| {
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
                .await
                .expect("child event transaction");
            submit_check(&store, &checks[0], NOW, &CrashBeforeLaunch)
                .await
                .expect("child event launch");
        } else {
            tick_batch(&store, 0, NOW, &LocalRuleFiles, &CrashBeforeLaunch)
                .await
                .expect("child tick");
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
                )
                .await;
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
                deliveries: Mutex::new(0),
            };
            if state_kind == CheckReconciliation::Submitted {
                monitor.launch(&check);
            }
            let report = recover_batch(&store, 0, NOW + 1.0, &monitor)
                .await
                .expect("reconcile startup");
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
                assert_eq!(*monitor.deliveries.lock().expect("delivery count"), 0);
            } else {
                assert_eq!(rule(&store)["status"], "active");
                assert_eq!(*monitor.deliveries.lock().expect("delivery count"), 1);
                assert_eq!(
                    recover_batch(&store, 0, NOW + 2.0, &monitor)
                        .await
                        .expect("repeat recovery")
                        .launched,
                    0
                );
                // Even a duplicate postcommit caller cannot send an already reserved identity twice.
                assert!(
                    submit_check(&store, &check, NOW + 2.0, &monitor)
                        .await
                        .expect("idempotent monitor boundary")
                );
                assert_eq!(*monitor.deliveries.lock().expect("delivery count"), 1);
            }
            assert!(
                finish(
                    &store,
                    &check,
                    CheckResult {
                        code: Some(0),
                        error: None,
                        cancelled: false,
                        output: "done"
                    },
                    NOW + 3.0
                )
                .await
                .expect("exact completion")
            );
            assert!(rule(&store).get("launchIntent").is_none());
            assert_eq!(events(&store).len(), 1);
            assert_eq!(
                recover_batch(&store, 0, NOW + 4.0, &monitor)
                    .await
                    .expect("completed intent absent")
                    .launched,
                0
            );
            assert!(
                !submit_check(&store, &check, NOW + 4.0, &monitor)
                    .await
                    .expect("stale postcommit caller")
            );
        }
    }
}
