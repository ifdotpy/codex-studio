#![cfg(unix)]
#![allow(clippy::unwrap_used)]

#[path = "../../../packages/runtime-services/tests/support/prepared_python.rs"]
mod prepared_python;

use axum::{body::Body, extract::ConnectInfo, http::Request};
use serde_json::{Value, json};
use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use studio_runtime_http::{HttpState, api_schema_hash, router};
use studio_runtime_services::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, NotSentReason, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    account_worker::preparation::PreparationResources,
    model_catalog::ModelCatalogService,
};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "restore-failpoint-token";
const ACCOUNT: &str = "default";
const CONNECTION: &str = "restore-fixture-connection";
const GENERATION: u64 = 7;
const HTTP_PORT: u16 = 49173;

struct Fixture {
    _root: TempDir,
    store: Option<Arc<RuntimeStore>>,
    agent_id: String,
    checkpoint_id: String,
    expected_tree: String,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_start(false)
    }

    fn new_start() -> Self {
        Self::with_start(true)
    }

    fn with_start(start: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
        let result = Command::new(prepared_python::prepared_python(&source))
            .arg("-B")
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p01_fixture.py"))
            .arg(root.path())
            .arg("dispatch")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let agents: Value = serde_json::from_slice(&result.stdout).unwrap();
        let agent_id = agents[0]["id"].as_str().unwrap().to_owned();
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("tracked.txt"), "checkpoint contents\n").unwrap();
        let status = Command::new("git")
            .current_dir(&cwd)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        for (key, value) in [
            ("user.name", "Restore Fixture"),
            ("user.email", "restore-fixture@invalid"),
        ] {
            assert!(
                Command::new("git")
                    .current_dir(&cwd)
                    .args(["config", key, value])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(
            Command::new("git")
                .current_dir(&cwd)
                .args(["add", "tracked.txt"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(&cwd)
                .args(["commit", "-qm", "checkpoint"])
                .status()
                .unwrap()
                .success()
        );
        let store = Arc::new(RuntimeStore::open(Some(root.path())).unwrap());
        store
            .transaction_with(|tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [&agent_id],
                    |row| row.get(0),
                )?;
                let mut record: Value = serde_json::from_str(&raw).unwrap();
                record["cwd"] = json!(cwd);
                record["worktreeReady"] = json!(true);
                record["accountKey"] = json!(ACCOUNT);
                record["threadId"] = if start {
                    Value::Null
                } else {
                    json!("old-thread")
                };
                record["lastCompletedTurn"] = if start {
                    Value::Null
                } else {
                    json!("old-turn")
                };
                record["inFlight"] = json!(false);
                record["nativeToolSource"] = json!("current");
                AgentRepository::put(tx, &AgentRecord::from_value(record).unwrap())?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .unwrap();
        let checkpoint = call(
            &store,
            None,
            "POST",
            "/api/checkpoint",
            json!({"agent":agent_id,"label":"restore checkpoint"}),
        );
        assert_eq!(checkpoint.0, 200, "{}", checkpoint.1);
        let checkpoint_id = checkpoint.1["id"].as_str().unwrap().to_owned();
        std::fs::write(cwd.join("tracked.txt"), "changed after checkpoint\n").unwrap();
        assert!(
            Command::new("git")
                .current_dir(&cwd)
                .args(["add", "tracked.txt"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(&cwd)
                .args(["commit", "-qm", "after checkpoint"])
                .status()
                .unwrap()
                .success()
        );
        let expected_tree = git(&cwd, &["rev-parse", "HEAD^{tree}"]);
        Self {
            _root: root,
            store: Some(store),
            agent_id,
            checkpoint_id,
            expected_tree,
            cwd,
        }
    }

    fn store(&self) -> &Arc<RuntimeStore> {
        self.store.as_ref().unwrap()
    }

    fn restart_store(&mut self) {
        let root = self._root.path().to_path_buf();
        drop(self.store.take());
        self.store = Some(Arc::new(RuntimeStore::open(Some(&root)).unwrap()));
    }
}

struct ScriptedProvider {
    calls: AtomicUsize,
    operations: Mutex<Vec<(String, String)>>,
    outcomes: Mutex<Vec<RequestCertainty>>,
}

impl ScriptedProvider {
    fn scripted(outcomes: Vec<RequestCertainty>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            operations: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes),
        })
    }
}

impl AccountSessionPort for ScriptedProvider {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == ACCOUNT).then(|| ConnectionScope {
            account: ACCOUNT.into(),
            connection: CONNECTION.into(),
            generation: GENERATION,
        })
    }

    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.operations
            .lock()
            .unwrap()
            .push((request.operation.clone(), format!("{:?}", request.method)));
        let certainty = self
            .outcomes
            .lock()
            .unwrap()
            .first()
            .copied()
            .unwrap_or(RequestCertainty::Written);
        if !self.outcomes.lock().unwrap().is_empty() {
            self.outcomes.lock().unwrap().remove(0);
        }
        let result = match certainty {
            RequestCertainty::Written => Some(ProviderResponse::Reply(
                json!({"thread":{"id":"restored-thread"}}),
            )),
            RequestCertainty::NotSent | RequestCertainty::Unknown => None,
        };
        Ok(SessionReceipt {
            scope: ConnectionScope {
                account: ACCOUNT.into(),
                connection: CONNECTION.into(),
                generation: GENERATION,
            },
            operation: request.operation,
            ticket: None,
            certainty,
            result,
            not_sent_reason: (certainty == RequestCertainty::NotSent)
                .then_some(NotSentReason::WorkerNotSent),
        })
    }
}

fn call(
    store: &Arc<RuntimeStore>,
    provider: Option<Arc<dyn AccountSessionPort>>,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    call_with_failpoint(store, provider, method, path, body, None)
}

#[derive(Clone, Copy)]
enum FailPoint {
    BeforeWrite(&'static str),
    AfterCommit(usize),
}

fn call_with_failpoint(
    store: &Arc<RuntimeStore>,
    provider: Option<Arc<dyn AccountSessionPort>>,
    method: &str,
    path: &str,
    body: Value,
    failpoint: Option<FailPoint>,
) -> (u16, Value) {
    if let Some(FailPoint::BeforeWrite(name)) = failpoint {
        install_before_write_trigger(store, name);
    }
    let root = store.root().to_path_buf();
    let mut state = HttpState::new(
        TOKEN.into(),
        "restore-fixture-workspace".into(),
        None,
        root.to_string_lossy().into_owned(),
        HTTP_PORT,
        None,
        Some(store.clone()),
        None,
    );
    if let Some(FailPoint::AfterCommit(target)) = failpoint {
        let commits = Arc::new(AtomicUsize::new(0));
        store.set_commit_notifier({
            let commits = commits.clone();
            move || {
                if commits.fetch_add(1, Ordering::SeqCst) + 1 == target {
                    panic!("restore failpoint after commit {target}");
                }
            }
        });
    }
    state.preparation_resources = Some(Arc::new(
        PreparationResources::bundled(
            String::new(),
            ModelCatalogService::new(None),
            PathBuf::from("/isolated/python"),
        )
        .unwrap(),
    ));
    state.account_session = provider;
    let app = router(state).unwrap();
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", format!("127.0.0.1:{HTTP_PORT}"))
        .header("content-type", "application/json")
        .header("x-canvas-token", TOKEN)
        .header("x-studio-api-schema", api_schema_hash())
        .extension(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = tokio_test_block_on(app.oneshot(request)).unwrap();
    let status = response.status().as_u16();
    let bytes =
        tokio_test_block_on(axum::body::to_bytes(response.into_body(), usize::MAX)).unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn install_before_write_trigger(store: &RuntimeStore, name: &str) {
    let trigger = match name {
        "reserve" => {
            "CREATE TRIGGER restore_fail_before_reserve BEFORE INSERT ON runtime_workspace_operations WHEN NEW.id LIKE 'restore:%' BEGIN SELECT RAISE(ABORT,'restore failpoint reserve'); END;"
        }
        "provider_ready" => {
            "CREATE TRIGGER restore_fail_before_provider_ready BEFORE UPDATE OF record ON runtime_workspace_operations WHEN json_extract(NEW.record,'$.phase')='provider_ready' BEGIN SELECT RAISE(ABORT,'restore failpoint provider_ready'); END;"
        }
        "local_mutation" => {
            "CREATE TRIGGER restore_fail_before_local_mutation BEFORE UPDATE OF record ON runtime_workspace_operations WHEN json_extract(NEW.record,'$.phase')='local_mutation' BEGIN SELECT RAISE(ABORT,'restore failpoint local_mutation'); END;"
        }
        "before_checkpoint" => {
            "CREATE TRIGGER restore_fail_before_checkpoint BEFORE INSERT ON runtime_checkpoints WHEN json_extract(NEW.record,'$.label')='Before restore' BEGIN SELECT RAISE(ABORT,'restore failpoint before_checkpoint'); END;"
        }
        "completed" => {
            "CREATE TRIGGER restore_fail_before_completed BEFORE UPDATE OF record ON runtime_workspace_operations WHEN json_extract(NEW.record,'$.phase')='completed' BEGIN SELECT RAISE(ABORT,'restore failpoint completed'); END;"
        }
        _ => panic!("unknown store failpoint {name}"),
    };
    store
        .transaction(|tx| {
            tx.execute_batch(trigger)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .unwrap();
}

fn clear_before_write_triggers(store: &RuntimeStore) {
    store
        .transaction(|tx| {
            tx.execute_batch(
                "DROP TRIGGER IF EXISTS restore_fail_before_reserve;
                 DROP TRIGGER IF EXISTS restore_fail_before_provider_ready;
                 DROP TRIGGER IF EXISTS restore_fail_before_local_mutation;
                 DROP TRIGGER IF EXISTS restore_fail_before_checkpoint;
                 DROP TRIGGER IF EXISTS restore_fail_before_completed;",
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .unwrap();
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn tokio_test_block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn restore_request(fixture: &Fixture, provider: Arc<ScriptedProvider>) -> (u16, Value) {
    call(
        fixture.store(),
        Some(provider),
        "POST",
        "/api/checkpoint/restore",
        json!({"agent":fixture.agent_id,"checkpoint":fixture.checkpoint_id,"expectedTree":fixture.expected_tree}),
    )
}

#[test]
fn restore_http_path_accepts_the_scripted_provider_receipt() {
    let fixture = Fixture::new();
    let provider = ScriptedProvider::scripted(Vec::new());
    let result = restore_request(&fixture, provider.clone());
    assert_eq!(result.0, 200, "{}", result.1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.operations.lock().unwrap().len(), 1);
    assert_eq!(
        git(&fixture.cwd, &["rev-parse", "HEAD^{tree}"]),
        fixture.expected_tree
    );
}

#[test]
fn fresh_start_restore_uses_the_broker_and_retries_not_sent_with_same_id() {
    let mut fixture = Fixture::new_start();
    let provider =
        ScriptedProvider::scripted(vec![RequestCertainty::NotSent, RequestCertainty::Written]);
    let first = restore_request(&fixture, provider.clone());
    assert_eq!(first.0, 400, "{}", first.1);
    let operation = provider.operations.lock().unwrap()[0].clone();
    assert!(operation.1.contains("ThreadStart"), "{operation:?}");

    fixture.restart_store();
    let replay = restore_request(&fixture, provider.clone());
    assert_eq!(replay.0, 200, "{}", replay.1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider.operations.lock().unwrap().as_slice(),
        &[operation.clone(), operation]
    );
    assert_restore_operation(&fixture, "completed");
    assert_workspace_tree(&fixture, &checkpoint_tree(&fixture));
}

#[test]
fn unknown_provider_outcome_stays_held_after_store_restart() {
    let mut fixture = Fixture::new();
    let provider = ScriptedProvider::scripted(vec![RequestCertainty::Unknown]);
    let first = restore_request(&fixture, provider.clone());
    assert_eq!(first.0, 400, "{}", first.1);
    let operation = provider.operations.lock().unwrap()[0].clone();

    fixture.restart_store();
    let replay = restore_request(&fixture, provider.clone());
    assert_eq!(replay.0, 400, "{}", replay.1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.operations.lock().unwrap().as_slice(), &[operation]);
    assert_restore_operation(&fixture, "recovery_required");
    assert_workspace_tree(&fixture, &fixture.expected_tree);
}

#[test]
fn not_sent_provider_outcome_retries_with_the_same_operation_id() {
    let mut fixture = Fixture::new();
    let provider =
        ScriptedProvider::scripted(vec![RequestCertainty::NotSent, RequestCertainty::Written]);
    let first = restore_request(&fixture, provider.clone());
    assert_eq!(first.0, 400, "{}", first.1);
    let operation = provider.operations.lock().unwrap()[0].clone();

    fixture.restart_store();
    let replay = restore_request(&fixture, provider.clone());
    assert_eq!(replay.0, 200, "{}", replay.1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider.operations.lock().unwrap().as_slice(),
        &[operation.clone(), operation]
    );
    assert_restore_operation(&fixture, "completed");
    assert_workspace_tree(&fixture, &checkpoint_tree(&fixture));
}

#[test]
fn committed_phase_failpoints_replay_without_repeating_provider_call() {
    let baseline = Fixture::new();
    let baseline_provider = ScriptedProvider::scripted(Vec::new());
    assert_eq!(restore_request(&baseline, baseline_provider).0, 200);
    let baseline_fingerprint = final_state_fingerprint(&baseline);
    for committed_write in 2..=5 {
        let mut fixture = Fixture::new();
        let provider = ScriptedProvider::scripted(Vec::new());
        let failed = call_with_failpoint(
            fixture.store(),
            Some(provider.clone()),
            "POST",
            "/api/checkpoint/restore",
            json!({"agent":fixture.agent_id,"checkpoint":fixture.checkpoint_id,"expectedTree":fixture.expected_tree}),
            Some(FailPoint::AfterCommit(committed_write)),
        );
        assert_ne!(
            failed.0, 200,
            "commit failpoint {committed_write} did not fire"
        );
        let before_restart = restore_operation(&fixture);
        fixture.restart_store();

        let replay = restore_request(&fixture, provider.clone());
        if before_restart.is_null() || !before_restart["provider"].is_null() {
            assert_eq!(replay.0, 200, "commit {committed_write}: {}", replay.1);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_restore_operation(&fixture, "completed");
            assert_workspace_tree(&fixture, &checkpoint_tree(&fixture));
            assert_eq!(final_state_fingerprint(&fixture), baseline_fingerprint);
            let completed_state = final_state_fingerprint(&fixture);
            let idempotent_replay = restore_request(&fixture, provider.clone());
            assert_eq!(idempotent_replay.0, 200, "{}", idempotent_replay.1);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(final_state_fingerprint(&fixture), completed_state);
        } else {
            assert_ne!(replay.0, 200, "uncertain provider outcome must stay held");
            assert!(provider.calls.load(Ordering::SeqCst) <= 1);
            assert_restore_operation(&fixture, "recovery_required");
            let held_calls = provider.calls.load(Ordering::SeqCst);
            let held_tree = git(&fixture.cwd, &["write-tree"]);
            assert!(held_tree == fixture.expected_tree || held_tree == checkpoint_tree(&fixture));
            let replay_again = restore_request(&fixture, provider.clone());
            assert_ne!(replay_again.0, 200, "held operation must remain held");
            assert_eq!(provider.calls.load(Ordering::SeqCst), held_calls);
            assert_eq!(git(&fixture.cwd, &["write-tree"]), held_tree);
        }
    }
}

#[test]
fn before_write_failpoints_replay_or_hold_without_resending() {
    let baseline = Fixture::new();
    let baseline_provider = ScriptedProvider::scripted(Vec::new());
    assert_eq!(restore_request(&baseline, baseline_provider).0, 200);
    let baseline_fingerprint = final_state_fingerprint(&baseline);
    for failpoint in [
        "reserve",
        "provider_ready",
        "local_mutation",
        "before_checkpoint",
        "completed",
    ] {
        let mut fixture = Fixture::new();
        let provider = ScriptedProvider::scripted(Vec::new());
        let failed = call_with_failpoint(
            fixture.store(),
            Some(provider.clone()),
            "POST",
            "/api/checkpoint/restore",
            json!({"agent":fixture.agent_id,"checkpoint":fixture.checkpoint_id,"expectedTree":fixture.expected_tree}),
            Some(FailPoint::BeforeWrite(failpoint)),
        );
        assert_ne!(failed.0, 200, "write failpoint {failpoint} did not fire");
        clear_before_write_triggers(fixture.store());
        fixture.restart_store();

        let replay = restore_request(&fixture, provider.clone());
        match failpoint {
            "provider_ready" => {
                assert_ne!(replay.0, 200, "unknown provider result must stay held");
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                assert_restore_operation(&fixture, "recovery_required");
                assert_workspace_tree(&fixture, &fixture.expected_tree);
            }
            _ => {
                assert_eq!(replay.0, 200, "write failpoint {failpoint}: {}", replay.1);
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                assert_restore_operation(&fixture, "completed");
                assert_workspace_tree(&fixture, &checkpoint_tree(&fixture));
                assert_eq!(final_state_fingerprint(&fixture), baseline_fingerprint);
            }
        }
    }
}

#[test]
fn git_command_boundaries_replay_to_the_uninterrupted_state() {
    let scratch = tempfile::tempdir().unwrap();
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$RESTORE_GIT_LOG\"\n".to_owned()
            + "match=0; before=0; case \"$RESTORE_GIT_FAIL\" in before-*) before=1 ;; esac\n"
            + "test_name=${RESTORE_GIT_FAIL#before-}\n"
            + "case \"$test_name:$*\" in\n"
            + "  commit-tree:*commit-tree*) match=1 ;;\n"
            + "  update-ref:*update-ref*) match=1 ;;\n"
            + "  read-tree-checkpoint:*read-tree\\ --reset\\ -u*) match=1 ;;\n"
            + "  read-tree-expected:*read-tree\\ [0-9a-f]*) match=1 ;;\n"
            + "esac\n"
            + "if [ \"$match\" -eq 1 ] && [ ! -e \"$RESTORE_GIT_ONCE\" ]; then\n"
            + "  if [ \"$before\" -eq 1 ]; then : > \"$RESTORE_GIT_ONCE\"; exit 86; fi\n"
            + "  /usr/bin/git \"$@\"; result=$?; [ \"$result\" -eq 0 ] || exit \"$result\"\n"
            + "  : > \"$RESTORE_GIT_ONCE\"; exit 86\n"
            + "fi\nexec /usr/bin/git \"$@\"\n",
    )
    .unwrap();
    assert!(
        Command::new("chmod")
            .args(["+x", wrapper.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    let executable = env::current_exe().unwrap();
    for failpoint in [
        "before-commit-tree",
        "commit-tree",
        "before-update-ref",
        "update-ref",
        "before-read-tree-expected",
        "read-tree-expected",
        "before-read-tree-checkpoint",
        "read-tree-checkpoint",
    ] {
        let once = scratch.path().join(format!("once-{failpoint}"));
        let log = scratch.path().join(format!("log-{failpoint}"));
        std::fs::write(&once, "baseline pass\n").unwrap();
        let path = format!("{}:{}", bin.display(), env::var("PATH").unwrap());
        let output = Command::new(&executable)
            .args([
                "--exact",
                "git_failpoint_child_runs_one_restart_replay",
                "--nocapture",
            ])
            .env("PATH", path)
            .env("RESTORE_GIT_FAIL", failpoint)
            .env("RESTORE_GIT_ONCE", &once)
            .env("RESTORE_GIT_LOG", &log)
            .env("RESTORE_GIT_CHILD", "1")
            .output()
            .unwrap();
        let log_text = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            output.status.success(),
            "Git failpoint {failpoint}: log={log_text} stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!log_text.is_empty(), "Git wrapper saw no commands");
    }
}

#[test]
fn git_failpoint_child_runs_one_restart_replay() {
    if env::var_os("RESTORE_GIT_CHILD").is_none() {
        return;
    }
    let baseline = Fixture::new();
    let baseline_provider = ScriptedProvider::scripted(Vec::new());
    assert_eq!(restore_request(&baseline, baseline_provider).0, 200);
    let baseline_fingerprint = final_state_fingerprint(&baseline);

    let mut fixture = Fixture::new();
    let provider = ScriptedProvider::scripted(Vec::new());
    let once = PathBuf::from(env::var_os("RESTORE_GIT_ONCE").unwrap());
    std::fs::remove_file(once).unwrap();
    let first = restore_request(&fixture, provider.clone());
    assert_ne!(first.0, 200, "configured Git failpoint did not fire");
    fixture.restart_store();
    let replay = restore_request(&fixture, provider.clone());
    assert_eq!(replay.0, 200, "{}", replay.1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_restore_operation(&fixture, "completed");
    assert_eq!(final_state_fingerprint(&fixture), baseline_fingerprint);
}

fn assert_restore_operation(fixture: &Fixture, phase: &str) {
    assert_eq!(restore_operation(fixture)["phase"], phase);
}

fn restore_operation(fixture: &Fixture) -> Value {
    let connection = fixture.store().read().unwrap();
    let operation: String = connection
        .query_row(
            "SELECT COALESCE((SELECT record FROM runtime_workspace_operations WHERE id=?1),'null')",
            [restore_operation_id(fixture)],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&operation).unwrap()
}

fn assert_workspace_tree(fixture: &Fixture, expected: &str) {
    assert_eq!(git(&fixture.cwd, &["write-tree"]), expected);
    assert!(
        Command::new("git")
            .current_dir(&fixture.cwd)
            .args(["diff", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
}

fn final_state_fingerprint(fixture: &Fixture) -> Value {
    let connection = fixture.store().read().unwrap();
    let operation: String = connection
        .query_row(
            "SELECT record FROM runtime_workspace_operations WHERE id=?1",
            [restore_operation_id(fixture)],
            |row| row.get(0),
        )
        .unwrap();
    let agent: String = connection
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [&fixture.agent_id],
            |row| row.get(0),
        )
        .unwrap();
    let checkpoint_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM runtime_checkpoints WHERE json_extract(record,'$.agent')=?1",
            [&fixture.agent_id],
            |row| row.get(0),
        )
        .unwrap();
    let item_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM runtime_items WHERE agent=?1",
            [&fixture.agent_id],
            |row| row.get(0),
        )
        .unwrap();
    let event_counts: (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), SUM(CASE WHEN status='pending' THEN 1 ELSE 0 END) FROM runtime_events WHERE agent=?1",
            [&fixture.agent_id],
            |row| Ok((row.get(0)?, row.get(1).unwrap_or(0))),
        )
        .unwrap();
    let reference_names = git(
        &fixture.cwd,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/codex-agents/checkpoints/{}", fixture.agent_id),
        ],
    );
    let mut reference_trees = reference_names
        .lines()
        .map(|reference| {
            git(
                &fixture.cwd,
                &["rev-parse", &format!("{reference}^{{tree}}")],
            )
        })
        .collect::<Vec<_>>();
    reference_trees.sort();
    let operation: Value = serde_json::from_str(&operation).unwrap();
    let agent: Value = serde_json::from_str(&agent).unwrap();
    json!({
        "phase":operation["phase"],
        "restoreCheckpointCount":checkpoint_count,
        "restoreItemCount":item_count,
        "eventCount":event_counts.0,
        "pendingEventCount":event_counts.1,
        "restoredCheckpointPresent":!agent["restoredCheckpoint"].is_null(),
        "threadId":agent["threadId"],
        "lastCompletedTurn":agent["lastCompletedTurn"],
        "workspaceOperation":agent["workspaceOperation"],
        "beforeCheckpointPresent":!operation["beforeCheckpoint"].is_null(),
        "providerThreadId":operation["provider"]["thread"]["id"],
        "workspaceTree":git(&fixture.cwd, &["write-tree"]),
        "checkpointRefTrees":reference_trees,
    })
}

fn checkpoint_tree(fixture: &Fixture) -> String {
    let connection = fixture.store().read().unwrap();
    let raw: String = connection
        .query_row(
            "SELECT record FROM runtime_checkpoints WHERE id=?1",
            [&fixture.checkpoint_id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str::<Value>(&raw).unwrap()["tree"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn restore_operation_id(fixture: &Fixture) -> String {
    let digest = studio_runtime_store::sha256_hex(fixture.expected_tree.as_bytes());
    format!(
        "restore:{}:{}:{}",
        fixture.agent_id,
        fixture.checkpoint_id,
        &digest[..32]
    )
}
