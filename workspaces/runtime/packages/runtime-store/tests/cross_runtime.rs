use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, MessageRepository, QueueEventId, QueueEventRepository, RuntimeLock,
    RuntimeStore, StoreError, TaskId, TaskRepository,
};

const CREATE_PYTHON_STATE: &str = r#"
import json, pathlib, sqlite3, sys
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agent = runtime.create({"name":"Rust store fixture", "cwd":str(root), "prompt":"temporary"}, defer=True)
agent["fixtureFlag"] = True
agent["fixtureInteger"] = 7
with runtime.lock, runtime.db() as db:
    runtime.put(db, "agents", agent)
    task = {"id":"cross-task", "rootId":agent["id"], "status":"ready", "title":"fixture",
            "description":"", "owner":None, "created":1720000000.25, "updated":1720000000.5,
            "createdBy":agent["id"], "version":1, "dependencies":[], "decisions":[], "results":[],
            "fixtureFlag":True, "fixtureInteger":7}
    runtime.put(db, "tasks", task)
    runtime.enqueue(db, agent, "work_review", json.dumps({"fixture": True}), key="cross-event")
    db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
               ("cross-message", "room-fixture", agent["id"], "Python message", 1720000000.5,
                json.dumps({"delivery": True, "attempt": 1})))
agent_id = agent["id"]
runtime.close()
reference = pathlib.Path(sys.argv[3])
reference.mkdir(parents=True, exist_ok=True)
for name in ("canvas.sqlite3", "analytics.sqlite3"):
    with sqlite3.connect(root / name) as source, sqlite3.connect(reference / name) as target:
        source.backup(target)
(reference / "runtime.lock").touch()
print(json.dumps({"agent_id": agent_id}))
"#;

const VERIFY_AND_WRITE_PYTHON: &str = r#"
import json, pathlib, sqlite3, sys
import hashlib
sys.path.insert(0, sys.argv[2])
from codex_sync_entities import register_functions
from codex_runtime import Runtime
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
agent_id = sys.argv[3]
reference_root = pathlib.Path(sys.argv[4])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agent = runtime.agent(agent_id)
assert agent["fixtureFlag"] is True and type(agent["fixtureInteger"]) is int
assert agent["rustAgentFlag"] is True
with runtime.read_db() as db:
    tasks = runtime.records(db, "tasks")
    task = next(row for row in tasks if row["id"] == "cross-task")
    event = db.execute("SELECT text FROM runtime_events WHERE id='cross-event'").fetchone()[0]
    rust_event_row = db.execute(
        "SELECT seq, payload, hash, deleted FROM sync_entities WHERE collection='event' AND id='cross-event'"
    ).fetchone()
    message = db.execute("SELECT deliveries FROM runtime_chat_messages WHERE id='cross-message'").fetchone()[0]
assert task["rustFlag"] is True and task["fixtureInteger"] == 7
assert json.loads(event)["rust"] is True
entity = json.loads(rust_event_row[1])
assert entity["collection"] == "event" and entity["id"] == "cross-event"
assert entity["value"]["id"] == "cross-event" and "rust" not in entity["value"]
assert hashlib.sha256(rust_event_row[1].encode("utf-8")).hexdigest() == rust_event_row[2]
assert json.loads(message)["delivery"] is True and json.loads(message)["rustReceipt"] is True
with sqlite3.connect(reference_root / "canvas.sqlite3") as db:
    register_functions(db)
    db.execute("BEGIN IMMEDIATE")
    db.execute("UPDATE runtime_events SET status='running' WHERE id='cross-event'")
    python_event_row = db.execute(
        "SELECT seq, payload, hash, deleted FROM sync_entities WHERE collection='event' AND id='cross-event'"
    ).fetchone()
assert tuple(rust_event_row) == tuple(python_event_row), (tuple(rust_event_row), tuple(python_event_row))
task["pythonRollbackGate"] = True
with runtime.lock, runtime.db() as db:
    runtime.put(db, "tasks", task)
runtime.close()
print(json.dumps({"task": task, "message": json.loads(message)}))
"#;

#[test]
fn python_flock_excludes_rust_and_rust_flock_excludes_python() {
    let root = temporary_state("locks");
    let python_lock = start_python_lock(&root);
    assert!(matches!(
        RuntimeLock::acquire(&root),
        Err(StoreError::AlreadyOwned)
    ));
    stop_python_lock(python_lock);

    let rust_lock = RuntimeLock::acquire(&root).expect("cross-runtime test operation succeeds");
    let output = python(
        &root,
        &format!(
            "import sys, pathlib; sys.path.insert(0, {:?}); from codex_file_lock import flock,LOCK_EX,LOCK_NB; f=open(pathlib.Path(sys.argv[1]) / 'runtime.lock', 'a+');\ntry: flock(f,LOCK_EX|LOCK_NB)\nexcept BlockingIOError: print('busy')\nelse: print('acquired')",
            server_source().display().to_string()
        ),
        &[],
    );
    drop(rust_lock);
    assert_eq!(output.trim(), "busy");
    root.cleanup().expect("temporary lock state is removed");
}

#[test]
fn python_fixture_round_trips_through_rust_and_python_write_rollback_gate() {
    let root = temporary_state("cross-runtime");
    let python_reference = temporary_state("python-reference");
    let created = python(
        &root,
        CREATE_PYTHON_STATE,
        &[python_reference.to_str().expect("temporary path is UTF-8")],
    );
    let agent_id: String = serde_json::from_str::<Value>(&created)
        .expect("cross-runtime test operation succeeds")["agent_id"]
        .as_str()
        .expect("cross-runtime test operation succeeds")
        .to_owned();

    let store = RuntimeStore::open(Some(&root)).expect("cross-runtime test operation succeeds");
    let reader = store.read().expect("cross-runtime test operation succeeds");
    let agent = AgentRepository::get(&reader, &AgentId::from(agent_id.as_str()))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(agent.extra["fixtureFlag"], json!(true));
    assert_eq!(agent.extra["fixtureInteger"].as_i64(), Some(7));
    let task = TaskRepository::get(&reader, &TaskId::from("cross-task"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(task.extra["fixtureFlag"], json!(true));
    assert_eq!(task.extra["fixtureInteger"].as_i64(), Some(7));
    let event = QueueEventRepository::get(&reader, &QueueEventId::from("cross-event"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(
        serde_json::from_str::<Value>(&event.text).expect("cross-runtime test operation succeeds")
            ["fixture"],
        true
    );
    let message = MessageRepository::get(
        &reader,
        &studio_runtime_store::ChatMessageId::from("cross-message"),
    )
    .expect("cross-runtime test operation succeeds")
    .expect("cross-runtime test operation succeeds");
    assert_eq!(message.deliveries["delivery"], true);
    drop(reader);

    let failure: Result<(), StoreError> = store.transaction(|_| {
        Err(StoreError::SchemaMismatch(
            "original transaction failure".to_owned(),
        ))
    });
    assert!(matches!(
        failure,
        Err(StoreError::SchemaMismatch(message)) if message == "original transaction failure"
    ));

    store
        .transaction(|transaction| {
            let mut event =
                QueueEventRepository::get(transaction, &QueueEventId::from("cross-event"))?
                    .expect("Python fixture contains the event");
            event.text = json!({"rust": true}).to_string();
            event.status = "running".to_owned();
            QueueEventRepository::put(transaction, &event)?;
            let mut agent = AgentRepository::get(transaction, &AgentId::from(agent_id.as_str()))?
                .expect("Python fixture contains the agent");
            agent.extra.insert("rustAgentFlag".to_owned(), json!(true));
            AgentRepository::put(transaction, &agent)?;
            let mut task = TaskRepository::get(transaction, &TaskId::from("cross-task"))?
                .expect("Python fixture contains the task");
            task.extra.insert("rustFlag".to_owned(), json!(true));
            TaskRepository::put(transaction, &task)?;
            let mut message = MessageRepository::get(
                transaction,
                &studio_runtime_store::ChatMessageId::from("cross-message"),
            )?
            .expect("Python fixture contains the chat message");
            message.deliveries["rustReceipt"] = json!(true);
            MessageRepository::put(transaction, &message)?;
            Ok(())
        })
        .expect("cross-runtime test operation succeeds");
    store
        .verify_schema()
        .expect("cross-runtime test operation succeeds");
    drop(store);

    let verified = python(
        &root,
        VERIFY_AND_WRITE_PYTHON,
        &[
            agent_id.as_str(),
            python_reference.to_str().expect("temporary path is UTF-8"),
        ],
    );
    let python_output =
        serde_json::from_str::<Value>(&verified).expect("cross-runtime test operation succeeds");
    assert_eq!(python_output["task"]["rustFlag"], true);

    let reopened = RuntimeStore::open(Some(&root)).expect("cross-runtime test operation succeeds");
    let read_back = reopened
        .read()
        .expect("cross-runtime test operation succeeds");
    let task = TaskRepository::get(&read_back, &TaskId::from("cross-task"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(task.extra["pythonRollbackGate"], true);
    reopened
        .verify_schema()
        .expect("cross-runtime test operation succeeds");
    drop(read_back);
    drop(reopened);
    python_reference
        .cleanup()
        .expect("Python reference state is removed");
    root.cleanup().expect("temporary Python state is removed");
}

fn python(root: &Path, code: &str, args: &[&str]) -> String {
    let output = Command::new(python_executable())
        .arg("-c")
        .arg(code)
        .arg(root)
        .arg(server_source())
        .args(args)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("cross-runtime test operation succeeds");
    assert!(
        output.status.success(),
        "Python helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cross-runtime test operation succeeds")
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned()
}

fn start_python_lock(root: &Path) -> (Child, ChildStdin) {
    let code = format!(
        "import sys; sys.path.insert(0, {:?}); from codex_file_lock import flock,LOCK_EX,LOCK_NB; f=open(sys.argv[1], 'a+'); flock(f,LOCK_EX|LOCK_NB); print('held',flush=True); sys.stdin.readline()",
        server_source().display().to_string()
    );
    let mut child = Command::new(python_executable())
        .arg("-c")
        .arg(code)
        .arg(root.join("runtime.lock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .spawn()
        .expect("cross-runtime test operation succeeds");
    let stdout = child
        .stdout
        .take()
        .expect("cross-runtime test operation succeeds");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("cross-runtime test operation succeeds");
    assert_eq!(line.trim(), "held");
    let stdin = child
        .stdin
        .take()
        .expect("cross-runtime test operation succeeds");
    (child, stdin)
}

fn stop_python_lock((mut child, mut stdin): (Child, ChildStdin)) {
    stdin
        .write_all(b"\n")
        .expect("cross-runtime test operation succeeds");
    drop(stdin);
    assert!(
        child
            .wait()
            .expect("cross-runtime test operation succeeds")
            .success()
    );
}

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

fn python_executable() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON.get_or_init(resolve_python).as_path()
}

fn resolve_python() -> PathBuf {
    let selected = std::env::var_os("CODEX_AGENTS_PYTHON")
        .or_else(|| std::env::var_os("PYTHON"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let output = Command::new("python3")
                .arg(server_source().join("codex_python.py"))
                .output()
                .expect("cross-runtime tests need python3 to resolve the managed server runtime");
            assert!(
                output.status.success(),
                "cross-runtime tests require the prepared server Python runtime; run `python3 workspaces/runtime/apps/server/src/install-cli.py --dev --bin-dir <test-bin>` first. Resolver error: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            PathBuf::from(
                String::from_utf8(output.stdout)
                    .expect("Python path is UTF-8")
                    .trim(),
            )
        });

    let probe = Command::new(&selected)
        .arg("-c")
        .arg("import sys; sys.path.insert(0, sys.argv[1]); import codex_runtime; import studio_operations_native")
        .arg(server_source())
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "cross-runtime tests require the prepared server Python interpreter at {}: {error}",
                selected.display()
            )
        });
    assert!(
        probe.status.success(),
        "cross-runtime tests require server requirements and the built studio_operations_native module; prepare them with `cargo build --release -p studio-operations-python` and `python3 workspaces/runtime/apps/server/src/install-cli.py --dev --bin-dir <test-bin>`. Interpreter {} failed its import probe: {}",
        selected.display(),
        String::from_utf8_lossy(&probe.stderr)
    );
    selected
}

struct TemporaryState(PathBuf);

impl TemporaryState {
    fn cleanup(self) -> std::io::Result<()> {
        fs::remove_dir_all(&self.0)
    }
}

impl std::ops::Deref for TemporaryState {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TemporaryState {
    fn drop(&mut self) {
        let _cleanup_result = fs::remove_dir_all(&self.0);
    }
}

fn temporary_state(label: &str) -> TemporaryState {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("cross-runtime test operation succeeds")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "codex-runtime-store-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("cross-runtime test operation succeeds");
    TemporaryState(root)
}
