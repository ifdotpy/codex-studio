use serde_json::{Value, json};
use studio_runtime_services::{
    continuation::{ContinuationError, Result},
    continuation_driver::{self, Boundary, Fault},
    usage_resume,
};
use studio_runtime_store::{ContinuationTable, RuntimeStore};
/// A fresh state holding agent `a` with a scheduled usage resume; `capacity`
/// adds a scheduled capacity retry and `wake` the events of a progress batch.
async fn seeded(kind: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("isolated state");
    let store = RuntimeStore::open(Some(dir.path()))
        .await
        .expect("fresh state opens");
    let mut agent = json!({"id":"a","rootId":"a","epoch":1,"status":"failed","accountKey":"default",
        "threadId":"thread","lastCompletedTurn":"turn","autoWake":true,"nativeFailureHold":true,
        "cwd":"/fixture","isLead":true});
    let resume = json!({"id":"resume","accountKey":"default","threadId":"thread","epoch":1,
        "turnId":"turn","cause":"usage_limit","status":"scheduled","failedAt":900.0,"authAttempt":0,
        "authRefreshMarker":null,"dueAt":990.0,"resetAt":null,"plannedAt":null,"reason":null,
        "taskClaims":[],"updatedAt":800.0});
    agent["usageResume"] = resume.clone();
    let mut rows = vec![("runtime_usage_resumes", resume)];
    if kind == "capacity" {
        let retry = json!({"id":"retry","accountKey":"default","threadId":"thread","epoch":1,
            "turnId":"turn","cause":"serverOverloaded","status":"scheduled","dueAt":990.0,
            "attempt":1,"maxAttempts":4,"cwd":"/fixture","settings":{},"taskClaims":[],
            "updatedAt":800.0});
        agent["capacityRetry"] = retry.clone();
        rows.push(("runtime_capacity_retries", retry));
    }
    let events: Vec<Value> = if kind == "wake" {
        let cases: Value =
            serde_json::from_str(include_str!("continuation/cases.json")).expect("corpus JSON");
        cases
            .as_array()
            .expect("cases")
            .iter()
            .find(|c| c["name"] == "progress-(1, 2)")
            .expect("progress fixture")["input"]["events"]
            .as_array()
            .expect("events")
            .clone()
    } else {
        vec![]
    };
    store
        .transaction_with(move |tx| {
            for (table, record) in &rows {
                tx.execute(
                    &format!("INSERT INTO {table}(id,agent,record) VALUES (?1,'a',?2)"),
                    [record["id"].as_str().expect("id"), record.to_string().as_str()],
                )?;
            }
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('a',?1)",
                [agent.to_string()],
            )?;
            for e in &events {
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    rusqlite::params![e["id"].as_str(), e["agent"].as_str(), e["kind"].as_str(),
                        e["text"].as_str(), e["status"].as_str(), e["created"].as_f64(),
                        e["epoch"].as_i64(), e["turn_id"].as_str(), e["error"].as_str()],
                )?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("seed state");
    drop(store);
    dir
}
fn read(store: &RuntimeStore) -> Value {
    let db = store.read().expect("reader");
    let mut statement = db
        .prepare("SELECT record FROM runtime_usage_resumes WHERE id='resume'")
        .expect("statement");
    let r: Value = serde_json::from_str(
        &statement
            .query_row([], |r| r.get::<_, String>(0))
            .expect("row"),
    )
    .expect("record");
    let a: Value = serde_json::from_str(
        &db.query_row("SELECT record FROM runtime_agents WHERE id='a'", [], |r| {
            r.get::<_, String>(0)
        })
        .expect("agent"),
    )
    .expect("agent JSON");
    let events: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='usage-resume:resume'",
            [],
            |r| r.get(0),
        )
        .expect("events");
    json!({"agent":a,"record":r,"eventCount":events})
}
struct StopAt(Boundary);
impl Fault for StopAt {
    fn at(&mut self, b: Boundary) -> Result<()> {
        if b == self.0 {
            Err(ContinuationError::Invalid("injected stop".into()))
        } else {
            Ok(())
        }
    }
}
fn observation() -> usage_resume::LimitsObservation {
    usage_resume::LimitsObservation::from_record(json!({"accountKey":"default","at":1000.0,"data":{"rateLimits":{"primary":{"usedPercent":10.0,"resetsAt":2000.0}}}}),None).expect("observation")
}
#[tokio::test]
async fn every_store_boundary_rolls_back_or_replays_the_one_committed_followup() {
    let full = std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1");
    for boundary in [
        Boundary::BeforeSnapshot,
        Boundary::AfterSnapshot,
        Boundary::BeforeRecord,
        Boundary::AfterRecord,
        Boundary::BeforeEffectRead,
        Boundary::AfterEffectRead,
        Boundary::BeforeEffect,
        Boundary::AfterEffect,
        Boundary::BeforeAgent,
        Boundary::AfterAgent,
        Boundary::BeforeCommit,
        Boundary::AfterCommit,
    ] {
        if !full
            && !matches!(
                boundary,
                Boundary::BeforeRecord | Boundary::AfterEffect | Boundary::AfterCommit
            )
        {
            continue;
        }
        let root = seeded("usage").await;
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("seeded state opens");
        let before = read(&store);
        let result = continuation_driver::drive_with_fault(
            &store,
            "a",
            ContinuationTable::Usage,
            Some("resume"),
            1000.0,
            |s| usage_resume::tick(s, &observation(), 1000.0),
            StopAt(boundary),
        )
        .await;
        assert!(result.is_err(), "fault {boundary:?} reached");
        if boundary != Boundary::AfterCommit {
            assert_eq!(read(&store), before, "rollback at {boundary:?}");
        }
        drop(store);
        let restarted = RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart");
        continuation_driver::drive(
            &restarted,
            "a",
            ContinuationTable::Usage,
            Some("resume"),
            1000.0,
            |s| usage_resume::tick(s, &observation(), 1000.0),
        )
        .await
        .expect("retry safely discovers durable state");
        let after = read(&restarted);
        assert_eq!(after["eventCount"], 1);
        assert_eq!(after["record"]["status"], "started");
        assert_eq!(after["agent"]["status"], "queued");
        continuation_driver::drive(
            &restarted,
            "a",
            ContinuationTable::Usage,
            Some("resume"),
            1100.0,
            |s| usage_resume::tick(s, &observation(), 1100.0),
        )
        .await
        .expect("second replay");
        assert_eq!(read(&restarted), after);
    }
}
#[tokio::test]
async fn changed_task_cancels_resume_and_notifies_parent_atomically() {
    let root = seeded("usage").await;
    let store = RuntimeStore::open(Some(root.path())).await.expect("store");
    store.transaction(|tx|{
        let mut a:Value=serde_json::from_str(&tx.query_row("SELECT record FROM runtime_agents WHERE id='a'",[],|r|r.get::<_,String>(0))?)?;
        a["isLead"]=json!(false);a["parentId"]=json!("parent");a["rootId"]=json!("parent");
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='a'",[a.to_string()])?;
        let parent=json!({"id":"parent","rootId":"parent","epoch":1,"status":"waiting","autoWake":true});
        tx.execute("INSERT INTO runtime_agents VALUES('parent',?1)",[parent.to_string()])?;
        let work=json!({"id":"new","rootId":"parent","owner":"a","status":"running"});tx.execute("INSERT INTO runtime_work VALUES('new',?1)",[work.to_string()])?;Ok(())
    }).await.expect("changed ownership fixture");
    let result = continuation_driver::drive(
        &store,
        "a",
        ContinuationTable::Usage,
        Some("resume"),
        1000.0,
        |s| usage_resume::tick(s, &observation(), 1000.0),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let after = read(&store);
    assert_eq!(after["record"]["status"], "cancelled");
    assert_eq!(after["agent"]["usageResume"]["status"], "cancelled");
    let db = store.read().expect("reader");
    let parent_event: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='hold:resume:task-changed' AND kind='child_result'",
            [],
            |row| row.get(0),
        )
        .expect("parent hold notification");
    assert_eq!(parent_event, 1);
}

#[tokio::test]
async fn foreign_followup_identity_refuses_and_rolls_back() {
    let root = seeded("usage").await;
    let store = RuntimeStore::open(Some(root.path())).await.expect("store");
    store.transaction(|tx|{tx.execute("INSERT INTO runtime_events VALUES('usage-resume:resume','foreign','followup','foreign payload','pending',1.0,2,NULL,NULL)",[])?;Ok(())}).await.expect("foreign event");
    let before = read(&store);
    let result = continuation_driver::drive(
        &store,
        "a",
        ContinuationTable::Usage,
        Some("resume"),
        1000.0,
        |s| usage_resume::tick(s, &observation(), 1000.0),
    )
    .await;
    assert!(matches!(result,Err(ContinuationError::Invalid(ref at)) if at=="/events/identity"));
    assert_eq!(read(&store), before);
}

type WakeState = (Vec<(String, String, String)>, Vec<(String, String)>);
fn wake_state(store: &RuntimeStore) -> WakeState {
    let db = store.read().expect("reader");
    let mut events = db
        .prepare("SELECT id,status,text FROM runtime_events ORDER BY id")
        .expect("events");
    let rows = events
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("rows")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("event rows");
    let mut metadata = db
        .prepare("SELECT id,record FROM runtime_event_meta ORDER BY id")
        .expect("metadata");
    let meta = metadata
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("rows")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("metadata rows");
    (rows, meta)
}
#[tokio::test]
async fn wakeup_event_and_metadata_writes_restart_atomically() {
    let full = std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1");
    for boundary in [
        Boundary::BeforeEvent,
        Boundary::AfterEvent,
        Boundary::BeforeMetadata,
        Boundary::AfterMetadata,
        Boundary::BeforeCommit,
        Boundary::AfterCommit,
    ] {
        if !full && !matches!(boundary, Boundary::AfterEvent | Boundary::AfterMetadata) {
            continue;
        }
        let root = seeded("wake").await;
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        let before = wake_state(&store);
        let result = continuation_driver::drive_with_fault(
            &store,
            "a",
            ContinuationTable::Usage,
            None,
            1000.0,
            |s| Ok(studio_runtime_services::wakeups::pending_batch(s)),
            StopAt(boundary),
        )
        .await;
        assert!(result.is_err(), "fault {boundary:?} reached");
        if boundary != Boundary::AfterCommit {
            assert_eq!(wake_state(&store), before);
        }
        drop(store);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart");
        continuation_driver::drive(&store, "a", ContinuationTable::Usage, None, 1000.0, |s| {
            Ok(studio_runtime_services::wakeups::pending_batch(s))
        })
        .await
        .expect("replay");
        let after = wake_state(&store);
        assert_eq!(
            after
                .0
                .iter()
                .filter(|(_, status, _)| status == "pending")
                .count(),
            1
        );
        assert_eq!(after.1.len(), 1);
        continuation_driver::drive(&store, "a", ContinuationTable::Usage, None, 1000.0, |s| {
            Ok(studio_runtime_services::wakeups::pending_batch(s))
        })
        .await
        .expect("idempotent replay");
        assert_eq!(wake_state(&store), after);
    }
}

#[tokio::test]
async fn corrupt_continuation_rows_name_the_table_and_identity() {
    let template = seeded("capacity").await;
    let store = RuntimeStore::open(Some(template.path()))
        .await
        .expect("seeded state");
    let agent: String = store
        .read()
        .expect("reader")
        .query_row("SELECT record FROM runtime_agents WHERE id='a'", [], |r| {
            r.get(0)
        })
        .expect("seeded agent");
    for (table, id) in [
        (ContinuationTable::Usage, "resume"),
        (ContinuationTable::Capacity, "retry"),
    ] {
        // Expression indexes reject corrupt JSON on insertion. An unindexed isolated table
        // lets the repository exercise its read diagnostic for already-corrupt stored bytes.
        let db = studio_runtime_store::Connection::open_in_memory().expect("isolated row fixture");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT,record TEXT); CREATE TABLE runtime_usage_resumes(id TEXT,agent TEXT,record TEXT); CREATE TABLE runtime_capacity_retries(id TEXT,agent TEXT,record TEXT);").expect("tables");
        db.execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        db.execute("INSERT INTO runtime_agents VALUES('a',?1)", [&agent])
            .expect("agent");
        db.execute(
            &format!("INSERT INTO {} VALUES(?1,'a',?2)", table.name()),
            [id, "private malformed bytes {"],
        )
        .expect("corrupt row");
        let error =
            studio_runtime_store::ContinuationRepository::snapshot(&db, "a", table, Some(id))
                .expect_err("invalid row");
        assert!(
            matches!(&error,studio_runtime_store::StoreError::InvalidStoredObject { table: name,id: stored_id } if *name==table.name() && stored_id==id)
        );
        assert!(!error.to_string().contains("private malformed bytes"));
    }
}

fn capacity_state(store: &RuntimeStore) -> Value {
    let db = store.read().expect("reader");
    let record: String = db
        .query_row(
            "SELECT record FROM runtime_capacity_retries WHERE id='retry'",
            [],
            |r| r.get(0),
        )
        .expect("retry");
    let agent: String = db
        .query_row("SELECT record FROM runtime_agents WHERE id='a'", [], |r| {
            r.get(0)
        })
        .expect("agent");
    let events: i64 = db
        .query_row("SELECT count(*) FROM runtime_events", [], |r| r.get(0))
        .expect("events");
    json!({"record":serde_json::from_str::<Value>(&record).expect("record JSON"),"agent":serde_json::from_str::<Value>(&agent).expect("agent JSON"),"events":events})
}
struct FixtureAdmission;
impl continuation_driver::CapacityAdmission for FixtureAdmission {
    fn check(
        &mut self,
        tx: &studio_runtime_store::Transaction<'_>,
        snapshot: &studio_runtime_services::continuation::Snapshot,
        attempt: &Value,
    ) -> Result<()> {
        assert_eq!(snapshot.agent_id(), "a");
        assert_eq!(attempt["capacityRetryId"], "retry");
        let status:String=tx.query_row("SELECT json_extract(record,'$.status') FROM runtime_capacity_retries WHERE id='retry'",[],|r|r.get(0)).expect("fresh admission read");
        assert_eq!(status, "scheduled");
        Ok(())
    }
}
#[tokio::test]
async fn capacity_claim_commits_record_and_agent_together_and_recovery_never_resends() {
    use studio_runtime_services::capacity_retry::{self, Action};
    let full = std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1");
    for boundary in [
        Boundary::BeforeSnapshot,
        Boundary::AfterSnapshot,
        Boundary::BeforeCapacityAdmission,
        Boundary::AfterCapacityAdmission,
        Boundary::BeforeRecord,
        Boundary::AfterRecord,
        Boundary::BeforeEffect,
        Boundary::AfterEffect,
        Boundary::BeforeAgent,
        Boundary::AfterAgent,
        Boundary::BeforeCommit,
        Boundary::AfterCommit,
    ] {
        if !full
            && !matches!(
                boundary,
                Boundary::AfterRecord | Boundary::AfterAgent | Boundary::AfterCommit
            )
        {
            continue;
        }
        let root = seeded("capacity").await;
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        let before = capacity_state(&store);
        // The default adapter still refuses absent admission without persisting a claim.
        assert!(matches!(
            continuation_driver::drive(
                &store,
                "a",
                ContinuationTable::Capacity,
                Some("retry"),
                1000.0,
                |s| capacity_retry::transition(
                    s,
                    Action::Claim {
                        settings: &json!({}),
                        automatic: true,
                    },
                    1000.0
                )
            )
            .await,
            Err(ContinuationError::Unavailable(
                "turn-driver/capacity-admission"
            ))
        ));
        assert_eq!(capacity_state(&store), before);
        let result = continuation_driver::drive_with_hooks(
            &store,
            continuation_driver::ContinuationTarget {
                agent: "a",
                table: ContinuationTable::Capacity,
                record: Some("retry"),
            },
            1000.0,
            |s| {
                capacity_retry::transition(
                    s,
                    Action::Claim {
                        settings: &json!({}),
                        automatic: true,
                    },
                    1000.0,
                )
            },
            StopAt(boundary),
            FixtureAdmission,
        )
        .await;
        assert!(result.is_err(), "fault {boundary:?} reached");
        if boundary != Boundary::AfterCommit {
            assert_eq!(capacity_state(&store), before);
        }
        drop(store);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart");
        let committed = continuation_driver::drive_with_hooks(
            &store,
            continuation_driver::ContinuationTarget {
                agent: "a",
                table: ContinuationTable::Capacity,
                record: Some("retry"),
            },
            1000.0,
            |s| {
                capacity_retry::transition(
                    s,
                    Action::Claim {
                        settings: &json!({}),
                        automatic: true,
                    },
                    1000.0,
                )
            },
            continuation_driver::NoFault,
            FixtureAdmission,
        )
        .await
        .expect("retry discovers commit");
        assert_eq!(
            committed.effects.len(),
            usize::from(boundary != Boundary::AfterCommit)
        );
        let after = capacity_state(&store);
        assert_eq!(after["record"]["status"], "starting");
        assert_eq!(after["agent"]["capacityRetry"], after["record"]);
        assert_eq!(after["agent"]["startAttempt"]["submitted"], false);
        assert_eq!(after["events"], before["events"]);
        let replay = continuation_driver::drive(
            &store,
            "a",
            ContinuationTable::Capacity,
            Some("retry"),
            1000.0,
            |s| {
                capacity_retry::transition(
                    s,
                    Action::Claim {
                        settings: &json!({}),
                        automatic: true,
                    },
                    1000.0,
                )
            },
        )
        .await
        .expect("claim replay");
        assert!(replay.effects.is_empty());
        assert_eq!(capacity_state(&store), after);
        let recovered = continuation_driver::drive(
            &store,
            "a",
            ContinuationTable::Capacity,
            Some("retry"),
            1000.0,
            |s| capacity_retry::transition(s, Action::Restart, 1000.0),
        )
        .await
        .expect("unsent recovery");
        assert!(!recovered.effects.iter().any(|e| matches!(
            e,
            studio_runtime_services::continuation::Effect::CapacityStart { .. }
        )));
        assert_eq!(capacity_state(&store)["record"]["status"], "cancelled");
    }
}

#[tokio::test]
async fn automatic_capacity_retry_cancels_an_excluded_account_with_a_permanent_hold() {
    let template = seeded("capacity").await;
    let store = RuntimeStore::open(Some(template.path()))
        .await
        .expect("seeded state opens");
    store
        .transaction_with(|tx| {
            let mut agent: Value = tx
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id='a'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map(|raw| serde_json::from_str(&raw).expect("agent JSON"))?;
            agent["excludedAccountKeys"] = json!(["default"]);
            studio_runtime_store::ContinuationRepository::put_agent(tx, &agent)?;
            let stage = studio_runtime_services::continuation_runtime::stage_capacity_retry(
                tx, "a", "retry", 1000.0, true,
            )
            .map_err(|error| studio_runtime_store::StoreError::SchemaMismatch(error.to_string()))?;
            assert!(!stage.effects().iter().any(|effect| matches!(
                effect,
                studio_runtime_services::continuation::Effect::CapacityStart { .. }
            )));
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("atomic exclusion settlement");
    let state = capacity_state(&store);
    assert_eq!(state["record"]["status"], "cancelled");
    assert_eq!(state["record"]["dueAt"], Value::Null);
    assert_eq!(
        state["record"]["reason"],
        "Account default is disabled for this chat"
    );
    assert_eq!(state["agent"]["status"], "failed");
    assert_eq!(state["agent"]["inFlight"], false);
    assert_eq!(
        state["agent"]["error"],
        "Account default is disabled for this chat"
    );
}

const MIB: u64 = 1024 * 1024;
const INCIDENT: &str = "ENOSPC: no space left on device, rename '/state/sessions/55f43293.meta.json.7c1e.tmp' -> '/state/sessions/55f43293.meta.json'";

fn capacity_start(effects: &[studio_runtime_services::continuation::Effect]) -> bool {
    effects.iter().any(|effect| {
        matches!(
            effect,
            studio_runtime_services::continuation::Effect::CapacityStart { .. }
        )
    })
}

#[tokio::test]
async fn a_due_capacity_retry_waits_while_the_disk_is_full_and_claims_after() {
    let template = seeded("capacity").await;
    let store = RuntimeStore::open(Some(template.path()))
        .await
        .expect("seeded state opens");
    let before = capacity_state(&store);
    let staged = store
        .transaction_with(|tx| {
            studio_runtime_services::continuation_runtime::stage_capacity_retry_observed(
                tx,
                "a",
                "retry",
                1000.0,
                true,
                Some(100 * MIB),
            )
            .map_err(|error| studio_runtime_store::StoreError::SchemaMismatch(error.to_string()))
        })
        .await
        .expect("disk-full wait");
    assert!(
        !capacity_start(staged.effects()),
        "no turn starts while the disk is full"
    );
    let held = capacity_state(&store);
    assert_eq!(held["record"]["status"], "scheduled");
    assert_eq!(held["record"]["dueAt"], 1015.0);
    assert_eq!(
        held["record"]["reason"],
        "Disk is full: 100 MiB free. Studio resumes this agent when free space returns."
    );
    assert_eq!(held["record"].get("claimedAt"), None);
    assert_eq!(held["record"].get("waits"), before["record"].get("waits"));
    assert_eq!(
        held["agent"].get("startAttempt"),
        before["agent"].get("startAttempt")
    );
    assert_eq!(held["agent"]["capacityRetry"], held["record"]);
    assert_eq!(held["events"], before["events"]);

    store
        .transaction_with(|tx| {
            studio_runtime_services::continuation_runtime::stage_capacity_retry_observed(
                tx, "a", "retry", 1016.0, true, None,
            )
            .map_err(|error| studio_runtime_store::StoreError::SchemaMismatch(error.to_string()))
        })
        .await
        .expect("claim after space returns");
    let resumed = capacity_state(&store);
    assert!(
        !resumed["record"]["reason"]
            .as_str()
            .unwrap_or("")
            .starts_with("Disk is full"),
        "free space ends the disk wait: {resumed}"
    );
}

#[tokio::test]
async fn a_disk_full_turn_failure_creates_a_free_space_retry() {
    let root = seeded("usage").await;
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("seeded state opens");
    let turn = json!({"id":"turn","status":"failed","error":{"message":INCIDENT}});
    let staged = store
        .transaction_with(move |tx| {
            studio_runtime_services::continuation_runtime::stage(
                tx,
                studio_runtime_services::continuation_runtime::Completion::TerminalTurn {
                    agent_id: "a",
                    turn: &turn,
                    known: true,
                    now: 1000.0,
                    limits: None,
                },
            )
            .map_err(|error| studio_runtime_store::StoreError::SchemaMismatch(error.to_string()))
        })
        .await
        .expect("disk-full failure stages a continuation");
    assert!(staged.changed());
    let db = store.read().expect("reader");
    let agent: Value = db
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='a'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(|raw| serde_json::from_str(&raw).expect("agent JSON"))
        .expect("agent");
    let retry = &agent["capacityRetry"];
    assert_eq!(retry["cause"], "diskFull");
    assert_eq!(retry["status"], "scheduled");
    assert_eq!(retry["dueAt"], 1000.0);
    assert_eq!(retry["turnId"], "turn");
    assert_eq!(retry["maxAttempts"], 5);
    assert!(
        retry["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("Disk is full: ")),
        "{retry}"
    );
    let stored: String = db
        .query_row(
            "SELECT record FROM runtime_capacity_retries WHERE agent='a'",
            [],
            |row| row.get(0),
        )
        .expect("durable retry row");
    assert_eq!(
        &serde_json::from_str::<Value>(&stored).expect("retry JSON"),
        retry
    );
}
