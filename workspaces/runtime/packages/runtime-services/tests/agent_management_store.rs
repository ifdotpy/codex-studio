#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
#[path = "support/projections.rs"]
mod projections;
use projections::full_rebuild;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use studio_runtime_services::{
    agent_lifecycle_runtime::{self, LifecycleEffect, LifecyclePorts},
    agent_management::{Action, ManagementError, Records},
    agent_management_store::execute,
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
async fn seed(store: &RuntimeStore, records: Records) {
    store.transaction(move |tx| {
        for (table, records) in records.tables {
            for row in records {
                if table == "runtime_events" {
                    tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,'user','input',?3,1,?4)", params![row["id"].as_str(),row["agent"].as_str(),row["status"].as_str(),row["epoch"].as_i64()])?;
                } else { agent_management::write(tx, &table, &row)?; }
            }
        }
        Ok(())
    }).await.expect("Python rows seed");
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

struct AcceptLifecycleEffects;
impl LifecyclePorts for AcceptLifecycleEffects {
    fn stage(&self, _: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
        Ok(())
    }

    fn committed(&self) -> agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn stop_clears_the_matching_unsubmitted_capacity_retry_attempt() {
    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "stop-capacity-retry-unsubmitted")
        .expect("Python retry-stop case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    )
    .await;
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    let result = agent_lifecycle_runtime::execute(
        std::sync::Arc::clone(&store),
        &action,
        case["now"].as_f64().expect("time"),
        std::sync::Arc::new(AcceptLifecycleEffects),
    )
    .await
    .expect("stop");
    assert_eq!(result, case["expected"]["result"]);
    assert_eq!(
        state(&store)["tables"]["runtime_agents"],
        case["expected"]["records"]["tables"]["runtime_agents"]
    );
}

#[tokio::test]
async fn transaction_failure_matrix_rolls_back_rows_and_projections_after_restart() {
    let cases = cases();
    let mut count = 0;
    for case_name in ["unassign", "stop-True"] {
        let case = cases
            .iter()
            .find(|c| c["name"] == case_name)
            .expect("witness");
        let base = tempfile::tempdir().expect("base");
        template(base.path());
        let store = std::sync::Arc::new(
            RuntimeStore::open(Some(base.path()))
                .await
                .expect("template opens"),
        );
        seed(
            &store,
            serde_json::from_value(case["records"].clone()).expect("rows"),
        )
        .await;
        // The clones below copy only the database files; the seed must not stay in the WAL.
        store
            .checkpoint_wal_passive()
            .await
            .expect("seed database checkpoint");
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
                let store = std::sync::Arc::new(
                    RuntimeStore::open(Some(root.path()))
                        .await
                        .expect("open clone"),
                );
                let before = state(&store);
                let event = if table == "sync_entities" {
                    "INSERT"
                } else {
                    "UPDATE"
                };
                let trigger = format!(
                    "CREATE TRIGGER lifecycle_fail {timing} {event} ON {table} BEGIN SELECT RAISE(ABORT,'lifecycle failpoint'); END"
                );
                store
                    .transaction(move |tx| {
                        tx.execute_batch(&trigger)?;
                        Ok(())
                    })
                    .await
                    .expect("failpoint installed");
                let action: Action =
                    serde_json::from_value(case["action"].clone()).expect("action");
                assert!(
                    execute(std::sync::Arc::clone(&store), &action, 1234567.5)
                        .await
                        .is_err(),
                    "{case_name} {timing} {table}"
                );
                assert_eq!(
                    state(&store),
                    before,
                    "rollback {case_name} {timing} {table}"
                );
                drop(store);
                let store = std::sync::Arc::new(
                    RuntimeStore::open(Some(root.path()))
                        .await
                        .expect("restart"),
                );
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
                    .await
                    .expect("remove failpoint");
                let result = execute(std::sync::Arc::clone(&store), &action, 1234567.5)
                    .await
                    .expect("recovery applies once");
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

#[tokio::test]
async fn committed_archive_response_loss_replays_after_restart_without_duplicate_release() {
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unassign")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("records"),
    )
    .await;
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    let first = execute(std::sync::Arc::clone(&store), &action, 1234567.5)
        .await
        .expect("archive");
    assert_eq!(first, case["expected"]["result"]);
    let committed = state(&store);
    drop(store);
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart"),
    );
    let result = execute(std::sync::Arc::clone(&store), &action, 1234568.0)
        .await
        .expect("replay");
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

#[tokio::test]
async fn deleted_team_broadcast_projection_survives_repeated_delete() {
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "delete")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("records"),
    )
    .await;
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
        .await
        .expect("room");
    let action = Action::Delete {
        target: "lead".to_owned(),
    };
    execute(std::sync::Arc::clone(&store), &action, 100.0)
        .await
        .expect("delete");
    let room = |store: &RuntimeStore| -> Value {
        let raw:String=store.read().expect("reader").query_row("SELECT payload FROM sync_entities WHERE collection='room' AND id='broadcast' AND deleted=0",[],|r| r.get(0)).expect("retained broadcast");
        serde_json::from_str::<Value>(&raw).expect("payload")["value"].clone()
    };
    let before = room(&store);
    assert_eq!(before["members"], json!(["lead"]));
    execute(std::sync::Arc::clone(&store), &action, 200.0)
        .await
        .expect("repeated deletion");
    assert_eq!(room(&store), before);
}

#[test]
fn cancellation_keeps_reserved_uncertain_and_foreign_events() {
    let mut connection = Connection::open_in_memory().expect("memory database");
    connection.execute_batch("CREATE TABLE runtime_events(id TEXT, agent TEXT, status TEXT);
        INSERT INTO runtime_events VALUES ('pending','worker','pending'),('reserved','worker','reserved'),('uncertain','worker','uncertain'),('foreign','other','pending');").expect("delivery states");
    let transaction = connection.transaction().expect("transaction");
    agent_management::cancel_pending(&transaction, "worker").expect("cancellation");
    transaction.commit().expect("commit");
    let rows: Vec<(String, String)> = connection
        .prepare("SELECT id,status FROM runtime_events ORDER BY id")
        .expect("query")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("states");
    assert_eq!(
        rows,
        vec![
            ("foreign".into(), "pending".into()),
            ("pending".into(), "cancelled".into()),
            ("reserved".into(), "reserved".into()),
            ("uncertain".into(), "uncertain".into())
        ]
    );
}

#[tokio::test]
async fn lifecycle_effect_refusals_preserve_rows_and_projections() {
    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "stop-True")
        .expect("Python seed");
    let base = tempfile::tempdir().expect("base");
    template(base.path());
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(base.path()))
            .await
            .expect("template"),
    );
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("records"),
    )
    .await;
    store
        .checkpoint_wal_passive()
        .await
        .expect("seed database checkpoint");
    for delete in [false, true] {
        for effect in [
            "monitor-starting",
            "monitor-running",
            "monitor-approval",
            "voice-connecting",
            "voice-ready",
            "child-stop",
        ] {
            let root = tempfile::tempdir().expect("case");
            for database in ["canvas.sqlite3", "analytics.sqlite3"] {
                fs::copy(base.path().join(database), root.path().join(database)).expect("clone");
            }
            let store =
                std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
            let dependency = if let Some(status) = effect.strip_prefix("monitor-") {
                let status = status.to_owned();
                store
                    .transaction(move |tx| {
                        tx.execute(
                            "INSERT INTO runtime_monitors(id,record) VALUES('live-monitor',?1)",
                            [
                                json!({"id":"live-monitor","agent":"worker","status":status})
                                    .to_string(),
                            ],
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("monitor");
                "monitor-driver/agent-stop"
            } else if let Some(status) = effect.strip_prefix("voice-") {
                let status = status.to_owned();
                store.transaction(move |tx| {
                    tx.execute("INSERT INTO voice_sessions(id,agent,created,state) VALUES('live-voice','worker',1,?1)", [status])?;
                    Ok(())
                }).await.expect("voice");
                "voice-driver/agent-stop"
            } else {
                "event-driver/child-stop-notification"
            };
            let action = if delete {
                Action::Delete {
                    target: "worker".into(),
                }
            } else {
                Action::Stop {
                    target: "worker".into(),
                    descendants: true,
                    reason: None,
                    sender: None,
                    sender_epoch: None,
                }
            };
            let before = state(&store);
            let result = execute(std::sync::Arc::clone(&store), &action, 1234567.5).await;
            if delete && effect.starts_with("voice-") {
                // This fixture has no native thread or connection identity to
                // cancel. Deletion can remove its stale session row; live
                // provider sessions are covered by the production-port tests.
                assert_eq!(
                    result.expect("delete stale voice session"),
                    json!({"deleted":["worker"]})
                );
                assert_eq!(state(&store)["tables"]["voice_sessions"], json!([]));
                continue;
            }
            if delete && effect == "child-stop" {
                // Python tombstones the worker before stopping it, suppressing the parent event.
                assert_eq!(
                    result.expect("delete needs no parent notification"),
                    json!({"deleted":["worker"]})
                );
            } else {
                assert!(
                    matches!(result, Err(ManagementError::Unavailable(actual)) if actual == dependency),
                    "{effect} delete={delete}: {result:?}"
                );
                assert_eq!(state(&store), before, "{effect} delete={delete}");
                drop(store);
                assert_eq!(
                    state(
                        &RuntimeStore::open(Some(root.path()))
                            .await
                            .expect("restart")
                    ),
                    before
                );
            }
        }
    }
}

#[tokio::test]
async fn ended_voice_allows_stop_and_delete_clears_saved_voice_in_transaction() {
    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "stop-ended-voice")
        .expect("Python ended session");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
    let mut records: Records = serde_json::from_value(case["records"].clone()).expect("records");
    records.tables.remove("voice_sessions");
    seed(&store, records).await;
    store.transaction(|tx| {
        tx.execute("INSERT INTO voice_sessions(id,agent,created,state,ended) VALUES('ended-voice','lead',1,'ended',?1)", [1234557.5])?;
        tx.execute("INSERT INTO voice_approvals(id,agent,speech,transcript,status) VALUES('saved-approval','lead','speech','transcript','accepted')", [])?;
        Ok(())
    }).await.expect("saved voice");
    let result = execute(
        std::sync::Arc::clone(&store),
        &Action::Stop {
            target: "lead".into(),
            descendants: true,
            reason: None,
            sender: None,
            sender_epoch: None,
        },
        1234567.5,
    )
    .await
    .expect("history does not need termination");
    assert_eq!(result, case["expected"]["result"]);
    let saved = state(&store);
    for (table, rows) in case["expected"]["records"]["tables"]
        .as_object()
        .expect("tables")
    {
        assert_eq!(&saved["tables"][table], rows, "{table}");
    }
    let result = execute(
        std::sync::Arc::clone(&store),
        &Action::Delete {
            target: "lead".into(),
        },
        1234568.0,
    )
    .await
    .expect("ended voice history is removed with the conversation");
    assert_eq!(result["deleted"], json!(["lead", "worker"]));
    let after_delete = state(&store);
    for table in [
        "voice_sessions",
        "voice_records",
        "voice_deliveries",
        "voice_approvals",
    ] {
        assert!(
            after_delete["tables"][table]
                .as_array()
                .expect("voice table")
                .is_empty(),
            "{table}"
        );
    }
    assert_ne!(after_delete, saved);
}

/// Teams in the parity fixture. Unit: rows (lead agents).
const PARITY_TEAMS_ROWS: usize = 4;
/// Task history in the parity fixture; above the 100-row task window so a
/// delete must backfill it. Unit: rows (tasks).
const PARITY_TASKS_ROWS: usize = 400;
/// Teams in the manual benchmark fixture. Unit: rows (lead agents).
const BENCHMARK_TEAMS_ROWS: usize = 500;
/// Task history in the manual benchmark fixture. Unit: rows (tasks).
const BENCHMARK_TASKS_ROWS: usize = 300_000;
/// Tasks per monitor and per event row in the synthetic fixture. Unit: rows.
const TASKS_PER_HISTORY_ROW: usize = 50;
/// Lifecycle decision time used by the synthetic fixtures. Unit: seconds.
const SYNTHETIC_NOW_SECS: f64 = 1234567.5;

/// Synthetic conversation state: `leads` teams of one lead and three workers,
/// one broadcast room per team, a private room between consecutive leads,
/// `tasks` task rows spread over all agents (one running task per team),
/// and proportional monitor and event history. Projections are seeded by
/// one full rebuild so later lifecycle actions start from current views.
async fn synthetic_store(root: &Path, leads: usize, tasks: usize) -> std::sync::Arc<RuntimeStore> {
    if !root.join("canvas.sqlite3").exists() {
        template(root);
    }
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root)).await.expect("open"));
    let cwd = root.to_string_lossy().into_owned();
    store
        .transaction(move |tx| {
            let mut agents = Vec::new();
            for lead in 0..leads {
                let root_id = format!("lead-{lead:04}");
                agents.push(root_id.clone());
                let lead_record = json!({"id":root_id,"rootId":root_id,"epoch":1,"status":"idle",
                    "isLead":true,"autoWake":true,"name":format!("Lead {lead}"),"cwd":cwd,"threadId":format!("thread-{root_id}")});
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    params![root_id, lead_record.to_string()],
                )?;
                for worker in 0..3 {
                    let id = format!("worker-{lead:04}-{worker}");
                    agents.push(id.clone());
                    let record = json!({"id":id,"rootId":root_id,"parentId":root_id,"epoch":1,
                        "status":"idle","name":format!("Worker {lead}.{worker}"),"cwd":cwd});
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        params![id, record.to_string()],
                    )?;
                }
                let broadcast = format!("broadcast-{lead:04}");
                tx.execute(
                    "INSERT INTO runtime_rooms(id,record) VALUES(?1,?2)",
                    params![
                        broadcast,
                        json!({"id":broadcast,"kind":"broadcast","rootId":root_id}).to_string()
                    ],
                )?;
                if lead > 0 {
                    let private = format!("private-{lead:04}");
                    let other = format!("lead-{:04}", lead - 1);
                    tx.execute(
                        "INSERT INTO runtime_rooms(id,record) VALUES(?1,?2)",
                        params![
                            private,
                            json!({"id":private,"kind":"private","members":[other,root_id]})
                                .to_string()
                        ],
                    )?;
                }
                let work = format!("work-{lead:04}");
                tx.execute(
                    "INSERT INTO runtime_work(id,record) VALUES(?1,?2)",
                    params![
                        work,
                        json!({"id":work,"owner":format!("worker-{lead:04}-0"),"rootId":root_id,
                            "status":"active","title":"work"})
                        .to_string()
                    ],
                )?;
            }
            let mut task = tx.prepare("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)")?;
            for index in 0..tasks {
                let agent = &agents[index % agents.len()];
                let status = if index < leads { "running" } else { "completed" };
                let id = format!("task-{index:07}");
                task.execute(params![
                    id,
                    json!({"id":id,"agent":agent,"kind":"command","status":status,"created":index as f64})
                        .to_string()
                ])?;
            }
            let mut monitor =
                tx.prepare("INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)")?;
            let mut event = tx.prepare(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                 VALUES(?1,?2,'user','input','delivered',?3,1)",
            )?;
            for index in 0..tasks / TASKS_PER_HISTORY_ROW {
                let agent = &agents[index % agents.len()];
                let id = format!("monitor-{index:06}");
                monitor.execute(params![
                    id,
                    json!({"id":id,"agent":agent,"status":"completed","created":index as f64})
                        .to_string()
                ])?;
                event.execute(params![format!("event-{index:06}"), agent, index as f64])?;
            }
            Ok(())
        })
        .await
        .expect("synthetic rows");
    full_rebuild(&store).await;
    store
}

/// Current entity hashes and tombstone flags, keyed by collection and id.
fn entities(store: &RuntimeStore) -> std::collections::BTreeMap<(String, String), (String, i64)> {
    let db = store.read().expect("reader");
    let mut statement = db
        .prepare("SELECT collection,id,hash,deleted FROM sync_entities")
        .expect("entity read");
    statement
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?))))
        .expect("entity rows")
        .collect::<Result<_, _>>()
        .expect("entity values")
}

/// Lifecycle actions publish only the changed rows and their dependent views,
/// yet a later full rebuild of the committed state changes nothing except
/// the broadcast room Python retains for a deleted team.
#[tokio::test]
async fn incremental_lifecycle_projection_matches_full_rebuild() {
    let root = tempfile::tempdir().expect("root");
    let store = synthetic_store(root.path(), PARITY_TEAMS_ROWS, PARITY_TASKS_ROWS).await;
    let epoch = 1;
    for (action, retained) in [
        (
            Action::Stop {
                target: "lead-0001".to_owned(),
                descendants: true,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            None,
        ),
        (
            Action::Archive {
                actor: "lead-0002".to_owned(),
                epoch,
                target: "worker-0002-0".to_owned(),
                reason: "done".to_owned(),
                unassign_work: true,
            },
            None,
        ),
        (
            Action::Restore {
                actor: "lead-0002".to_owned(),
                epoch,
                target: "worker-0002-0".to_owned(),
            },
            None,
        ),
        (
            Action::Delete {
                target: "lead-0003".to_owned(),
            },
            Some("broadcast-0003"),
        ),
    ] {
        let before = entities(&store);
        // Archive and restore also settle workspace cleanup; leads with a
        // native thread need the effect ports for their interrupt.
        let result = if matches!(action, Action::Archive { .. } | Action::Restore { .. }) {
            execute(std::sync::Arc::clone(&store), &action, SYNTHETIC_NOW_SECS).await
        } else {
            agent_lifecycle_runtime::execute(
                std::sync::Arc::clone(&store),
                &action,
                SYNTHETIC_NOW_SECS,
                std::sync::Arc::new(AcceptLifecycleEffects),
            )
            .await
        };
        let result = result.unwrap_or_else(|error| panic!("{action:?}: {error:?}"));
        assert!(result.get("status") != Some(&json!("blocked")), "{result}");
        let incremental = entities(&store);
        assert_ne!(incremental, before, "{action:?} publishes its changes");
        full_rebuild(&store).await;
        let rebuilt = entities(&store);
        let differences = rebuilt
            .iter()
            .filter(|(key, value)| incremental.get(*key) != Some(*value))
            .map(|(key, _)| key.clone())
            .chain(
                incremental
                    .keys()
                    .filter(|key| !rebuilt.contains_key(*key))
                    .cloned(),
            )
            .collect::<Vec<_>>();
        let expected = retained
            .map(|room| vec![("room".to_owned(), room.to_owned())])
            .unwrap_or_default();
        assert_eq!(differences, expected, "{action:?}");
    }
}

/// Manual measurement, not a check: run with `--ignored --nocapture` and
/// `CODEX_RUNTIME_STAGE_TIMINGS=1`. It reports lifecycle transaction times on
/// a synthetic 300k-task state and asserts no timing.
#[tokio::test]
#[ignore = "manual projection benchmark on a 300k-task synthetic state"]
async fn lifecycle_projection_benchmark_on_large_state() {
    let scratch = std::env::var_os("TMPDIR").unwrap_or_else(|| "/dev/shm".into());
    // The live schema fixture carries Python-created task indexes; a state
    // directory created only by Rust has just the indexes Rust owns.
    for drop_unowned_task_indexes in [false, true] {
        let root = tempfile::tempdir_in(&scratch).expect("root");
        if drop_unowned_task_indexes {
            template(root.path());
            Connection::open(root.path().join("canvas.sqlite3"))
                .expect("fixture db")
                .execute_batch("DROP INDEX runtime_task_status; DROP INDEX runtime_task_history;")
                .expect("drop indexes Rust did not own");
        }
        let started = std::time::Instant::now();
        let store = synthetic_store(root.path(), BENCHMARK_TEAMS_ROWS, BENCHMARK_TASKS_ROWS).await;
        eprintln!(
            "benchmark drop_unowned_task_indexes={drop_unowned_task_indexes} fixture_ms={}",
            started.elapsed().as_millis()
        );
        for action in [
            Action::Stop {
                target: "lead-0010".to_owned(),
                descendants: true,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            Action::Delete {
                target: "lead-0011".to_owned(),
            },
        ] {
            let started = std::time::Instant::now();
            agent_lifecycle_runtime::execute(
                std::sync::Arc::clone(&store),
                &action,
                SYNTHETIC_NOW_SECS,
                std::sync::Arc::new(AcceptLifecycleEffects),
            )
            .await
            .expect("lifecycle action");
            eprintln!(
                "benchmark action={action:?} elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
    }
}
