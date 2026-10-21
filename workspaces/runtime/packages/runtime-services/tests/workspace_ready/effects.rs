#[allow(clippy::duplicate_mod)]
#[path = "../support/prepared_python.rs"]
mod prepared_python_support;
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use studio_runtime_store::{AgentRecord, AgentRepository};

/// The reservation snapshot through an independent test reader.
fn snapshot(store: &RuntimeStore, id: &str) -> Result<WorkspaceSnapshot, WorkspaceError> {
    reservation_snapshot(&store.test_reader().expect("test reader"), id)
}

fn fixture_parent() -> PathBuf {
    let path = std::env::var_os("STUDIO_WORKSPACE_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&path).expect("isolated fixture parent");
    // Git and the workspace gate report resolved paths; the macOS temporary
    // folder is under the `/var` link.
    path_identity::resolve(&path).expect("resolved fixture parent")
}

struct Fixture {
    root: tempfile::TempDir,
    store: std::sync::Arc<RuntimeStore>,
    id: String,
    repo: PathBuf,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_python(false).await
    }
    async fn with_python(python: bool) -> Self {
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
                json!({"command":"seed","root":state,"cwd":repo.join("project"),"codexHome":root.path().join("codex")}),
            );
            actor["id"].as_str().expect("actor id").to_owned()
        } else {
            seed_schema(&state, &repo.join("project"));
            "workspace-fixture".to_owned()
        };
        let store = std::sync::Arc::new(
            RuntimeStore::open(Some(&state))
                .await
                .expect("open Python state"),
        );
        if !python {
            store.transaction(|tx| { tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('workspace-input','workspace-fixture','user','fixture','pending',1,0)",[])?; Ok(()) }).await.expect("queued event");
        }
        Self {
            root,
            store,
            id,
            repo,
        }
    }
    fn context(&self) -> Context {
        let snapshot = snapshot(&self.store, &self.id).expect("snapshot");
        let id = provision_id(&actor(&snapshot).expect("actor"));
        let journal =
            WorkspaceReadyRepository::journal(&self.store.test_reader().expect("test reader"), &id)
                .expect("journal");
        observe(snapshot, journal, &git()).expect("observe")
    }
    fn plan(&self) -> crate::workspace_ready::WorktreePlan {
        match self.context().decide() {
            WorkspaceDecision::Provision(p) => p,
            other => panic!("expected plan: {other:?}"),
        }
    }
    fn paths(
        &self,
        plan: &ProvisionIdentity,
    ) -> studio_runtime_store::workspace_ready::WorkspacePaths {
        let connection = self.store.test_reader().expect("reader");
        let snapshot = CheckpointsRepository::reservation_snapshot(&connection, &self.id)
            .expect("snapshot")
            .unwrap_or_else(|| WorkspaceSnapshot {
                agent: studio_runtime_store::StoredCheckpointObject {
                    id: self.id.clone(),
                    record: plan.source.clone(),
                },
                parent_checkpoint: None,
                agents: Vec::new(),
                operations: Vec::new(),
                monitors: Vec::new(),
                tasks: Vec::new(),
            });
        let paths =
            studio_runtime_store::AgentIndexedReads::admission_cwds(&connection).expect("paths");
        studio_runtime_store::workspace_ready::WorkspacePaths::capture(
            &snapshot,
            paths
                .into_iter()
                .flatten()
                .map(PathBuf::from)
                .chain([plan.destination.clone()]),
        )
    }
    async fn reserve(&self, plan: &ProvisionIdentity) {
        let store = std::sync::Arc::clone(&self.store);
        let snapshot = snapshot(&self.store, &self.id).expect("snapshot");
        let paths = self.paths(plan);
        let plan = plan.clone();
        store
            .transaction(move |tx| {
                WorkspaceReadyRepository::reserve(tx, &snapshot, &plan, &paths, now())
            })
            .await
            .expect("reserve");
    }
    fn actor(&self) -> Value {
        snapshot(&self.store, &self.id)
            .expect("snapshot")
            .agent
            .record
    }
    async fn patch(&self, key: &str, value: Value) {
        let store = std::sync::Arc::clone(&self.store);
        let id = self.id.clone();
        let key = key.to_owned();
        store
            .transaction(move |tx| {
                let mut a = CheckpointsRepository::agent(tx, &id)?
                    .expect("actor")
                    .record;
                a[key] = value;
                AgentRepository::put(tx, &AgentRecord::from_value(a)?)
            })
            .await
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
        cwd.starts_with(fixture_parent()),
        "Git writes only in disposable repositories"
    );
    let output = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", cwd.join(".fixture-home"))
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
struct OracleWorker {
    _root: tempfile::TempDir,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl OracleWorker {
    fn start() -> Self {
        let root = tempfile::Builder::new()
            .prefix("workspace-oracle-worker-")
            .tempdir_in(fixture_parent())
            .expect("oracle worker root");
        for name in ["home", "codex", "claude"] {
            std::fs::create_dir_all(root.path().join(name)).expect("isolated worker home");
        }
        let python = prepared_python_support::prepared_python(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/turn_lifecycle/workspace_ready_oracle.py");
        let mut child = Command::new(python)
            .arg("-B")
            .arg(script)
            .env("HOME", root.path().join("home"))
            .env("CODEX_HOME", root.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", root.path().join("claude"))
            .env("CODEX_BIN", "/bin/false")
            .env("STUDIO_CLAUDE_BIN", "/bin/false")
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("oracle worker starts");
        let stdin = child.stdin.take().expect("oracle worker stdin");
        let stdout = BufReader::new(child.stdout.take().expect("oracle worker stdout"));
        Self {
            _root: root,
            child,
            stdin,
            stdout,
        }
    }

    fn request(&mut self, request: Value) -> Value {
        writeln!(
            self.stdin,
            "{}",
            serde_json::to_string(&request).expect("serialize oracle request")
        )
        .expect("send oracle case");
        self.stdin.flush().expect("flush oracle case");
        let mut response = String::new();
        let bytes = self
            .stdout
            .read_line(&mut response)
            .expect("read oracle response");
        assert!(
            bytes > 0,
            "oracle worker closed stdout: {:?}",
            self.child.try_wait().expect("check oracle worker status")
        );
        serde_json::from_str(&response).expect("oracle JSON")
    }
}

fn oracle(root: &Path, mut request: Value) -> Value {
    for name in ["home", "codex", "claude"] {
        std::fs::create_dir_all(root.join(name)).expect("isolated home");
    }
    if request["command"] == "seed" {
        request["codexHome"] = json!(root.join("codex"));
    }
    static WORKER: OnceLock<Mutex<OracleWorker>> = OnceLock::new();
    WORKER
        .get_or_init(|| Mutex::new(OracleWorker::start()))
        .lock()
        .expect("oracle worker mutex")
        .request(request)
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn image_readiness_refuses_with_python_message_without_workspace_writes() {
    let fixture = Fixture::new().await;
    fixture.patch("workspaceMode", json!("image")).await;
    fixture.patch("imageWorkspace", json!(true)).await;
    let before = std::fs::read_dir(fixture.root.path())
        .expect("fixture contents")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<std::collections::BTreeSet<_>>();

    let decision = prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
        .await
        .expect("readiness decision");
    assert!(matches!(decision, WorkspaceDecision::Refuse(refusal)
        if refusal.to_string() == crate::image_workspace::PLATFORM_REFUSAL));

    let after = std::fs::read_dir(fixture.root.path())
        .expect("fixture contents after refusal")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(after, before);
    assert!(!fixture.repo.join(".worktrees").exists());
}

/// An agent at the repository root gets the worktree root as its cwd. The
/// stored text has no trailing separator, because the room gate and other
/// readers compare the stored text as a path identity.
#[tokio::test]
async fn repository_root_cwd_is_stored_without_a_trailing_separator() {
    let fixture = Fixture::new().await;
    let repository = fixture.repo.canonicalize().expect("canonical repository");
    fixture.patch("cwd", json!(repository)).await;
    let result = prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
        .await
        .expect("prepare");
    let cwd = admitted(&fixture, result).await;
    let expected = repository
        .join(".worktrees/codex-agents")
        .join(&fixture.id)
        .to_str()
        .expect("UTF-8 path")
        .to_owned();
    assert_eq!(fixture.actor()["cwd"], json!(expected));
    assert_eq!(cwd.to_str(), Some(expected.as_str()));
}

#[tokio::test]
async fn creates_subproject_checkpoint_and_preserves_queued_input_and_source_index() {
    let fixture = Fixture::new().await;
    let original = std::fs::read(fixture.repo.join(".git/index")).expect("source index");
    let result = prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
        .await
        .expect("prepare");
    let cwd = admitted(&fixture, result).await;
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("checkout"),
        "base\n"
    );
    let actor = fixture.actor();
    assert_eq!(actor["worktreeReady"], true);
    assert!(actor.get("worktreePreparation").is_none());
    assert!(actor.get("workspaceReservationId").is_none());
    let checkpoint = CheckpointsRepository::checkpoint(
        &fixture.store.test_reader().expect("read"),
        actor["checkpointHistoryHead"]
            .as_str()
            .expect("checkpoint id"),
    )
    .expect("checkpoint")
    .expect("checkpoint saved");
    assert_eq!(checkpoint.record["cwd"], json!(cwd));
    let event: String = fixture
        .store
        .test_reader()
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
    let replay = prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
        .await
        .expect("replay");
    assert_eq!(admitted(&fixture, replay).await, cwd);
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("file"),
        "user edits\n"
    );
}

#[tokio::test]
async fn crash_before_effect_reuses_identity_and_crash_after_effect_publishes_without_recreation() {
    for effect_finished in [false, true] {
        let fixture = Fixture::new().await;
        let plan = fixture.plan();
        fixture.reserve(&plan.identity).await;
        if effect_finished {
            git().create(&plan.identity).expect("effect before crash");
        }
        let decision = fixture.context().decide();
        assert!(
            matches!(decision, WorkspaceDecision::Provision(ref p) if p.identity == plan.identity && p.effect == if effect_finished {ProvisionEffect::Publish} else {ProvisionEffect::Create})
        );
        assert!(matches!(
            prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
                .await
                .expect("recover"),
            WorkspaceDecision::Ready { .. }
        ));
        let count: i64 = fixture.store.test_reader().expect("read").query_row("SELECT count(*) FROM runtime_workspace_operations WHERE json_extract(record,'$.kind')='provision'", [], |r|r.get(0)).expect("count");
        assert_eq!(count, 1);
    }
}

#[tokio::test]
async fn partial_or_dirty_effect_is_never_adopted_reset_or_cleaned() {
    for scenario in [
        "dirty",
        "branch-only",
        "file",
        "wrong-head",
        "missing-file",
        "unregistered",
    ] {
        let fixture = Fixture::new().await;
        let plan = fixture.plan();
        fixture.reserve(&plan.identity).await;
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
                prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
                    .await
                    .expect("refusal"),
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

#[tokio::test]
async fn late_completion_never_attaches_to_deleted_stopped_replaced_or_new_epoch_actor() {
    for (key, value) in [
        ("deletedAt", json!(10)),
        ("autoWake", json!(false)),
        ("epoch", json!(999)),
        ("cwd", json!("/var/tmp/changed")),
        ("workspaceReservationId", json!("replacement")),
        ("accountKey", json!("replacement")),
    ] {
        let fixture = Fixture::new().await;
        let plan = fixture.plan();
        fixture.reserve(&plan.identity).await;
        git().create(&plan.identity).expect("effect");
        fixture.patch(key, value.clone()).await;
        let identity = plan.identity.clone();
        assert!(
            !fixture
                .store
                .transaction({
                    let paths = fixture.paths(&identity);
                    move |tx| {
                        WorkspaceReadyRepository::publish(
                            tx,
                            &identity,
                            &paths,
                            Err("fixture"),
                            now(),
                        )
                    }
                })
                .await
                .expect("stale completion")
        );
        assert_eq!(fixture.actor()[key], value);
        assert_eq!(fixture.actor()["worktreeReady"], false);
        assert!(plan.identity.cwd.is_dir());
        assert_eq!(
            WorkspaceReadyRepository::journal(
                &fixture.store.test_reader().expect("read"),
                &plan.identity.id
            )
            .expect("journal")
            .expect("journal row")["phase"],
            "recovery_required"
        );
    }
}

#[tokio::test]
async fn deadline_after_durable_reservation_retains_evidence_and_checkpoint_failure_is_nonfatal() {
    let fixture = Fixture::new().await;
    let context = fixture.context();
    let plan = fixture.plan();
    let deadline = Instant::now();
    let outcome = provision(
        std::sync::Arc::clone(&fixture.store),
        context,
        plan.clone(),
        deadline,
    )
    .await
    .expect("deadline classified");
    assert!(matches!(outcome, WorkspaceDecision::Refuse(_)));
    assert!(!plan.identity.destination.exists());
    assert_eq!(
        WorkspaceReadyRepository::journal(
            &fixture.store.test_reader().expect("read"),
            &plan.identity.id
        )
        .expect("journal")
        .expect("journal row")["phase"],
        "recovery_required"
    );
    git()
        .create(&plan.identity)
        .expect("later confirmed absent retry");
    let identity = plan.identity.clone();
    assert!(
        fixture
            .store
            .transaction({
                let paths = fixture.paths(&identity);
                move |tx| {
                    WorkspaceReadyRepository::publish(
                        tx,
                        &identity,
                        &paths,
                        Err("Checkpoint skipped: fixture failure"),
                        now(),
                    )
                }
            })
            .await
            .expect("publish without checkpoint")
    );
    assert_eq!(
        fixture.actor()["checkpointError"],
        "Checkpoint skipped: fixture failure"
    );
    assert_eq!(fixture.actor()["worktreeReady"], true);
}

#[tokio::test]
async fn common_git_directory_serialization_defers_without_starting_another_effect() {
    let fixture = Fixture::new().await;
    let plan = fixture.plan();
    let lock = repository_lock(&plan.identity.common_dir).expect("lock");
    let _guard = lock.lock().await;
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("defer"),
        WorkspaceDecision::Defer { .. }
    ));
    assert!(!plan.identity.destination.exists());
    assert!(
        WorkspaceReadyRepository::journal(
            &fixture.store.test_reader().expect("read"),
            &plan.identity.id
        )
        .expect("journal")
        .is_none()
    );
    let other = repository_lock(&fixture.root.path().join("other-repo")).expect("independent key");
    assert!(other.try_lock().is_ok());
}

#[tokio::test]
async fn workspace_gate_matches_guarded_python_witnesses() {
    gate_corpus(false);
}

#[test]
#[ignore = "full runtime corpus: just test-rust-runtime-corpora"]
fn workspace_gate_full_python_corpus() {
    gate_corpus(true);
}

#[tokio::test]
#[ignore = "full runtime corpus: real Python writer bootstrap"]
async fn workspace_reads_real_python_written_state() {
    let fixture = Fixture::with_python(true).await;
    compare_publication_with_python(&fixture, true).await;
    compare_publication_with_python(&Fixture::with_python(true).await, false).await;
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

#[tokio::test]
async fn requested_base_is_pinned_and_unsupported_or_restore_refusals_do_not_write() {
    let fixture = Fixture::new().await;
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
    fixture.patch("workerBaseCommit", json!(base)).await;
    let key = "workspaceMode";
    let original = fixture.actor().get(key).cloned().unwrap_or(Value::Null);
    fixture.patch(key, json!("image")).await;
    let before = snapshot(&fixture.store, &fixture.id).expect("before");
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("unsupported"),
        WorkspaceDecision::Refuse(_)
    ));
    assert_eq!(
        snapshot(&fixture.store, &fixture.id).expect("after"),
        before
    );
    fixture.patch(key, original).await;
    let original_vm_fields = fixture.actor();
    fixture.patch("executionMode", json!("vm")).await;
    fixture.patch("layrReady", json!(true)).await;
    fixture.patch("layrProjectId", json!("fake-project")).await;
    fixture.patch("layrLine", json!("fake-line")).await;
    fixture
        .patch("cwd", json!("/guest/project/lines/fake-line"))
        .await;
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("ready VM workspace"),
        WorkspaceDecision::Ready { .. }
    ));
    fixture.patch("executionMode", json!("native")).await;
    fixture.patch("layrReady", json!(false)).await;
    for key in ["layrProjectId", "layrLine", "cwd"] {
        fixture
            .patch(
                key,
                original_vm_fields.get(key).cloned().unwrap_or(Value::Null),
            )
            .await;
    }
    let fixture_id = fixture.id.clone();
    let fixture_cwd = fixture.actor()["cwd"].clone();
    fixture.store.transaction(move |tx| {tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('restore:lost-marker',?1)", [json!({"id":"restore:lost-marker","kind":"restore","agent":fixture_id,"cwd":fixture_cwd,"phase":"recovery_required"}).to_string()])?;Ok(())}).await.expect("orphan restore");
    let before = snapshot(&fixture.store, &fixture.id).expect("before");
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("restore hold"),
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
        .await
        .expect("fixture resolves restore");
    let cwd = admitted(
        &fixture,
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("pinned provision"),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("pinned checkout"),
        "base\n"
    );
}

#[tokio::test]
async fn physical_actor_deletion_and_changed_journal_payload_cannot_publish() {
    let fixture = Fixture::new().await;
    let plan = fixture.plan();
    fixture.reserve(&plan.identity).await;
    let mut different = plan.identity.clone();
    different.base_commit = "f".repeat(40);
    let different_snapshot = snapshot(&fixture.store, &fixture.id).expect("snapshot");
    assert!(
        !fixture
            .store
            .transaction({
                let paths = fixture.paths(&different);
                move |tx| {
                    WorkspaceReadyRepository::reserve(
                        tx,
                        &different_snapshot,
                        &different,
                        &paths,
                        now(),
                    )
                }
            })
            .await
            .expect("changed payload conflict")
    );
    git().create(&plan.identity).expect("effect");
    let deleted_id = fixture.id.clone();
    fixture
        .store
        .transaction(move |tx| {
            tx.execute("DELETE FROM runtime_agents WHERE id=?1", [&deleted_id])?;
            Ok(())
        })
        .await
        .expect("deleted actor");
    let identity = plan.identity.clone();
    assert!(
        !fixture
            .store
            .transaction({
                let paths = fixture.paths(&identity);
                move |tx| {
                    WorkspaceReadyRepository::publish(tx, &identity, &paths, Err("fixture"), now())
                }
            })
            .await
            .expect("orphan completion")
    );
    assert!(plan.identity.cwd.is_dir());
    assert_eq!(
        WorkspaceReadyRepository::journal(
            &fixture.store.test_reader().expect("read"),
            &plan.identity.id
        )
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

#[tokio::test]
async fn hidden_index_flags_and_sparse_checkout_never_prove_recovery_complete() {
    for flag in ["--skip-worktree", "--assume-unchanged", "sparse"] {
        let fixture = Fixture::new().await;
        let plan = fixture.plan().identity;
        fixture.reserve(&plan).await;
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
        assert_recovery(
            prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
                .await
                .expect("recovery"),
        );
        assert_eq!(fixture.actor()["worktreeReady"], false);
    }
}

async fn reserve_other(fixture: &Fixture, cwd: &Path) {
    let cwd = cwd.to_path_buf();
    fixture.store.transaction(move |tx| {
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES('other-restore',?1)",
            [json!({"id":"other-restore","agent":"other","kind":"restore","phase":"local_mutation","cwd":cwd}).to_string()])?;
        Ok(())
    }).await.expect("other reservation");
}

#[tokio::test]
async fn parent_and_destination_reservations_block_but_progress_does_not() {
    for destination in [false, true] {
        let fixture = Fixture::new().await;
        let plan = fixture.plan().identity;
        reserve_other(
            &fixture,
            if destination {
                &plan.destination
            } else {
                &fixture.repo
            },
        )
        .await;
        assert_recovery(
            prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
                .await
                .expect("overlap refused"),
        );
        assert!(!plan.destination.exists());
    }
    let fixture = Fixture::new().await;
    let context = fixture.context();
    let plan = fixture.plan();
    fixture.patch("progress", json!("ordinary update")).await;
    assert!(matches!(
        provision(
            std::sync::Arc::clone(&fixture.store),
            context,
            plan,
            Instant::now() + std::time::Duration::from_secs(30),
        )
        .await
        .expect("progress ignored"),
        WorkspaceDecision::Ready { .. }
    ));
}

#[tokio::test]
async fn checkpoint_window_revalidates_git_and_reservations_and_recovers_exact_replay() {
    for change in ["dirty", "reservation", "none"] {
        let fixture = Fixture::new().await;
        let plan = fixture.plan().identity;
        fixture.reserve(&plan).await;
        git().create(&plan).expect("effect");
        let checkpoint = git().checkpoint(&plan).expect("checkpoint effect");
        match change {
            "dirty" => std::fs::write(plan.cwd.join("file"), "late edit\n").expect("user edit"),
            "reservation" => reserve_other(&fixture, &plan.destination).await,
            _ => (),
        }
        let decision = publish_verified(
            std::sync::Arc::clone(&fixture.store),
            plan.clone(),
            Ok(checkpoint),
            Instant::now() + std::time::Duration::from_secs(30),
        )
        .await
        .expect("publication");
        if change == "none" {
            assert!(matches!(decision, WorkspaceDecision::Ready { .. }));
        } else {
            assert_recovery(decision);
            assert_eq!(fixture.actor()["worktreeReady"], false);
        }
    }
    // Process died after update-ref, before publication: same reference is an exact replay.
    let fixture = Fixture::new().await;
    let plan = fixture.plan().identity;
    fixture.reserve(&plan).await;
    git().create(&plan).expect("effect");
    let checkpoint = git().checkpoint(&plan).expect("checkpoint effect");
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("crash replay"),
        WorkspaceDecision::Ready { .. }
    ));
    assert_eq!(fixture.actor()["checkpointHistoryHead"], checkpoint["id"]);
}

#[tokio::test]
async fn ready_token_requires_current_generation_and_no_conflicting_reservation() {
    let fixture = Fixture::new().await;
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("create"),
        WorkspaceDecision::Ready { .. }
    ));
    let WorkspaceDecision::Ready { token } =
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("already ready")
    else {
        panic!("ready")
    };
    let admitted_token = token.clone();
    let cwd = fixture
        .store
        .transaction(move |tx| admitted_token.validate(tx))
        .await
        .expect("transaction")
        .expect("admit");
    fixture
        .patch("progress", json!("does not invalidate"))
        .await;
    let unchanged_token = token.clone();
    assert!(
        fixture
            .store
            .transaction(move |tx| unchanged_token.validate(tx))
            .await
            .expect("admit")
            .is_ok()
    );
    fixture.patch("epoch", json!(1)).await;
    let stale_token = token.clone();
    assert!(
        !fixture
            .store
            .transaction(move |tx| stale_token.validate(tx))
            .await
            .expect("reject stale generation")
            .is_ok()
    );
    fixture.patch("epoch", json!(0)).await;
    // A thread/start between preparation and submission leaves the workspace unchanged.
    fixture.patch("threadId", json!("changed session")).await;
    let thread_token = token.clone();
    assert!(
        fixture
            .store
            .transaction(move |tx| thread_token.validate(tx))
            .await
            .expect("validate after thread start")
            .is_ok()
    );
    fixture.patch("accountKey", json!("changed account")).await;
    let changed_token = token.clone();
    assert!(
        !fixture
            .store
            .transaction(move |tx| changed_token.validate(tx))
            .await
            .expect("reject changed source")
            .is_ok()
    );
    let actor = fixture.actor();
    fixture
        .store
        .transaction(move |tx| AgentRepository::put(tx, &AgentRecord::from_value(actor)?))
        .await
        .expect("restore source");
    reserve_other(&fixture, cwd.parent().expect("root")).await;
    let reserved_token = token.clone();
    assert!(
        !fixture
            .store
            .transaction(move |tx| reserved_token.validate(tx))
            .await
            .expect("reject late reservation")
            .is_ok()
    );
}

#[tokio::test]
async fn checkpoint_ref_collision_is_preserved_and_reported() {
    let fixture = Fixture::new().await;
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
    fixture
        .patch("workerBaseCommit", json!(plan.base_commit))
        .await;
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("checkpoint failure nonfatal"),
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
#[tokio::test]
async fn post_effect_symlink_escape_is_refused_without_cleanup() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new().await;
    let context = fixture.context();
    let plan = fixture.plan();
    let outside = fixture.root.path().join("outside");
    std::fs::create_dir(&outside).expect("outside destination");
    // Inject the exact interval after observation and before effect.
    symlink(&outside, fixture.repo.join(".worktrees")).expect("ancestor replaced");
    assert_recovery(
        provision(
            std::sync::Arc::clone(&fixture.store),
            context,
            plan.clone(),
            Instant::now() + std::time::Duration::from_secs(30),
        )
        .await
        .expect("classified"),
    );
    assert_eq!(fixture.actor()["worktreeReady"], false);
    assert!(
        outside.join("codex-agents").join(&fixture.id).is_dir(),
        "effect remains inspectable"
    );
}

#[tokio::test]
async fn linked_worktree_and_submodule_sources_preserve_git_identity() {
    let fixture = Fixture::new().await;
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
    fixture.patch("cwd", json!(linked.join("project"))).await;
    let plan = fixture.plan().identity;
    assert_eq!(plan.common_dir, fixture.repo.join(".git"));
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("linked source"),
        WorkspaceDecision::Ready { .. }
    ));
    // A local gitlink fixture, without clone or any network/file transport.
    let fixture = Fixture::new().await;
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
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("gitlink checkout"),
        WorkspaceDecision::Ready { .. }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn running_git_deadline_kills_child_and_keeps_durable_identity() {
    let fixture = Fixture::new().await;
    let plan = fixture.plan().identity;
    fixture.reserve(&plan).await;
    command(
        &fixture.repo,
        &[
            "config",
            "alias.workspace-stall",
            "!echo $$ > running-marker; exec sleep 300",
        ],
    );
    // Startup is not the behavior under test. Expire the runner's clock only
    // after the real child has published its PID, then exercise real termination.
    let marker = fixture.repo.join("running-marker");
    crate::bounded_child::DEADLINE_OVERRIDE.set(Some(Box::new(move || {
        let guard = Instant::now() + Duration::from_secs(120);
        while Instant::now() < guard {
            if std::fs::read_to_string(&marker).is_ok_and(|pid| pid.trim().parse::<u32>().is_ok()) {
                break;
            }
            std::thread::park_timeout(Duration::from_millis(1));
        }
        Instant::now()
    })));
    let result = Git {
        deadline: Instant::now() + Duration::from_secs(120),
    }
    .run(&fixture.repo, &["workspace-stall"]);
    assert!(matches!(result, Err(WorkspaceError::Deadline(_))));
    assert!(
        fixture.repo.join("running-marker").exists(),
        "child actually started before the readiness hang guard"
    );
    let pid =
        std::fs::read_to_string(fixture.repo.join("running-marker")).expect("child PID marker");
    let guard = Instant::now() + Duration::from_secs(120);
    loop {
        let status = Command::new("ps")
            .args(["-p", pid.trim(), "-o", "stat="])
            .output()
            .expect("check child termination");
        let status = String::from_utf8(status.stdout).expect("process status");
        // An orphan may await init's reaper after group termination; a zombie
        // has already stopped executing and cannot retain the worktree lock.
        if status.trim().is_empty() || status.trim().starts_with('Z') {
            break;
        }
        assert!(Instant::now() < guard, "timed-out child remains alive");
        std::thread::park_timeout(Duration::from_millis(1));
    }
    assert!(
        WorkspaceReadyRepository::journal(&fixture.store.test_reader().expect("read"), &plan.id)
            .expect("journal")
            .is_some()
    );
    assert!(!plan.destination.exists());
}

#[tokio::test]
async fn isolated_child_rejects_all_ambient_git_overrides() {
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

#[tokio::test]
async fn ambient_git_child() {
    if std::env::var_os("WORKSPACE_ENV_CHILD").is_none() {
        return;
    }
    let fixture = Fixture::new().await;
    let cwd = admitted(
        &fixture,
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("isolated prepare"),
    )
    .await;
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

#[tokio::test]
async fn publication_and_checkpoint_history_match_python() {
    compare_publication_with_python(&Fixture::new().await, true).await;
}

async fn compare_publication_with_python(fixture: &Fixture, has_parent: bool) {
    let items = json!([{"id":"old"}, {"id":"new"}, {"id":"hidden","afterRestore":true}]);
    let parent = if has_parent {
        json!({"id":"parent-checkpoint","agent":fixture.id,"historyBoundary":1})
    } else {
        Value::Null
    };
    let id = fixture.id.clone();
    let items_for_store = items.clone();
    let parent_for_store = parent.clone();
    fixture.store.transaction(move |tx| {
        // Explicit rowids make the parent boundary and excluded item deterministic.
        for (i, item) in items_for_store.as_array().expect("items").iter().enumerate() {
            tx.execute("INSERT INTO runtime_items(rowid,id,agent,record,created) VALUES(?1,?2,?3,?4,1)",
                rusqlite::params![i as i64 + 1, item["id"].as_str(), id, item.to_string()])?;
        }
        if has_parent { tx.execute("INSERT INTO runtime_checkpoints(id,record) VALUES('parent-checkpoint',?1)", [parent_for_store.to_string()])?; }
        Ok(())
    }).await.expect("history fixture");
    if has_parent {
        fixture
            .patch("checkpointHistoryHead", json!("parent-checkpoint"))
            .await;
    }
    fixture
        .patch("lastCompletedTurn", json!("previous-turn"))
        .await;
    fixture.patch("threadId", json!("previous-thread")).await;
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
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("publish"),
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
        &fixture.store.test_reader().expect("read"),
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
    assert_eq!(
        checkpoint["historyDelta"],
        if has_parent {
            json!(["new"])
        } else {
            json!(["old", "new"])
        }
    );
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

#[tokio::test]
async fn stale_reservation_precondition_never_starts_git_effect() {
    let fixture = Fixture::new().await;
    let context = fixture.context();
    let plan = fixture.plan();
    fixture.patch("epoch", json!(1)).await;
    assert!(matches!(
        provision(
            std::sync::Arc::clone(&fixture.store),
            context,
            plan.clone(),
            Instant::now() + std::time::Duration::from_secs(30),
        )
        .await
        .expect("defer"),
        WorkspaceDecision::Defer { .. }
    ));
    assert!(!plan.identity.destination.exists());
    assert!(
        WorkspaceReadyRepository::journal(
            &fixture.store.test_reader().expect("read"),
            &plan.identity.id
        )
        .expect("journal")
        .is_none()
    );
}

#[tokio::test]
async fn absorbed_submodule_source_provisions_without_network() {
    let mut fixture = Fixture::new().await;
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
    fixture.patch("cwd", json!(submodule.join("project"))).await;
    let plan = fixture.plan().identity;
    assert_eq!(plan.common_dir, parent.join(".git/modules/submodule"));
    assert!(matches!(
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("submodule source"),
        WorkspaceDecision::Ready { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(plan.cwd.join("file")).expect("checkout"),
        "base\n"
    );
}

#[tokio::test]
async fn repository_clean_smudge_filter_is_preserved() {
    let fixture = Fixture::new().await;
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
    let cwd = admitted(
        &fixture,
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("filtered creation"),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(cwd.join("file")).expect("smudged file"),
        "filtered\n"
    );
    assert!(command(&cwd, &["status", "--porcelain=v1"]).is_empty());
}

async fn admitted(fixture: &Fixture, decision: WorkspaceDecision) -> PathBuf {
    let WorkspaceDecision::Ready { token } = decision else {
        panic!("not ready: {decision:?}")
    };
    let token = token.clone();
    fixture
        .store
        .transaction(move |tx| token.validate(tx))
        .await
        .expect("admission transaction")
        .expect("validated cwd")
}

#[cfg(unix)]
async fn public_probe(fixture: Fixture, action: &str, expectation: &str) -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let Fixture {
        root,
        store,
        id,
        repo,
    } = fixture;
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).expect("wrapper directory");
    let script = format!(
        r#"#!/usr/bin/env python3
import os, pathlib, sqlite3, subprocess, sys
root = pathlib.Path(os.environ["WORKSPACE_PROBE_ROOT"])
repo = root / "repo"
args = sys.argv[1:]
result = subprocess.run(["/usr/bin/git", *args], capture_output=True)
marker = root / "probe-executed"
refs = subprocess.run(["/usr/bin/git", "-C", str(repo), "for-each-ref", "--format=%(refname)", "refs/codex-agents/checkpoints/"], capture_output=True).stdout.decode().splitlines()
{action}
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
sys.exit(result.returncode)
"#
    );
    std::fs::write(bin.join("git"), script).expect("wrapper");
    std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o755))
        .expect("executable");
    drop(store); // The child is the single owner of this disposable state root.
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "workspace_ready_git::tests::public_probe_child",
            "--nocapture",
        ])
        .env("WORKSPACE_PROBE_ROOT", root.path())
        .env("WORKSPACE_PROBE_ID", &id)
        .env("WORKSPACE_PROBE_EXPECT", expectation)
        .env("FIXTURE_FILTER_KEY", "present")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .expect("probe subprocess");
    assert!(
        output.status.success(),
        "probe failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(&root.path().join("state")))
            .await
            .expect("reopen state"),
    );
    Fixture {
        root,
        store,
        id,
        repo,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn public_probe_child() {
    let Some(root) = std::env::var_os("WORKSPACE_PROBE_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(root.starts_with(fixture_parent()));
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(&root.join("state")))
            .await
            .expect("probe store"),
    );
    let id = std::env::var("WORKSPACE_PROBE_ID").expect("probe actor");
    let result = prepare(std::sync::Arc::clone(&store), id.clone())
        .await
        .expect("public prepare");
    let expected = std::env::var("WORKSPACE_PROBE_EXPECT").expect("expectation");
    if expected == "recovery" {
        assert_recovery(result);
        assert_eq!(
            snapshot(&store, &id).expect("actor").agent.record["worktreeReady"],
            false
        );
    } else {
        let WorkspaceDecision::Ready { token } = result else {
            panic!("not ready: {result:?}")
        };
        let token = token.clone();
        let cwd = store
            .transaction(move |tx| token.validate(tx))
            .await
            .expect("transaction")
            .expect("validated cwd");
        if expected == "filtered" {
            assert_eq!(
                std::fs::read_to_string(cwd.join("file")).expect("filtered"),
                "filtered\n"
            );
        }
        if expected == "ref-conflict" {
            assert!(
                snapshot(&store, &id).expect("actor").agent.record["checkpointError"]
                    .as_str()
                    .expect("ref conflict")
                    .contains("Checkpoint reference already names a different commit")
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn final_git_observation_does_not_hold_store_writer() {
    let fixture = public_probe(
        Fixture::new().await,
        r#"
if refs and 'diff' in args and '--cached' in args and not marker.exists():
    with sqlite3.connect(root / 'state/canvas.sqlite3', timeout=0.1) as db:
        db.execute('BEGIN IMMEDIATE')
        db.execute('CREATE TABLE fixture_writer_probe (value TEXT)')
        db.execute("INSERT INTO fixture_writer_probe VALUES ('concurrent write')")
    marker.write_text('writer was available')
"#,
        "ready",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("probe-executed")).expect("probe ran"),
        "writer was available"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn public_adapter_refuses_escape_after_final_git_observation() {
    let fixture = public_probe(
        Fixture::new().await,
        r#"
if refs and 'diff' in args and '--cached' in args and not marker.exists():
    (repo / '.worktrees').rename(root / 'outside')
    (repo / '.worktrees').symlink_to(root / 'outside', target_is_directory=True)
    marker.write_text('escaped')
"#,
        "recovery",
    )
    .await;
    assert!(fixture.root.path().join("probe-executed").exists());
    assert!(
        fixture
            .root
            .path()
            .join("outside/codex-agents")
            .join(&fixture.id)
            .join("project/file")
            .is_file(),
        "escaped effect is inspectable"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exact_checkpoint_replay_atomically_rejects_ref_changed_after_read() {
    let fixture = Fixture::new().await;
    let plan = fixture.plan().identity;
    fixture.reserve(&plan).await;
    git().create(&plan).expect("effect");
    let checkpoint = git().checkpoint(&plan).expect("checkpoint before crash");
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
            "other",
        ],
    );
    let other = command(&fixture.repo, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    std::fs::write(fixture.root.path().join("other-oid"), &other).expect("other commit");
    let fixture = public_probe(
        fixture,
        r#"
if 'for-each-ref' in args and any(a.startswith('refs/codex-agents/checkpoints/') for a in args) and result.stdout.strip() and not marker.exists():
    subprocess.run(['/usr/bin/git', '-C', str(repo), 'update-ref', refs[0], (root / 'other-oid').read_text()], check=True)
    marker.write_text('ref changed after read')
"#,
        "ref-conflict",
    ).await;
    assert!(fixture.root.path().join("probe-executed").exists());
    assert_eq!(
        command(
            &fixture.repo,
            &["rev-parse", checkpoint["ref"].as_str().expect("ref")]
        )
        .trim(),
        other
    );
}

#[tokio::test]
async fn timestamp_only_change_preserves_ready_and_recovery_without_index_writes() {
    for ready in [false, true] {
        let fixture = Fixture::new().await;
        let plan = fixture.plan().identity;
        if ready {
            admitted(
                &fixture,
                prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
                    .await
                    .expect("create"),
            )
            .await;
        } else {
            fixture.reserve(&plan).await;
            git().create(&plan).expect("effect before crash");
        }
        let index = command(
            &plan.cwd,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        );
        let before = std::fs::read(index.trim()).expect("index bytes");
        std::fs::File::open(plan.cwd.join("file"))
            .expect("file")
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() + Duration::from_secs(3600)),
            )
            .expect("timestamp only");
        admitted(
            &fixture,
            prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
                .await
                .expect("unchanged contents"),
        )
        .await;
        assert_eq!(std::fs::read(index.trim()).expect("index bytes"), before);
        assert_eq!(
            std::fs::read_to_string(plan.cwd.join("file")).expect("content"),
            "base\n"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn required_filter_receives_non_git_caller_environment() {
    let fixture = Fixture::new().await;
    std::fs::write(
        fixture.repo.join(".gitattributes"),
        "project/file filter=fixture\n",
    )
    .expect("attributes");
    command(
        &fixture.repo,
        &[
            "config",
            "filter.fixture.smudge",
            "test \"$FIXTURE_FILTER_KEY\" = present && sed s/base/filtered/",
        ],
    );
    command(
        &fixture.repo,
        &["config", "filter.fixture.clean", "sed s/filtered/base/"],
    );
    command(
        &fixture.repo,
        &["config", "filter.fixture.required", "true"],
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
            "required filter",
        ],
    );
    public_probe(fixture, "", "filtered").await;
}

/// A worker whose linked worktree Python created and marked ready (agent
/// 50f9a796 in the 18 October 2026 cutover).
async fn ready_linked_worktree(fixture: &Fixture) -> PathBuf {
    let branch = format!("codex-agent/{}", fixture.id);
    let linked = fixture
        .repo
        .join(".worktrees/codex-agents")
        .join(&fixture.id);
    command(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            path_string(&linked).expect("path"),
        ],
    );
    fixture.patch("cwd", json!(linked.join("project"))).await;
    fixture.patch("branch", json!(branch)).await;
    fixture.patch("worktreeReady", json!(true)).await;
    linked
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_worktree_git_budget_excludes_waiting_for_a_store_reader() {
    let fixture = Fixture::new().await;
    let linked = ready_linked_worktree(&fixture).await;
    // Every reader stays busy for longer than the whole Git budget, as on a
    // backend that has just started on a large state. The budget still leaves
    // the four Git commands ample time on a loaded test host.
    let budget = Duration::from_secs(2);
    let held = std::sync::Arc::new(std::sync::Barrier::new(
        studio_runtime_store::READ_CONNECTION_POOL_CAPACITY + 1,
    ));
    let readers = (0..studio_runtime_store::READ_CONNECTION_POOL_CAPACITY)
        .map(|_| {
            let store = std::sync::Arc::clone(&fixture.store);
            let held = std::sync::Arc::clone(&held);
            std::thread::spawn(move || {
                let reader = store.test_reader().expect("reader");
                held.wait();
                std::thread::sleep(budget + Duration::from_millis(500));
                drop(reader);
            })
        })
        .collect::<Vec<_>>();
    held.wait();
    let decision = prepare_with_timeout(
        std::sync::Arc::clone(&fixture.store),
        fixture.id.clone(),
        budget,
    )
    .await
    .expect("prepare");
    for reader in readers {
        reader.join().expect("reader thread");
    }
    assert_eq!(admitted(&fixture, decision).await, linked.join("project"));
}

#[tokio::test]
async fn ready_worktree_needs_only_its_registration_like_python() {
    let fixture = Fixture::new().await;
    let linked = ready_linked_worktree(&fixture).await;
    // Python starts a ready worker without reading its index. A damaged index is
    // the worker's own state, not a missing workspace identity.
    let index = PathBuf::from(command(&linked, &["rev-parse", "--git-path", "index"]).trim());
    let index = if index.is_absolute() {
        index
    } else {
        linked.join(index)
    };
    std::fs::write(&index, "not an index").expect("damage the worker index");
    let decision = prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
        .await
        .expect("prepare");
    assert_eq!(admitted(&fixture, decision).await, linked.join("project"));
}

#[tokio::test]
async fn git_failure_and_git_deadline_have_distinct_messages() {
    let fixture = Fixture::new().await;
    let outside = fixture.root.path().join("not-a-repository");
    std::fs::create_dir(&outside).expect("plain directory");
    fixture.patch("cwd", json!(outside)).await;
    fixture.patch("worktreeReady", json!(true)).await;
    let failure = prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
        .await
        .expect("prepare");
    let WorkspaceDecision::Refuse(refusal) = failure else {
        panic!("expected refusal: {failure:?}")
    };
    let failure = refusal.to_string();
    assert!(
        failure.starts_with(
            "WorkspaceRecoveryRequired: Workspace identity could not be established: \
             git rev-parse --show-toplevel: "
        ) && failure.contains("not a git repository"),
        "{failure}"
    );
    assert!(!failure.contains("deadline"), "{failure}");
    let expired = Git {
        deadline: Instant::now(),
    }
    .run(&fixture.repo, &["status"]);
    let Err(WorkspaceError::Deadline(step)) = expired else {
        panic!("expected deadline: {expired:?}")
    };
    assert_eq!(step, "git status");
}

#[tokio::test]
async fn concurrent_workers_and_lead_of_one_repository_all_reach_ready() {
    const WORKERS: usize = 40;
    // The preparations queue on one repository lock, and each lock wait counts
    // against its Git deadline. This test checks that concurrency refuses and
    // defers nothing, not host speed: on a macOS host under shared disk load one
    // preparation takes about one second, so the production 30-second deadline
    // would defer the last ones. Unit: wall time per preparation.
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(150);
    let fixture = Fixture::new().await;
    let seed = fixture.actor();
    let mut ids = vec![fixture.id.clone()];
    let lead_cwd = fixture.repo.clone();
    let records = (1..WORKERS)
        .map(|n| {
            let mut worker = seed.clone();
            worker["id"] = json!(format!("worker-{n}"));
            worker["rootId"] = worker["id"].clone();
            worker
        })
        .chain(std::iter::once({
            let mut lead = seed.clone();
            lead["id"] = json!("lead");
            lead["rootId"] = json!("lead");
            lead["cwd"] = json!(lead_cwd);
            lead["worktree"] = json!(false);
            lead
        }))
        .collect::<Vec<_>>();
    ids.extend(
        records
            .iter()
            .map(|r| r["id"].as_str().expect("id").to_owned()),
    );
    fixture
        .store
        .transaction(move |tx| {
            for record in records {
                let id = record["id"].as_str().expect("id").to_owned();
                tx.execute("INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('agent',?1,1,'fixture',?2,0)",
                    rusqlite::params![id, json!({"collection":"agent","id":id,"value":record}).to_string()])?;
                AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
            }
            Ok(())
        })
        .await
        .expect("seed agents");
    let tasks = ids
        .into_iter()
        .map(|id| {
            let store = std::sync::Arc::clone(&fixture.store);
            tokio::spawn(
                async move { (id.clone(), prepare_with_timeout(store, id, DEADLINE).await) },
            )
        })
        .collect::<Vec<_>>();
    for task in tasks {
        let (id, decision) = task.await.expect("preparation task");
        assert!(
            matches!(decision, Ok(WorkspaceDecision::Ready { .. })),
            "{id}: {decision:?}"
        );
    }
}

/// Python runs an image agent's turn in `<mount>/repo/<imageWorkspaceSubpath>`.
#[test]
fn an_image_turn_runs_in_the_saved_subpath_of_the_mounted_repo() {
    let repo = Path::new("/state/workspaces/mnt/agent/repo");
    assert_eq!(image_turn_cwd(repo, None).expect("default"), repo);
    assert_eq!(image_turn_cwd(repo, Some(".")).expect("root"), repo);
    assert_eq!(
        image_turn_cwd(repo, Some("crates/backend")).expect("subpath"),
        repo.join("crates/backend")
    );
    for outside in ["..", "../other", "/etc", "crates/../../x"] {
        assert!(
            matches!(
                image_turn_cwd(repo, Some(outside)),
                Err(WorkspaceError::InvalidRecord)
            ),
            "{outside}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reservation_snapshot_yields_the_reservations_of_the_full_snapshot() {
    let fixture = Fixture::new().await;
    let seed = fixture.actor();
    let variants: Vec<(&str, Value)> = vec![
        ("shared", json!({})),
        (
            "branch-op",
            json!({"workspaceOperation":"branch","cwd":"/elsewhere"}),
        ),
        (
            "provision-op",
            json!({"workspaceOperation":"provision","workspaceReservationId":"provision:p:0"}),
        ),
        (
            "tombstone-op",
            json!({"deletedAt":5,"workspaceOperation":"restore","cwd":"/tomb"}),
        ),
        ("tombstone", json!({"deletedAt":5})),
        ("stopped", json!({"autoWake":false})),
    ];
    let records = variants
        .iter()
        .map(|(name, patch)| {
            let mut record = seed.clone();
            record["id"] = json!(name);
            record["rootId"] = json!(name);
            for (key, value) in patch.as_object().expect("patch") {
                record[key] = value.clone();
            }
            record
        })
        .collect::<Vec<_>>();
    let ids = records
        .iter()
        .map(|r| r["id"].as_str().expect("id").to_owned())
        .collect::<Vec<_>>();
    fixture
        .store
        .transaction(move |tx| {
            for record in records {
                AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
            }
            tx.execute(
                "INSERT INTO runtime_workspace_operations(id,record) VALUES('provision:p:0',?1)",
                [
                    json!({"id":"provision:p:0","kind":"provision","agent":"provision-op",
                    "cwd":"/dest","phase":"local_mutation"})
                    .to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .expect("seed corpus");
    let db = fixture.store.test_reader().expect("reader");
    for id in ids {
        let full = CheckpointsRepository::workspace_snapshot(&db, &id)
            .expect("full")
            .expect("agent");
        let light = CheckpointsRepository::reservation_snapshot(&db, &id)
            .expect("light")
            .expect("agent");
        assert_eq!(full.agent.record, light.agent.record, "{id}");
        // Row order differs (rowid scan versus index); every consumer is order-independent.
        let sorted = |snapshot: &WorkspaceSnapshot| {
            let mut all = studio_runtime_store::workspace_ready::reservations(snapshot);
            all.sort_by(|a, b| a.agent.cmp(&b.agent));
            all
        };
        assert_eq!(sorted(&full), sorted(&light), "{id}");
    }
}

#[tokio::test]
async fn unrelated_reservation_started_after_path_capture_does_not_refuse_ready_token() {
    let fixture = Fixture::new().await;
    let other = tempfile::tempdir().expect("unrelated workspace");
    let other_cwd = other.path().to_string_lossy().into_owned();
    fixture.store.transaction(move |tx| {
        studio_runtime_store::AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(json!({
            "id":"unrelated","rootId":"unrelated","epoch":1,"status":"idle","autoWake":true,"cwd":other_cwd
        }))?)?;
        Ok(())
    }).await.expect("idle unrelated agent");
    let WorkspaceDecision::Ready { token } =
        prepare(std::sync::Arc::clone(&fixture.store), fixture.id.clone())
            .await
            .expect("ready workspace")
    else {
        panic!("workspace must become ready");
    };
    fixture
        .store
        .transaction(move |tx| {
            let mut agent = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from("unrelated"),
            )?
            .expect("unrelated agent");
            agent
                .extra
                .insert("workspaceOperation".into(), json!("checkpoint"));
            studio_runtime_store::AgentRepository::put(tx, &agent)?;
            assert!(
                token.validate(tx)?.is_ok(),
                "an idle agent's path was included before it reserved"
            );
            Ok(())
        })
        .await
        .expect("unrelated reservation permits the ready workspace");
}
