use serde_json::{Value, json};
use std::path::Path;
use studio_runtime_services::{
    agent_lifecycle_runtime::{self, LifecycleEffect, LifecyclePorts},
    agent_management::{Action, ManagementError, ManagementSnapshot, Records, decide},
    agents_create::mode_fields,
    workspace_cleanup_runtime::{self, CleanupEffect, CleanupPorts, EffectFailure},
};
use studio_runtime_store::{RuntimeStore, Transaction, agent_management as repository};

const NOW: f64 = 1_234_567.5;

fn seed() -> Value {
    let mut lead = json!({"id":"lead","rootId":"lead","parentId":null,"epoch":3,"status":"idle","name":"Lead","cwd":"/fixture","isLead":true,"autoWake":true,"inFlight":false,"created":1.0});
    let mut worker = lead.clone();
    worker["id"] = json!("worker");
    worker["parentId"] = json!("lead");
    worker["isLead"] = json!(false);
    worker["status"] = json!("completed");
    worker["name"] = json!("Worker");
    worker["epoch"] = json!(8);
    mode_fields(&mut lead);
    mode_fields(&mut worker);
    json!({"tables":{"runtime_agents":[lead, worker]}})
}

fn archive_action() -> Value {
    json!({"action":"archive","actor":"lead","epoch":3,"target":"worker","reason":"Finished work","unassign_work":false})
}

fn case(records: Value, action: Value) -> Value {
    json!({"records":records,"action":action,"now":NOW})
}

fn unassign_records() -> Value {
    let mut records = seed();
    records["tables"]["runtime_work"] = json!([{"id":"work","owner":"worker","rootId":"lead","version":2,"status":"review","title":"Assignment","results":[{"id":"result","text":"Keep this result"}],"decisions":[]}]);
    records
}

fn template(root: &Path) {
    let schema: Value = serde_json::from_str(include_str!(
        "../../runtime-store/tests/fixtures/live-state-schema.json"
    ))
    .expect("runtime schema fixture");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        let connection = rusqlite::Connection::open(root.join(database)).expect("fixture database");
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; BEGIN;")
            .expect("fixture durability");
        let mut objects = schema[database]["sqlite_master"]
            .as_array()
            .expect("schema objects")
            .iter()
            .filter(|object| {
                ![
                    "runtime_search_next_config",
                    "runtime_search_next_data",
                    "runtime_search_next_docsize",
                    "runtime_search_next_idx",
                ]
                .contains(&object["name"].as_str().expect("schema object name"))
            })
            .filter_map(|object| {
                Some((
                    object["type"].as_str()?,
                    object["name"].as_str()?,
                    object["sql"].as_str()?,
                ))
            })
            .collect::<Vec<_>>();
        objects.sort_by_key(|(kind, _, _)| match *kind {
            "table" => 0,
            "index" => 1,
            "view" => 2,
            _ => 3,
        });
        for (_, _, sql) in objects {
            connection
                .execute_batch(sql)
                .expect("fixture schema object");
        }
        connection
            .execute_batch("COMMIT")
            .expect("fixture schema commit");
    }
}

async fn seed_store(store: &RuntimeStore, records: &Records) {
    let records = records.clone();
    store
        .transaction(move |tx| {
            for (table, rows) in records.tables {
                for row in rows {
                    if table == "runtime_events" {
                        tx.execute(
                            "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,'user','input',?3,1,?4)",
                            rusqlite::params![row["id"].as_str(), row["agent"].as_str(), row["status"].as_str(), row["epoch"].as_i64()],
                        )?;
                    } else if table == "voice_sessions" {
                        tx.execute(
                            "INSERT INTO voice_sessions(id,agent,created,ended,state) VALUES(?1,?2,?3,?4,?5)",
                            rusqlite::params![
                                row["id"].as_str(),
                                row["agent"].as_str(),
                                row["created"].as_f64().unwrap_or(1.0),
                                row["ended"].as_f64(),
                                row["state"].as_str(),
                            ],
                        )?;
                    } else if [
                        "runtime_monitors",
                        "runtime_tasks",
                        "runtime_tool_requests",
                    ]
                    .contains(&table.as_str())
                    {
                        tx.execute(
                            &format!("INSERT INTO {table}(id,record) VALUES(?1,?2)"),
                            rusqlite::params![
                                row["id"].as_str(),
                                serde_json::to_string(&row)?,
                            ],
                        )?;
                    } else {
                        repository::write(tx, &table, &row)?;
                    }
                }
            }
            Ok(())
        })
        .await
        .expect("records seed into store");
}

struct FakeCleanup;
impl CleanupPorts for FakeCleanup {
    fn perform(
        &self,
        _agent: &Value,
        _effect: &CleanupEffect,
        _request_id: &str,
    ) -> Result<Value, EffectFailure> {
        Ok(json!({"state":"done"}))
    }
}

struct PostCommitCleanup {
    store: std::sync::Arc<RuntimeStore>,
}
impl LifecyclePorts for PostCommitCleanup {
    fn stage(&self, _: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
        Ok(())
    }

    fn committed(&self) -> agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        let store = std::sync::Arc::clone(&self.store);
        Box::pin(async move {
            let queued = {
                let reader = store.test_reader()?;
                repository::read(&reader)?
                    .get("runtime_agents")
                    .into_iter()
                    .flatten()
                    .filter(|agent| {
                        agent
                            .pointer("/workspaceCleanupIntent/phase")
                            .and_then(Value::as_str)
                            == Some("queued")
                    })
                    .map(|agent| {
                        let action = serde_json::from_value(
                            agent
                                .pointer("/workspaceCleanupIntent/action")
                                .cloned()
                                .ok_or(ManagementError::Invalid("Missing cleanup action"))?,
                        )
                        .map_err(|_| ManagementError::Invalid("Invalid cleanup action"))?;
                        let id = agent["id"]
                            .as_str()
                            .ok_or(ManagementError::Invalid("Invalid cleanup agent"))?
                            .to_owned();
                        Ok::<_, ManagementError>((id, action))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            for (id, action) in queued {
                workspace_cleanup_runtime::cleanup_agent_workspace_with_ports(
                    std::sync::Arc::clone(&store),
                    id,
                    action,
                    std::sync::Arc::new(FakeCleanup),
                )
                .await
                .map_err(|_| ManagementError::Unavailable("workspace-cleanup/agent-management"))?;
            }
            Ok(())
        })
    }
}

async fn run_lifecycle_end_state(case: &Value) -> (Value, Value) {
    let root = tempfile::tempdir().expect("tmpfs test root");
    template(root.path());
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("fixture store opens"),
    );
    let records: Records = serde_json::from_value(case["records"].clone()).expect("records");
    seed_store(&store, &records).await;
    let action: Action = serde_json::from_value(case["action"].clone()).expect("lifecycle action");
    let ports = std::sync::Arc::new(PostCommitCleanup {
        store: std::sync::Arc::clone(&store),
    });
    let result = agent_lifecycle_runtime::execute(
        std::sync::Arc::clone(&store),
        &action,
        case["now"].as_f64().expect("time"),
        ports,
    )
    .await;
    let saved = store.test_reader().expect("read final store");
    let rows = repository::read(&saved).expect("complete final rows");
    let final_state = serde_json::to_value(Records { tables: rows }).expect("serialize final rows");
    let result = match result {
        Ok(result) => json!({"result":result}),
        Err(error) => json!({"error":error.to_string()}),
    };
    (result, final_state)
}

struct CrashBeforeCleanup;
impl LifecyclePorts for CrashBeforeCleanup {
    fn stage(&self, _: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
        Ok(())
    }

    fn committed(&self) -> agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        Box::pin(async { Err(ManagementError::Invalid("simulated cleanup wake loss")) })
    }
}

#[tokio::test]
async fn archive_cleanup_wake_loss_keeps_queued_intent_for_restart() {
    let case = case(seed(), archive_action());
    let root = tempfile::tempdir().expect("tmpfs test root");
    template(root.path());
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("fixture store opens"),
    );
    let records: Records = serde_json::from_value(case["records"].clone()).expect("records");
    seed_store(&store, &records).await;
    let action: Action = serde_json::from_value(case["action"].clone()).expect("archive action");
    assert!(
        agent_lifecycle_runtime::execute(
            std::sync::Arc::clone(&store),
            &action,
            case["now"].as_f64().expect("time"),
            std::sync::Arc::new(CrashBeforeCleanup),
        )
        .await
        .is_ok(),
        "a lost cleanup wake still reports the committed archive"
    );
    let saved = store.test_reader().expect("read committed intent");
    let rows = repository::read(&saved).expect("read complete rows");
    let worker = rows["runtime_agents"]
        .iter()
        .find(|row| row["id"] == "worker")
        .expect("archived worker");
    assert_eq!(
        worker.pointer("/workspaceCleanupIntent/phase"),
        Some(&json!("queued"))
    );
    assert_eq!(
        worker.pointer("/agentArchive/cleanupPending"),
        Some(&json!(true))
    );
    drop(saved);
    drop(store);

    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart store");
    let saved = store.test_reader().expect("read after restart");
    let rows = repository::read(&saved).expect("read restarted rows");
    let worker = rows["runtime_agents"]
        .iter()
        .find(|row| row["id"] == "worker")
        .expect("archived worker after restart");
    assert_eq!(
        worker.pointer("/workspaceCleanupIntent/phase"),
        Some(&json!("queued"))
    );
    assert_eq!(
        worker.pointer("/agentArchive/cleanupPending"),
        Some(&json!(true))
    );
}

#[tokio::test]
async fn finished_workers_archive_and_clean_up_in_every_finished_status() {
    for status in ["completed", "failed", "interrupted", "idle", "paused"] {
        let mut records = seed();
        records["tables"]["runtime_agents"][1]["status"] = json!(status);
        let (result, state) = run_lifecycle_end_state(&case(records, archive_action())).await;
        assert_eq!(result["result"]["status"], "archived", "{status}");
        let saved = state["tables"]["runtime_agents"]
            .as_array()
            .expect("agent rows")
            .iter()
            .find(|row| row["id"] == "worker")
            .expect("archived agent");
        assert_eq!(
            saved.pointer("/agentArchive/cleanupPending"),
            Some(&json!(false)),
            "{status}"
        );
        assert!(saved.get("workspaceCleanupIntent").is_none(), "{status}");
    }
}

#[tokio::test]
async fn archive_is_refused_while_the_worker_has_pending_work() {
    for (table, row) in [
        (
            "runtime_events",
            json!({"id":"pending","agent":"worker","epoch":8,"status":"pending"}),
        ),
        (
            "runtime_requests",
            json!({"id":"question","agent":"worker","status":"pending","method":"agent/asyncQuestion"}),
        ),
        (
            "runtime_monitors",
            json!({"id":"monitor","agent":"worker","status":"approval"}),
        ),
    ] {
        let mut records = seed();
        records["tables"][table] = json!([row]);
        let (result, state) = run_lifecycle_end_state(&case(records, archive_action())).await;
        assert!(
            result.get("error").is_some() || result["result"]["status"] != "archived",
            "{table}"
        );
        let worker = state["tables"]["runtime_agents"]
            .as_array()
            .expect("agent rows")
            .iter()
            .find(|row| row["id"] == "worker")
            .expect("worker");
        assert!(
            worker
                .get("agentArchive")
                .is_none_or(|archive| archive.is_null()),
            "{table}"
        );
    }
}

#[test]
fn malformed_stored_lifecycle_identity_is_located() {
    let base = seed();
    for (field, value) in [
        ("epoch", json!(-1)),
        ("epoch", json!(true)),
        ("epoch", json!("8")),
        ("autoWake", json!("yes")),
        ("parentId", json!(7)),
        ("inFlight", json!([])),
        ("agentArchive", json!({"epoch":8})),
        ("agentArchive", json!({"epoch":-1,"at":2.0})),
        ("deletedAt", json!("timestamp")),
        ("worktreeReady", json!({})),
    ] {
        let mut raw = base.clone();
        raw["tables"]["runtime_agents"][1][field] = value;
        let records: Records = serde_json::from_value(raw).expect("raw JSON rows");
        assert!(
            matches!(ManagementSnapshot::from_records(records), Err(ManagementError::Stored { table, id }) if table == "runtime_agents" && id == "worker"),
            "{field}"
        );
    }
}

#[test]
fn management_snapshot_imposes_no_row_cap() {
    let records = Records {
        tables: std::collections::BTreeMap::from([(
            "runtime_events".to_owned(),
            (0..10_001)
                .map(|index| json!({"id":format!("event-{index}"),"agent":"agent","status":"pending","epoch":0}))
                .collect(),
        )]),
    };

    assert!(ManagementSnapshot::from_records(records).is_ok());
}

#[test]
fn managed_interrupt_requires_current_sender_and_descendant_scope() {
    let records: Records = serde_json::from_value(seed()).expect("lifecycle rows");
    let snapshot = ManagementSnapshot::from_records(records).expect("snapshot");
    let action = |sender_epoch| Action::Stop {
        target: "worker".to_owned(),
        descendants: true,
        reason: None,
        sender: Some("lead".to_owned()),
        sender_epoch: Some(sender_epoch),
    };
    assert!(matches!(
        decide(&snapshot, &action(2), 42.0),
        Err(ManagementError::Invalid("Sender was stopped"))
    ));
    assert!(matches!(
        decide(&snapshot, &action(3), 42.0),
        Err(ManagementError::Unavailable(
            "event-driver/child-stop-notification"
        ))
    ));
    let foreign = Action::Stop {
        target: "lead".to_owned(),
        descendants: true,
        reason: None,
        sender: Some("worker".to_owned()),
        sender_epoch: Some(8),
    };
    assert!(matches!(
        decide(&snapshot, &foreign, 42.0),
        Err(ManagementError::Invalid(
            "You can interrupt only your descendants"
        ))
    ));
}

#[test]
fn missing_native_and_remote_effects_produce_no_decision() {
    let base = seed();
    for (field, value, dependency) in [
        (
            "threadId",
            json!("native-thread"),
            "account-worker/agent-interrupt",
        ),
        ("inFlight", json!(true), "account-worker/agent-interrupt"),
        (
            "remoteWorker",
            json!({"id":"remote"}),
            "cross-server/agent-management",
        ),
    ] {
        let mut raw = base.clone();
        raw["tables"]["runtime_agents"][1][field] = value;
        let snapshot =
            ManagementSnapshot::from_records(serde_json::from_value(raw).expect("records"))
                .expect("snapshot");
        let result = decide(
            &snapshot,
            &Action::Delete {
                target: "lead".to_owned(),
            },
            42.0,
        );
        assert!(
            matches!(result, Err(ManagementError::Unavailable(actual)) if actual == dependency),
            "{field}"
        );
    }
}

#[test]
fn malformed_assignment_fields_cannot_bypass_archive_blockers() {
    let base = unassign_records();
    for (field, value) in [
        ("owner", json!(true)),
        ("version", json!("2")),
        ("version", Value::Null),
        ("releases", json!({})),
        ("releases", Value::Null),
    ] {
        let mut records = base.clone();
        records["tables"]["runtime_work"][0][field] = value;
        let result =
            ManagementSnapshot::from_records(serde_json::from_value(records).expect("rows"));
        assert!(
            matches!(result,Err(ManagementError::Stored { table,id }) if table=="runtime_work" && id=="work"),
            "{field}"
        );
    }
}
