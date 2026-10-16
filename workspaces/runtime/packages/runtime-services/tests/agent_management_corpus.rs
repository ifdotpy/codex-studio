#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use studio_runtime_services::{
    agent_lifecycle_runtime::{self, LifecycleEffect, LifecyclePorts},
    agent_management::{Action, ManagementError, ManagementSnapshot, Records, decide},
    workspace_cleanup_runtime::{self, CleanupEffect, CleanupPorts, EffectFailure},
};
use studio_runtime_store::{RuntimeStore, Transaction, agent_management as repository};

fn corpus() -> Vec<Value> {
    let python = prepared_python_support::prepared_python(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/agent_management/oracle.py");
    let output = Command::new(python)
        .arg(path)
        .output()
        .expect("Python oracle starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("oracle emits corpus")
}

fn apply(
    mut records: Records,
    writes: &[studio_runtime_services::agent_management::RowWrite],
    cancel: &[String],
) -> Records {
    for write in writes {
        let rows = records.tables.get_mut(&write.table).expect("source table");
        let row = rows
            .iter_mut()
            .find(|row| row["id"] == write.record["id"])
            .expect("existing row");
        *row = write.record.clone();
    }
    if let Some(events) = records.tables.get_mut("runtime_events") {
        for event in events {
            if event["status"] == "pending" && cancel.iter().any(|id| event["agent"] == *id) {
                event["status"] = json!("cancelled");
            }
        }
    }
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
    store
        .transaction(|tx| {
            for (table, rows) in &records.tables {
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
                                serde_json::to_string(row)?,
                            ],
                        )?;
                    } else {
                        repository::write(tx, table, row)?;
                    }
                }
            }
            Ok(())
        })
        .await
        .expect("Python records seed into store");
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
                let reader = store.read()?;
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
    let saved = store.read().expect("read final store");
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
    let case = corpus()
        .into_iter()
        .find(|case| case["name"] == "archive-completed")
        .expect("archive witness");
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
        .is_err()
    );
    let saved = store.read().expect("read committed intent");
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
    let saved = store.read().expect("read after restart");
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
async fn python_lifecycle_decisions_and_complete_stored_rows_match() {
    let cases = corpus();
    assert!(cases.len() >= 35);
    for case in cases {
        if matches!(
            case["action"]["action"].as_str(),
            Some("archive" | "stop" | "delete")
        ) {
            let (actual_result, actual_records) = run_lifecycle_end_state(&case).await;
            let mut expected_tables = case["expected"]["records"]["tables"].clone();
            let actual_tables = actual_records["tables"]
                .as_object()
                .expect("complete runtime tables");
            let expected_tables = expected_tables
                .as_object_mut()
                .expect("Python oracle tables");
            for table in actual_tables.keys() {
                expected_tables
                    .entry(table.clone())
                    .or_insert_with(|| json!([]));
            }
            assert_eq!(
                actual_tables, expected_tables,
                "complete post-cleanup rows for {}",
                case["name"]
            );
            if case["expected"].get("error").is_some() {
                assert_eq!(
                    actual_result,
                    json!({"error":case["expected"]["error"]}),
                    "{}",
                    case["name"]
                );
            } else if case["action"]["action"] == "archive" {
                assert_eq!(
                    actual_result["result"]["status"], case["expected"]["result"]["status"],
                    "{}",
                    case["name"]
                );
                if actual_result["result"]["status"] == "archived" {
                    let saved = actual_records["tables"]["runtime_agents"]
                        .as_array()
                        .expect("agent rows")
                        .iter()
                        .find(|row| row["id"] == case["action"]["target"])
                        .expect("archived agent");
                    assert_eq!(
                        saved.pointer("/agentArchive/cleanupPending"),
                        Some(&json!(false))
                    );
                    assert!(saved.get("workspaceCleanupIntent").is_none());
                }
            } else {
                assert_eq!(
                    actual_result["result"], case["expected"]["result"],
                    "lifecycle result for {}",
                    case["name"]
                );
            }
            continue;
        }
        let records: Records = serde_json::from_value(case["records"].clone()).expect("records");
        let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
        let snapshot = ManagementSnapshot::from_records(records.clone())
            .expect("checked Python-written snapshot");
        let actual = match decide(&snapshot, &action, case["now"].as_f64().expect("time")) {
            Ok(decision) => {
                json!({"result": decision.result, "records":apply(records, &decision.writes, &decision.cancel_pending)})
            }
            Err(error) => json!({"error":error.to_string(), "records":records}),
        };
        assert_eq!(actual, case["expected"], "case {}", case["name"]);
    }
}

#[test]
fn malformed_stored_lifecycle_identity_is_located() {
    let base = corpus().remove(0)["records"].clone();
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
fn management_snapshot_does_not_impose_a_python_absent_row_cap() {
    // Python codex_runtime.py:2956-2973 traverses the full descendant set and
    // has no row-count or per-record byte cap for these management snapshots.
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
    let records: Records =
        serde_json::from_value(corpus()[0]["records"].clone()).expect("lifecycle rows");
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
    let base = corpus().remove(0)["records"].clone();
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
    let base = corpus()
        .into_iter()
        .find(|case| case["name"] == "unassign")
        .expect("assignment case")["records"]
        .clone();
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
