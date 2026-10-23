use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_services::event_write::{AnnotationWrite, annotate};
use studio_runtime_services::scheduler::{
    SchedulerError, TurnStart, claim_intent, mark_submitted_for_provider, recover_on_startup,
    settle_start_outcome, settle_started_provider,
};
use studio_runtime_store::{AgentId, AgentRepository, AgentStatus, RuntimeStore};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Seed<T> {
    expected: T,
    statements: Vec<String>,
}
async fn seeded<T: serde::de::DeserializeOwned>(
    state: &TemporaryState,
    fixture: &str,
) -> (RuntimeStore, T) {
    let seed: Seed<T> = serde_json::from_str(fixture).expect("frozen scheduler seed");
    let store = RuntimeStore::open(Some(&state.0))
        .await
        .expect("scheduler store");
    let statements = seed
        .statements
        .join("\n")
        .replace("${ROOT}", &state.0.to_string_lossy().replace('\'', "''"));
    store
        .transaction(move |tx| {
            tx.execute_batch(&statements)?;
            Ok(())
        })
        .await
        .expect("seed scheduler state");
    (store, seed.expected)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    agent: String,
    epoch: i64,
    events: Vec<String>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PolicyAgent {
    agent_id: String,
    epoch: i64,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    team: PolicyAgent,
    radio: PolicyAgent,
}

async fn fixture() -> (TemporaryState, RuntimeStore, String, i64, Vec<String>) {
    let state = TemporaryState::new();
    let (store, fixture): (_, Events) =
        seeded(&state, include_str!("fixtures/scheduler-events.json")).await;
    (state, store, fixture.agent, fixture.epoch, fixture.events)
}

#[tokio::test]
async fn pending_selection_matches_python_and_claims_rust_event_write_events() {
    let (_state, store, agent, epoch, fixture) = fixture().await;
    let intent = claim_intent(&store, &agent, epoch, "python-attempt", 10.0)
        .await
        .expect("claim")
        .expect("input");
    assert_eq!(
        json!(intent.events.iter().map(|e| &e.id).collect::<Vec<_>>()),
        json!(fixture)
    );
    let reader = store.test_reader().expect("read");
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
        .await
        .expect("unsent restores pending");
    let event_agent = agent.clone();
    store
        .transaction(move |tx| {
            annotate(
                tx,
                AnnotationWrite {
                    agent_id: event_agent,
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
        .await
        .expect("Rust event writer");
    let intent = claim_intent(&store, &agent, epoch, "accepted-attempt", 13.0)
        .await
        .expect("claim")
        .expect("input");
    assert_eq!(
        intent.events.last().map(|e| e.id.as_str()),
        Some("annotation:rust-written")
    );
    assert!(
        mark_submitted_for_provider(&store, &intent, "accepted-request", 1, "test-connection")
            .await
            .expect("submit")
    );
    assert_eq!(
        settle_started_provider(&store, &intent, "accepted-request", 1, "turn-1")
            .await
            .expect("accept"),
        Some(false)
    );
    let reader = store.test_reader().expect("read");
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
#[tokio::test]
async fn named_team_and_radio_refusals_match_python_preparation_and_preserve_whole_state() {
    let state = TemporaryState::new();
    let (store, expected): (_, Policy) =
        seeded(&state, include_str!("fixtures/scheduler-policy.json")).await;
    for (name, case) in [("team", &expected.team), ("radio", &expected.radio)] {
        let agent = &case.agent_id;
        let epoch = case.epoch;
        let before = rust_policy_snapshot(&store, agent);
        let result = claim_intent(&store, agent, epoch, &format!("attempt-{name}"), 100.0).await;
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
    let reader = store.test_reader().expect("policy snapshot reads");
    let raw_agent = studio_runtime_store::agent_volatile::merged_record(&reader, agent)
        .expect("agent snapshot reads")
        .expect("agent row exists");
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

#[tokio::test]
async fn submitted_marker_survives_settlement_write_failure() {
    let (_state, store, agent, epoch, _) = fixture().await;
    let intent = claim_intent(&store, &agent, epoch, "failure", 10.0)
        .await
        .expect("claim")
        .expect("input");
    assert!(
        mark_submitted_for_provider(&store, &intent, "failure-request", 1, "test-connection")
            .await
            .expect("submit")
    );
    store.transaction(|tx| {tx.execute_batch("CREATE TRIGGER reject_delivery BEFORE UPDATE OF status ON runtime_events WHEN NEW.status='delivered' BEGIN SELECT RAISE(ABORT,'injected settlement failure'); END;")?;Ok(())}).await.expect("failure injection");
    assert!(
        settle_started_provider(&store, &intent, "failure-request", 1, "accepted")
            .await
            .is_err()
    );
    let reader = store.test_reader().expect("read");
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
    assert_eq!(
        recover_on_startup(&store)
            .await
            .expect("uncertain recovery"),
        1
    );
    let record = AgentRepository::get(
        &store.test_reader().expect("read"),
        &AgentId::from(agent.as_str()),
    )
    .expect("agent")
    .expect("present");
    // Python restart recovery restores an accepted submission to the queue
    // until native history confirms its outcome (codex_restart_recovery.py).
    assert_eq!(record.status, AgentStatus::Queued);
    assert_eq!(record.extra["restartRecovery"]["stage"], "held");
}
#[tokio::test]
async fn restart_keeps_a_scoped_receipt_without_replaying_events() {
    let (state, store, agent, epoch, _) = fixture().await;
    let intent = claim_intent(&store, &agent, epoch, "restart", 10.0)
        .await
        .expect("claim")
        .expect("input");
    assert!(
        mark_submitted_for_provider(&store, &intent, "restart-request", 1, "test-connection")
            .await
            .expect("submit")
    );
    assert_eq!(
        settle_started_provider(&store, &intent, "restart-request", 1, "known")
            .await
            .expect("accept"),
        Some(false)
    );
    drop(store);
    let store = RuntimeStore::open(Some(&state.0)).await.expect("reopen");
    assert_eq!(recover_on_startup(&store).await.expect("recovery"), 1);
    let reader = store.test_reader().expect("read");
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

#[tokio::test]
async fn startup_recovers_valid_submitted_agent_while_skipping_malformed_neighbor() {
    let (_state, store, good_agent, epoch, _) = fixture().await;
    let intent = claim_intent(&store, &good_agent, epoch, "good-submitted", 10.0)
        .await
        .expect("claim submitted attempt")
        .expect("pending input");
    assert!(
        mark_submitted_for_provider(&store, &intent, "good-request", 1, "connection-1")
            .await
            .expect("persist submitted attempt")
    );

    let malformed = json!({
        "id": "malformed-submitted",
        "rootId": "malformed-submitted",
        "epoch": "invalid",
        "status": "running",
        "autoWake": true,
        "inFlight": true,
        "startAttempt": {"submitted": true, "executionOutcome": "unknown"}
    });
    let malformed_json = malformed.to_string();
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                ("malformed-submitted", malformed_json),
            )?;
            Ok(())
        })
        .await
        .expect("seed malformed submitted neighbor");

    assert_eq!(
        recover_on_startup(&store)
            .await
            .expect("recover submitted rows"),
        1
    );
    let reader = store.test_reader().expect("read recovered state");
    let good = AgentRepository::get(&reader, &AgentId::from(good_agent.as_str()))
        .expect("read healthy agent")
        .expect("healthy agent remains");
    assert_eq!(good.status, AgentStatus::Queued);
    assert_eq!(good.extra["restartRecovery"]["stage"], "held");
    let malformed_after: String = reader
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='malformed-submitted'",
            [],
            |row| row.get(0),
        )
        .expect("malformed row remains available for repair");
    assert_eq!(malformed_after, malformed.to_string());
}

#[tokio::test]
async fn proven_not_submitted_preserves_failed_projection_and_pending_input() {
    let (_state, store, agent, epoch, _) = fixture().await;
    let intent = claim_intent(&store, &agent, epoch, "unsent", 10.0)
        .await
        .expect("claim")
        .expect("input");
    settle_start_outcome(&store, &intent, TurnStart::NotSubmitted)
        .await
        .expect("unsent");
    let reader = store.test_reader().expect("read");
    let projected:(String,bool,Option<String>)=reader.query_row("SELECT json_extract(payload,'$.value.status'),json_extract(payload,'$.value.inFlight'),json_extract(payload,'$.value.turnId') FROM sync_entities WHERE collection='agent' AND id=?1",[&agent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).expect("projection");
    assert_eq!(projected, ("failed".into(), false, None));
    let pending: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='pending'",
            [&agent],
            |r| r.get(0),
        )
        .expect("pending");
    assert_eq!(pending, 3);
}
#[tokio::test]
async fn late_acceptance_preserves_a_stopped_agent_and_delivers_exact_input() {
    let (_state, store, agent, epoch, _) = fixture().await;
    let intent = claim_intent(&store, &agent, epoch, "stop", 10.0)
        .await
        .expect("claim")
        .expect("input");
    assert!(
        mark_submitted_for_provider(&store, &intent, "stopped-request", 1, "test-connection")
            .await
            .expect("submit")
    );
    let agent_id = agent.clone();
    store
        .transaction(move |tx| {
            let mut record =
                AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.expect("agent");
            record.epoch += 1;
            record.status = AgentStatus::Paused;
            record.extra.insert("autoWake".into(), json!(false));
            AgentRepository::put(tx, &record)
        })
        .await
        .expect("stop revocation");
    settle_start_outcome(&store, &intent, TurnStart::Unknown)
        .await
        .expect("unknown retains input");
    let mut stale = intent.clone();
    stale.epoch += 2;
    assert_eq!(
        settle_started_provider(&store, &stale, "stopped-request", 1, "late")
            .await
            .expect("stale"),
        None
    );
    assert_eq!(
        settle_started_provider(&store, &intent, "stopped-request", 1, "late")
            .await
            .expect("late"),
        Some(true)
    );
    let reader = store.test_reader().expect("read");
    let record = AgentRepository::get(&reader, &AgentId::from(agent.as_str()))
        .expect("agent")
        .expect("present");
    assert_eq!(record.status, AgentStatus::Paused);
    assert_eq!(record.epoch, epoch + 1);
    let delivered:i64=reader.query_row("SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND status='delivered' AND turn_id='late'",[&agent],|r|r.get(0)).expect("delivered");
    assert_eq!(delivered, 3);
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
