use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, MessageRepository, QueueEventId, QueueEventRepository, RuntimeLock,
    RuntimeStore, StoreError, TaskId, TaskRepository,
};

const CREATE_PYTHON_STATE: &str = r#"
import json, pathlib, sys
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
print(json.dumps({"agent_id": agent_id}))
"#;

const VERIFY_AND_WRITE_PYTHON: &str = r#"
import json, pathlib, sys
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
agent_id = sys.argv[3]
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agent = runtime.agent(agent_id)
assert agent["fixtureFlag"] is True and type(agent["fixtureInteger"]) is int
with runtime.read_db() as db:
    tasks = runtime.records(db, "tasks")
    task = next(row for row in tasks if row["id"] == "cross-task")
    event = db.execute("SELECT text FROM runtime_events WHERE id='cross-event'").fetchone()[0]
    message = db.execute("SELECT deliveries FROM runtime_chat_messages WHERE id='cross-message'").fetchone()[0]
assert task["rustFlag"] is True and task["fixtureInteger"] == 7
assert json.loads(event)["rust"] is True
assert json.loads(message)["delivery"] is True
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

    let rust_lock = RuntimeLock::acquire(&root).unwrap();
    let output = python(
        &root,
        &format!(
            "import sys; sys.path.insert(0, {:?}); from codex_file_lock import flock,LOCK_EX,LOCK_NB; f=open(sys.argv[1], 'a+');\ntry: flock(f,LOCK_EX|LOCK_NB)\nexcept BlockingIOError: print('busy')\nelse: print('acquired')",
            server_source().display().to_string()
        ),
        &[],
    );
    drop(rust_lock);
    assert_eq!(output.trim(), "busy");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn python_fixture_round_trips_through_rust_and_python_write_rollback_gate() {
    let root = temporary_state("cross-runtime");
    let created = python(&root, CREATE_PYTHON_STATE, &[]);
    let agent_id: String = serde_json::from_str::<Value>(&created).unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let store = RuntimeStore::open(Some(&root)).unwrap();
    let reader = store.read().unwrap();
    let agent = AgentRepository::get(&reader, &AgentId::from(agent_id.as_str()))
        .unwrap()
        .unwrap();
    assert_eq!(agent.extra["fixtureFlag"], json!(true));
    assert_eq!(agent.extra["fixtureInteger"].as_i64(), Some(7));
    let task = TaskRepository::get(&reader, &TaskId::from("cross-task"))
        .unwrap()
        .unwrap();
    assert_eq!(task.extra["fixtureFlag"], json!(true));
    assert_eq!(task.extra["fixtureInteger"].as_i64(), Some(7));
    let event = QueueEventRepository::get(&reader, &QueueEventId::from("cross-event"))
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&event.text).unwrap()["fixture"],
        true
    );
    let message = MessageRepository::get(
        &reader,
        &studio_runtime_store::ChatMessageId::from("cross-message"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(message.deliveries["delivery"], true);
    drop(reader);

    store
        .transaction(|transaction| {
            let mut task = TaskRepository::get(transaction, &TaskId::from("cross-task"))?
                .expect("Python fixture contains the task");
            task.extra.insert("rustFlag".to_owned(), json!(true));
            TaskRepository::put(transaction, &task)?;
            let mut event =
                QueueEventRepository::get(transaction, &QueueEventId::from("cross-event"))?
                    .expect("Python fixture contains the event");
            event.text = json!({"rust": true}).to_string();
            QueueEventRepository::put(transaction, &event)?;
            Ok(())
        })
        .unwrap();
    store.verify_schema().unwrap();
    drop(store);

    let verified = python(&root, VERIFY_AND_WRITE_PYTHON, &[agent_id.as_str()]);
    let python_output = serde_json::from_str::<Value>(&verified).unwrap();
    assert_eq!(python_output["task"]["rustFlag"], true);

    let reopened = RuntimeStore::open(Some(&root)).unwrap();
    let read_back = reopened.read().unwrap();
    let task = TaskRepository::get(&read_back, &TaskId::from("cross-task"))
        .unwrap()
        .unwrap();
    assert_eq!(task.extra["pythonRollbackGate"], true);
    reopened.verify_schema().unwrap();
    drop(read_back);
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

fn python(root: &Path, code: &str, args: &[&str]) -> String {
    let output = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()))
        .arg("-c")
        .arg(code)
        .arg(root)
        .arg(server_source())
        .args(args)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Python helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
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
    let mut child = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()))
        .arg("-c")
        .arg(code)
        .arg(root.join("runtime.lock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "held");
    let stdin = child.stdin.take().unwrap();
    (child, stdin)
}

fn stop_python_lock((mut child, mut stdin): (Child, ChildStdin)) {
    stdin.write_all(b"\n").unwrap();
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

fn temporary_state(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = PathBuf::from("/var/tmp/cs-rust-store/tests")
        .join(format!("{label}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    root
}
