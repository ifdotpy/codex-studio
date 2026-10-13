#[path = "support/prepared_python.rs"]
mod prepared_python;

use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use studio_runtime_services::{
    continuation::ContinuationError,
    continuation_runtime::{self, Completion, WakePort},
    usage_resume::LimitsObservation,
};
use studio_runtime_store::{RuntimeStore, StoreError};

fn template() -> tempfile::TempDir {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = tempfile::tempdir().expect("isolated Python state");
    let python = prepared_python::prepared_python(&root.join("../../apps/server/src"));
    let output = Command::new(python)
        .arg(root.join("tests/turn_lifecycle/continuation_oracle.py"))
        .arg("template")
        .arg(dir.path())
        .arg("usage")
        .output()
        .expect("Python continuation writer");
    assert!(
        output.status.success(),
        "Python writer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dir
}

fn clone_template(source: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("per-case state");
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(source.join(name), dir.path().join(name)).expect("clone Python state");
    }
    dir
}

fn limits() -> LimitsObservation {
    LimitsObservation::from_record(
        json!({
            "accountKey":"default",
            "at":1000.0,
            "data":{"rateLimits":{"primary":{"usedPercent":100.0,"resetsAt":1300.0}}}
        }),
        None,
    )
    .expect("limits observation")
}

fn usage_rows(store: &RuntimeStore) -> (i64, Option<String>) {
    let db = store.read().expect("reader");
    let count = db
        .query_row("SELECT count(*) FROM runtime_usage_resumes", [], |row| {
            row.get(0)
        })
        .expect("resume count");
    let status = db
        .query_row(
            "SELECT json_extract(record,'$.status') FROM runtime_usage_resumes ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();
    (count, status)
}

struct WakeCounter(AtomicUsize, AtomicUsize);

impl WakePort for WakeCounter {
    fn wake_account(&self, _: &str, _: i64) -> Result<(), ContinuationError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn wake_events(&self, _: &str, _: i64) -> Result<(), ContinuationError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn terminal_failure_commits_one_resume_before_the_scheduler_wake() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("Python state opens");
    let observation = limits();
    let turn =
        json!({"id":"turn-new","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}});
    let completion = || Completion::TerminalTurn {
        agent_id: "a",
        turn: &turn,
        known: true,
        now: 1000.0,
        limits: Some(&observation),
        claude_auth: None,
    };

    let staged = store
        .transaction_with(|tx| continuation_runtime::stage(tx, completion()))
        .expect("stage completion transaction");
    assert!(staged.changed());
    assert_eq!(usage_rows(&store), (2, Some("scheduled".into())));

    let wake = WakeCounter(AtomicUsize::new(0), AtomicUsize::new(0));
    continuation_runtime::committed(&staged, &wake).expect("post-commit wake");
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    assert_eq!(wake.1.load(Ordering::SeqCst), 0);

    let replay = store
        .transaction_with(|tx| continuation_runtime::stage(tx, completion()))
        .expect("replayed completion");
    assert!(!replay.changed());
    continuation_runtime::committed(&replay, &wake).expect("no duplicate wake");
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    assert_eq!(wake.1.load(Ordering::SeqCst), 0);
    assert_eq!(usage_rows(&store), (2, Some("scheduled".into())));
}

#[test]
fn missing_limits_observation_aborts_without_a_partial_write() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("Python state opens");
    let before = usage_rows(&store);
    let error = store
        .transaction_with(|tx| {
            continuation_runtime::stage(
                tx,
                Completion::TerminalTurn {
                    agent_id: "a",
                    turn: &json!({"id":"turn-new","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}),
                    known: true,
                    now: 1000.0,
                    limits: None,
                    claude_auth: None,
                },
            )
        })
        .expect_err("missing observation is named");
    assert!(matches!(
        error,
        ContinuationError::Unavailable("usage-policy/limits-snapshot")
    ));
    assert_eq!(usage_rows(&store), before);
}

#[test]
fn unknown_or_malformed_completion_does_not_create_continuation_state() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("Python state opens");
    let before = usage_rows(&store);
    let unknown = store
        .transaction_with(|tx| {
            continuation_runtime::stage(
                tx,
                Completion::TerminalTurn {
                    agent_id: "a",
                    turn: &json!({"id":"turn-new","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}),
                    known: false,
                    now: 1000.0,
                    limits: None,
                    claude_auth: None,
                },
            )
        })
        .expect("unknown outcome does not continue");
    assert!(!unknown.changed());
    assert_eq!(usage_rows(&store), before);

    let missing_agent = store.transaction_with(|tx| {
        continuation_runtime::stage(
            tx,
            Completion::TerminalTurn {
                agent_id: "missing",
                turn: &Value::Null,
                known: true,
                now: 1000.0,
                limits: None,
                claude_auth: None,
            },
        )
    });
    assert!(matches!(
        missing_agent,
        Err(ContinuationError::Store(
            StoreError::InvalidStoredObject { .. }
        ))
    ));
}
