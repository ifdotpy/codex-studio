use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use studio_runtime_services::rules_engine::{CheckResult, RuleCheck};
use studio_runtime_services::rules_tick::{
    CheckSubmission, LocalRuleFiles, RuleFiles, RuleMonitor, RulesTickError, UnavailableMonitor,
    finish, finish_in_transaction, recover_batch, tick_batch,
};
use studio_runtime_store::{RuleRecord, RulesRepository, RuntimeStore, StoreError};
use tempfile::TempDir;

const NOW: f64 = 1_800_000_000.0;

struct Template(TempDir);
impl Template {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("isolated template");
        let python = std::env::var_os("CODEX_AGENTS_PYTHON").expect("isolated Python required");
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
        let state = tempfile::tempdir().expect("isolated state");
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
    for table in [
        "sync_entities",
        "runtime_rules",
        "runtime_events",
        "runtime_agents",
        "runtime_event_meta",
    ] {
        for timing in ["BEFORE", "AFTER"] {
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
            recover_batch(&store, 0, NOW).expect("recover");
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
    for unknown in [false, true] {
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
        recover_batch(&store, 0, NOW + 120.0).expect("recovery holds unknown check");
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
}

#[test]
fn local_metadata_matches_real_python_fingerprint_shape() {
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
