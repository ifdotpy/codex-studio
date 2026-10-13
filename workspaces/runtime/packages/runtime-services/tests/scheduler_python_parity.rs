use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_services::event_write::{AnnotationWrite, annotate};
use studio_runtime_services::scheduler::consume_wake;
use studio_runtime_services::scheduler::{TurnDriver, TurnIntent, TurnStart, WakeSignal};
use studio_runtime_store::{
    AgentId, AgentRepository, EventWriteRepository, QueueEvent, QueueEventId, QueueEventRepository,
    RuntimeStore,
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

const CREATE_DISPATCH_POLICY_CASES: &str = r#"
import json, pathlib, shutil, sys, time
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
from codex_wakeups import pending_batch
class QuietRuntime(Runtime):
    def schedule(self):
        return
class CaptureExecutor:
    def __init__(self):
        self.selected = {}
    def submit(self, _function, agent, rows):
        self.selected[agent['id']] = [dict(row) for row in rows]
root = pathlib.Path(sys.argv[1])
now = time.time()
policy_cases = json.loads(pathlib.Path(sys.argv[3]).read_text())
cases = []
def event(kind, text='input', created=1.0, meta=None):
    return {'kind':kind, 'text':text, 'created':created, 'meta':meta or {}}
def add(name, specs, provider='codex'):
    cases.append({'name':name, 'specs':specs, 'provider':provider})
for case in policy_cases['kindCases']:
    add(case['name'], [event(case['kind'], case['text'], meta=case.get('meta'))])
for complaint in policy_cases['complaints']:
    add(complaint['name'], [event('complaint', json.dumps({'complaints':[{
        'complaint_id':complaint['complaintId']}]}))])
add('event-limit', [event('plan_update', f'event-{i:02}', created=float(i))
                    for i in range(policy_cases['eventLimit'])])
add('asset-limit', [event('plan_update', f'asset-{i}', meta={'assets':[i for i in range(count)]})
                    for i, count in enumerate(policy_cases['assetCounts'])])
add('composer-limit', [event(row['kind'], row['text'], meta=row.get('meta'))
                       for row in policy_cases['composerEvents']])
add('claude-command', [event(row['kind'], row['text'])
                       for row in policy_cases['claudeCommandEvents']], provider='claude')
add('progress-delay', [event('agent_message', json.dumps({'importance':'progress','sender':'worker',
     'room':'team','progress_key':'delay','progress_version':version}),
     created=now+policy_cases['progressDelaySeconds'])
     for version in policy_cases['progressVersions']])
add('duplicate-progress', [event('agent_message', json.dumps({'importance':'progress','sender':'worker',
     'room':'team','progress_key':'duplicate','progress_version':policy_cases['duplicateProgressVersion']}))
     for _ in range(2)])
add('urgent-progress', [event('agent_message', json.dumps({'importance':'progress','sender':'worker',
     'room':'team','progress_key':f'urgent-{i}','progress_version':1}))
     for i in range(policy_cases['urgentProgressCount'])] + [event('user','urgent')])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agents = {}
for case in cases:
    agent = runtime.create({'name':case['name'], 'cwd':str(root), 'prompt':'temporary'}, defer=True)
    agent.update(autoWake=True, status='queued', provider=case['provider'])
    agents[case['name']] = agent
with runtime.lock, runtime.db() as db:
    for case in cases:
        agent = agents[case['name']]
        runtime.put(db, 'agents', agent)
        for index, spec in enumerate(case['specs']):
            event_id = f"{case['name']}:{index}"
            db.execute('INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)',
                (event_id, agent['id'], spec['kind'], spec['text'], 'pending', spec['created'],
                 agent['epoch'], None, None))
            if spec['meta']:
                db.execute('INSERT INTO runtime_event_meta VALUES(?,?)',
                    (event_id, json.dumps(spec['meta'])))
    db.execute('CREATE TABLE IF NOT EXISTS runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL)')
    for complaint in policy_cases['complaintRecords']:
        db.execute('INSERT INTO runtime_complaints VALUES(?,?)', (complaint['id'], json.dumps({
            'leadId':agents[complaint['agentCase']]['id'], 'status':complaint['status'],
            'recipient':complaint['recipient'], 'responses':complaint['responses']})))
agent_ids = {name:{'id':agent['id'], 'epoch':agent['epoch']} for name, agent in agents.items()}
runtime.close()
python_root = root.parent / (root.name + '-python')
shutil.copytree(root, python_root)
reference = QuietRuntime(python_root, lambda *_args, **_kwargs: None)
executor = CaptureExecutor()
reference.delivery_executor = lambda: executor
import codex_team_isolation
original_cancel_pending = codex_team_isolation.cancel_pending
codex_team_isolation.cancel_pending = lambda *_args, **_kwargs: None
pending = {}
retired = {}
for case in cases:
    agent_id = agent_ids[case['name']]['id']
    with reference.lock, reference.db() as db:
        agent = reference.agent(agent_id, db)
        pending[case['name']] = [row['id'] for row in pending_batch(reference, db, agent)]
        retired[case['name']] = [list(row) for row in db.execute(
            "SELECT id,status,error FROM runtime_events WHERE agent=? AND epoch=? AND status!='pending' ORDER BY created,rowid",
            (agent_id, agent['epoch']))]
    reference.dispatch_candidates(agent_id)
    with reference.lock, reference.db() as db:
        agent = reference.agent(agent_id, db)
        agent.update(status='queued', inFlight=False)
        agent.pop('startAttempt', None)
        reference.put(db, 'agents', agent)
        db.execute("UPDATE runtime_events SET status='pending' WHERE agent=? AND status='reserved'",
                   (agent_id,))
codex_team_isolation.cancel_pending = original_cancel_pending
selected = {case['name']:[row['id'] for row in executor.selected.get(agent_ids[case['name']]['id'], [])]
            for case in cases}
print(json.dumps({'agents':agent_ids, 'pending':pending, 'selected':selected,
                  'retired':retired, 'python_root':str(python_root), 'now':now}))
reference.close()
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
}

#[test]
fn pending_and_dispatch_selection_matches_python_policy_kinds_and_boundaries() {
    let state = TemporaryState::new();
    let policy_corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/scheduler_policy_cases.json");
    let policy_corpus = policy_corpus
        .to_str()
        .expect("scheduler policy corpus path is UTF-8");
    let fixture = run_python(&state.0, CREATE_DISPATCH_POLICY_CASES, &[policy_corpus]);
    let fixture: Value =
        serde_json::from_str(&fixture).expect("Python policy fixture returns JSON");
    let agents = fixture["agents"]
        .as_object()
        .expect("Python fixture returns agents by case");
    let python_selected = fixture["selected"]
        .as_object()
        .expect("Python dispatch returns selected IDs by case");
    let python_pending = fixture["pending"]
        .as_object()
        .expect("Python pending_batch returns pending IDs by case");
    let python_retired = fixture["retired"]
        .as_object()
        .expect("Python pending_batch returns retired records by case");
    let attempt_created = fixture["now"]
        .as_f64()
        .expect("Python fixture returns a clock");
    let expected_refusals = [
        ("kind-agent_message", "team-isolation-policy"),
        ("kind-complaint_response", "team-isolation-policy"),
        ("kind-followup", "team-isolation-policy"),
        ("kind-radio", "radio-policy"),
        ("kind-radio_turn", "radio-policy"),
        ("kind-radio-answer", "radio-policy"),
        ("duplicate-progress", "team-isolation-policy"),
        ("urgent-progress", "team-isolation-policy"),
    ]
    .into_iter()
    .collect::<std::collections::BTreeMap<_, _>>();
    let store = RuntimeStore::open(Some(&state.0)).expect("Rust opens Python-written policy state");
    for name in [
        "kind-user",
        "kind-plan_update",
        "kind-agent_message",
        "kind-chat_review",
        "kind-complaint_response",
        "kind-followup",
        "kind-radio",
        "kind-radio_turn",
        "kind-complaint-open",
        "kind-work-review",
        "kind-radio-answer",
        "complaint-resolved",
        "event-limit",
        "asset-limit",
        "composer-limit",
        "claude-command",
        "progress-delay",
        "duplicate-progress",
        "urgent-progress",
    ] {
        let agent = &agents[name];
        let agent_id = agent["id"].as_str().expect("agent ID is a string");
        let epoch = agent["epoch"].as_i64().expect("agent epoch is an integer");
        let expected_ids = python_selected[name]
            .as_array()
            .expect("Python selected IDs are an array")
            .iter()
            .map(|value| value.as_str().expect("event ID is a string").to_owned())
            .collect::<Vec<_>>();
        let mut driver = CaptureDriver::default();
        let mut wake = PendingWake(true);
        let result = consume_wake(
            &store,
            &mut wake,
            agent_id,
            epoch,
            &format!("attempt-{name}"),
            attempt_created,
            &mut driver,
        );
        if let Some(policy) = expected_refusals.get(name) {
            assert!(
                matches!(result, Err(studio_runtime_services::scheduler::SchedulerError::PolicyRefused(ref reason))
                    if reason == &format!("scheduler cannot claim {}: requires {policy}", if name == "urgent-progress" || name == "duplicate-progress" { "agent_message" } else if name == "kind-radio-answer" { "user" } else { name.strip_prefix("kind-").unwrap_or(name) })),
                "{name} must fail closed with its named policy: {result:?}"
            );
            assert!(
                driver.ids.is_empty(),
                "{name} unexpectedly reached the driver"
            );
            if name == "kind-radio_turn" {
                assert!(
                    expected_ids.is_empty(),
                    "Python radio policy rejects stale turns"
                );
            } else {
                assert!(!expected_ids.is_empty(), "Python dispatch selected {name}");
            }
            if name == "urgent-progress" {
                assert_eq!(
                    expected_ids.first().map(String::as_str),
                    Some("urgent-progress:32")
                );
            }
        } else {
            assert!(result.expect("safe scheduler case completes"));
            assert_eq!(
                driver.ids, expected_ids,
                "Python dispatch mismatch for {name}"
            );
        }
        let expected_pending = python_pending[name]
            .as_array()
            .expect("Python pending IDs are an array")
            .iter()
            .map(|value| value.as_str().expect("event ID is a string").to_owned())
            .collect::<Vec<_>>();
        let reader = store.read().expect("Rust policy state reads");
        let actual_pending = reader
            .prepare(
                "SELECT id FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending' ORDER BY created,rowid",
            )
            .expect("pending state query prepares")
            .query_map((agent_id, epoch), |row| row.get::<_, String>(0))
            .expect("pending state query executes")
            .collect::<Result<Vec<_>, _>>()
            .expect("pending state query reads");
        assert_eq!(
            actual_pending, expected_pending,
            "pending preparation mismatch for {name}"
        );
        let expected_retired = python_retired[name]
            .as_array()
            .expect("Python retired records are an array")
            .iter()
            .map(|record| {
                (
                    record[0].as_str().expect("event ID is a string").to_owned(),
                    record[1]
                        .as_str()
                        .expect("event status is a string")
                        .to_owned(),
                    record[2].as_str().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>();
        let actual_retired = reader
            .prepare(
                "SELECT id,status,error FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status!='pending' ORDER BY created,rowid",
            )
            .expect("retired state query prepares")
            .query_map((agent_id, epoch), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get(2)?))
            })
            .expect("retired state query executes")
            .collect::<Result<Vec<(String, String, Option<String>)>, _>>()
            .expect("retired state query reads");
        assert_eq!(
            actual_retired, expected_retired,
            "pending retirement mismatch for {name}"
        );
    }
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
        0
    );
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
        for path in [self.0.clone(), python_reference_path(&self.0)] {
            if let Err(error) = fs::remove_dir_all(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("temporary scheduler state cleanup failed: {error}");
            }
        }
    }
}

fn python_reference_path(root: &Path) -> PathBuf {
    let name = format!(
        "{}-python",
        root.file_name().unwrap_or_default().to_string_lossy()
    );
    root.with_file_name(name)
}
