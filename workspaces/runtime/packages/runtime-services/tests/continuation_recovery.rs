use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use studio_runtime_services::{
    continuation::{ContinuationError, Result},
    continuation_driver::{self, Boundary, Fault},
    usage_resume,
};
use studio_runtime_store::{ContinuationTable, RuntimeStore};
fn template_kind(kind: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("isolated template");
    let oracle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/turn_lifecycle/continuation_oracle.py");
    let result = Command::new(std::env::var("CODEX_AGENTS_PYTHON").expect("isolated Python"))
        .arg(oracle)
        .arg("template")
        .arg(dir.path())
        .arg(kind)
        .output()
        .expect("Python writer");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    dir
}
fn template() -> tempfile::TempDir {
    template_kind("usage")
}
fn clone_template(root: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("case");
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(root.join(name), dir.path().join(name)).expect("clone Python template");
    }
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
#[test]
fn every_store_boundary_rolls_back_or_replays_the_one_committed_followup() {
    let template = template();
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
        let root = clone_template(template.path());
        let store = RuntimeStore::open(Some(root.path())).expect("Python state opens");
        let before = read(&store);
        let result = continuation_driver::drive_with_fault(
            &store,
            "a",
            ContinuationTable::Usage,
            Some("resume"),
            1000.0,
            |s| usage_resume::tick(s, &observation(), 1000.0),
            &mut StopAt(boundary),
        );
        assert!(result.is_err(), "fault {boundary:?} reached");
        if boundary != Boundary::AfterCommit {
            assert_eq!(read(&store), before, "rollback at {boundary:?}");
        }
        drop(store);
        let restarted = RuntimeStore::open(Some(root.path())).expect("restart");
        continuation_driver::drive(
            &restarted,
            "a",
            ContinuationTable::Usage,
            Some("resume"),
            1000.0,
            |s| usage_resume::tick(s, &observation(), 1000.0),
        )
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
        .expect("second replay");
        assert_eq!(read(&restarted), after);
    }
}
#[test]
fn changed_task_worker_hold_refuses_without_any_write() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    store.transaction(|tx|{
        let mut a:Value=serde_json::from_str(&tx.query_row("SELECT record FROM runtime_agents WHERE id='a'",[],|r|r.get::<_,String>(0))?)?;
        a["isLead"]=json!(false);a["parentId"]=json!("parent");a["rootId"]=json!("parent");
        tx.execute("UPDATE runtime_agents SET record=?1 WHERE id='a'",[a.to_string()])?;
        let parent=json!({"id":"parent","rootId":"parent","epoch":1,"status":"waiting","autoWake":true});
        tx.execute("INSERT INTO runtime_agents VALUES('parent',?1)",[parent.to_string()])?;
        let work=json!({"id":"new","rootId":"parent","owner":"a","status":"running"});tx.execute("INSERT INTO runtime_work VALUES('new',?1)",[work.to_string()])?;Ok(())
    }).expect("changed ownership fixture");
    let before = read(&store);
    let result = continuation_driver::drive(
        &store,
        "a",
        ContinuationTable::Usage,
        Some("resume"),
        1000.0,
        |s| usage_resume::tick(s, &observation(), 1000.0),
    );
    assert!(matches!(
        result,
        Err(ContinuationError::Unavailable(
            "event-driver/permanent-worker-hold"
        ))
    ));
    assert_eq!(read(&store), before);
}

#[test]
fn foreign_followup_identity_refuses_and_rolls_back() {
    let template = template();
    let root = clone_template(template.path());
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    store.transaction(|tx|{tx.execute("INSERT INTO runtime_events VALUES('usage-resume:resume','foreign','followup','foreign payload','pending',1.0,2,NULL,NULL)",[])?;Ok(())}).expect("foreign event");
    let before = read(&store);
    let result = continuation_driver::drive(
        &store,
        "a",
        ContinuationTable::Usage,
        Some("resume"),
        1000.0,
        |s| usage_resume::tick(s, &observation(), 1000.0),
    );
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
#[test]
fn wakeup_event_and_metadata_writes_restart_atomically() {
    let template = template_kind("wake");
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
        let root = clone_template(template.path());
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        let before = wake_state(&store);
        let result = continuation_driver::drive_with_fault(
            &store,
            "a",
            ContinuationTable::Usage,
            None,
            1000.0,
            |s| Ok(studio_runtime_services::wakeups::pending_batch(s)),
            &mut StopAt(boundary),
        );
        assert!(result.is_err(), "fault {boundary:?} reached");
        if boundary != Boundary::AfterCommit {
            assert_eq!(wake_state(&store), before);
        }
        drop(store);
        let store = RuntimeStore::open(Some(root.path())).expect("restart");
        continuation_driver::drive(&store, "a", ContinuationTable::Usage, None, 1000.0, |s| {
            Ok(studio_runtime_services::wakeups::pending_batch(s))
        })
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
        .expect("idempotent replay");
        assert_eq!(wake_state(&store), after);
    }
}
