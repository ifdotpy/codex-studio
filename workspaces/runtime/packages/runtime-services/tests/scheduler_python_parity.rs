use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_services::event_write::{AnnotationWrite, annotate};
use studio_runtime_services::scheduler::{
    TurnStart, claim_intent, mark_submitted_for_provider, recover_on_startup, settle_start_outcome,
    settle_started_provider,
};
use studio_runtime_store::{AgentId, AgentRepository, AgentStatus, RuntimeStore};

const CREATE_PYTHON_EVENTS: &str = r#"
import json, pathlib, sys
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
from codex_wakeups import pending_batch
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agent = runtime.create({"name":"scheduler fixture", "cwd":str(root), "prompt":"temporary"}, defer=True)
agent["autoWake"] = True
agent["status"] = "queued"
agent.update(accountKey="default", threadId="fixture-thread", model="fixture-model")
with runtime.lock, runtime.db() as db:
    runtime.put(db, "agents", agent)
    for event_id, created in (("python-later", 2.0), ("python-first", 1.0), ("python-tie", 1.0)):
        db.execute("INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)",
                   (event_id, agent["id"], "user", event_id, "pending", created, agent["epoch"], None, None))
    selected = pending_batch(runtime, db, agent)
    print(json.dumps({"agent":agent["id"], "epoch":agent["epoch"], "events":[item["id"] for item in selected]}))
runtime.close()
"#;

fn fixture() -> (TemporaryState, RuntimeStore, String, i64, Value) {
    let state = TemporaryState::new();
    let fixture: Value = serde_json::from_str(&run_python(&state.0, CREATE_PYTHON_EVENTS, &[]))
        .expect("Python fixture JSON");
    let store = RuntimeStore::open(Some(&state.0)).expect("Python-written store");
    let agent = fixture["agent"].as_str().expect("agent ID").to_owned();
    let epoch = fixture["epoch"].as_i64().expect("epoch");
    (state, store, agent, epoch, fixture)
}
#[test]
fn pending_selection_matches_python_and_claims_rust_event_write_events() {
    let (_state, store, agent, epoch, fixture) = fixture();
    let intent = claim_intent(&store, &agent, epoch, "python-attempt", 10.0)
        .expect("claim")
        .expect("input");
    assert_eq!(
        json!(intent.events.iter().map(|e| &e.id).collect::<Vec<_>>()),
        fixture["events"]
    );
    let reader = store.read().expect("read");
    let reserved: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='reserved'",
            [&agent],
            |r| r.get(0),
        )
        .expect("reserved events");
    assert_eq!(reserved, 3);
    drop(reader);
    settle_start_outcome(&store, &intent, TurnStart::NotSubmitted)
        .expect("unsent restores pending");
    store
        .transaction(|tx| {
            annotate(
                tx,
                AnnotationWrite {
                    agent_id: agent.clone(),
                    id: Some("rust-written".into()),
                    path: "README.md".into(),
                    line: 1,
                    text: "scheduler annotation".into(),
                    turn_id: None,
                    signature: "scheduler-test".into(),
                },
            )
            .map_err(|_| {
                studio_runtime_store::StoreError::SchemaMismatch("annotation fixture".into())
            })?;
            Ok(())
        })
        .expect("Rust event writer");
    let intent = claim_intent(&store, &agent, epoch, "accepted-attempt", 13.0)
        .expect("claim")
        .expect("input");
    assert_eq!(
        intent.events.last().map(|e| e.id.as_str()),
        Some("annotation:rust-written")
    );
    assert!(mark_submitted_for_provider(&store, &intent, "accepted-request", 1).expect("submit"));
    assert_eq!(
        settle_started_provider(&store, &intent, "accepted-request", 1, "turn-1").expect("accept"),
        Some(false)
    );
    let reader = store.read().expect("read");
    let delivered: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='delivered'",
            [&agent],
            |r| r.get(0),
        )
        .expect("delivered");
    assert_eq!(delivered, 4);
    let projected:(String,bool,String)=reader.query_row("SELECT json_extract(payload,'$.value.status'),json_extract(payload,'$.value.inFlight'),json_extract(payload,'$.value.turnId') FROM sync_entities WHERE collection='agent' AND id=?1",[&agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).expect("projection");
    assert_eq!(projected, ("running".into(), true, "turn-1".into()));
}
#[test]
fn submitted_marker_survives_settlement_write_failure() {
    let (_state, store, agent, epoch, _) = fixture();
    let intent = claim_intent(&store, &agent, epoch, "failure", 10.0)
        .expect("claim")
        .expect("input");
    assert!(mark_submitted_for_provider(&store, &intent, "failure-request", 1).expect("submit"));
    store.transaction(|tx| {tx.execute_batch("CREATE TRIGGER reject_delivery BEFORE UPDATE OF status ON runtime_events WHEN NEW.status='delivered' BEGIN SELECT RAISE(ABORT,'injected settlement failure'); END;")?;Ok(())}).expect("failure injection");
    assert!(settle_started_provider(&store, &intent, "failure-request", 1, "accepted").is_err());
    let reader = store.read().expect("read");
    let record = AgentRepository::get(&reader, &AgentId::from(agent.as_str()))
        .expect("agent")
        .expect("present");
    assert_eq!(record.extra["startAttempt"]["submitted"], true);
    let dispatching: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='dispatching'",
            [&agent],
            |r| r.get(0),
        )
        .expect("dispatching");
    assert_eq!(dispatching, 3);
    drop(reader);
    assert_eq!(recover_on_startup(&store).expect("uncertain recovery"), 1);
    let record = AgentRepository::get(&store.read().expect("read"), &AgentId::from(agent.as_str()))
        .expect("agent")
        .expect("present");
    assert_eq!(record.status, AgentStatus::Interrupted);
    assert_eq!(record.extra["restartRecovery"]["stage"], "held");
}
#[test]
fn restart_keeps_a_scoped_receipt_without_replaying_events() {
    let (state, store, agent, epoch, _) = fixture();
    let intent = claim_intent(&store, &agent, epoch, "restart", 10.0)
        .expect("claim")
        .expect("input");
    assert!(mark_submitted_for_provider(&store, &intent, "restart-request", 1).expect("submit"));
    assert_eq!(
        settle_started_provider(&store, &intent, "restart-request", 1, "known").expect("accept"),
        Some(false)
    );
    drop(store);
    let store = RuntimeStore::open(Some(&state.0)).expect("reopen");
    assert_eq!(recover_on_startup(&store).expect("recovery"), 1);
    let reader = store.read().expect("read");
    let record = AgentRepository::get(&reader, &AgentId::from(agent.as_str()))
        .expect("agent")
        .expect("present");
    assert_eq!(record.status, AgentStatus::Interrupted);
    assert_eq!(record.extra["restartRecovery"]["turnId"], "known");
    assert_eq!(record.extra["inFlight"], false);
    assert_eq!(record.extra["autoWake"], false);
    let counts:(i64,i64)=reader.query_row("SELECT SUM(status='delivered'),SUM(status='pending') FROM runtime_events WHERE agent=?1",[&agent],|r|Ok((r.get(0)?,r.get(1)?))).expect("events");
    assert_eq!(counts, (3, 0));
}
#[test]
fn proven_not_submitted_releases_projected_lifecycle() {
    let (_state, store, agent, epoch, _) = fixture();
    let intent = claim_intent(&store, &agent, epoch, "unsent", 10.0)
        .expect("claim")
        .expect("input");
    settle_start_outcome(&store, &intent, TurnStart::NotSubmitted).expect("unsent");
    let reader = store.read().expect("read");
    let projected:(String,bool,Option<String>)=reader.query_row("SELECT json_extract(payload,'$.value.status'),json_extract(payload,'$.value.inFlight'),json_extract(payload,'$.value.turnId') FROM sync_entities WHERE collection='agent' AND id=?1",[&agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).expect("projection");
    assert_eq!(projected, ("queued".into(), false, None));
    let pending: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='pending'",
            [&agent],
            |r| r.get(0),
        )
        .expect("pending");
    assert_eq!(pending, 3);
}
#[test]
fn late_acceptance_preserves_a_stopped_agent_and_delivers_exact_input() {
    let (_state, store, agent, epoch, _) = fixture();
    let intent = claim_intent(&store, &agent, epoch, "stop", 10.0)
        .expect("claim")
        .expect("input");
    assert!(mark_submitted_for_provider(&store, &intent, "stopped-request", 1).expect("submit"));
    store
        .transaction(|tx| {
            let mut record =
                AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.expect("agent");
            record.epoch += 1;
            record.status = AgentStatus::Paused;
            record.extra.insert("autoWake".into(), json!(false));
            AgentRepository::put(tx, &record)
        })
        .expect("stop revocation");
    settle_start_outcome(&store, &intent, TurnStart::Unknown).expect("unknown retains input");
    let mut stale = intent.clone();
    stale.epoch += 2;
    assert_eq!(
        settle_started_provider(&store, &stale, "stopped-request", 1, "late").expect("stale"),
        None
    );
    assert_eq!(
        settle_started_provider(&store, &intent, "stopped-request", 1, "late").expect("late"),
        Some(true)
    );
    let reader = store.read().expect("read");
    let record = AgentRepository::get(&reader, &AgentId::from(agent.as_str()))
        .expect("agent")
        .expect("present");
    assert_eq!(record.status, AgentStatus::Paused);
    assert_eq!(record.epoch, epoch + 1);
    let delivered:i64=reader.query_row("SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='delivered' AND turn_id='late'",[&agent],|r|r.get(0)).expect("delivered");
    assert_eq!(delivered, 3);
}

fn run_python(root: &Path, code: &str, args: &[&str]) -> String {
    let output = Command::new(python_executable())
        .arg("-c")
        .arg(code)
        .arg(root)
        .arg(server_source())
        .args(args)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("prepared Python runtime starts");
    assert!(
        output.status.success(),
        "Python scheduler fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Python output is UTF-8")
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned()
}

fn python_executable() -> PathBuf {
    if let Some(python) = std::env::var_os("CODEX_AGENTS_PYTHON") {
        return python.into();
    }
    let output = Command::new("python3")
        .arg(server_source().join("codex_python.py"))
        .output()
        .expect("python resolver starts");
    assert!(
        output.status.success(),
        "python resolver succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("resolved interpreter is UTF-8")
            .trim(),
    )
}

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

struct TemporaryState(PathBuf);

static TEMP_STATE_ID: AtomicUsize = AtomicUsize::new(0);

impl TemporaryState {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock follows Unix epoch")
            .as_nanos();
        let id = TEMP_STATE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "studio-scheduler-parity-{}-{nonce}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temporary state directory creates");
        Self(root)
    }
}

impl Drop for TemporaryState {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("temporary scheduler state cleanup failed: {error}");
        }
    }
}
