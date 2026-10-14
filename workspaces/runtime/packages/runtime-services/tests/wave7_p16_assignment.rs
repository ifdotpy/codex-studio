use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_services::work_assignment_runtime::assign_in_transaction;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, WorkItemsRepository};

#[path = "support/prepared_python.rs"]
mod prepared_python;

fn database() -> Connection {
    let connection = Connection::open_in_memory().expect("memory SQLite opens");
    connection
        .execute_batch(
            "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
        )
        .expect("work tables create");
    for agent in [
        json!({"id":"lead","rootId":"lead","epoch":1,"status":"running","isLead":true,"autoWake":true}),
        json!({"id":"child","rootId":"lead","parentId":"lead","epoch":2,"status":"starting","autoWake":true}),
        json!({"id":"other-child","rootId":"lead","parentId":"lead","epoch":3,"status":"starting","autoWake":true}),
        json!({"id":"other-lead","rootId":"other-lead","epoch":1,"status":"running","isLead":true,"autoWake":true}),
        json!({"id":"reviewer","rootId":"reviewer","epoch":1,"status":"running","isLead":true,"role":"reviewer","autoWake":true}),
        json!({"id":"impostor","rootId":"impostor","epoch":1,"status":"running","autoWake":true}),
    ] {
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES (?1,?2)",
                rusqlite::params![agent["id"].as_str().expect("agent id"), agent.to_string()],
            )
            .expect("agent row inserts");
    }
    connection
}

fn work(id: &str, owner: Option<&str>, status: &str) -> Value {
    json!({"id":id,"rootId":"lead","owner":owner,"status":status,"title":id,
        "description":"","created":1.0,"updated":1.0,"version":2,
        "dependencies":[],"results":[],"decisions":[],"createdBy":"lead"})
}

fn insert_work(connection: &Connection, value: &Value) {
    connection
        .execute(
            "INSERT INTO runtime_work VALUES (?1,?2)",
            rusqlite::params![value["id"].as_str().expect("work id"), value.to_string()],
        )
        .expect("work row inserts");
}

fn lead(connection: &Connection) -> AgentRecord {
    AgentRepository::get(connection, &AgentId::from("lead"))
        .expect("lead lookup succeeds")
        .expect("lead exists")
}

fn stored_work(connection: &Connection, id: &str) -> Value {
    WorkItemsRepository::work_record(connection, id)
        .expect("work lookup succeeds")
        .expect("work exists")
}

#[test]
fn spawn_assignment_is_atomic_and_same_owner_replay_is_read_only() {
    let mut connection = database();
    insert_work(&connection, &work("task", None, "ready"));
    let actor = lead(&connection);
    let tx = connection.transaction().expect("transaction starts");

    let first = assign_in_transaction(&tx, "task", "child", &actor, 12.5)
        .expect("task assigns to new child");
    assert!(first.changed);
    assert_eq!(first.version, 3);
    let replay = assign_in_transaction(&tx, "task", "child", &actor, 20.0)
        .expect("same work and owner replay succeeds");
    assert!(!replay.changed);
    assert_eq!(replay.version, 3);
    tx.commit().expect("assignment commits");

    let saved = stored_work(&connection, "task");
    assert_eq!(saved["owner"], "child");
    assert_eq!(saved["version"], 3);
    assert_eq!(saved["updated"], 12.5);

    let tx = connection.transaction().expect("replay transaction starts");
    let after_lost_response = assign_in_transaction(&tx, "task", "child", &actor, 99.0)
        .expect("same identity remains successful after commit");
    assert!(!after_lost_response.changed);
    tx.commit().expect("read-only replay commits");
    let saved = stored_work(&connection, "task");
    assert_eq!(saved["version"], 3);
    assert_eq!(saved["updated"], 12.5);
}

#[test]
fn spawn_assignment_rollback_leaves_the_work_item_unowned() {
    let mut connection = database();
    insert_work(&connection, &work("task", None, "ready"));
    let actor = lead(&connection);
    let tx = connection.transaction().expect("transaction starts");
    assign_in_transaction(&tx, "task", "child", &actor, 12.5)
        .expect("task assigns in open transaction");
    tx.rollback().expect("pre-commit failure rolls back");

    let saved = stored_work(&connection, "task");
    assert!(saved["owner"].is_null());
    assert_eq!(saved["version"], 2);
}

#[test]
fn spawn_assignment_refuses_other_owners_and_review_states_without_writes() {
    let mut connection = database();
    insert_work(&connection, &work("owned", Some("child"), "running"));
    insert_work(&connection, &work("review", None, "review"));
    insert_work(&connection, &work("accepted", None, "accepted"));
    let actor = lead(&connection);

    for task_id in ["owned", "review", "accepted"] {
        let before = stored_work(&connection, task_id);
        let tx = connection.transaction().expect("transaction starts");
        let result = assign_in_transaction(&tx, task_id, "other-child", &actor, 12.5);
        assert!(result.is_err(), "{task_id} must refuse assignment");
        tx.rollback().expect("refusal rolls back");
        assert_eq!(stored_work(&connection, task_id), before);
    }
}

#[test]
fn spawn_assignment_rechecks_the_active_lead_and_same_team_child() {
    let mut connection = database();
    insert_work(&connection, &work("task", None, "ready"));
    let nonlead = AgentRepository::get(&connection, &AgentId::from("child"))
        .expect("child lookup succeeds")
        .expect("child exists");
    let foreign = AgentRepository::get(&connection, &AgentId::from("other-lead"))
        .expect("foreign lead lookup succeeds")
        .expect("foreign lead exists");
    let reviewer = AgentRepository::get(&connection, &AgentId::from("reviewer"))
        .expect("reviewer lookup succeeds")
        .expect("reviewer exists");
    let impostor = AgentRepository::get(&connection, &AgentId::from("impostor"))
        .expect("impostor lookup succeeds")
        .expect("impostor exists");

    let lead_snapshot = lead(&connection);
    for (owner_id, actor) in [
        ("child", &nonlead),
        ("other-lead", &lead_snapshot),
        ("child", &reviewer),
        ("child", &impostor),
    ] {
        let tx = connection.transaction().expect("transaction starts");
        assert!(assign_in_transaction(&tx, "task", owner_id, actor, 12.5).is_err());
        tx.rollback().expect("unauthorized assignment rolls back");
    }
    let tx = connection.transaction().expect("transaction starts");
    assert!(assign_in_transaction(&tx, "task", "child", &foreign, 12.5).is_err());
    tx.rollback().expect("foreign actor assignment rolls back");
    assert!(stored_work(&connection, "task")["owner"].is_null());
}

#[test]
fn real_python_spawn_assignment_corpus_matches_complete_work_rows() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = manifest.join("../../apps/server/src");
    let oracle = manifest.join("../../apps/server/tests/p16-spawn-assignment-oracle.py");
    let output = std::process::Command::new(prepared_python::prepared_python(&source))
        .arg(&oracle)
        .output()
        .expect("Python spawn oracle starts");
    assert!(
        output.status.success(),
        "Python spawn oracle: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("oracle output is UTF-8");
    let cases: Vec<Value> = serde_json::from_str(stdout.lines().last().expect("oracle JSON line"))
        .expect("Python writes assignment cases");
    assert_eq!(cases.len(), 6);

    for case in cases {
        let before = case["before"].clone();
        let id = before["id"].as_str().expect("work id");
        let root = before["rootId"].as_str().expect("root id");
        let connection = Connection::open_in_memory().expect("database opens");
        connection.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);").expect("tables create");
        let lead = json!({"id":root,"rootId":root,"epoch":1,"status":"running","isLead":true,"autoWake":true});
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES (?1,?2)",
                rusqlite::params![root, lead.to_string()],
            )
            .expect("lead inserts");
        let actor = AgentRecord::from_value(lead).expect("lead deserializes");
        let owner = case["after"]["owner"].as_str();
        if case["error"].is_null() {
            let child = json!({"id":owner,"rootId":root,"parentId":root,"epoch":2,"status":"starting","autoWake":true});
            connection
                .execute(
                    "INSERT INTO runtime_agents VALUES (?1,?2)",
                    rusqlite::params![owner.expect("owner"), child.to_string()],
                )
                .expect("spawned child inserts");
        }
        if case["name"] == "owned" {
            let mut other = before.clone();
            other["owner"] = json!("someone-else");
            insert_work(&connection, &other);
        } else if case["name"] == "accepted" || case["name"] == "review" {
            insert_work(&connection, &before);
        } else if case["name"] == "unknown" {
            // The Python source wrote a task row, but spawn is asked to assign a missing id.
            insert_work(&connection, &before);
        } else {
            insert_work(&connection, &before);
        }
        let result = {
            let mut connection = connection;
            let tx = connection.transaction().expect("transaction starts");
            let task = if case["name"] == "unknown" {
                "missing-task"
            } else {
                id
            };
            let result = assign_in_transaction(&tx, task, owner.unwrap_or("child"), &actor, 42.5);
            if let Some(expected_error) = case["error"].as_str() {
                assert!(result.is_err(), "{} must refuse", case["name"]);
                let saved = stored_work(&tx, id);
                assert_eq!(
                    saved, before,
                    "{} refusal preserves its full work row",
                    case["name"]
                );
                tx.rollback().expect("refusal rolls back");
                assert!(
                    expected_error.contains("Unknown task_id")
                        || expected_error.contains("accepted, under review or owned")
                );
                None
            } else {
                result.expect("Python accepted assignment");
                let saved = stored_work(&tx, id);
                assert_eq!(
                    saved, case["after"],
                    "{} matches Python complete work row",
                    case["name"]
                );
                if case["name"] == "same-owner-replay" {
                    let replay = assign_in_transaction(
                        &tx,
                        id,
                        owner.expect("same-owner replay has an owner"),
                        &actor,
                        99.0,
                    )
                    .expect("same owner replay");
                    assert!(!replay.changed);
                    assert_eq!(
                        stored_work(&tx, id),
                        case["after"],
                        "replay preserves version and updated timestamp"
                    );
                }
                tx.commit().expect("assignment commits");
                Some(())
            }
        };
        assert_eq!(result.is_some(), case["error"].is_null());
    }
}

#[test]
fn spawn_creator_failpoints_before_and_after_assignment_roll_back_child_and_work() {
    for fail_before_update in [true, false] {
        let mut connection = database();
        let work = work("task", None, "ready");
        insert_work(&connection, &work);
        let actor = lead(&connection);
        if fail_before_update {
            connection.execute_batch("CREATE TRIGGER fail_assignment BEFORE UPDATE ON runtime_work BEGIN SELECT RAISE(ABORT,'assignment failpoint'); END;").expect("before-update failpoint installs");
        }
        let tx = connection
            .transaction()
            .expect("creator transaction starts");
        let child = json!({"id":"created-child","rootId":"lead","parentId":"lead","epoch":2,"status":"starting","autoWake":true});
        AgentRepository::put(
            &tx,
            &AgentRecord::from_value(child).expect("creator child is a typed record"),
        )
        .expect("creator inserts child");
        let assigned = assign_in_transaction(&tx, "task", "created-child", &actor, 12.5);
        if fail_before_update {
            assert!(assigned.is_err(), "before-update failpoint fires");
            tx.rollback()
                .expect("creator rolls back after assignment failure");
        } else {
            assigned.expect("assignment updates work row");
            tx.rollback()
                .expect("post-update creator failpoint rolls back transaction");
        }
        assert!(
            AgentRepository::get(&connection, &AgentId::from("created-child"))
                .expect("rolled-back child lookup")
                .is_none(),
            "no orphan child"
        );
        assert_eq!(
            stored_work(&connection, "task"),
            work,
            "work row remains unchanged"
        );
    }
}
