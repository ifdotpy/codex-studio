use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_services::event_write::{AnnotationWrite, annotate};
use studio_runtime_services::scheduler::consume_wake;
use studio_runtime_services::scheduler::{
    TurnDriver, TurnIntent, TurnOutcome, TurnStart, WakeSignal, settle_started, settle_terminal,
};
use studio_runtime_store::{
    AgentId, AgentRepository, AgentStatus, EventWriteRepository, QueueEvent, QueueEventId,
    QueueEventRepository, RuntimeStore,
};

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
with runtime.lock, runtime.db() as db:
    runtime.put(db, "agents", agent)
    for event_id, created in (("python-later", 2.0), ("python-first", 1.0), ("python-tie", 1.0)):
        db.execute("INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)",
                   (event_id, agent["id"], "user", event_id, "pending", created, agent["epoch"], None, None))
    selected = pending_batch(runtime, db, agent)
    print(json.dumps({"agent":agent["id"], "epoch":agent["epoch"], "events":[item["id"] for item in selected]}))
runtime.close()
"#;

#[test]
fn pending_selection_matches_python_and_claims_rust_event_write_events() {
    let state = TemporaryState::new();
    let fixture = run_python(&state.0, CREATE_PYTHON_EVENTS, &[]);
    let fixture: Value = serde_json::from_str(&fixture).expect("Python fixture returns JSON");
    let agent = fixture["agent"]
        .as_str()
        .expect("Python fixture returns an agent ID");
    let epoch = fixture["epoch"]
        .as_i64()
        .expect("Python fixture returns an epoch");
    let python_selected = fixture["events"]
        .as_array()
        .expect("Python fixture returns selected event IDs")
        .iter()
        .map(|value| value.as_str().expect("event ID is a string").to_owned())
        .collect::<Vec<_>>();

    let store = RuntimeStore::open(Some(&state.0)).expect("Rust opens Python-written state");
    let mut driver = CaptureDriver::default();
    let mut wake = PendingWake(true);
    assert!(
        consume_wake(
            &store,
            &mut wake,
            agent,
            epoch,
            "python-attempt",
            10.0,
            &mut driver
        )
        .expect("Rust consumes wake")
    );
    assert_eq!(driver.ids, python_selected);
    store
        .transaction(|transaction| {
            let result = annotate(
                transaction,
                AnnotationWrite {
                    agent_id: agent.to_owned(),
                    id: Some("rust-written".to_owned()),
                    path: "README.md".to_owned(),
                    line: 1,
                    text: "scheduler test annotation".to_owned(),
                    turn_id: None,
                    signature: "scheduler-test".to_owned(),
                },
            )
            .map_err(|error| studio_runtime_store::StoreError::SchemaMismatch(error.to_string()))?;
            assert!(result.event_inserted);
            Ok(())
        })
        .expect("Rust event-write service creates a durable pending event");

    let mut driver = CaptureDriver::default();
    let mut wake = PendingWake(false);
    assert!(
        !consume_wake(
            &store,
            &mut wake,
            agent,
            epoch,
            "no-wake",
            12.0,
            &mut driver
        )
        .expect("absent wake is consumed as a no-op")
    );
    assert!(driver.ids.is_empty());

    let mut driver = InspectingDriver::new(
        &store,
        TurnStart::Accepted {
            turn_id: "turn-1".to_owned(),
        },
    );
    let mut wake = PendingWake(true);
    consume_wake(
        &store,
        &mut wake,
        agent,
        epoch,
        "accepted-attempt",
        13.0,
        &mut driver,
    )
    .expect("accepted turn intent completes");
    assert_eq!(
        driver.ids.last().map(String::as_str),
        Some("annotation:rust-written")
    );
    let reader = store.read().expect("accepted event state reads");
    let delivered: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='delivered'",
            [agent],
            |row| row.get(0),
        )
        .expect("delivered event count reads");
    assert_eq!(delivered, 4);
    let (projected_status, projected_in_flight, projected_turn_id): (String, bool, String) = reader
        .query_row(
            "SELECT json_extract(payload,'$.value.status'),json_extract(payload,'$.value.inFlight'),json_extract(payload,'$.value.turnId') FROM sync_entities WHERE collection='agent' AND id=?1",
            [agent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("accepted scheduler lifecycle projection reads");
    assert_eq!(projected_status, "running");
    assert!(projected_in_flight);
    assert_eq!(projected_turn_id, "turn-1");
    drop(reader);
    drop(store);

    let store = RuntimeStore::open(Some(&state.0)).expect("Rust reopens accepted state");
    store
        .transaction(|transaction| {
            let mut agent_record = AgentRepository::get(transaction, &AgentId::from(agent))?
                .ok_or_else(|| {
                    studio_runtime_store::StoreError::SchemaMismatch(
                        "scheduler fixture agent is missing".to_owned(),
                    )
                })?;
            agent_record.status = studio_runtime_store::AgentStatus::Queued;
            agent_record
                .extra
                .insert("inFlight".to_owned(), json!(false));
            agent_record.extra.remove("startAttempt");
            agent_record.extra.remove("turnId");
            AgentRepository::put(transaction, &agent_record)?;
            EventWriteRepository::set_agent_sync_status(transaction, agent, "queued")?;
            QueueEventRepository::put(
                transaction,
                &QueueEvent {
                    id: QueueEventId::from("rust-uncertain"),
                    agent: AgentId::from(agent),
                    kind: "user".to_owned(),
                    text: "uncertain".to_owned(),
                    status: "pending".to_owned(),
                    created: 4.0,
                    epoch,
                    turn_id: None,
                    error: None,
                },
            )?;
            Ok(())
        })
        .expect("next scheduler input writes");
    let mut driver = CaptureDriver::returning(TurnStart::Unknown);
    let mut wake = PendingWake(true);
    consume_wake(
        &store,
        &mut wake,
        agent,
        epoch,
        "unknown-attempt",
        14.0,
        &mut driver,
    )
    .expect("unknown turn outcome is persisted");
    let reader = store.read().expect("uncertain event state reads");
    let uncertain: String = reader
        .query_row(
            "SELECT status FROM runtime_events WHERE id='rust-uncertain'",
            [],
            |row| row.get(0),
        )
        .expect("uncertain event status reads");
    assert_eq!(uncertain, "uncertain");
    drop(reader);

    let intent = TurnIntent {
        agent_id: agent.to_owned(),
        epoch,
        attempt_id: "unknown-attempt".to_owned(),
        events: Vec::new(),
    };
    let mut stale_intent = intent.clone();
    stale_intent.attempt_id = "stale-attempt".to_owned();
    assert!(
        !settle_started(&store, &stale_intent, "late-turn").expect("stale callback is ignored")
    );
    assert!(settle_started(&store, &intent, "late-turn").expect("late acceptance settles"));
    let reader = store.read().expect("late acceptance state reads");
    let delivered: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE id='rust-uncertain' AND status='delivered' AND turn_id='late-turn'",
            [],
            |row| row.get(0),
        )
        .expect("late accepted event reads");
    assert_eq!(delivered, 1);
    drop(reader);
    assert!(
        settle_terminal(&store, &intent, "late-turn", TurnOutcome::Completed,)
            .expect("terminal callback settles")
    );
    let reader = store.read().expect("terminal scheduler state reads");
    let agent_record = AgentRepository::get(&reader, &AgentId::from(agent))
        .expect("terminal agent reads")
        .expect("terminal agent exists");
    assert_eq!(
        agent_record.status,
        studio_runtime_store::AgentStatus::Completed
    );
    assert_eq!(agent_record.extra["inFlight"], false);
    assert_eq!(agent_record.extra["lastCompletedTurn"], "late-turn");
}

#[test]
fn submitted_marker_survives_settlement_write_failure() {
    let state = TemporaryState::new();
    let fixture = run_python(&state.0, CREATE_PYTHON_EVENTS, &[]);
    let fixture: Value = serde_json::from_str(&fixture).expect("Python fixture returns JSON");
    let agent = fixture["agent"].as_str().expect("agent ID is present");
    let epoch = fixture["epoch"].as_i64().expect("agent epoch is present");
    let store = RuntimeStore::open(Some(&state.0)).expect("Rust opens Python-written state");
    store
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TRIGGER reject_scheduler_delivery BEFORE UPDATE OF status ON runtime_events
                 WHEN NEW.status='delivered' BEGIN SELECT RAISE(ABORT,'injected settlement failure'); END;",
            )?;
            Ok(())
        })
        .expect("settlement failure trigger installs");
    let mut driver = CaptureDriver::returning(TurnStart::Accepted {
        turn_id: "accepted-before-write-failure".to_owned(),
    });
    let mut wake = PendingWake(true);
    assert!(
        consume_wake(
            &store,
            &mut wake,
            agent,
            epoch,
            "settlement-failure-attempt",
            15.0,
            &mut driver,
        )
        .is_err()
    );
    let reader = store.read().expect("persisted attempt reads");
    let agent_record = AgentRepository::get(&reader, &AgentId::from(agent))
        .expect("agent record reads")
        .expect("agent record exists");
    assert_eq!(agent_record.extra["startAttempt"]["submitted"], true);
    let status: String = reader
        .query_row(
            "SELECT status FROM runtime_events WHERE agent=?1 AND status='dispatching' LIMIT 1",
            [agent],
            |row| row.get(0),
        )
        .expect("submitted input remains unresolved");
    assert_eq!(status, "dispatching");
    drop(reader);
    assert_eq!(
        studio_runtime_services::scheduler::recover_on_startup(&store)
            .expect("submitted attempt is not recovered"),
        1
    );
    let reader = store.read().expect("restart recovery state reads");
    let agent_record = AgentRepository::get(&reader, &AgentId::from(agent))
        .expect("recovered agent reads")
        .expect("recovered agent exists");
    assert_eq!(agent_record.status, AgentStatus::Waiting);
}

#[test]
fn proven_not_submitted_releases_projected_lifecycle() {
    let state = TemporaryState::new();
    let fixture = run_python(&state.0, CREATE_PYTHON_EVENTS, &[]);
    let fixture: Value = serde_json::from_str(&fixture).expect("Python fixture returns JSON");
    let agent = fixture["agent"].as_str().expect("agent ID is present");
    let epoch = fixture["epoch"].as_i64().expect("agent epoch is present");
    let store = RuntimeStore::open(Some(&state.0)).expect("Rust opens Python-written state");
    let mut driver = CaptureDriver::default();
    let mut wake = PendingWake(true);
    consume_wake(
        &store,
        &mut wake,
        agent,
        epoch,
        "rejected-attempt",
        15.5,
        &mut driver,
    )
    .expect("proven rejection is safely settled");
    assert!(!driver.ids.is_empty());
    let reader = store.read().expect("rejected state reads");
    let (status, in_flight, turn_id): (String, bool, Option<String>) = reader
        .query_row(
            "SELECT json_extract(payload,'$.value.status'),json_extract(payload,'$.value.inFlight'),json_extract(payload,'$.value.turnId') FROM sync_entities WHERE collection='agent' AND id=?1",
            [agent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("rejected lifecycle projection reads");
    assert_eq!(status, "queued");
    assert!(!in_flight);
    assert_eq!(turn_id, None);
    let pending: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='pending'",
            [agent],
            |row| row.get(0),
        )
        .expect("proven rejected input remains pending");
    assert_eq!(pending, 3);
}

#[test]
fn late_acceptance_preserves_a_stopped_agent_and_marks_input_uncertain() {
    let state = TemporaryState::new();
    let fixture = run_python(&state.0, CREATE_PYTHON_EVENTS, &[]);
    let fixture: Value = serde_json::from_str(&fixture).expect("Python fixture returns JSON");
    let agent = fixture["agent"].as_str().expect("agent ID is present");
    let epoch = fixture["epoch"].as_i64().expect("agent epoch is present");
    let store = RuntimeStore::open(Some(&state.0)).expect("Rust opens Python-written state");
    let mut driver = StoppingDriver {
        store: &store,
        agent_id: agent.to_owned(),
        ids: Vec::new(),
    };
    let mut wake = PendingWake(true);
    consume_wake(
        &store,
        &mut wake,
        agent,
        epoch,
        "stopped-attempt",
        16.0,
        &mut driver,
    )
    .expect("late turn result settles safely");
    let reader = store.read().expect("stopped state reads");
    let agent_record = AgentRepository::get(&reader, &AgentId::from(agent))
        .expect("agent record reads")
        .expect("agent record exists");
    assert_eq!(
        agent_record.status,
        studio_runtime_store::AgentStatus::Paused
    );
    assert_eq!(agent_record.epoch, epoch + 1);
    assert_eq!(agent_record.extra["autoWake"], false);
    let uncertain: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='uncertain'",
            [agent],
            |row| row.get(0),
        )
        .expect("stopped input remains unresolved");
    assert_eq!(uncertain, driver.ids.len() as i64);
}

struct CaptureDriver {
    ids: Vec<String>,
    outcome: TurnStart,
}

impl CaptureDriver {
    fn returning(outcome: TurnStart) -> Self {
        Self {
            ids: Vec::new(),
            outcome,
        }
    }
}

impl Default for CaptureDriver {
    fn default() -> Self {
        Self::returning(TurnStart::NotSubmitted)
    }
}

impl TurnDriver for CaptureDriver {
    fn start(&mut self, intent: TurnIntent) -> TurnStart {
        self.ids = intent.events.into_iter().map(|event| event.id).collect();
        self.outcome.clone()
    }
}

struct InspectingDriver<'a> {
    store: &'a RuntimeStore,
    ids: Vec<String>,
    outcome: TurnStart,
}

impl<'a> InspectingDriver<'a> {
    fn new(store: &'a RuntimeStore, outcome: TurnStart) -> Self {
        Self {
            store,
            ids: Vec::new(),
            outcome,
        }
    }
}

impl TurnDriver for InspectingDriver<'_> {
    fn start(&mut self, intent: TurnIntent) -> TurnStart {
        let reader = self.store.read().expect("driver observes committed marker");
        let agent = AgentRepository::get(&reader, &AgentId::from(intent.agent_id.as_str()))
            .expect("agent reads before handoff")
            .expect("agent exists before handoff");
        assert_eq!(agent.extra["startAttempt"]["submitted"], true);
        for event in &intent.events {
            let status: String = reader
                .query_row(
                    "SELECT status FROM runtime_events WHERE id=?1",
                    [&event.id],
                    |row| row.get(0),
                )
                .expect("event reservation reads before handoff");
            assert_eq!(status, "dispatching");
        }
        self.ids = intent.events.into_iter().map(|event| event.id).collect();
        self.outcome.clone()
    }
}

struct StoppingDriver<'a> {
    store: &'a RuntimeStore,
    agent_id: String,
    ids: Vec<String>,
}

impl TurnDriver for StoppingDriver<'_> {
    fn start(&mut self, intent: TurnIntent) -> TurnStart {
        self.ids = intent.events.into_iter().map(|event| event.id).collect();
        self.store
            .transaction(|transaction| {
                let mut agent =
                    AgentRepository::get(transaction, &AgentId::from(self.agent_id.as_str()))?
                        .ok_or_else(|| {
                            studio_runtime_store::StoreError::SchemaMismatch(
                                "scheduler test agent is missing".to_owned(),
                            )
                        })?;
                agent.epoch += 1;
                agent.status = studio_runtime_store::AgentStatus::Paused;
                agent.extra.insert("autoWake".to_owned(), json!(false));
                AgentRepository::put(transaction, &agent)?;
                EventWriteRepository::set_agent_sync_status(transaction, &self.agent_id, "paused")?;
                Ok(())
            })
            .expect("stop commits while driver call is in progress");
        TurnStart::Accepted {
            turn_id: "accepted-after-stop".to_owned(),
        }
    }
}

struct PendingWake(bool);

impl WakeSignal for PendingWake {
    fn consume(&mut self) -> bool {
        std::mem::replace(&mut self.0, false)
    }
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

impl TemporaryState {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock follows Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("studio-scheduler-parity-{nonce}"));
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
