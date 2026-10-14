#[path = "support/prepared_python.rs"]
mod prepared_python;

use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use studio_runtime_services::{
    continuation::{ContinuationError, Snapshot},
    continuation_runtime::{
        self, ClaudeAccountPin, ClaudeAttemptIdentity, ClaudeAuthObservation, Completion,
        PreparationSettings, WakePort,
    },
    usage_resume::{self, LimitsObservation},
};
use studio_runtime_store::{ContinuationRepository, ContinuationTable, RuntimeStore, StoreError};

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

fn complete_continuation_rows(store: &RuntimeStore) -> Value {
    let db = store.read().expect("reader");
    let table_exists = |table: &str| -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .expect("table lookup")
    };
    let rows = |table: &str| -> Vec<Value> {
        if !table_exists(table) {
            return Vec::new();
        }
        let mut stmt = db
            .prepare(&format!("SELECT id,record FROM {table} ORDER BY id"))
            .expect("continuation rows");
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("rows")
        .map(|row| {
            let (id, record) = row.expect("record row");
            json!({"id":id,"record":serde_json::from_str::<Value>(&record).expect("record JSON")})
        })
        .collect()
    };
    let events = if table_exists("runtime_events") {
        let mut stmt = db
            .prepare("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id")
            .expect("event rows");
        stmt.query_map([], |row| {
            Ok(json!({
                "id":row.get::<_,String>(0)?,
                "agent":row.get::<_,String>(1)?,
                "kind":row.get::<_,String>(2)?,
                "text":row.get::<_,String>(3)?,
                "status":row.get::<_,String>(4)?,
                "created":row.get::<_,f64>(5)?,
                "epoch":row.get::<_,i64>(6)?,
                "turnId":row.get::<_,Option<String>>(7)?,
                "error":row.get::<_,Option<String>>(8)?,
            }))
        })
        .expect("events")
        .map(|row| row.expect("event row"))
        .collect()
    } else {
        Vec::new()
    };
    json!({
        "agents": rows("runtime_agents"),
        "capacity": rows("runtime_capacity_retries"),
        "events": events,
        "eventMetadata": rows("runtime_event_meta"),
        "usage": rows("runtime_usage_resumes"),
    })
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

#[derive(Default)]
struct FailingFirstWake(Mutex<Vec<String>>);

impl WakePort for FailingFirstWake {
    fn wake_account(&self, agent_id: &str, _: i64) -> Result<(), ContinuationError> {
        let mut attempts = self.0.lock().expect("wake attempts");
        attempts.push(agent_id.to_owned());
        if attempts.len() == 1 {
            Err(ContinuationError::Unavailable("test/account-wake"))
        } else {
            Ok(())
        }
    }

    fn wake_events(&self, agent_id: &str, _: i64) -> Result<(), ContinuationError> {
        self.0
            .lock()
            .expect("wake attempts")
            .push(format!("events:{agent_id}"));
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
    let rows_after_stage = complete_continuation_rows(&store);

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
        complete_continuation_rows(&store),
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
    let before = complete_continuation_rows(&store);
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
    assert_eq!(complete_continuation_rows(&store), before);
}

#[test]
fn limits_changed_refuses_foreign_account_without_changing_complete_rows() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let before = complete_continuation_rows(&store);
    let foreign = LimitsObservation::from_record(
        json!({
            "accountKey":"other","at":1000.0,
            "data":{"rateLimits":{"primary":{"usedPercent":100.0,"resetsAt":9000.0}}}
        }),
        Some("refresh-x".into()),
    )
    .expect("typed observation");
    let result = store.transaction_with(|tx| {
        let raw =
            ContinuationRepository::snapshot(tx, "a", ContinuationTable::Usage, Some("resume"))?;
        let snapshot = Snapshot::from_records(raw)?;
        usage_resume::limits_changed(&snapshot, &foreign, 1000.0)
    });
    assert!(matches!(
        result,
        Err(ContinuationError::Unavailable(
            "usage-policy/limits-account-mismatch"
        ))
    ));
    assert_eq!(complete_continuation_rows(&store), before);
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
    let before = complete_continuation_rows(&store);
    let staged = store.transaction_with(|tx| continuation_runtime::stage(tx, Completion::TerminalTurn {
        agent_id:"a", turn:&json!({"id":"turn-new","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}),
        known:true, now:1000.0, limits:None, claude_auth:None,
    })).expect("ineligible failure returns quietly");
    assert!(!staged.changed());
    assert_eq!(complete_continuation_rows(&store), before);
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
    let before = complete_continuation_rows(&store);
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
    assert_eq!(complete_continuation_rows(&store), before);
}

#[test]
fn auth_proof_cursors_survive_two_accounts_and_wake_errors_do_not_drop_later_agents() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    let (first, second) = store
        .transaction_with(|tx| -> std::result::Result<_, ContinuationError> {
            let base: String = tx
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id='a'",
                    [],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)?;
            let base_agent: Value = serde_json::from_str(&base).map_err(StoreError::from)?;
            let base_resume: String = tx
                .query_row(
                    "SELECT record FROM runtime_usage_resumes WHERE id='resume'",
                    [],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)?;
            let base_resume: Value =
                serde_json::from_str(&base_resume).map_err(StoreError::from)?;
            for (prefix, account_key) in [("alpha", "default"), ("beta", "other")] {
                for index in 0..34 {
                    let agent_id = format!("{prefix}-agent-{index:02}");
                    let mut agent = base_agent.clone();
                    agent["id"] = json!(agent_id);
                    agent["rootId"] = json!(agent_id);
                    agent["accountKey"] = json!(account_key);
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        rusqlite::params![agent_id, agent.to_string()],
                    )
                    .map_err(StoreError::from)?;

                    let id = format!("{prefix}-wait-{index:02}");
                    let mut record = base_resume.clone();
                    record["id"] = json!(id);
                    record["accountKey"] = json!(account_key);
                    record["cause"] = json!("auth");
                    record["failedAt"] = json!(900.0);
                    record
                        .as_object_mut()
                        .ok_or_else(|| StoreError::InvalidStoredObject {
                            table: "runtime_usage_resumes",
                            id: id.clone(),
                        })?
                        .remove("proofAt");
                    tx.execute(
                        "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES(?1,?2,?3)",
                        rusqlite::params![id, agent_id, record.to_string()],
                    )
                    .map_err(StoreError::from)?;
                }
            }
            let first = continuation_runtime::stage(
                tx,
                Completion::TerminalTurn {
                    agent_id: "a",
                    turn: &json!({"id":"done-a","status":"completed"}),
                    known: true,
                    now: 1000.0,
                    limits: None,
                    claude_auth: None,
                },
            )?;
            let owner_raw: String = tx
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id='a'",
                    [],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)?;
            let mut owner: Value = serde_json::from_str(&owner_raw).map_err(StoreError::from)?;
            owner["accountKey"] = json!("other");
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='a'",
                [owner.to_string()],
            )
            .map_err(StoreError::from)?;
            let second = continuation_runtime::stage(
                tx,
                Completion::TerminalTurn {
                    agent_id: "a",
                    turn: &json!({"id":"done-b","status":"completed"}),
                    known: true,
                    now: 1001.0,
                    limits: None,
                    claude_auth: None,
                },
            )?;
            Ok((first, second))
        })
        .expect("two bounded completion fanouts");
    assert!(first.changed());
    assert!(second.changed());
    {
        let db = store.read().expect("reader");
        let alpha_cursor: Option<String> = db
            .query_row("SELECT json_extract(record,'$.authResumeProofPending.default.afterId') FROM runtime_agents WHERE id='a'", [], |row| row.get(0))
            .expect("alpha cursor");
        let beta_cursor: Option<String> = db
            .query_row("SELECT json_extract(record,'$.authResumeProofPending.other.afterId') FROM runtime_agents WHERE id='a'", [], |row| row.get(0))
            .expect("beta cursor");
        assert_eq!(alpha_cursor.as_deref(), Some("alpha-wait-31"));
        assert_eq!(beta_cursor.as_deref(), Some("beta-wait-31"));
    }
    let wake = FailingFirstWake::default();
    let wake_error =
        continuation_runtime::committed(&first, &wake).expect_err("all wake errors are aggregated");
    assert!(matches!(wake_error, ContinuationError::WakeFailures(ref errors) if errors.len() == 1));
    let attempts = wake.0.lock().expect("wake attempts");
    assert_eq!(
        attempts.len(),
        33,
        "one failed wake does not stop later wakes"
    );
    for index in 0..32 {
        assert!(attempts.contains(&format!("alpha-agent-{index:02}")));
    }
    drop(attempts);

    let rows_after_completions = complete_continuation_rows(&store);
    let before_ticks = all_usage_rows(&store);
    assert_eq!(
        before_ticks
            .iter()
            .filter(|r| r["proofAt"].as_f64().is_some())
            .count(),
        64
    );
    for owner in ["a", "a"] {
        let tick = store
            .transaction_with(|tx| continuation_runtime::stage_tick(tx, owner, 1002.0, None))
            .expect("tick resumes one account cursor");
        assert!(tick.changed());
    }
    let rows_after_ticks = all_usage_rows(&store);
    assert_eq!(rows_after_ticks.len(), 69, "existing row plus 68 waits");
    assert_eq!(
        rows_after_ticks
            .iter()
            .filter(|r| r["proofAt"].as_f64().is_some())
            .count(),
        68
    );
    {
        let db = store.read().expect("reader");
        let marker: Option<String> = db
            .query_row(
                "SELECT json_extract(record,'$.authResumeProofPending') FROM runtime_agents WHERE id='a'",
                [],
                |row| row.get(0),
            )
            .expect("cursor cleared");
        assert_eq!(marker, None);
    }
    assert_ne!(rows_after_completions, complete_continuation_rows(&store));
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
