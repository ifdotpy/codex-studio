use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_services::event_write::{AnnotationWrite, annotate};
use studio_runtime_services::scheduler::{SchedulerError, claim_intent, settle_start_outcome};
use studio_runtime_services::scheduler::{
    TurnIntent, TurnStart, mark_submitted_for_provider, settle_started_provider,
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

const CREATE_POLICY_REFUSAL_STATE: &str = r#"
import json, pathlib, sys
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
from codex_wakeups import pending_batch
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agents = {}
for name in ('team', 'radio'):
    agent = runtime.create({'name':name, 'cwd':str(root), 'prompt':'temporary'}, defer=True)
    agent.update(autoWake=True, status='queued', provider='codex')
    agents[name] = agent
with runtime.lock, runtime.db() as db:
    for name, agent in agents.items():
        runtime.put(db, 'agents', agent)
        if name == 'team':
            specs = [('team:message', 'agent_message', '{"importance":"note"}', 1.0)]
        else:
            specs = [(f'radio:user:{i}', 'user', f'input-{i}', float(i)) for i in range(32)]
            specs.append(('radio:turn', 'radio_turn', '{}', 99.0))
        for event_id, kind, text, created in specs:
            db.execute('INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)',
                       (event_id, agent['id'], kind, text, 'pending', created, agent['epoch'], None, None))
        pending_batch(runtime, db, agent)
    def snapshot(db, agent, name):
        events = [list(row) for row in db.execute(
            'SELECT id,kind,text,status,created,epoch,turn_id,error FROM runtime_events WHERE agent=? ORDER BY created,rowid',
            (agent['id'],))]
        ids = [row[0] for row in events]
        metadata = [list(row) for row in db.execute(
            'SELECT id,record FROM runtime_event_meta WHERE id IN (' + ','.join('?' for _ in ids) + ') ORDER BY id', ids)] if ids else []
        sync = [list(row) for row in db.execute(
            'SELECT collection,id,seq,hash,payload,deleted FROM sync_entities WHERE id=? OR id IN (' + ','.join('?' for _ in ids) + ') ORDER BY collection,id',
            [agent['id'], *ids])]
        completed = [row[0] for row in db.execute('SELECT id FROM runtime_completed_turns ORDER BY id')]
        sync_documents = [row[0] for row in db.execute('SELECT seq FROM sync_documents ORDER BY seq')]
        sync_versions = [row[0] for row in db.execute('SELECT seq FROM sync_versions ORDER BY seq')]
        raw_agent = db.execute('SELECT record FROM runtime_agents WHERE id=?', (agent['id'],)).fetchone()[0]
        return {'agent':raw_agent, 'events':events, 'metadata':metadata, 'sync':sync,
                'completed':completed, 'syncDocuments':sync_documents, 'syncVersions':sync_versions}
    result = {}
    for name, agent in agents.items():
        result[name] = {'agentId':agent['id'], 'epoch':agent['epoch'], 'snapshot':snapshot(db, agent, name)}
    print(json.dumps(result))
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
    assert_eq!(delivered, 0);
    let (projected_status, projected_in_flight, projected_turn_id): (String, bool, Option<String>) = reader
        .query_row(
            "SELECT json_extract(payload,'$.value.status'),json_extract(payload,'$.value.inFlight'),json_extract(payload,'$.value.turnId') FROM sync_entities WHERE collection='agent' AND id=?1",
            [agent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("accepted scheduler lifecycle projection reads");
    assert_eq!(projected_status, "starting");
    assert!(projected_in_flight);
    assert_eq!(projected_turn_id, None);
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
            transaction.execute(
                "UPDATE runtime_events SET status='delivered' WHERE agent=?1 AND status='reserved'",
                [agent],
            )?;
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
    let mut driver = StartThenUnknownDriver(&store);
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
        account_key: "default".to_owned(),
        provider: "codex".to_owned(),
        thread_id: Some("fixture-thread".to_owned()),
        cwd: Some("/tmp".to_owned()),
        model: Some("fixture-model".to_owned()),
        events: Vec::new(),
    };
    let mut stale_intent = intent.clone();
    stale_intent.attempt_id = "stale-attempt".to_owned();
    let mut stale_epoch = intent.clone();
    stale_epoch.epoch -= 1;
    assert!(
        !settle_started(&store, &stale_intent, "late-turn").expect("stale callback is ignored")
    );
    assert!(!settle_started(&store, &stale_epoch, "late-turn").expect("stale epoch is ignored"));
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
        store
            .transaction(
                |tx| studio_runtime_store::SchedulerRepository::settle_terminal(
                    tx,
                    &intent.agent_id,
                    intent.epoch,
                    &intent.attempt_id,
                    "late-turn",
                    AgentStatus::Completed,
                    None
                )
            )
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
fn named_team_and_radio_refusals_match_python_preparation_and_preserve_whole_state() {
    let state = TemporaryState::new();
    let fixture = run_python(&state.0, CREATE_POLICY_REFUSAL_STATE, &[]);
    let expected: Value =
        serde_json::from_str(&fixture).expect("Python policy fixture returns state");
    let store = RuntimeStore::open(Some(&state.0)).expect("Rust opens Python-written policy state");
    for name in ["team", "radio"] {
        let case = &expected[name];
        let agent = case["agentId"]
            .as_str()
            .expect("Python agent ID is present");
        let epoch = case["epoch"].as_i64().expect("Python epoch is present");
        let before = rust_policy_snapshot(&store, agent);
        assert_eq!(
            before, case["snapshot"],
            "Python and Rust pre-admission state differ for {name}"
        );
        let result = claim_intent(&store, agent, epoch, &format!("attempt-{name}"), 100.0);
        assert!(
            matches!(result, Err(SchedulerError::PolicyRefused(_))),
            "{name} must fail closed: {result:?}"
        );
        let after = rust_policy_snapshot(&store, agent);
        assert_eq!(
            after, before,
            "named refusal changed persisted state for {name}"
        );
    }
}

fn rust_policy_snapshot(store: &RuntimeStore, agent: &str) -> Value {
    let reader = store.read().expect("policy snapshot reads");
    let raw_agent: String = reader
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent],
            |row| row.get(0),
        )
        .expect("agent snapshot reads");
    let events = reader.prepare("SELECT id,kind,text,status,created,epoch,turn_id,error FROM runtime_events WHERE agent=?1 ORDER BY created,rowid")
        .expect("event snapshot prepares").query_map([agent], |row| Ok((
            row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,String>(3)?,
            row.get::<_,f64>(4)?, row.get::<_,i64>(5)?, row.get::<_,Option<String>>(6)?, row.get::<_,Option<String>>(7)?,
        ))).expect("event snapshot queries").collect::<Result<Vec<_>,_>>().expect("event snapshot reads");
    let ids = events
        .iter()
        .map(|event| event.0.clone())
        .collect::<Vec<_>>();
    let ids_json = serde_json::to_string(&ids).expect("event IDs serialize");
    let metadata = reader.prepare("SELECT id,record FROM runtime_event_meta WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY id")
        .expect("metadata snapshot prepares").query_map([&ids_json], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))
        .expect("metadata snapshot queries").collect::<Result<Vec<_>,_>>().expect("metadata snapshot reads");
    let sync = reader.prepare("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities WHERE id=?1 OR id IN (SELECT value FROM json_each(?2)) ORDER BY collection,id")
        .expect("sync snapshot prepares").query_map((agent,&ids_json), |row| Ok((
            row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,i64>(5)?,
        ))).expect("sync snapshot queries").collect::<Result<Vec<_>,_>>().expect("sync snapshot reads");
    let completed = reader
        .prepare("SELECT id FROM runtime_completed_turns ORDER BY id")
        .expect("completed-turn snapshot prepares")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("completed-turn snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("completed-turn snapshot reads");
    let sync_documents = reader
        .prepare("SELECT seq FROM sync_documents ORDER BY seq")
        .expect("sync-document snapshot prepares")
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("sync-document snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("sync-document snapshot reads");
    let sync_versions = reader
        .prepare("SELECT seq FROM sync_versions ORDER BY seq")
        .expect("sync-version snapshot prepares")
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("sync-version snapshot queries")
        .collect::<Result<Vec<_>, _>>()
        .expect("sync-version snapshot reads");
    json!({"agent":raw_agent,"events":events,"metadata":metadata,"sync":sync,
        "completed":completed,"syncDocuments":sync_documents,"syncVersions":sync_versions})
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
        .is_ok()
    );
    let reader = store.read().expect("persisted attempt reads");
    let agent_record = AgentRepository::get(&reader, &AgentId::from(agent))
        .expect("agent record reads")
        .expect("agent record exists");
    assert_eq!(agent_record.extra["startAttempt"]["submitted"], false);
    let status: String = reader
        .query_row(
            "SELECT status FROM runtime_events WHERE agent=?1 AND status='reserved' LIMIT 1",
            [agent],
            |row| row.get(0),
        )
        .expect("submitted input remains unresolved");
    assert_eq!(status, "reserved");
    drop(reader);
    assert_eq!(
        studio_runtime_services::scheduler::recover_on_startup(&store)
            .expect("proven unsubmitted attempt is released"),
        3
    );
    let reader = store.read().expect("restart recovery state reads");
    let agent_record = AgentRepository::get(&reader, &AgentId::from(agent))
        .expect("recovered agent reads")
        .expect("recovered agent exists");
    assert_eq!(agent_record.status, AgentStatus::Queued);
}

#[test]
fn restart_keeps_a_scoped_receipt_without_replaying_events() {
    let state = TemporaryState::new();
    let fixture = run_python(&state.0, CREATE_PYTHON_EVENTS, &[]);
    let fixture: Value = serde_json::from_str(&fixture).expect("Python fixture returns JSON");
    let agent = fixture["agent"].as_str().expect("agent ID is present");
    let epoch = fixture["epoch"].as_i64().expect("agent epoch is present");
    let store = RuntimeStore::open(Some(&state.0)).expect("Rust opens Python-written state");
    let mut driver = StartAndAcceptDriver(&store);
    let mut wake = PendingWake(true);
    consume_wake(
        &store,
        &mut wake,
        agent,
        epoch,
        "known-restart-attempt",
        16.5,
        &mut driver,
    )
    .expect("provider acceptance and turn identity are persisted before restart");
    drop(store);

    let store = RuntimeStore::open(Some(&state.0)).expect("Rust reopens submitted state");
    assert_eq!(
        studio_runtime_services::scheduler::recover_on_startup(&store)
            .expect("known submitted turn recovers conservatively"),
        1
    );
    let reader = store.read().expect("recovered turn state reads");
    let agent_record = AgentRepository::get(&reader, &AgentId::from(agent))
        .expect("agent reads")
        .expect("agent exists");
    assert_eq!(agent_record.status, AgentStatus::Interrupted);
    assert_eq!(agent_record.extra["turnId"], "known-restart-turn");
    assert_eq!(agent_record.extra["autoWake"], false);
    assert_eq!(agent_record.extra["inFlight"], false);
    assert_eq!(
        agent_record.extra["restartRecovery"]["turnId"],
        "known-restart-turn"
    );
    let (delivered, pending): (i64, i64) = reader
        .query_row(
            "SELECT SUM(status='delivered'),SUM(status='pending') FROM runtime_events WHERE agent=?1",
            [agent],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("recovered event statuses read");
    assert_eq!(delivered, 3);
    assert_eq!(pending, 0);
    let pending_agents = studio_runtime_store::SchedulerRepository::pending_agents(&reader)
        .expect("recovered agent is not resubmitted");
    assert!(pending_agents.is_empty());
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
    assert_eq!(uncertain, 0);
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

struct StartAndAcceptDriver<'a>(&'a RuntimeStore);

impl TurnDriver for StartAndAcceptDriver<'_> {
    fn start(&mut self, intent: TurnIntent) -> TurnStart {
        let request_id = "known-restart-request";
        assert!(
            mark_submitted_for_provider(self.0, &intent, request_id, 1)
                .expect("turn/start identity is persisted")
        );
        assert!(
            settle_started_provider(self.0, &intent, request_id, 1, "known-restart-turn")
                .expect("provider acceptance settles once")
                .is_some()
        );
        TurnStart::Accepted {
            turn_id: "known-restart-turn".to_owned(),
        }
    }
}

struct StartThenUnknownDriver<'a>(&'a RuntimeStore);

impl TurnDriver for StartThenUnknownDriver<'_> {
    fn start(&mut self, intent: TurnIntent) -> TurnStart {
        assert!(
            mark_submitted_for_provider(self.0, &intent, "unknown-request", 1)
                .expect("unknown provider request identity is persisted")
        );
        TurnStart::Unknown
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
        assert_eq!(agent.extra["startAttempt"]["submitted"], false);
        for event in &intent.events {
            let status: String = reader
                .query_row(
                    "SELECT status FROM runtime_events WHERE id=?1",
                    [&event.id],
                    |row| row.get(0),
                )
                .expect("event reservation reads before handoff");
            assert_eq!(status, "reserved");
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

trait TurnDriver {
    fn start(&mut self, intent: TurnIntent) -> TurnStart;
}
trait WakeSignal {
    fn consume(&mut self) -> bool;
}
fn consume_wake(
    store: &RuntimeStore,
    wake: &mut impl WakeSignal,
    agent: &str,
    epoch: i64,
    attempt: &str,
    created: f64,
    driver: &mut impl TurnDriver,
) -> Result<bool, SchedulerError> {
    if !wake.consume() {
        return Ok(false);
    }
    if let Some(intent) = claim_intent(store, agent, epoch, attempt, created)? {
        let outcome = driver.start(intent.clone());
        settle_start_outcome(store, &intent, outcome)?;
    }
    Ok(true)
}

fn settle_started(
    store: &RuntimeStore,
    intent: &TurnIntent,
    turn: &str,
) -> Result<bool, SchedulerError> {
    Ok(store.transaction(|tx| {
        studio_runtime_store::SchedulerRepository::settle_started(
            tx,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            turn,
        )
    })?)
}
