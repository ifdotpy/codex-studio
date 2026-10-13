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
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", fixture_parent().join("home"))
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
    let WorkspaceDecision::Ready { cwd, .. } = result else {
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
        WorkspaceDecision::Ready {
            cwd: cwd.clone(),
            token: ReadyToken::new(&fixture.actor(), cwd.clone())
        }
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
    compare_publication_with_python(&fixture);
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
    let WorkspaceDecision::Ready { cwd, .. } =
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

fn assert_recovery(decision: WorkspaceDecision) {
    assert!(
        matches!(
            decision,
            WorkspaceDecision::Refuse(
                crate::workspace_ready::WorkspaceRefusal::WorkspaceRecoveryRequired { .. }
            )
        ),
        "{decision:?}"
    );
}

#[test]
fn hidden_index_flags_and_sparse_checkout_never_prove_recovery_complete() {
    for flag in ["--skip-worktree", "--assume-unchanged", "sparse"] {
        let fixture = Fixture::new();
        let plan = fixture.plan().identity;
        fixture.reserve(&plan);
        git().create(&plan).expect("effect completed");
        if flag == "sparse" {
            command(
                &plan.destination,
                &["config", "core.sparseCheckout", "true"],
            );
        } else {
            command(&plan.destination, &["update-index", flag, "project/file"]);
            // Even unchanged flagged entries are ambiguous; never silently clear the flags.
        }
        assert_recovery(prepare(&fixture.store, &fixture.id).expect("recovery"));
        assert_eq!(fixture.actor()["worktreeReady"], false);
    }
}

fn reserve_other(fixture: &Fixture, cwd: &Path) {
    fixture.store.transaction(|tx| {
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('other-restore',?1)",
            [json!({"id":"other-restore","agent":"other","kind":"restore","phase":"local_mutation","cwd":cwd}).to_string()])?;
        Ok(())
    }).expect("other reservation");
}

#[test]
fn parent_and_destination_reservations_block_but_progress_does_not() {
    for destination in [false, true] {
        let fixture = Fixture::new();
        let plan = fixture.plan().identity;
        reserve_other(
            &fixture,
            if destination {
                &plan.destination
            } else {
                &fixture.repo
            },
        );
        assert_recovery(prepare(&fixture.store, &fixture.id).expect("overlap refused"));
        assert!(!plan.destination.exists());
    }
    let fixture = Fixture::new();
    let context = fixture.context();
    let plan = fixture.plan();
    fixture.patch("progress", json!("ordinary update"));
    assert!(matches!(
        provision(&fixture.store, &context, plan, &git()).expect("progress ignored"),
        WorkspaceDecision::Ready { .. }
    ));
}

#[test]
fn checkpoint_window_revalidates_git_and_reservations_and_recovers_exact_replay() {
    for change in ["dirty", "reservation", "none"] {
        let fixture = Fixture::new();
        let plan = fixture.plan().identity;
        fixture.reserve(&plan);
        git().create(&plan).expect("effect");
        let checkpoint = git().checkpoint(&plan).expect("checkpoint effect");
        match change {
            "dirty" => std::fs::write(plan.cwd.join("file"), "late edit\n").expect("user edit"),
            "reservation" => reserve_other(&fixture, &plan.destination),
            _ => (),
        }
        let decision =
            publish_verified(&fixture.store, &plan, Ok(&checkpoint), &git()).expect("publication");
        if change == "none" {
            assert!(matches!(decision, WorkspaceDecision::Ready { .. }));
        } else {
            assert_recovery(decision);
            assert_eq!(fixture.actor()["worktreeReady"], false);
        }
    }
    // Process died after update-ref, before publication: same reference is an exact replay.
    let fixture = Fixture::new();
    let plan = fixture.plan().identity;
    fixture.reserve(&plan);
    git().create(&plan).expect("effect");
    let checkpoint = git().checkpoint(&plan).expect("checkpoint effect");
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("crash replay"),
        WorkspaceDecision::Ready { .. }
    ));
    assert_eq!(fixture.actor()["checkpointHistoryHead"], checkpoint["id"]);
}

#[test]
fn ready_token_requires_current_generation_and_no_conflicting_reservation() {
    let fixture = Fixture::new();
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("create"),
        WorkspaceDecision::Ready { .. }
    ));
    let WorkspaceDecision::Ready { token, cwd } =
        prepare(&fixture.store, &fixture.id).expect("already ready")
    else {
        panic!("ready")
    };
    assert!(
        fixture
            .store
            .transaction(|tx| token.validate(tx))
            .expect("admit")
    );
    fixture.patch("progress", json!("does not invalidate"));
    assert!(
        fixture
            .store
            .transaction(|tx| token.validate(tx))
            .expect("admit")
    );
    fixture.patch("epoch", json!(1));
    assert!(
        !fixture
            .store
            .transaction(|tx| token.validate(tx))
            .expect("reject stale generation")
    );
    fixture.patch("epoch", json!(0));
    fixture.patch("threadId", json!("changed session"));
    assert!(
        !fixture
            .store
            .transaction(|tx| token.validate(tx))
            .expect("reject changed source")
    );
    fixture
        .store
        .transaction(|tx| {
            let mut a = fixture.actor();
            a.as_object_mut().expect("actor").remove("threadId");
            AgentRepository::put(tx, &AgentRecord::from_value(a)?)
        })
        .expect("restore source");
    reserve_other(&fixture, cwd.parent().expect("root"));
    assert!(
        !fixture
            .store
            .transaction(|tx| token.validate(tx))
            .expect("reject late reservation")
    );
}

#[test]
fn checkpoint_ref_collision_is_preserved_and_reported() {
    let fixture = Fixture::new();
    let plan = fixture.plan().identity;
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, plan.id.as_bytes());
    let reference = format!("refs/codex-agents/checkpoints/{}/{id}", fixture.id);
    // A distinct preexisting object must survive even though its tree matches the base.
    command(
        &fixture.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "different",
        ],
    );
    let other = command(&fixture.repo, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    command(&fixture.repo, &["update-ref", &reference, &other]);
    fixture.patch("workerBaseCommit", json!(plan.base_commit));
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("checkpoint failure nonfatal"),
        WorkspaceDecision::Ready { .. }
    ));
    assert_eq!(
        command(&fixture.repo, &["rev-parse", &reference]).trim(),
        other
    );
    assert!(
        fixture.actor()["checkpointError"]
            .as_str()
            .expect("reported conflict")
            .contains("Checkpoint reference already names a different commit")
    );
}

#[cfg(unix)]
#[test]
fn post_effect_symlink_escape_is_refused_without_cleanup() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let context = fixture.context();
    let plan = fixture.plan();
    let outside = fixture.root.path().join("outside");
    std::fs::create_dir(&outside).expect("outside destination");
    // Inject the exact interval after observation and before effect.
    symlink(&outside, fixture.repo.join(".worktrees")).expect("ancestor replaced");
    assert_recovery(provision(&fixture.store, &context, plan.clone(), &git()).expect("classified"));
    assert!(!confined(&plan.identity));
    assert_eq!(fixture.actor()["worktreeReady"], false);
    assert!(
        outside.join("codex-agents").join(&fixture.id).is_dir(),
        "effect remains inspectable"
    );
}

#[test]
fn linked_worktree_and_submodule_sources_preserve_git_identity() {
    let fixture = Fixture::new();
    let linked = fixture.root.path().join("linked");
    command(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            path_string(&linked).expect("path"),
        ],
    );
    fixture.patch("cwd", json!(linked.join("project")));
    let plan = fixture.plan().identity;
    assert_eq!(plan.common_dir, fixture.repo.join(".git"));
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("linked source"),
        WorkspaceDecision::Ready { .. }
    ));
    // A local gitlink fixture, without clone or any network/file transport.
    let fixture = Fixture::new();
    let oid = command(&fixture.repo, &["rev-parse", "HEAD"]);
    command(
        &fixture.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},submodule", oid.trim()),
        ],
    );
    command(
        &fixture.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "gitlink",
        ],
    );
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("gitlink checkout"),
        WorkspaceDecision::Ready { .. }
    ));
}

#[cfg(unix)]
#[test]
fn running_git_deadline_kills_child_and_keeps_durable_identity() {
    let fixture = Fixture::new();
    let plan = fixture.plan().identity;
    fixture.reserve(&plan);
    command(
        &fixture.repo,
        &[
            "config",
            "alias.workspace-stall",
            "!echo running > running-marker; exec sleep 30",
        ],
    );
    let started = Instant::now();
    let result = Git {
        deadline: started + Duration::from_millis(150),
    }
    .run(&fixture.repo, &["workspace-stall"]);
    assert!(matches!(result, Err(WorkspaceError::Deadline)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        fixture.repo.join("running-marker").exists(),
        "child actually started"
    );
    assert!(
        WorkspaceReadyRepository::journal(&fixture.store.read().expect("read"), &plan.id)
            .expect("journal")
            .is_some()
    );
    assert!(!plan.destination.exists());
}

#[test]
fn isolated_child_rejects_all_ambient_git_overrides() {
    let root = tempfile::Builder::new()
        .prefix("ambient-")
        .tempdir_in(fixture_parent())
        .expect("root");
    let index = root.path().join("foreign-index");
    std::fs::write(&index, "foreign index must not be read or changed").expect("foreign file");
    let output = Command::new(std::env::current_exe().expect("test exe"))
        .args([
            "--exact",
            "workspace_ready_git::tests::ambient_git_child",
            "--nocapture",
        ])
        .env("WORKSPACE_ENV_CHILD", "1")
        .env("GIT_INDEX_FILE", &index)
        .env("GIT_DIR", root.path().join("foreign-git"))
        .env("GIT_WORK_TREE", root.path())
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .output()
        .expect("isolated subprocess");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(index).expect("foreign file"),
        "foreign index must not be read or changed"
    );
}

#[test]
fn ambient_git_child() {
    if std::env::var_os("WORKSPACE_ENV_CHILD").is_none() {
        return;
    }
    let fixture = Fixture::new();
    let WorkspaceDecision::Ready { cwd, .. } =
        prepare(&fixture.store, &fixture.id).expect("isolated prepare")
    else {
        panic!("not ready")
    };
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("own checkout"),
        "base\n"
    );
    let index = command(
        &cwd,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    assert!(Path::new(index.trim()).is_file());
}

#[test]
fn publication_and_checkpoint_history_match_python() {
    compare_publication_with_python(&Fixture::new());
}

fn compare_publication_with_python(fixture: &Fixture) {
    let items = json!([{"id":"old"}, {"id":"new"}, {"id":"hidden","afterRestore":true}]);
    let parent = json!({"id":"parent-checkpoint","agent":fixture.id,"historyBoundary":1});
    fixture.store.transaction(|tx| {
        // Explicit rowids make the parent boundary and excluded item deterministic.
        for (i, item) in items.as_array().expect("items").iter().enumerate() {
            tx.execute("INSERT INTO runtime_items(rowid,id,agent,record,created) VALUES(?1,?2,?3,?4,1)",
                rusqlite::params![i as i64 + 1, item["id"].as_str(), fixture.id, item.to_string()])?;
        }
        tx.execute("INSERT INTO runtime_checkpoints(id,record) VALUES('parent-checkpoint',?1)", [parent.to_string()])?;
        Ok(())
    }).expect("history fixture");
    fixture.patch("checkpointHistoryHead", json!("parent-checkpoint"));
    fixture.patch("lastCompletedTurn", json!("previous-turn"));
    fixture.patch("threadId", json!("previous-thread"));
    let before = fixture.actor();
    let plan = fixture.plan().identity;
    let tree = command(&fixture.repo, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_owned();
    let python = oracle(
        fixture.root.path(),
        json!({"command":"publication", "repo":fixture.repo,
        "actor":before, "items":items,"parent":parent,"tree":tree,"commit":plan.base_commit}),
    );
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("publish"),
        WorkspaceDecision::Ready { .. }
    ));
    let actor = fixture.actor();
    for field in [
        "cwd",
        "branch",
        "worktreeReady",
        "worktreePreparation",
        "threadId",
        "lastCompletedTurn",
    ] {
        assert_eq!(actor[field], python["actor"][field], "published {field}");
    }
    let checkpoint = CheckpointsRepository::checkpoint(
        &fixture.store.read().expect("read"),
        actor["checkpointHistoryHead"].as_str().expect("id"),
    )
    .expect("checkpoint")
    .expect("saved")
    .record;
    for field in [
        "agent",
        "rootId",
        "label",
        "tree",
        "commit",
        "threadId",
        "turnId",
        "cwd",
        "historyParent",
        "historyDelta",
        "historyBoundary",
    ] {
        assert_eq!(
            checkpoint[field], python["checkpoint"][field],
            "checkpoint {field}"
        );
    }
    assert_eq!(checkpoint["historyDelta"], json!(["new"]));
    assert_eq!(checkpoint["historyBoundary"], 2);
    assert_eq!(
        command(
            &plan.destination,
            &["rev-parse", checkpoint["ref"].as_str().expect("ref")]
        )
        .trim(),
        checkpoint["commit"].as_str().expect("commit")
    );
}

#[test]
fn stale_reservation_precondition_never_starts_git_effect() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let plan = fixture.plan();
    fixture.patch("epoch", json!(1));
    assert!(matches!(
        provision(&fixture.store, &context, plan.clone(), &git()).expect("defer"),
        WorkspaceDecision::Defer { .. }
    ));
    assert!(!plan.identity.destination.exists());
    assert!(
        WorkspaceReadyRepository::journal(&fixture.store.read().expect("read"), &plan.identity.id)
            .expect("journal")
            .is_none()
    );
}

#[test]
fn absorbed_submodule_source_provisions_without_network() {
    let mut fixture = Fixture::new();
    let parent = fixture.root.path().join("superproject");
    std::fs::create_dir(&parent).expect("parent");
    command(&parent, &["init", "-b", "main"]);
    command(&parent, &["config", "core.hooksPath", "/dev/null"]);
    let submodule = parent.join("submodule");
    std::fs::rename(&fixture.repo, &submodule).expect("move local repository");
    fixture.repo = submodule.clone();
    std::fs::write(
        parent.join(".gitmodules"),
        "[submodule \"submodule\"]\n\tpath = submodule\n\turl = ./unused-local-source\n",
    )
    .expect("submodule metadata");
    command(&parent, &["add", ".gitmodules", "submodule"]);
    command(
        &parent,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "local submodule",
        ],
    );
    command(&parent, &["submodule", "absorbgitdirs", "submodule"]);
    fixture.patch("cwd", json!(submodule.join("project")));
    let plan = fixture.plan().identity;
    assert_eq!(plan.common_dir, parent.join(".git/modules/submodule"));
    assert!(matches!(
        prepare(&fixture.store, &fixture.id).expect("submodule source"),
        WorkspaceDecision::Ready { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(plan.cwd.join("file")).expect("checkout"),
        "base\n"
    );
}

#[test]
fn repository_clean_smudge_filter_is_preserved() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.repo.join(".gitattributes"),
        "project/file filter=fixture\n",
    )
    .expect("attributes");
    command(
        &fixture.repo,
        &["config", "filter.fixture.smudge", "sed s/base/filtered/"],
    );
    command(
        &fixture.repo,
        &["config", "filter.fixture.clean", "sed s/filtered/base/"],
    );
    command(&fixture.repo, &["add", ".gitattributes"]);
    command(
        &fixture.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "filter",
        ],
    );
    let WorkspaceDecision::Ready { cwd, .. } =
        prepare(&fixture.store, &fixture.id).expect("filtered creation")
    else {
        panic!("not ready")
    };
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("smudged file"),
        "filtered\n"
    );
    assert!(command(&cwd, &["status", "--porcelain=v1"]).is_empty());
}
