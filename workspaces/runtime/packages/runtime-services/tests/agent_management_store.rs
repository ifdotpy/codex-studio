use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use studio_runtime_services::{
    agent_management::{Action, Records},
    agent_management_store::execute,
};
use studio_runtime_store::{RuntimeStore, agent_management};

fn template(root: &Path) {
    let schema: Value = serde_json::from_str(include_str!(
        "../../runtime-store/tests/fixtures/live-state-schema.json"
    ))
    .expect("schema fixture");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        let db = Connection::open(root.join(database)).expect("fixture db");
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
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
    }
}
fn cases() -> Vec<Value> {
    let output = Command::new(std::env::var("CODEX_AGENTS_PYTHON").expect("isolated Python"))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/agent_management/oracle.py"))
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

#[test]
fn transaction_failure_matrix_rolls_back_rows_and_projections_after_restart() {
    let cases = cases();
    let mut count = 0;
    for case_name in ["unassign", "stop-True"] {
        let case = cases
            .iter()
            .find(|c| c["name"] == case_name)
            .expect("witness");
        let base = tempfile::tempdir().expect("base");
        template(base.path());
        let store = RuntimeStore::open(Some(base.path())).expect("template opens");
        seed(
            &store,
            serde_json::from_value(case["records"].clone()).expect("rows"),
        );
        drop(store);
        let tables = if case_name == "unassign" {
            vec!["runtime_work", "runtime_agents", "sync_entities"]
        } else {
            vec![
                "runtime_agents",
                "runtime_requests",
                "runtime_events",
                "sync_entities",
            ]
        };
        for table in tables {
            for timing in ["BEFORE", "AFTER"] {
                let root = tempfile::tempdir().expect("case");
                for database in ["canvas.sqlite3", "analytics.sqlite3"] {
                    fs::copy(base.path().join(database), root.path().join(database))
                        .expect("clone prepared template");
                }
                let store = RuntimeStore::open(Some(root.path())).expect("open clone");
                let before = state(&store);
                let event = if table == "sync_entities" {
                    "INSERT"
                } else {
                    "UPDATE"
                };
                store.transaction(|tx| { tx.execute_batch(&format!("CREATE TRIGGER lifecycle_fail {timing} {event} ON {table} BEGIN SELECT RAISE(ABORT,'lifecycle failpoint'); END"))?; Ok(()) }).expect("failpoint installed");
                let action: Action =
                    serde_json::from_value(case["action"].clone()).expect("action");
                assert!(
                    execute(&store, &action, 1234567.5).is_err(),
                    "{case_name} {timing} {table}"
                );
                assert_eq!(
                    state(&store),
                    before,
                    "rollback {case_name} {timing} {table}"
                );
                drop(store);
                let store = RuntimeStore::open(Some(root.path())).expect("restart");
                assert_eq!(
                    state(&store),
                    before,
                    "restart {case_name} {timing} {table}"
                );
                store
                    .transaction(|tx| {
                        tx.execute_batch("DROP TRIGGER lifecycle_fail")?;
                        Ok(())
                    })
                    .expect("remove failpoint");
                let result = execute(&store, &action, 1234567.5).expect("recovery applies once");
                assert_eq!(result, case["expected"]["result"]);
                let saved = state(&store);
                for (table, rows) in case["expected"]["records"]["tables"]
                    .as_object()
                    .expect("expected tables")
                {
                    assert_eq!(
                        &saved["tables"][table], rows,
                        "complete stored rows {case_name} {table}"
                    );
                }

                count += 1;
            }
        }
    }
    assert_eq!(count, 14);
}

#[test]
fn committed_archive_response_loss_replays_after_restart_without_duplicate_release() {
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unassign")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("records"),
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    let first = execute(&store, &action, 1234567.5).expect("archive");
    assert_eq!(first, case["expected"]["result"]);
    let committed = state(&store);
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    let result = execute(&store, &action, 1234568.0).expect("replay");
    assert_eq!(result["replayed"], true);
    assert_eq!(state(&store), committed);
    assert_eq!(
        committed["tables"]["runtime_work"][0]["releases"]
            .as_array()
            .expect("releases")
            .len(),
        1
    );
}

#[test]
fn deleted_team_broadcast_projection_survives_repeated_delete() {
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "delete")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("records"),
    );
    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_rooms(id,record) VALUES('broadcast',?1)",
                [
                    json!({"id":"broadcast","rootId":"lead","kind":"broadcast","created":1.0})
                        .to_string(),
                ],
            )?;
            Ok(())
        })
        .expect("room");
    let action = Action::Delete {
        target: "lead".to_owned(),
    };
    execute(&store, &action, 100.0).expect("delete");
    let room = |store: &RuntimeStore| -> Value {
        let raw:String=store.read().expect("reader").query_row("SELECT payload FROM sync_entities WHERE collection='room' AND id='broadcast' AND deleted=0",[],|r| r.get(0)).expect("retained broadcast");
        serde_json::from_str::<Value>(&raw).expect("payload")["value"].clone()
    };
    let before = room(&store);
    assert_eq!(before["members"], json!(["lead"]));
    execute(&store, &action, 200.0).expect("repeated deletion");
    assert_eq!(room(&store), before);
}
