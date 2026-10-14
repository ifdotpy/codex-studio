#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, process::Command};
use studio_runtime_services::{
    agent_lifecycle_runtime::{
        self, LifecycleEffect, LifecycleEffectKind, LifecyclePorts, UnavailableLifecyclePorts,
    },
    agent_management::{Action, ManagementError, Records},
};
use studio_runtime_store::{RuntimeStore, Transaction, agent_management};

fn template(root: &Path) {
    let schema: Value = serde_json::from_str(include_str!(
        "../../runtime-store/tests/fixtures/live-state-schema.json"
    ))
    .expect("schema fixture");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        let db = Connection::open(root.join(database)).expect("fixture db");
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; BEGIN;")
            .expect("fixture durability");
        let mut objects = schema[database]["sqlite_master"]
            .as_array()
            .expect("objects")
            .iter()
            .filter(|o| {
                ![
                    "runtime_search_next_config",
                    "runtime_search_next_data",
                    "runtime_search_next_docsize",
                    "runtime_search_next_idx",
                ]
                .contains(&o["name"].as_str().expect("name"))
            })
            .collect::<Vec<_>>();
        objects.sort_by_key(|o| {
            (
                match o["type"].as_str() {
                    Some("table") => 0,
                    Some("index") => 1,
                    Some("view") => 2,
                    _ => 3,
                },
                o["name"].as_str(),
            )
        });
        for object in objects {
            db.execute_batch(object["sql"].as_str().expect("ddl"))
                .expect("fixture ddl");
        }
        db.execute_batch("COMMIT")
            .expect("fixture schema commits once");
    }
}
fn cases() -> Vec<Value> {
    let output = Command::new(prepared_python_support::prepared_python(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    ))
    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p10_lifecycle/oracle.py"))
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cases")
}
fn seed(store: &RuntimeStore, records: Records) {
    store.transaction(|tx| {
        for (table, records) in records.tables {
            for row in records {
                if table == "runtime_events" {
                    tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,'user','input',?3,1,?4)", params![row["id"].as_str(),row["agent"].as_str(),row["status"].as_str(),row["epoch"].as_i64()])?;
                } else { agent_management::write(tx, &table, &row)?; }
            }
        }
        Ok(())
    }).expect("Python rows seed");
}
fn state(store: &RuntimeStore) -> Value {
    let db = store.read().expect("reader");
    let records = agent_management::read(&db).expect("records");
    let mut statement = db.prepare("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities ORDER BY collection,id").expect("sync read");
    let sync = statement
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?
            ]))
        })
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("sync rows");
    json!({"tables":records,"sync":sync})
}

// A fixture for the transaction boundary only. Provider transport is owned by
// account_worker, whose direct Stop receipt tests run in the crate suite.
#[derive(Default)]
struct InterruptFixture {
    notification_payloads: BTreeMap<String, String>,
}
impl InterruptFixture {
    fn for_case(case: &Value) -> Self {
        let notification_payloads = case["expected"]["notificationPayloads"]
            .as_array()
            .expect("Python notification payloads")
            .iter()
            .map(|row| {
                (
                    row[0].as_str().expect("notification id").to_owned(),
                    row[1].as_str().expect("notification payload").to_owned(),
                )
            })
            .collect();
        Self {
            notification_payloads,
        }
    }
}
impl LifecyclePorts for InterruptFixture {
    fn committed(&self) -> Result<(), ManagementError> {
        Ok(())
    }
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        if effect.kind() == LifecycleEffectKind::NotifyParent {
            let before = effect.before();
            let stopped = effect.stopped();
            let marker = before
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| before.pointer("/startAttempt/id").and_then(Value::as_str))
                .unwrap_or("stop");
            let id = format!(
                "child-stop:{}:{}:stop:{marker}",
                stopped["id"].as_str().unwrap_or_default(),
                stopped["epoch"].as_i64().unwrap_or_default()
            );
            let parent_id = before["parentId"].as_str().unwrap_or_default();
            let parent_epoch: i64 = tx
                .query_row(
                    "SELECT json_extract(record,'$.epoch') FROM runtime_agents WHERE id=?1",
                    [parent_id],
                    |row| row.get(0),
                )
                .map_err(studio_runtime_store::StoreError::from)?;
            let text = self
                .notification_payloads
                .get(&id)
                .map(String::as_str)
                .unwrap_or("{}");
            tx.execute(
                "INSERT OR IGNORE INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                 VALUES(?1,?2,'child_result',?3,'pending',?4,?5)",
                rusqlite::params![id, parent_id, text, effect.at(), parent_epoch],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            return Ok(());
        }
        if effect.kind() != LifecycleEffectKind::Interrupt {
            return Err(ManagementError::Unavailable(effect.kind().dependency()));
        }
        let before = effect.before();
        let stopped = effect.stopped();
        assert_eq!(
            stopped["epoch"].as_i64(),
            before["epoch"].as_i64().map(|v| v + 1)
        );
        assert_eq!(before.get("startAttempt"), stopped.get("startAttempt"));
        assert_eq!(stopped["autoWake"], false);
        let saved: String = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [stopped["id"].as_str()],
                |r| r.get(0),
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        assert_eq!(
            serde_json::from_str::<Value>(&saved).expect("saved agent"),
            *stopped
        );
        Ok(())
    }
}

fn clone_store(base: &Path, root: &Path) -> RuntimeStore {
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        fs::copy(base.join(name), root.join(name)).expect("clone schema");
    }
    RuntimeStore::open(Some(root)).expect("open cloned fixture")
}

fn assert_records(store: &RuntimeStore, case: &Value) {
    let saved = state(store);
    for (table, expected) in case["expected"]["records"]["tables"]
        .as_object()
        .expect("tables")
    {
        assert_eq!(
            &saved["tables"][table], expected,
            "{} {table}",
            case["name"]
        );
    }
    let actual_sync = saved["sync"]
        .as_array()
        .expect("sync projection rows")
        .iter()
        .filter(|row| row[0] != "workspace")
        .map(|row| json!([row[0], row[1], row[4], row[5]]))
        .collect::<Vec<_>>();
    let mut expected_sync = case["expected"]["syncEntities"]
        .as_array()
        .expect("Python sync projection oracle")
        .clone();
    expected_sync.iter_mut().for_each(|row| {
        *row = json!([row[0], row[1], row[2], row[3]]);
    });
    expected_sync.sort_by_key(|row| {
        (
            row[0].as_str().unwrap_or_default().to_owned(),
            row[1].as_str().unwrap_or_default().to_owned(),
        )
    });
    assert_eq!(actual_sync, expected_sync, "{} sync_entities", case["name"]);
}

#[test]
fn python_stop_corpus_complete_records_and_exact_descendants() {
    let cases = cases();
    assert_eq!(cases.len(), 18);
    let base = tempfile::tempdir().expect("template");
    template(base.path());
    for case in cases {
        let root = tempfile::tempdir().expect("case");
        let store = clone_store(base.path(), root.path());
        seed(
            &store,
            serde_json::from_value(case["records"].clone()).expect("Python rows"),
        );
        let actions = case["actions"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![case["action"].clone()]);
        let ports = InterruptFixture::for_case(&case);
        let mut result = Value::Null;
        for action in actions {
            let action: Action = serde_json::from_value(action).expect("action");
            result = agent_lifecycle_runtime::execute(&store, &action, 1234567.5, &ports)
                .expect("stop commits");
        }
        assert_eq!(result, case["expected"]["result"], "{}", case["name"]);
        let db = store.read().expect("notification reader");
        let notifications = db
            .prepare("SELECT id,agent,status FROM runtime_events WHERE id LIKE 'child-stop:%' ORDER BY id")
            .expect("notification query")
            .query_map([], |row| {
                Ok(json!([row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?]))
            })
            .expect("notification rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("notification result");
        assert_eq!(
            notifications,
            case["expected"]["notifications"]
                .as_array()
                .expect("Python notifications")
                .clone(),
            "{} notification recipients and count",
            case["name"]
        );
        assert_records(&store, &case);
        drop(store);
        let store = RuntimeStore::open(Some(root.path())).expect("restart");
        assert_records(&store, &case);
    }
}

#[test]
fn lifecycle_store_failpoints_and_missing_consumer_leave_no_writes() {
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unknown-True")
        .expect("unknown corpus");
    let base = tempfile::tempdir().expect("template");
    template(base.path());
    let store = RuntimeStore::open(Some(base.path())).expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    );
    drop(store);
    for (table, operation) in [
        ("runtime_agents", "UPDATE"),
        ("runtime_requests", "UPDATE"),
        ("runtime_events", "UPDATE"),
        ("sync_entities", "INSERT"),
    ] {
        for boundary in ["BEFORE", "AFTER"] {
            let root = tempfile::tempdir().expect("case");
            let store = clone_store(base.path(), root.path());
            let before = state(&store);
            store.transaction(|tx| { tx.execute_batch(&format!("CREATE TRIGGER failpoint {boundary} {operation} ON {table} BEGIN SELECT RAISE(ABORT, 'stop failure'); END"))?; Ok(()) }).expect("failpoint");
            let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
            assert!(
                agent_lifecycle_runtime::execute(
                    &store,
                    &action,
                    1234567.5,
                    &InterruptFixture::default(),
                )
                .is_err()
            );
            assert_eq!(state(&store), before, "{boundary} {table}");
            drop(store);
            let store = RuntimeStore::open(Some(root.path())).expect("restart");
            assert_eq!(state(&store), before);
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER failpoint")?;
                    Ok(())
                })
                .expect("remove fault");
            let refusal = agent_lifecycle_runtime::execute(
                &store,
                &action,
                1234567.5,
                &UnavailableLifecyclePorts,
            );
            assert!(matches!(
                refusal,
                Err(ManagementError::Unavailable(
                    "account-worker/agent-interrupt"
                ))
            ));
            assert_eq!(state(&store), before);
            agent_lifecycle_runtime::execute(
                &store,
                &action,
                1234567.5,
                &InterruptFixture::default(),
            )
            .expect("recover");
            assert_records(&store, &case);
        }
    }
}

#[test]
fn deletion_requires_release_only_for_releasable_deleted_owner_work() {
    let mut case = cases()
        .into_iter()
        .find(|c| c["name"] == "delete-worker-review-work")
        .expect("review work corpus case");
    case["records"]["tables"]["runtime_work"][0]["status"] = json!("ready");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    );
    let before = state(&store);
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(matches!(
        agent_lifecycle_runtime::execute(&store, &action, 1234567.5, &UnavailableLifecyclePorts),
        Err(ManagementError::Unavailable("work-driver/deletion-release"))
    ));
    assert_eq!(state(&store), before);
}

#[test]
fn effect_adapter_failure_rolls_back_its_own_writes() {
    struct Fails;
    impl LifecyclePorts for Fails {
        fn committed(&self) -> Result<(), ManagementError> {
            panic!("must not wake after rollback")
        }
        fn stage(&self, tx: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
            tx.execute(
                "UPDATE runtime_events SET status='pending' WHERE status='uncertain'",
                [],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            Err(ManagementError::Invalid("injected adapter failure"))
        }
    }
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unknown-True")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    );
    let before = state(&store);
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(agent_lifecycle_runtime::execute(&store, &action, 1234567.5, &Fails).is_err());
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    assert_eq!(state(&store), before);
}

#[test]
fn successful_adapter_cannot_resurrect_stopped_agent() {
    struct Resumes;
    impl LifecyclePorts for Resumes {
        fn committed(&self) -> Result<(), ManagementError> {
            panic!("must not wake after rollback")
        }
        fn stage(
            &self,
            tx: &Transaction<'_>,
            effect: &LifecycleEffect,
        ) -> Result<(), ManagementError> {
            let mut resumed = effect.stopped().clone();
            resumed["autoWake"] = json!(true);
            agent_management::write(tx, "runtime_agents", &resumed)?;
            Ok(())
        }
    }
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unknown-True")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    );
    let before = state(&store);
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(matches!(
        agent_lifecycle_runtime::execute(&store, &action, 1234567.5, &Resumes),
        Err(ManagementError::Invalid(
            "Lifecycle adapter changed stop identity"
        ))
    ));
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    assert_eq!(state(&store), before);
}

#[test]
fn successful_adapter_cannot_replace_attempt_or_provider_identity() {
    struct ReplacesIdentity(&'static str);
    impl LifecyclePorts for ReplacesIdentity {
        fn committed(&self) -> Result<(), ManagementError> {
            panic!("must not wake after rollback")
        }
        fn stage(
            &self,
            tx: &Transaction<'_>,
            effect: &LifecycleEffect,
        ) -> Result<(), ManagementError> {
            let mut changed = effect.stopped().clone();
            changed[self.0] = json!("replacement");
            agent_management::write(tx, "runtime_agents", &changed)?;
            Ok(())
        }
    }
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unknown-True")
        .expect("case");
    for field in ["startAttempt", "threadId", "accountKey", "provider"] {
        let root = tempfile::tempdir().expect("root");
        template(root.path());
        let store = RuntimeStore::open(Some(root.path())).expect("open");
        seed(
            &store,
            serde_json::from_value(case["records"].clone()).expect("rows"),
        );
        let before = state(&store);
        let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
        assert!(matches!(
            agent_lifecycle_runtime::execute(&store, &action, 1234567.5, &ReplacesIdentity(field)),
            Err(ManagementError::Invalid(
                "Lifecycle adapter changed stop identity"
            ))
        ));
        assert_eq!(state(&store), before, "{field}");
    }
}

#[test]
fn lost_wake_preserves_committed_stop_for_restart() {
    struct LostWake;
    impl LifecyclePorts for LostWake {
        fn stage(
            &self,
            tx: &Transaction<'_>,
            effect: &LifecycleEffect,
        ) -> Result<(), ManagementError> {
            InterruptFixture::default().stage(tx, effect)
        }
        fn committed(&self) -> Result<(), ManagementError> {
            Err(ManagementError::Invalid("injected lost wake"))
        }
    }
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unknown-True")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(matches!(
        agent_lifecycle_runtime::execute(&store, &action, 1234567.5, &LostWake),
        Err(ManagementError::Store(_))
    ));
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    assert_records(&store, &case);
}
