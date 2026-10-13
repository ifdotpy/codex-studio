#[path = "support/prepared_python.rs"]
mod prepared_python;

use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};
use studio_runtime_services::{
    continuation::ContinuationError,
    continuation_runtime::{
        self, ClaudeAccountPin, ClaudeAttemptIdentity, ClaudeAuthObservation, Completion,
        PreparationSettings, WakePort,
    },
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

fn all_usage_rows(store: &RuntimeStore) -> Vec<Value> {
    let db = store.read().expect("reader");
    let mut stmt = db
        .prepare("SELECT record FROM runtime_usage_resumes ORDER BY id")
        .expect("query");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("rows")
        .map(|row| serde_json::from_str(&row.expect("record")).expect("json record"))
        .collect()
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
    let rows_after_stage = all_usage_rows(&store);

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
    assert_eq!(
        all_usage_rows(&store),
        rows_after_stage,
        "replay preserves complete rows"
    );
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

#[test]
fn foreign_account_limits_refuse_before_any_completion_write() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let before = all_usage_rows(&store);
    let foreign = LimitsObservation::from_record(
        json!({
            "accountKey":"other","at":1000.0,
            "data":{"rateLimits":{"primary":{"usedPercent":100.0,"resetsAt":9000.0}}}
        }),
        Some("refresh-x".into()),
    )
    .expect("typed observation");
    let result = store.transaction_with(|tx| continuation_runtime::stage(tx, Completion::TerminalTurn {
        agent_id:"a", turn:&json!({"id":"turn-new","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}),
        known:true, now:1000.0, limits:Some(&foreign), claude_auth:None,
    }));
    assert!(matches!(
        result,
        Err(ContinuationError::Unavailable(
            "usage-policy/limits-account-mismatch"
        ))
    ));
    assert_eq!(all_usage_rows(&store), before);
}

#[test]
fn ineligible_terminal_failure_does_not_require_limits() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    store
        .transaction(|tx| {
            let mut agent: Value = serde_json::from_str(&tx.query_row(
                "SELECT record FROM runtime_agents WHERE id='a'",
                [],
                |r| r.get::<_, String>(0),
            )?)?;
            agent["autoWake"] = json!(false);
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='a'",
                [agent.to_string()],
            )?;
            Ok(())
        })
        .expect("disable auto wake");
    let before = all_usage_rows(&store);
    let staged = store.transaction_with(|tx| continuation_runtime::stage(tx, Completion::TerminalTurn {
        agent_id:"a", turn:&json!({"id":"turn-new","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}),
        known:true, now:1000.0, limits:None, claude_auth:None,
    })).expect("ineligible failure returns quietly");
    assert!(!staged.changed());
    assert_eq!(all_usage_rows(&store), before);
}

#[test]
fn stale_claude_timeout_identity_does_not_attach_to_current_attempt() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    store
        .transaction(|tx| {
            let mut agent: Value = serde_json::from_str(&tx.query_row(
                "SELECT record FROM runtime_agents WHERE id='a'",
                [],
                |r| r.get::<_, String>(0),
            )?)?;
            agent["startAttempt"] = json!({"id":"attempt-new","epoch":1,"submitted":false});
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='a'",
                [agent.to_string()],
            )?;
            Ok(())
        })
        .expect("new attempt");
    let pin = ClaudeAccountPin::from_value(
        json!({"id":"default","provider":"claude","home":"/tmp","accountId":"acct"}),
    )
    .expect("pin");
    let settings = PreparationSettings::from_agent(&json!({})).expect("settings");
    let observation = ClaudeAuthObservation::checked(pin, settings);
    let identity = ClaudeAttemptIdentity {
        attempt_id: "attempt-old".into(),
        epoch: 1,
    };
    let before = all_usage_rows(&store);
    let staged = store
        .transaction_with(|tx| {
            continuation_runtime::stage(
                tx,
                Completion::ClaudeAuthTimeout {
                    agent_id: "a",
                    now: 1000.0,
                    observation: Some(&observation),
                    attempt: Some(&identity),
                },
            )
        })
        .expect("stale timeout ignored");
    assert!(!staged.changed());
    assert_eq!(all_usage_rows(&store), before);
}

#[test]
fn auth_completion_proof_fanout_is_bounded_and_resumes_from_durable_tick_cursor() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let turn = json!({"id":"turn-complete","status":"completed"});
    let staged = store
        .transaction_with(|tx| {
            let base: String = tx
                .query_row(
                    "SELECT record FROM runtime_usage_resumes WHERE id='resume'",
                    [],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)?;
            let mut record: Value = serde_json::from_str(&base).map_err(StoreError::from)?;
            for index in 0..34 {
                let id = format!("auth-wait-{index:02}");
                record["id"] = json!(id);
                record["cause"] = json!("auth");
                record["failedAt"] = json!(900.0);
                record
                    .as_object_mut()
                    .expect("record object")
                    .remove("proofAt");
                tx.execute(
                    "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES(?1,'a',?2)",
                    rusqlite::params![id, record.to_string()],
                )
                .map_err(StoreError::from)?;
            }
            continuation_runtime::stage(
                tx,
                Completion::TerminalTurn {
                    agent_id: "a",
                    turn: &turn,
                    known: true,
                    now: 1000.0,
                    limits: None,
                    claude_auth: None,
                },
            )
        })
        .expect("bounded completion fanout");
    assert!(staged.changed());
    {
        let db = store.read().expect("reader");
        let proved: i64 = db
            .query_row("SELECT count(*) FROM runtime_usage_resumes WHERE json_extract(record,'$.cause')='auth' AND json_extract(record,'$.proofAt') IS NOT NULL", [], |row| row.get(0))
            .expect("proved count");
        assert_eq!(
            proved, 32,
            "completion transaction processes one bounded page"
        );
        let pending: String = db
            .query_row("SELECT json_extract(record,'$.authResumeProofPending.afterId') FROM runtime_agents WHERE id='a'", [], |row| row.get(0))
            .expect("durable cursor");
        assert_eq!(pending, "auth-wait-31");
    }
    let limits = limits();
    let resumed = store
        .transaction_with(|tx| continuation_runtime::stage_tick(tx, "a", 1001.0, Some(&limits)))
        .expect("tick resumes bounded proof page");
    assert!(resumed.changed());
    let db = store.read().expect("reader");
    let proved: i64 = db
        .query_row("SELECT count(*) FROM runtime_usage_resumes WHERE json_extract(record,'$.cause')='auth' AND json_extract(record,'$.proofAt') IS NOT NULL", [], |row| row.get(0))
        .expect("all proofs");
    assert_eq!(proved, 34);
    let marker: Option<String> = db
        .query_row("SELECT json_extract(record,'$.authResumeProofPending') FROM runtime_agents WHERE id='a'", [], |row| row.get(0))
        .expect("cursor cleared");
    assert_eq!(marker, None);
}

#[test]
fn production_classifier_matches_python_word_boundaries_and_bounded_evidence() {
    use studio_runtime_services::continuation_runtime::classify_usage_failure;
    let cases = vec![
        (json!({"message":"HTTP 4010 from service"}), "codex"),
        (json!({"message":"HTTP 401 from service"}), "codex"),
        (json!({"message":"403 invalid token"}), "codex"),
        (
            json!({"message":"403 invalid 012345678901234567890123456789012 token"}),
            "codex",
        ),
        (json!({"message":"403 invalid\ntoken"}), "codex"),
        (json!({"message":"please sign-in"}), "codex"),
        (json!({"message":"please signin"}), "codex"),
        (json!({"message":"please sign in"}), "codex"),
        (json!({"codexErrorInfo":"myAUTHFailure"}), "codex"),
        (json!({"message":"OAuth session expired"}), "claude"),
        (json!({"message":"OAuth session expired"}), "codex"),
        (
            json!({"codexErrorInfo":"usageLimitExceeded","message":"401"}),
            "codex",
        ),
        (json!({"codexErrorInfo":"rateLimitExceeded"}), "claude"),
    ];
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let python = prepared_python::prepared_python(&root.join("../../apps/server/src"));
    let script = root.join("tests/wave7_p12_classifier_oracle.py");
    let mut child = Command::new(python)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("isolated Python oracle starts");
    serde_json::to_writer(child.stdin.as_mut().expect("oracle stdin"), &cases)
        .expect("write cases");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("oracle completes");
    assert!(
        output.status.success(),
        "Python oracle: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<Option<String>> =
        serde_json::from_slice(&output.stdout).expect("oracle output");
    let actual: Vec<Option<String>> = cases
        .iter()
        .map(|(error, provider)| {
            classify_usage_failure(error, provider).map(|cause| match cause {
                studio_runtime_services::usage_resume::Cause::UsageLimit => "usage_limit".into(),
                studio_runtime_services::usage_resume::Cause::RateLimit => "rate_limit".into(),
                studio_runtime_services::usage_resume::Cause::Auth => "auth".into(),
            })
        })
        .collect();
    assert_eq!(actual, expected);
}
