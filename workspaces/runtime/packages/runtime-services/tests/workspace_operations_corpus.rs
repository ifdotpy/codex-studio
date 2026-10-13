use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use studio_runtime_services::workspace_operations::*;
use studio_runtime_services::workspace_operations_effects::*;
use studio_runtime_store::workspace_operations::WorkspaceOperationsRepository;
use studio_runtime_store::{
    AgentRecord, AgentRepository, CaptureKind, CheckpointsRepository, RuntimeStore,
};

#[path = "support/prepared_python.rs"]
mod prepared_python;

fn temporary() -> tempfile::TempDir {
    let parent = std::env::temp_dir().join("cs-rust-workspace-operations/fixtures");
    std::fs::create_dir_all(&parent).expect("fixture parent");
    tempfile::Builder::new()
        .prefix("operations-")
        .tempdir_in(parent)
        .expect("isolated root")
}
fn oracle(root: &Path, request: Value) -> Value {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/workspace_operations/oracle.py");
    for name in ["home", "codex", "claude"] {
        std::fs::create_dir_all(root.join(name)).expect("isolated home");
    }
    let mut child = Command::new("/usr/bin/timeout")
        .arg("120")
        .arg(&python)
        .arg("-B")
        .arg(script)
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", root.join("codex"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("PATH", "/usr/bin:/bin")
        .env_remove("CODEX_BIN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oracle starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("request");
    let out = child.wait_with_output().expect("oracle exit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("oracle JSON")
}
fn ticket() -> ProviderTicket {
    ProviderTicket {
        operation: "branch:request".into(),
        agent: "agent".into(),
        epoch: 1,
        account: "default".into(),
        thread: Some("thread".into()),
        connection: "connection".into(),
        generation: 1,
        request: "ticket".into(),
    }
}

#[test]
fn complete_python_outputs_match_checked_snapshots() {
    let root = temporary();
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("workspace_operations/cases.json")).expect("cases");
    let expected = oracle(root.path(), json!({"cases":cases}));
    for (case, output) in cases.iter().zip(expected.as_array().expect("results")) {
        let expected = &output["result"];
        let stored = &output["stored"];
        let result = match case["family"].as_str().expect("family") {
            "completion" => {
                let request =
                    CompletedTurn::from_record(case.get("request").unwrap_or(&case["actor"]))
                        .expect("request");
                // The constructor consumes the record read back by the real Python function harness.
                let snapshot = CompletionSnapshot::from_record(
                    &expected["actor"],
                    case.get("committed")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                )
                .expect("snapshot");
                json!({"actor":expected["actor"],"eligible":snapshot.eligible(&request)})
            }
            "history" => {
                let records = stored["checkpoints"].as_array().expect("checkpoints");
                let selected = records
                    .iter()
                    .find(|r| r["id"] == case["selected"])
                    .expect("selected");
                let parents = records
                    .iter()
                    .filter(|r| r["id"] != selected["id"])
                    .cloned()
                    .collect::<Vec<_>>();
                let fallback = stored["items"]
                    .as_array()
                    .expect("items")
                    .iter()
                    .map(|r| {
                        (
                            r["id"].as_str().expect("id").to_owned(),
                            r["created"].as_f64().expect("created"),
                        )
                    })
                    .collect();
                match CheckpointHistory::from_records(selected, &parents, fallback)
                    .and_then(|h| h.visible_ids())
                {
                    Ok(ids) => json!({"ids":ids}),
                    Err(e) => json!({"error":e.to_string()}),
                }
            }
            "recovery" => {
                if matches!(
                    case["operations"][0]["phase"].as_str(),
                    Some("completed" | "failed")
                ) {
                    continue;
                }
                let snapshot =
                    OperationSnapshot::from_records(&stored["operations"][0], &stored["actor"])
                        .expect("operation");
                let writes = snapshot.recover(10.0);
                json!({"operation":writes.operation,"agent":writes.agent})
            }
            "source" => CheckpointsRepository::workspace_source(&stored["actor"]),
            "provider" => {
                accept_provider_result(&ticket(), &ticket(), &case["response"]).expect("receipt")
            }
            _ => panic!("unknown family"),
        };
        assert_eq!(&result, expected, "case {}", case["name"]);
    }
}

#[test]
fn malformed_stored_fields_are_located_and_never_panic() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("workspace_operations/cases.json")).expect("cases");
    let base = &cases[0]["actor"];
    for (field, bad) in [
        ("epoch", json!(-1)),
        ("epoch", json!("1")),
        ("cwd", json!([])),
        ("inFlight", json!(1)),
        ("threadId", json!({})),
        ("startAttempt", json!(1)),
    ] {
        let mut a = base.clone();
        a[field] = bad;
        let error = CompletionSnapshot::from_record(&a, true)
            .expect_err("invalid")
            .to_string();
        assert!(error.contains(&format!("/{field}")), "{error}");
    }
    let bad = json!({"id":"c","agent":"agent","items":null});
    assert!(CheckpointHistory::from_records(&bad, &[], vec![]).is_err());
}

#[test]
fn receipt_identity_and_unknown_provider_outcomes_never_allow_redispatch() {
    let expected = ticket();
    for field in [
        "operation",
        "agent",
        "epoch",
        "account",
        "thread",
        "connection",
        "generation",
        "request",
    ] {
        let mut value = serde_json::to_value(&expected).expect("ticket");
        value[field] = if matches!(field, "epoch" | "generation") {
            json!(2)
        } else {
            json!("foreign")
        };
        let foreign = serde_json::from_value(value).expect("foreign ticket");
        assert!(
            accept_provider_result(&expected, &foreign, &json!({"thread":{"id":"new"}})).is_err(),
            "{field}"
        );
    }
    assert!(
        accept_provider_result(&expected, &expected, &json!({"thread":{"id":"thread"}})).is_err()
    );
    let actor = json!({"id":"agent","epoch":1,"cwd":"/workspace","status":"idle"});
    for kind in ["branch", "restore"] {
        for phase in ["provider_pending", "local_mutation", "recovery_required"] {
            let op = json!({"id":"operation","kind":kind,"agent":"agent","phase":phase,"signature":"signature","source":{"accountKey":"default","threadId":null,"cwd":"/workspace"}});
            let snapshot = OperationSnapshot::from_records(&op, &actor).expect("snapshot");
            assert_eq!(
                snapshot.retry("signature").expect("decision"),
                RetryDecision::InspectWorkspace
            );
            assert!(snapshot.retry("different").is_err());
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    store: RuntimeStore,
    actor: Value,
    repo: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = temporary();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).expect("repo");
        git(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("file"), "base\n").expect("file");
        git(&repo, &["add", "file"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "base",
            ],
        );
        let state = root.path().join("state");
        std::fs::create_dir(&state).expect("state");
        let actor = oracle(
            root.path(),
            json!({"command":"seed","root":state,"cwd":repo}),
        );
        let store = RuntimeStore::open(Some(&state)).expect("open Python-written template");
        Self {
            root,
            store,
            actor,
            repo,
        }
    }
    fn id(&self) -> &str {
        self.actor["id"].as_str().expect("id")
    }
    fn patch(&self, field: &str, value: Value) {
        self.store
            .transaction(|tx| {
                let mut a = CheckpointsRepository::agent(tx, self.id())?
                    .expect("agent")
                    .record;
                a[field] = value;
                AgentRepository::put(tx, &AgentRecord::from_value(a)?)
            })
            .expect("patch");
    }
    fn snapshot(&self) -> Value {
        let db = self.store.read().expect("read");
        let agent = CheckpointsRepository::agent(&db, self.id())
            .expect("agent")
            .expect("exists")
            .record;
        let count: i64 = db
            .query_row("SELECT count(*) FROM runtime_checkpoints", [], |r| r.get(0))
            .expect("count");
        let operations: i64 = db
            .query_row(
                "SELECT count(*) FROM runtime_workspace_operations",
                [],
                |r| r.get(0),
            )
            .expect("count");
        json!({"agent":agent,"checkpoints":count,"operations":operations})
    }
}
fn git(cwd: &Path, args: &[&str]) -> String {
    assert!(cwd.starts_with(std::env::temp_dir().join("cs-rust-workspace-operations")));
    let out = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", cwd)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
            "-C",
        ])
        .arg(cwd)
        .args(args)
        .output()
        .expect("Git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("Git UTF8")
}

#[test]
fn committed_turn_captures_once_preserving_user_index_and_files() {
    let f = Fixture::new();
    std::fs::write(f.repo.join("file"), "dirty\n").expect("dirty file");
    let index = std::fs::read(f.repo.join(".git/index")).expect("index");
    let request = CompletedTurn::from_record(&f.actor).expect("request");
    let checkpoint = checkpoint_after_turn(&f.store, &request)
        .expect("capture")
        .expect("checkpoint");
    assert_eq!(checkpoint["label"], "After turn");
    assert_eq!(checkpoint["turnId"], "turn");
    assert_eq!(
        std::fs::read(f.repo.join(".git/index")).expect("index"),
        index
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.join("file")).expect("file"),
        "dirty\n"
    );
    let state = f.snapshot();
    assert_eq!(
        checkpoint_after_turn(&f.store, &request).expect("replay"),
        Some(checkpoint)
    );
    assert_eq!(f.snapshot(), state);
    assert_eq!(state["agent"]["workspaceOperation"], Value::Null);
}

#[test]
fn late_completion_and_missing_receipt_write_nothing() {
    let f = Fixture::new();
    let request = CompletedTurn::from_record(&f.actor).expect("request");
    f.patch("epoch", json!(2));
    let before = f.snapshot();
    assert_eq!(
        checkpoint_after_turn(&f.store, &request).expect("stale"),
        None
    );
    assert_eq!(f.snapshot(), before);
    f.patch("epoch", json!(1));
    f.store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_completed_turns", [])?;
            Ok(())
        })
        .expect("remove receipt");
    let before = f.snapshot();
    assert_eq!(
        checkpoint_after_turn(&f.store, &request).expect("no receipt"),
        None
    );
    assert_eq!(f.snapshot(), before);
}

#[test]
fn failed_capture_and_restart_release_only_the_owned_reservation() {
    let f = Fixture::new();
    let snapshot = CheckpointsRepository::workspace_snapshot(&f.store.read().expect("db"), f.id())
        .expect("snapshot")
        .expect("agent");
    f.store
        .transaction(|tx| {
            assert!(CheckpointsRepository::reserve_checkpoint(
                tx,
                &snapshot,
                "checkpoint:interrupted",
                CaptureKind::Turn,
                Some("turn"),
                1.0
            )?);
            Ok(())
        })
        .expect("reserve");
    f.patch("workspaceReservationId", json!("checkpoint:replacement"));
    assert_eq!(recover_operations(&f.store, 2.0).expect("recovery"), 1);
    let state = f.snapshot();
    assert_eq!(
        state["agent"]["workspaceReservationId"],
        "checkpoint:replacement"
    );
    let op = WorkspaceOperationsRepository::operation(
        &f.store.read().expect("read"),
        "checkpoint:interrupted",
    )
    .expect("op")
    .expect("exists");
    assert_eq!(op["phase"], "failed");
}

#[test]
fn store_failpoint_matrix_rolls_back_or_recovers_without_duplicate_checkpoint() {
    let f = Fixture::new();
    let request = CompletedTurn::from_record(&f.actor).expect("request");
    // Every write transaction is forced to fail before its durable boundary.
    for (name, table, event, when) in [
        ("reservation", "runtime_workspace_operations", "INSERT", "1"),
        (
            "running",
            "runtime_workspace_operations",
            "UPDATE",
            "json_extract(NEW.record,'$.phase')='capture_running'",
        ),
        ("publication", "runtime_checkpoints", "INSERT", "1"),
    ] {
        let trigger = format!(
            "CREATE TRIGGER failpoint BEFORE {event} ON {table} WHEN {when} BEGIN SELECT RAISE(ABORT,'injected'); END"
        );
        f.store
            .transaction(|tx| {
                tx.execute_batch(&trigger)?;
                Ok(())
            })
            .expect("arm failpoint");
        assert!(checkpoint_after_turn(&f.store, &request).is_err(), "{name}");
        f.store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER failpoint")?;
                Ok(())
            })
            .expect("disarm");
        recover_operations(&f.store, 3.0).expect("restart recovery");
        assert_eq!(f.snapshot()["checkpoints"], 0, "{name}");
        assert!(
            f.snapshot()["agent"]["workspaceOperation"].is_null(),
            "{name}"
        );
        // Each case has its own completion identity and cannot retry a failed effect.
        f.store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_workspace_operations", [])?;
                Ok(())
            })
            .expect("next isolated failpoint");
    }
    assert!(
        checkpoint_after_turn(&f.store, &request)
            .expect("capture after faults")
            .is_some()
    );
    assert_eq!(f.snapshot()["checkpoints"], 1);
    assert!(f.root.path().exists());
}

#[test]
fn stale_source_at_publication_cannot_attach_checkpoint() {
    let f = Fixture::new();
    let snapshot = CheckpointsRepository::workspace_snapshot(&f.store.read().expect("db"), f.id())
        .expect("snapshot")
        .expect("agent");
    f.store
        .transaction(|tx| {
            assert!(CheckpointsRepository::reserve_checkpoint(
                tx,
                &snapshot,
                "checkpoint:stale",
                CaptureKind::Turn,
                Some("turn"),
                1.0
            )?);
            Ok(())
        })
        .expect("reserve");
    let reserved = CheckpointsRepository::workspace_snapshot(&f.store.read().expect("db"), f.id())
        .expect("snapshot")
        .expect("agent");
    f.store
        .transaction(|tx| {
            assert!(CheckpointsRepository::set_capture_running(
                tx,
                &reserved,
                "checkpoint:stale",
                2.0
            )?);
            Ok(())
        })
        .expect("start");
    f.patch("epoch", json!(2));
    f.store
        .transaction(|tx| {
            assert!(!CheckpointsRepository::complete_capture(
                tx,
                f.id(),
                "checkpoint:stale",
                None,
                &json!({"id":"stale","agent":f.id(),"historyParent":null}),
                3.0
            )?);
            Ok(())
        })
        .expect("stale publication refused");
    assert_eq!(f.snapshot()["checkpoints"], 0);
}

#[test]
fn restart_after_each_committed_capture_stage_never_repeats_git() {
    for running in [false, true] {
        let Fixture {
            root,
            store,
            actor,
            repo,
        } = Fixture::new();
        let request = CompletedTurn::from_record(&actor).expect("request");
        let id = format!("checkpoint:{}", request.checkpoint_id());
        let snapshot = CheckpointsRepository::workspace_snapshot(
            &store.read().expect("read"),
            request.agent_id(),
        )
        .expect("snapshot")
        .expect("agent");
        store
            .transaction(|tx| {
                assert!(CheckpointsRepository::reserve_checkpoint(
                    tx,
                    &snapshot,
                    &id,
                    CaptureKind::Turn,
                    request.turn_id(),
                    1.0
                )?);
                Ok(())
            })
            .expect("reserve");
        if running {
            let reserved = CheckpointsRepository::workspace_snapshot(
                &store.read().expect("read"),
                request.agent_id(),
            )
            .expect("snapshot")
            .expect("agent");
            store
                .transaction(|tx| {
                    assert!(CheckpointsRepository::set_capture_running(
                        tx, &reserved, &id, 2.0
                    )?);
                    Ok(())
                })
                .expect("running");
        }
        let index = std::fs::read(repo.join(".git/index")).expect("index");
        drop(store);
        let reopened = RuntimeStore::open(Some(&root.path().join("state"))).expect("restart");
        assert_eq!(
            checkpoint_after_turn(&reopened, &request).expect("recovered callback"),
            None
        );
        assert_eq!(
            std::fs::read(repo.join(".git/index")).expect("index"),
            index
        );
        let op = WorkspaceOperationsRepository::operation(&reopened.read().expect("read"), &id)
            .expect("operation")
            .expect("exists");
        assert_eq!(op["phase"], "failed");
    }
}

#[test]
fn durable_provider_ticket_rejects_foreign_and_stale_receipts_and_replays_exact_result() {
    let f = Fixture::new();
    let mut expected = ticket();
    expected.agent = f.id().into();
    let operation = json!({"id":expected.operation,"kind":"branch","agent":f.id(),"phase":"provider_pending",
        "signature":"signature","source":{"accountKey":"default","threadId":"thread","cwd":f.repo},"providerTicket":expected});
    f.store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)",
                rusqlite::params![expected.operation, operation.to_string()],
            )?;
            Ok(())
        })
        .expect("prepared ticket");
    let before = CheckpointsRepository::workspace_snapshot(&f.store.read().expect("read"), f.id())
        .expect("snapshot")
        .expect("actor");
    assert!(
        f.store
            .transaction(|tx| CheckpointsRepository::workspace_unchanged(tx, &before))
            .expect("unchanged")
    );
    f.patch("epoch", json!(2));
    assert!(
        !f.store
            .transaction(|tx| CheckpointsRepository::workspace_unchanged(tx, &before))
            .expect("changed")
    );
    f.patch("epoch", json!(1));
    let response = json!({"thread":{"id":"fork"},"approvalPolicy":"never"});
    let mut foreign = expected.clone();
    foreign.request = "other-request".into();
    assert!(record_provider_response(&f.store, &foreign, &response).is_err());
    assert_eq!(
        WorkspaceOperationsRepository::operation(
            &f.store.read().expect("read"),
            &expected.operation
        )
        .expect("read"),
        Some(operation.clone())
    );
    f.patch("epoch", json!(2));
    assert!(record_provider_response(&f.store, &expected, &response).is_err());
    assert_eq!(
        WorkspaceOperationsRepository::operation(
            &f.store.read().expect("read"),
            &expected.operation
        )
        .expect("read"),
        Some(operation)
    );
    f.patch("epoch", json!(1));
    assert_eq!(
        record_provider_response(&f.store, &expected, &response).expect("accept"),
        response
    );
    let accepted = WorkspaceOperationsRepository::operation(
        &f.store.read().expect("read"),
        &expected.operation,
    )
    .expect("read");
    assert_eq!(
        record_provider_response(&f.store, &expected, &response).expect("replay"),
        response
    );
    assert_eq!(
        WorkspaceOperationsRepository::operation(
            &f.store.read().expect("read"),
            &expected.operation
        )
        .expect("read"),
        accepted
    );
    assert_eq!(
        inspect_retry(&f.store, &expected.operation, "signature").expect("retry"),
        RetryDecision::ResumeLocal(response)
    );
}
