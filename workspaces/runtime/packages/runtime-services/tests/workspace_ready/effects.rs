use super::*;
use std::io::Write;
use std::process::{Command, Stdio};
use studio_runtime_store::{AgentRecord, AgentRepository};

fn fixture_parent() -> PathBuf {
    let path = PathBuf::from("/var/tmp/cs-rust-workspace-ready");
    std::fs::create_dir_all(&path).expect("isolated fixture parent");
    path
}

struct Fixture {
    root: tempfile::TempDir,
    store: RuntimeStore,
    id: String,
    repo: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::with_python(false)
    }
    fn with_python(python: bool) -> Self {
        let root = tempfile::Builder::new()
            .prefix("cs-workspace-")
            .tempdir_in(fixture_parent())
            .expect("fixture root");
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).expect("repo directory");
        command(&repo, &["init", "-b", "main"]);
        command(&repo, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::create_dir(repo.join("project")).expect("subproject");
        std::fs::write(repo.join("project/file"), "base\n").expect("base file");
        command(&repo, &["add", "."]);
        command(
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
        let id = if python {
            let actor = oracle(
                root.path(),
                json!({"command":"seed","root":state,"cwd":repo.join("project")}),
            );
            actor["id"].as_str().expect("actor id").to_owned()
        } else {
            seed_schema(&state, &repo.join("project"));
            "workspace-fixture".to_owned()
        };
        let store = RuntimeStore::open(Some(&state)).expect("open Python state");
        if !python {
            store.transaction(|tx| { tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('workspace-input','workspace-fixture','user','fixture','pending',1,0)",[])?; Ok(()) }).expect("queued event");
        }
        Self {
            root,
            store,
            id,
            repo,
        }
    }
    fn context(&self) -> Context {
        observe(
            &self.store,
            snapshot(&self.store, &self.id).expect("snapshot"),
            &git(),
        )
        .expect("observe")
    }
    fn plan(&self) -> crate::workspace_ready::WorktreePlan {
        match self.context().decide() {
            WorkspaceDecision::Provision(p) => p,
            other => panic!("expected plan: {other:?}"),
        }
    }
    fn reserve(&self, plan: &ProvisionIdentity) {
        self.store
            .transaction(|tx| {
                WorkspaceReadyRepository::reserve(
                    tx,
                    &snapshot(&self.store, &self.id).expect("snapshot"),
                    plan,
                    now(),
                )
            })
            .expect("reserve");
    }
    fn actor(&self) -> Value {
        snapshot(&self.store, &self.id)
            .expect("snapshot")
            .agent
            .record
    }
    fn patch(&self, key: &str, value: Value) {
        self.store
            .transaction(|tx| {
                let mut a = CheckpointsRepository::agent(tx, &self.id)?
                    .expect("actor")
                    .record;
                a[key] = value;
                AgentRepository::put(tx, &AgentRecord::from_value(a)?)
            })
            .expect("patch actor");
    }
}
// Reuse the repository's committed Python schema rather than bootstrapping Runtime per case.
fn seed_schema(root: &Path, cwd: &Path) {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../runtime-store/tests/fixtures/live-state-schema.json"
    ))
    .expect("Python schema fixture");
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        let mut db = rusqlite::Connection::open(root.join(name)).expect("fixture database");
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .expect("durable fixture");
        let tx = db.transaction().expect("one fixture transaction");
        let mut objects = schema[name]["sqlite_master"]
            .as_array()
            .expect("schema objects")
            .iter()
            .collect::<Vec<_>>();
        objects.sort_by_key(|o| match o["type"].as_str() {
            Some("table") => 0,
            Some("index") => 1,
            Some("trigger") => 2,
            _ => 3,
        });
        for object in objects {
            if matches!(
                object["name"].as_str(),
                Some(
                    "runtime_search_next_config"
                        | "runtime_search_next_data"
                        | "runtime_search_next_docsize"
                        | "runtime_search_next_idx"
                )
            ) {
                continue;
            }
            if let Some(sql) = object["sql"].as_str() {
                tx.execute_batch(sql).expect("fixture DDL");
            }
        }
        if name == "canvas.sqlite3" {
            let actor = json!({"id":"workspace-fixture","rootId":"workspace-fixture","epoch":0,"status":"queued","cwd":cwd,"autoWake":true,"worktree":true,"worktreeReady":false,"workspaceMode":"worktree"});
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('workspace-fixture',?1)",
                [actor.to_string()],
            )
            .expect("actor seed");
            tx.execute("INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('agent','workspace-fixture',1,'fixture',?1,0)", [json!({"collection":"agent","id":"workspace-fixture","value":actor}).to_string()]).expect("agent projection");
        }
        tx.commit().expect("fixture commits");
    }
}

fn git() -> Git {
    Git {
        deadline: Instant::now() + GIT_TIMEOUT,
    }
}
fn command(cwd: &Path, args: &[&str]) -> String {
    assert!(
        cwd.starts_with("/var/tmp/"),
        "Git writes only in disposable repositories"
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("Git executes");
    assert!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 Git")
}
fn oracle(root: &Path, request: Value) -> Value {
    let python = std::env::var_os("CODEX_AGENTS_PYTHON")
        .expect("CODEX_AGENTS_PYTHON must name the prepared isolated interpreter");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/turn_lifecycle/workspace_ready_oracle.py");
    for name in ["home", "codex", "claude"] {
        std::fs::create_dir_all(root.join(name)).expect("isolated home");
    }
    let mut child = Command::new(python)
        .arg("-B")
        .arg(script)
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", root.join("codex"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("CODEX_BIN", "/bin/false")
        .env("STUDIO_CLAUDE_BIN", "/bin/false")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
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
        .expect("send oracle case");
    let output = child.wait_with_output().expect("oracle exits");
    assert!(
        output.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("oracle JSON")
}

#[test]
fn creates_subproject_checkpoint_and_preserves_queued_input_and_source_index() {
    let fixture = Fixture::new();
    let original = std::fs::read(fixture.repo.join(".git/index")).expect("source index");
    let result = prepare(&fixture.store, &fixture.id).expect("prepare");
    let WorkspaceDecision::Ready { cwd } = result else {
        panic!("{result:?}")
    };
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("checkout"),
        "base\n"
    );
    let actor = fixture.actor();
    assert_eq!(actor["worktreeReady"], true);
    assert!(actor.get("worktreePreparation").is_none());
    assert!(actor.get("workspaceReservationId").is_none());
    let checkpoint = CheckpointsRepository::checkpoint(
        &fixture.store.read().expect("read"),
        actor["checkpointHistoryHead"]
            .as_str()
            .expect("checkpoint id"),
    )
    .expect("checkpoint")
    .expect("checkpoint saved");
    assert_eq!(checkpoint.record["cwd"], json!(cwd));
    let event: String = fixture
        .store
        .read()
        .expect("read")
        .query_row(
            "SELECT status FROM runtime_events WHERE id='workspace-input'",
            [],
            |r| r.get(0),
        )
        .expect("pending event");
    assert_eq!(event, "pending");
    assert_eq!(
        std::fs::read(fixture.repo.join(".git/index")).expect("source index"),
        original
    );
    // Lost successful response is an ordinary readiness check, preserving user dirt and new HEAD.
    std::fs::write(cwd.join("file"), "user edits\n").expect("dirty file");
    assert_eq!(
        prepare(&fixture.store, &fixture.id).expect("replay"),
        WorkspaceDecision::Ready { cwd: cwd.clone() }
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("file"),
        "user edits\n"
    );
}

#[test]
fn crash_before_effect_reuses_identity_and_crash_after_effect_publishes_without_recreation() {
    for effect_finished in [false, true] {
        let fixture = Fixture::new();
        let plan = fixture.plan();
        fixture.reserve(&plan.identity);
        if effect_finished {
            git().create(&plan.identity).expect("effect before crash");
        }
        let decision = fixture.context().decide();
        assert!(
            matches!(decision, WorkspaceDecision::Provision(ref p) if p.identity == plan.identity && p.effect == if effect_finished {ProvisionEffect::Publish} else {ProvisionEffect::Create})
        );
        assert!(matches!(
            prepare(&fixture.store, &fixture.id).expect("recover"),
            WorkspaceDecision::Ready { .. }
        ));
        let count: i64 = fixture.store.read().expect("read").query_row("SELECT count(*) FROM runtime_workspace_operations WHERE json_extract(record,'$.kind')='provision'", [], |r|r.get(0)).expect("count");
        assert_eq!(count, 1);
    }
}

#[test]
fn partial_or_dirty_effect_is_never_adopted_reset_or_cleaned() {
    for scenario in [
        "dirty",
        "branch-only",
        "file",
        "wrong-head",
        "missing-file",
        "unregistered",
    ] {
        let fixture = Fixture::new();
        let plan = fixture.plan();
        fixture.reserve(&plan.identity);
        match scenario {
            "branch-only" => {
                command(&fixture.repo, &["branch", &plan.identity.branch]);
            }
            "file" | "unregistered" => {
                std::fs::create_dir_all(plan.identity.destination.parent().expect("parent"))
                    .expect("parents");
                if scenario == "file" {
                    std::fs::write(&plan.identity.destination, "keep").expect("file");
                } else {
                    std::fs::create_dir(&plan.identity.destination).expect("unregistered dir");
                }
            }
            _ => {
                git().create(&plan.identity).expect("create");
                if scenario == "missing-file" {
                    std::fs::remove_file(plan.identity.cwd.join("file")).expect("remove fixture");
                } else {
                    std::fs::write(plan.identity.cwd.join("file"), "user change").expect("dirty");
                    if scenario == "wrong-head" {
                        command(&plan.identity.cwd, &["add", "."]);
                        command(
                            &plan.identity.cwd,
                            &[
                                "-c",
                                "user.name=Fixture",
                                "-c",
                                "user.email=fixture@example.invalid",
                                "commit",
                                "-m",
                                "new head",
                            ],
                        );
                    }
                }
            }
        }
        assert!(
            matches!(
                prepare(&fixture.store, &fixture.id).expect("refusal"),
                WorkspaceDecision::Refuse(_)
            ),
            "{scenario}"
        );
        assert_eq!(fixture.actor()["worktreeReady"], false);
        if scenario == "dirty" {
            assert_eq!(
                std::fs::read_to_string(plan.identity.cwd.join("file")).expect("user file"),
                "user change"
            );
        }
    }
}

#[test]
fn late_completion_never_attaches_to_deleted_stopped_replaced_or_new_epoch_actor() {
    for (key, value) in [
        ("deletedAt", json!(10)),
        ("autoWake", json!(false)),
        ("epoch", json!(999)),
        ("cwd", json!("/var/tmp/changed")),
        ("workspaceReservationId", json!("replacement")),
        ("accountKey", json!("replacement")),
    ] {
        let fixture = Fixture::new();
        let plan = fixture.plan();
        fixture.reserve(&plan.identity);
        git().create(&plan.identity).expect("effect");
        fixture.patch(key, value.clone());
        assert!(
            !fixture
                .store
                .transaction(|tx| WorkspaceReadyRepository::publish(
                    tx,
                    &plan.identity,
                    Err("fixture"),
                    now()
                ))
                .expect("stale completion")
        );
        assert_eq!(fixture.actor()[key], value);
        assert_eq!(fixture.actor()["worktreeReady"], false);
        assert!(plan.identity.cwd.is_dir());
        assert_eq!(
            WorkspaceReadyRepository::journal(
                &fixture.store.read().expect("read"),
                &plan.identity.id
            )
            .expect("journal")
            .expect("journal row")["phase"],
            "recovery_required"
        );
    }
}

#[test]
fn deadline_after_durable_reservation_retains_evidence_and_checkpoint_failure_is_nonfatal() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let plan = fixture.plan();
    let expired = Git {
        deadline: Instant::now(),
    };
    let outcome =
        provision(&fixture.store, &context, plan.clone(), &expired).expect("deadline classified");
    assert!(matches!(outcome, WorkspaceDecision::Refuse(_)));
    assert!(!plan.identity.destination.exists());
    assert_eq!(
        WorkspaceReadyRepository::journal(&fixture.store.read().expect("read"), &plan.identity.id)
            .expect("journal")
            .expect("journal row")["phase"],
        "recovery_required"
    );
    git()
        .create(&plan.identity)
        .expect("later confirmed absent retry");
    assert!(
        fixture
            .store
            .transaction(|tx| WorkspaceReadyRepository::publish(
                tx,
                &plan.identity,
                Err("Checkpoint skipped: fixture failure"),
                now()
            ))
            .expect("publish without checkpoint")
    );
    assert_eq!(
        fixture.actor()["checkpointError"],
        "Checkpoint skipped: fixture failure"
    );
    assert_eq!(fixture.actor()["worktreeReady"], true);
}

#[test]
fn common_git_directory_serialization_defers_without_starting_another_effect() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let lock = repository_lock(&plan.identity.common_dir).expect("lock");
    let _guard = lock.lock().expect("held lock");
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("defer"),
        WorkspaceDecision::Defer { .. }
    ));
    assert!(!plan.identity.destination.exists());
    assert!(
        WorkspaceReadyRepository::journal(&fixture.store.read().expect("read"), &plan.identity.id)
            .expect("journal")
            .is_none()
    );
    let other = repository_lock(&fixture.root.path().join("other-repo")).expect("independent key");
    assert!(other.try_lock().is_ok());
}

#[test]
fn workspace_gate_matches_guarded_python_witnesses() {
    gate_corpus(false);
}

#[test]
#[ignore = "full runtime corpus: just test-rust-runtime-corpora"]
fn workspace_gate_full_python_corpus() {
    gate_corpus(true);
}

#[test]
#[ignore = "full runtime corpus: real Python writer bootstrap"]
fn workspace_reads_real_python_written_state() {
    let fixture = Fixture::with_python(true);
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("Python-written state provisioning"),
        WorkspaceDecision::Ready { .. }
    ));
}

fn gate_corpus(full: bool) {
    let root = tempfile::Builder::new()
        .prefix("workspace-oracle-")
        .tempdir_in(fixture_parent())
        .expect("temp");
    let mut cases: Value =
        serde_json::from_str(include_str!("../turn_lifecycle/workspace_ready_cases.json"))
            .expect("corpus");
    if !full {
        let all = cases.as_array().expect("cases");
        cases = json!([all[0], all[14], all[21], all[42], all[49], all[56]]);
    }
    for case in cases.as_array_mut().expect("cases") {
        for a in case["agents"].as_array_mut().expect("agents") {
            a["cwd"] = json!(root.path().join(a["cwd"].as_str().expect("relative")));
        }
        case["actor"]["cwd"] = json!(
            root.path()
                .join(case["actor"]["cwd"].as_str().expect("relative"))
        );
    }
    let results = oracle(root.path(), json!({"command":"blockers","cases":cases}));
    for (case, python) in cases
        .as_array()
        .expect("cases")
        .iter()
        .zip(results.as_array().expect("results"))
    {
        let record = json!({"id":"target","epoch":0,"cwd":case["actor"]["cwd"],"autoWake":true});
        let actor: WorkspaceActor = serde_json::from_value(record.clone()).expect("actor");
        let cwd = PathBuf::from(case["actor"]["cwd"].as_str().expect("cwd"));
        let reservations = case["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .filter_map(|a| {
                let operation = a.get("workspaceOperation")?.as_str()?;
                let id = a.get("workspaceReservationId").and_then(Value::as_str);
                let op = case["operations"]
                    .as_array()
                    .expect("ops")
                    .iter()
                    .find(|o| o["id"].as_str() == id);
                Some(Reservation {
                    agent: a["id"].as_str().expect("id").into(),
                    cwd: PathBuf::from(a["cwd"].as_str().expect("cwd")),
                    operation: operation.into(),
                    id: id.map(str::to_owned),
                    phase: op.and_then(|o| o["phase"].as_str()).map(str::to_owned),
                })
            })
            .collect::<Vec<_>>();
        let decision = decide_workspace(
            &actor,
            &record,
            Some(&cwd),
            &reservations,
            None,
            false,
            &GitObservation::default(),
        );
        assert_eq!(
            matches!(decision, WorkspaceDecision::Ready { .. }),
            python.as_array().expect("blockers").is_empty(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn requested_base_is_pinned_and_unsupported_or_restore_refusals_do_not_write() {
    let fixture = Fixture::new();
    let base = command(&fixture.repo, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    std::fs::write(fixture.repo.join("project/file"), "later commit").expect("later content");
    command(&fixture.repo, &["add", "."]);
    command(
        &fixture.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "later",
        ],
    );
    fixture.patch("workerBaseCommit", json!(base));
    for (key, value) in [
        ("workspaceMode", json!("image")),
        ("executionMode", json!("vm")),
        ("environment", json!("linux")),
    ] {
        let original = fixture.actor().get(key).cloned().unwrap_or(Value::Null);
        fixture.patch(key, value);
        let before = snapshot(&fixture.store, &fixture.id).expect("before");
        assert!(matches!(
            prepare(&fixture.store, &fixture.id).expect("unsupported"),
            WorkspaceDecision::Refuse(_)
        ));
        assert_eq!(
            snapshot(&fixture.store, &fixture.id).expect("after"),
            before
        );
        fixture.patch(key, original);
    }
    fixture.store.transaction(|tx| {tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('restore:lost-marker',?1)", [json!({"id":"restore:lost-marker","kind":"restore","agent":fixture.id,"cwd":fixture.actor()["cwd"],"phase":"recovery_required"}).to_string()])?;Ok(())}).expect("orphan restore");
    let before = snapshot(&fixture.store, &fixture.id).expect("before");
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("restore hold"),
        WorkspaceDecision::Refuse(_)
    ));
    assert_eq!(
        snapshot(&fixture.store, &fixture.id).expect("after"),
        before
    );
    fixture
        .store
        .transaction(|tx| {
            tx.execute(
                "DELETE FROM runtime_workspace_operations WHERE id='restore:lost-marker'",
                [],
            )?;
            Ok(())
        })
        .expect("fixture resolves restore");
    let WorkspaceDecision::Ready { cwd } =
        prepare(&fixture.store, &fixture.id).expect("pinned provision")
    else {
        panic!("not ready")
    };
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("pinned checkout"),
        "base\n"
    );
}

#[test]
fn physical_actor_deletion_and_changed_journal_payload_cannot_publish() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.reserve(&plan.identity);
    let mut different = plan.identity.clone();
    different.base_commit = "f".repeat(40);
    assert!(
        !fixture
            .store
            .transaction(|tx| WorkspaceReadyRepository::reserve(
                tx,
                &snapshot(&fixture.store, &fixture.id).expect("snapshot"),
                &different,
                now()
            ))
            .expect("changed payload conflict")
    );
    git().create(&plan.identity).expect("effect");
    fixture
        .store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_agents WHERE id=?1", [&fixture.id])?;
            Ok(())
        })
        .expect("deleted actor");
    assert!(
        !fixture
            .store
            .transaction(|tx| WorkspaceReadyRepository::publish(
                tx,
                &plan.identity,
                Err("fixture"),
                now()
            ))
            .expect("orphan completion")
    );
    assert!(plan.identity.cwd.is_dir());
    assert_eq!(
        WorkspaceReadyRepository::journal(&fixture.store.read().expect("read"), &plan.identity.id)
            .expect("journal")
            .expect("row")["phase"],
        "recovery_required"
    );
}
