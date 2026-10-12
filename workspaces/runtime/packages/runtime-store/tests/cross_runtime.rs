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
agent["fixtureUnicode"] = "Zażółć 東京\n\u0000"
agent["fixtureNested"] = {"items":[0.1,5.0,9007199254740993,True,{"text":"🦀\t"}]}
with runtime.lock, runtime.db() as db:
    runtime.put(db, "agents", agent)
    task = {"id":"cross-task", "rootId":agent["id"], "status":"ready", "title":"fixture",
            "description":"", "owner":None, "created":1e-7, "updated":1e21,
            "createdBy":agent["id"], "version":1, "dependencies":[], "decisions":[], "results":[],
            "fixtureFlag":True, "fixtureInteger":7}
    runtime.put(db, "tasks", task)
    runtime.enqueue(db, agent, "work_review", json.dumps({"fixture": True}), key="cross-event")
    db.execute("UPDATE runtime_events SET created=1720000000.5 WHERE id='cross-event'")
    db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
               ("cross-message", "room-fixture", agent["id"], "Python message", 1720000000.5,
                json.dumps({"delivery": True, "attempt": 1})))
    numeric_events = [
        ("numeric-created-precision", 1791788985.5209818),
        ("numeric-created-tenth", 0.1),
        ("numeric-created-large-exponent", 1e21),
        ("numeric-created-integer", 7),
        ("numeric-created-integer-real", 5.0),
        ("numeric-created-large-integer-real", 1791788985.0),
        ("numeric-created-17-digits", 1.2345678901234567),
        ("numeric-created-over-2pow53", 9007199254740993),
        ("numeric-created-negative", -0.1),
        ("numeric-created-negative-exponent", -1e-7),
    ]
    for event_id, created in numeric_events:
        db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) "
                   "VALUES(?,?,?,?,'pending',?,0,NULL,?)",
                   (event_id, agent["id"], "work_review", "{}", created,
                    "Zażółć 東京\n\u0000" if event_id == "numeric-created-precision" else None))
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
rust_sqlite_version = sys.argv[5]
rust_json_object = sys.argv[6]
with sqlite3.connect(":memory:") as db:
    python_json_object = db.execute("SELECT json_object('x', 1791788985.5209818)").fetchone()[0]
diagnostics = {
    "python_sqlite_version": sqlite3.sqlite_version,
    "rust_sqlite_version": rust_sqlite_version,
    "python_json_object": python_json_object,
    "rust_json_object": rust_json_object,
}
def check(condition, description):
    if not condition:
        raise AssertionError(f"{description}; SQLite diagnostics={json.dumps(diagnostics, sort_keys=True)}")
with sqlite3.connect(root / "canvas.sqlite3") as raw_db:
    persisted_agent_record = raw_db.execute(
        "SELECT record FROM runtime_agents WHERE id=?", (agent_id,)
    ).fetchone()[0]
    persisted_task_record = raw_db.execute(
        "SELECT record FROM runtime_tasks WHERE id='cross-task'"
    ).fetchone()[0]
with sqlite3.connect(reference_root / "canvas.sqlite3") as raw_db:
    python_agent_record = raw_db.execute(
        "SELECT record FROM runtime_agents WHERE id=?", (agent_id,)
    ).fetchone()[0]
    python_task_record = raw_db.execute(
        "SELECT record FROM runtime_tasks WHERE id='cross-task'"
    ).fetchone()[0]
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agent = runtime.agent(agent_id)
check(agent["fixtureFlag"] is True and type(agent["fixtureInteger"]) is int, "agent boolean and integer distinctions survive")
check(agent["rustAgentFlag"] is True, "Rust agent field survives Python read")
check(agent["fixtureUnicode"] == "Zażółć 東京\n\u0000", "agent Unicode/control text survives")
check(agent["fixtureNested"] == {"items":[0.1,5.0,9007199254740993,True,{"text":"🦀\t"}]}, "nested value survives")
check(json.loads(persisted_agent_record) == agent, "agent record parses to the same value")
check(": " in python_agent_record and ": " not in persisted_agent_record, "record JSON spacing is intentionally different")
check(r"\u6771\u4eac" in python_agent_record and "東京" in persisted_agent_record, "record Unicode escaping is intentionally different")
check(r"\n\u0000" in python_agent_record and r"\n\u0000" in persisted_agent_record, "record control escaping agrees")
check(python_agent_record.index('"fixtureUnicode"') < python_agent_record.index('"fixtureNested"'), "Python record preserves insertion order")
check(persisted_agent_record.index('"fixtureNested"') < persisted_agent_record.index('"fixtureUnicode"'), "Rust record uses map key ordering")
check(r"\ud83e\udd80" in python_agent_record and "🦀" in persisted_agent_record, "emoji escaping is intentionally different")
with runtime.read_db() as db:
    tasks = runtime.records(db, "tasks")
    task = next(row for row in tasks if row["id"] == "cross-task")
    event = db.execute("SELECT text FROM runtime_events WHERE id='cross-event'").fetchone()[0]
    rust_event_row = db.execute(
        "SELECT seq, payload, hash, deleted FROM sync_entities WHERE collection='event' AND id='cross-event'"
    ).fetchone()
    message = db.execute("SELECT deliveries FROM runtime_chat_messages WHERE id='cross-message'").fetchone()[0]
check(task["rustFlag"] is True and task["fixtureInteger"] == 7, "task fields survive Python read")
check(task["created"] == 1e-7 and task["updated"] == 1e21, "task numeric values survive Python read")
check(json.loads(persisted_task_record) == task, "task record parses to the same value")
rust_task_created_token = persisted_task_record.split('"created":', 1)[1].split(',', 1)[0]
python_task_created_token = python_task_record.split('"created": ', 1)[1].split(',', 1)[0]
check(rust_task_created_token == "1e-7" and python_task_created_token == "1e-07", "record float exponent spellings differ by design")
check(json.loads(event)["rust"] is True, "Python reads Rust event queue JSON")
entity = json.loads(rust_event_row[1])
check(entity["collection"] == "event" and entity["id"] == "cross-event", "sync event envelope identifies the event")
check(entity["value"]["id"] == "cross-event" and "rust" not in entity["value"], "sync projection filters queue-only fields")
check(hashlib.sha256(rust_event_row[1].encode("utf-8")).hexdigest() == rust_event_row[2], "Rust sync hash matches payload bytes")
check(json.loads(message)["delivery"] is True and json.loads(message)["rustReceipt"] is True, "Python reads Rust message JSON")
with sqlite3.connect(reference_root / "canvas.sqlite3") as db:
    register_functions(db)
    db.execute("BEGIN IMMEDIATE")
    db.execute("UPDATE runtime_events SET status='running' WHERE id='cross-event'")
    numeric_event_ids = (
        "numeric-created-precision", "numeric-created-tenth", "numeric-created-large-exponent",
        "numeric-created-integer",
        "numeric-created-integer-real", "numeric-created-large-integer-real",
        "numeric-created-17-digits", "numeric-created-over-2pow53", "numeric-created-negative",
        "numeric-created-negative-exponent",
    )
    for event_id in numeric_event_ids:
        db.execute("UPDATE runtime_events SET status='running' WHERE id=?", (event_id,))
    python_numeric_rows = db.execute(
        "SELECT seq,payload,hash,deleted FROM sync_entities "
        "WHERE collection='event' AND id LIKE 'numeric-created-%' ORDER BY id"
    ).fetchall()
    python_event_row = db.execute(
        "SELECT seq, payload, hash, deleted FROM sync_entities WHERE collection='event' AND id='cross-event'"
    ).fetchone()
check(tuple(rust_event_row) == tuple(python_event_row), "stable event payload/hash agrees across runtimes")
for row in python_numeric_rows:
    check(hashlib.sha256(row[1].encode("utf-8")).hexdigest() == row[2], f"Python numeric row hash is valid: {row[1]}")
task["pythonRollbackGate"] = True
with runtime.lock, runtime.db() as db:
    runtime.put(db, "tasks", task)
runtime.close()
print(json.dumps({"task": task, "message": json.loads(message), "numeric_rows": python_numeric_rows,
                  "rust_task_created_token": rust_task_created_token,
                  "python_task_created_token": python_task_created_token,
                  "diagnostics": diagnostics}))
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
    assert_eq!(
        agent.extra["fixtureUnicode"],
        json!("Zażółć 東京\n\u{0000}")
    );
    assert_eq!(
        agent.extra["fixtureNested"],
        json!({"items":[0.1,5.0,9007199254740993_i64,true,{"text":"🦀\t"}]})
    );
    let task = TaskRepository::get(&reader, &TaskId::from("cross-task"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(task.extra["fixtureFlag"], json!(true));
    assert_eq!(task.extra["fixtureInteger"].as_i64(), Some(7));
    assert_eq!(task.created, 1e-7);
    assert_eq!(task.updated, 1e21);
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
            for event_id in [
                "numeric-created-precision",
                "numeric-created-tenth",
                "numeric-created-large-exponent",
                "numeric-created-integer",
                "numeric-created-integer-real",
                "numeric-created-large-integer-real",
                "numeric-created-17-digits",
                "numeric-created-over-2pow53",
                "numeric-created-negative",
                "numeric-created-negative-exponent",
            ] {
                let mut numeric_event =
                    QueueEventRepository::get(transaction, &QueueEventId::from(event_id))?
                        .expect("Python fixture contains each adversarial numeric event");
                numeric_event.status = "running".to_owned();
                QueueEventRepository::put(transaction, &numeric_event)?;
            }
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
    let reader = store.read().expect("cross-runtime test operation succeeds");
    let rust_numeric_rows = {
        let mut statement = reader
            .prepare("SELECT seq,payload,hash,deleted FROM sync_entities WHERE collection='event' AND id LIKE 'numeric-created-%' ORDER BY id")
            .expect("cross-runtime test operation succeeds");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .expect("cross-runtime test operation succeeds")
            .collect::<Result<Vec<_>, _>>()
            .expect("cross-runtime test operation succeeds")
    };
    let rust_sqlite_version = rusqlite::version().to_owned();
    let rust_json_object: String = {
        let reader = store.read().expect("cross-runtime test operation succeeds");
        reader
            .query_row("SELECT json_object('x', 1791788985.5209818)", [], |row| {
                row.get(0)
            })
            .expect("Rust SQLite JSON function is available")
    };
    drop(reader);
    drop(store);

    let verified = python(
        &root,
        VERIFY_AND_WRITE_PYTHON,
        &[
            agent_id.as_str(),
            python_reference.to_str().expect("temporary path is UTF-8"),
            rust_sqlite_version.as_str(),
            rust_json_object.as_str(),
        ],
    );
    let python_output =
        serde_json::from_str::<Value>(&verified).expect("cross-runtime test operation succeeds");
    assert_eq!(python_output["task"]["rustFlag"], true);
    assert_eq!(python_output["rust_task_created_token"], "1e-7");
    assert_eq!(python_output["python_task_created_token"], "1e-07");
    let diagnostic = format!("SQLite diagnostics: {}", python_output["diagnostics"]);
    let python_numeric_rows = python_output["numeric_rows"]
        .as_array()
        .expect("Python helper returns numeric event rows");
    assert_eq!(rust_numeric_rows.len(), 10, "{diagnostic}");
    assert_eq!(python_numeric_rows.len(), 10, "{diagnostic}");
    let exact_payload_ids = [
        "numeric-created-tenth",
        "numeric-created-large-exponent",
        "numeric-created-integer",
        "numeric-created-integer-real",
        "numeric-created-large-integer-real",
        "numeric-created-negative",
        "numeric-created-negative-exponent",
    ];
    let high_precision_ids = [
        "numeric-created-precision",
        "numeric-created-17-digits",
        "numeric-created-over-2pow53",
    ];
    let mut rust_by_id = std::collections::BTreeMap::new();
    for row in &rust_numeric_rows {
        let payload: Value = serde_json::from_str(&row.1).expect("Rust payload is JSON");
        rust_by_id.insert(payload["id"].as_str().unwrap_or_default().to_owned(), row);
    }
    let mut python_by_id = std::collections::BTreeMap::new();
    for row in python_numeric_rows {
        let payload_text = row[1].as_str().expect("Python sync payload is JSON text");
        let payload: Value = serde_json::from_str(payload_text).expect("Python payload is JSON");
        python_by_id.insert(payload["id"].as_str().unwrap_or_default().to_owned(), row);
    }
    for event_id in exact_payload_ids {
        let rust_row = rust_by_id[event_id];
        let python_row = python_by_id[event_id];
        assert_eq!(
            rust_row.1,
            python_row[1].as_str().expect("Python payload is text"),
            "{event_id}: {diagnostic}"
        );
        assert_eq!(
            rust_row.2,
            python_row[2].as_str().expect("Python hash is text"),
            "{event_id}: {diagnostic}"
        );
    }
    for event_id in high_precision_ids {
        let rust_row = rust_by_id[event_id];
        let python_row = python_by_id[event_id];
        let rust_value: Value = serde_json::from_str(&rust_row.1).expect("Rust payload is JSON");
        let python_value: Value =
            serde_json::from_str(python_row[1].as_str().expect("Python payload is text"))
                .expect("Python payload is JSON");
        let rust_number = rust_value["value"]["created"]
            .as_f64()
            .expect("Rust created is numeric");
        let python_number = python_value["value"]["created"]
            .as_f64()
            .expect("Python created is numeric");
        let sqlite_15_digit_rounding_bound = rust_number.abs().max(1.0) * 5.1e-15;
        assert!(
            (rust_number - python_number).abs() <= sqlite_15_digit_rounding_bound,
            "{event_id}: values differ beyond SQLite's documented 15-significant-digit rendering; {diagnostic}; Rust payload={}, Python payload={}",
            rust_row.1,
            python_row[1]
        );
        let expected = match event_id {
            "numeric-created-precision" => 1_791_788_985.520_981_8,
            "numeric-created-17-digits" => 1.234_567_890_123_456_7,
            _ => 9_007_199_254_740_992.0,
        };
        assert_eq!(
            rust_number, expected,
            "Rust projection must preserve the stored binary64 value; {diagnostic}"
        );
    }

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
