use crate::projection_fixtures as projections;
use projections::full_rebuild;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fs, path::Path};
use studio_runtime_services::{
    account_session_runtime::AccountSessionPort,
    agent_lifecycle_runtime::{self, LifecycleEffect, LifecyclePorts},
    agent_management::{Action, ManagementError, Records},
    agent_management_store::execute,
    agents_create::mode_fields,
    lifecycle_effects::ProductionLifecyclePorts,
    sync_policy::{EVENT_WINDOW_ROWS, TASK_WINDOW_HISTORY_ROWS},
};
use studio_runtime_store::{RuntimeStore, Transaction, agent_management};
use tokio::sync::Notify;

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
const NOW: f64 = 1_234_567.5;

fn seed_records() -> Value {
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

/// A lifecycle case: seeded rows, the action and the decision time.
fn case(name: &str) -> Value {
    let mut records = seed_records();
    let action = match name {
        "stop-capacity-retry-unsubmitted" => {
            records["tables"]["runtime_agents"][0]
                .as_object_mut()
                .expect("lead")
                .extend(
                    json!({"status":"starting","inFlight":true,"capacityRetry":{"id":"retry"},
                        "startAttempt":{"id":"attempt","capacityRetryId":"retry","submitted":false}})
                    .as_object()
                    .expect("fields")
                    .clone(),
                );
            json!({"action":"stop","target":"lead","descendants":false})
        }
        "unassign" => {
            records["tables"]["runtime_work"] = json!([{"id":"work","owner":"worker","rootId":"lead","version":2,"status":"review","title":"Assignment","results":[{"id":"result","text":"Keep this result"}],"decisions":[]}]);
            json!({"action":"archive","actor":"lead","epoch":3,"target":"worker","reason":"Finished work","unassign_work":true})
        }
        "stop-True" => {
            records["tables"]["runtime_events"] = json!([{"id":"pending","agent":"lead","epoch":3,"status":"pending"},{"id":"uncertain","agent":"lead","epoch":3,"status":"uncertain"}]);
            records["tables"]["runtime_requests"] = json!([{"id":"question","agent":"lead","status":"pending","method":"agent/asyncQuestion"},{"id":"approval","agent":"lead","status":"pending","method":"monitor/approve"}]);
            records["tables"]["runtime_agents"][0]
                .as_object_mut()
                .expect("lead")
                .extend(
                    json!({"parkedEvent":"release","parkReceipt":{"event":"release","epoch":3,"sequence":1},"parkAfterTurn":true})
                    .as_object()
                    .expect("fields")
                    .clone(),
                );
            json!({"action":"stop","target":"lead","descendants":true})
        }
        "delete" => json!({"action":"delete","target":"lead"}),
        "stop-ended-voice" => json!({"action":"stop","target":"lead","descendants":true}),
        other => panic!("unknown case {other}"),
    };
    json!({"name":name,"records":records,"action":action,"now":NOW})
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
    }).await.expect("rows seed");
}
fn state(store: &RuntimeStore) -> Value {
    let db = store.test_reader().expect("reader");
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
    let case = case("stop-capacity-retry-unsubmitted");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
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
    assert_eq!(result["stopped"], json!(["lead"]));
    let saved = state(&store);
    let lead = saved["tables"]["runtime_agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|row| row["id"] == "lead")
        .expect("lead");
    assert!(lead.get("startAttempt").is_none());
    assert_eq!(lead["inFlight"], false);
}

#[tokio::test]
async fn transaction_failure_matrix_rolls_back_rows_and_projections_after_restart() {
    let mut count = 0;
    for case_name in ["unassign", "stop-True"] {
        let case = case(case_name);
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
                execute(std::sync::Arc::clone(&store), &action, 1234567.5)
                    .await
                    .expect("recovery applies once");
                assert_ne!(
                    state(&store),
                    before,
                    "recovery changed rows {case_name} {timing} {table}"
                );

                count += 1;
            }
        }
    }
    assert_eq!(count, 14);
}

#[tokio::test]
async fn committed_archive_response_loss_replays_after_restart_without_duplicate_release() {
    let case = case("unassign");
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
    assert_eq!(first["status"], "archived");
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
    let case = case("delete");
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
        let raw:String=store.test_reader().expect("reader").query_row("SELECT payload FROM sync_entities WHERE collection='room' AND id='broadcast' AND deleted=0",[],|r| r.get(0)).expect("retained broadcast");
        serde_json::from_str::<Value>(&raw).expect("payload")["value"].clone()
    };
    let before = room(&store);
    assert_eq!(before["members"], json!(["lead"]));
    execute(std::sync::Arc::clone(&store), &action, 200.0)
        .await
        .expect("repeated deletion");
    assert_eq!(room(&store), before);
}

fn input_states(deleted_worker: bool) -> Connection {
    let connection = Connection::open_in_memory().expect("memory database");
    connection
        .execute_batch(&studio_runtime_store::agent_columns::fixture_sql(
            "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT);
        CREATE TABLE runtime_events(id TEXT, agent TEXT, status TEXT, epoch INTEGER, error TEXT);
        INSERT INTO runtime_events VALUES
          ('pending','worker','pending',1,NULL),('reserved','worker','reserved',1,NULL),
          ('dispatching','worker','dispatching',1,NULL),('uncertain','worker','uncertain',1,NULL),
          ('delivered','worker','delivered',1,NULL),('foreign','other','pending',1,NULL),
          ('stale','other','pending',0,NULL),('foreign-uncertain','other','uncertain',1,NULL);",
        ))
        .expect("delivery states");
    connection
        .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
        .expect("volatile agent table creates");
    let worker = if deleted_worker {
        r#"{"epoch":1,"deletedAt":5.0}"#
    } else {
        r#"{"epoch":1}"#
    };
    connection
        .execute(
            "INSERT INTO runtime_agents VALUES('worker',?1),('other','{\"epoch\":1}')",
            [worker],
        )
        .expect("agents");
    connection
}

fn input_statuses(connection: &Connection) -> Vec<(String, String)> {
    connection
        .prepare("SELECT id,status FROM runtime_events ORDER BY id")
        .expect("query")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("states")
}

fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(a, b)| ((*a).into(), (*b).into()))
        .collect()
}

#[test]
fn stopping_a_live_agent_keeps_reserved_and_uncertain_inputs() {
    let mut connection = input_states(false);
    let transaction = connection.transaction().expect("transaction");
    agent_management::settle_inputs(&transaction, "worker").expect("settlement");
    transaction.commit().expect("commit");
    assert_eq!(
        input_statuses(&connection),
        pairs(&[
            ("delivered", "delivered"),
            ("dispatching", "dispatching"),
            ("foreign", "pending"),
            ("foreign-uncertain", "uncertain"),
            ("pending", "cancelled"),
            ("reserved", "reserved"),
            ("stale", "pending"),
            ("uncertain", "uncertain"),
        ])
    );
}

#[test]
fn deleting_an_agent_cancels_unsent_inputs_and_abandons_uncertain_ones() {
    let mut connection = input_states(true);
    let transaction = connection.transaction().expect("transaction");
    agent_management::settle_inputs(&transaction, "worker").expect("settlement");
    transaction.commit().expect("commit");
    assert_eq!(
        input_statuses(&connection),
        pairs(&[
            ("delivered", "delivered"),
            ("dispatching", "failed"),
            ("foreign", "pending"),
            ("foreign-uncertain", "uncertain"),
            ("pending", "cancelled"),
            ("reserved", "cancelled"),
            ("stale", "pending"),
            ("uncertain", "failed"),
        ])
    );
    let error: String = connection
        .query_row(
            "SELECT error FROM runtime_events WHERE id='uncertain'",
            [],
            |r| r.get(0),
        )
        .expect("error");
    assert!(error.contains("outcome unknown"));
}

#[test]
fn startup_sweep_settles_deleted_agents_and_stale_epochs_only() {
    let mut connection = input_states(true);
    let transaction = connection.transaction().expect("transaction");
    agent_management::settle_orphaned_inputs(&transaction).expect("sweep");
    agent_management::settle_orphaned_inputs(&transaction).expect("idempotent sweep");
    transaction.commit().expect("commit");
    assert_eq!(
        input_statuses(&connection),
        pairs(&[
            ("delivered", "delivered"),
            ("dispatching", "failed"),
            ("foreign", "pending"),
            ("foreign-uncertain", "uncertain"),
            ("pending", "cancelled"),
            ("reserved", "cancelled"),
            ("stale", "cancelled"),
            ("uncertain", "failed"),
        ])
    );
}

#[test]
fn diagnostic_count_excludes_deleted_agents() {
    let connection = input_states(true);
    assert_eq!(
        studio_runtime_services::diagnostics::durable_input_pending(&connection).expect("count"),
        3
    );
}

#[tokio::test]
async fn lifecycle_effect_refusals_preserve_rows_and_projections() {
    let case = case("stop-True");
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
    let case = case("stop-ended-voice");
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
    assert!(result["stopped"].is_array());
    let saved = state(&store);
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
/// Task history in the parity fixture: several task windows, so a delete
/// must backfill the window. Unit: rows (tasks).
const PARITY_TASKS_ROWS: usize = 4 * TASK_WINDOW_HISTORY_ROWS;
/// Workers under each synthetic lead. Unit: rows (agents) per team.
const WORKERS_PER_TEAM_ROWS: usize = 3;
/// Teams in the manual benchmark fixture. Unit: rows (lead agents).
const BENCHMARK_TEAMS_ROWS: usize = 500;
/// Task history in the manual benchmark fixture. Unit: rows (tasks).
const BENCHMARK_TASKS_ROWS: usize = 300_000;
/// Padding added to each agent record of the live-shaped benchmark. Unit: bytes
/// (the live state averages about 13 KB per agent record).
const LIVE_AGENT_RECORD_PAD_BYTES: usize = 13_000;
/// Leads left undeleted in the live-shaped benchmark; the live state keeps
/// every other agent as a tombstone. Unit: rows (lead agents).
const LIVE_UNDELETED_LEADS_ROWS: usize = 20;
/// Tasks per monitor and per event row in the synthetic fixture; the event
/// history always exceeds the event window by one row. Unit: rows.
const TASKS_PER_HISTORY_ROW: usize = 50;
/// Lifecycle decision time used by the synthetic fixtures. Unit: seconds.
const SYNTHETIC_NOW_SECS: f64 = 1234567.5;

/// Synthetic conversation state: `leads` teams of one lead and
/// `WORKERS_PER_TEAM_ROWS` workers, one broadcast room per team, a private
/// room between consecutive leads, one project peer team of all leads, an
/// active and a running work item per team (release makes it `ready`),
/// `tasks` task rows spread over all agents (one running task per team),
/// proportional monitor and event history,
/// and one oldest undelivered event per team outside the event window.
/// Projections are seeded by one full rebuild so lifecycle actions start
/// from current views.
async fn synthetic_store(root: &Path, leads: usize, tasks: usize) -> std::sync::Arc<RuntimeStore> {
    if !root.join("canvas.sqlite3").exists() {
        template(root);
    }
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root)).await.expect("open"));
    studio_runtime_services::entity_publication::install(&store)
        .await
        .expect("source publisher initialized");
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
                for worker in 0..WORKERS_PER_TEAM_ROWS {
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
                let ready = format!("ready-{lead:04}");
                tx.execute(
                    "INSERT INTO runtime_work(id,record) VALUES(?1,?2)",
                    params![
                        ready,
                        json!({"id":ready,"rootId":root_id,"owner":format!("worker-{lead:04}-1"),
                            "status":"running","title":"Return task","description":"",
                            "created":1.0,"updated":1.0,"version":1,"dependencies":[],
                            "results":[],"decisions":[],"createdBy":root_id})
                        .to_string()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES(?1,?2,'user','input','pending',-1,1)",
                    params![format!("pending-{lead:04}"), format!("worker-{lead:04}-2")],
                )?;
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
            let leads_only = agents
                .iter()
                .filter(|id| id.starts_with("lead-"))
                .collect::<Vec<_>>();
            tx.execute(
                "INSERT INTO runtime_projects(id,record) VALUES('project',?1)",
                [json!({"id":"project","name":"Project","path":cwd,
                    "peerTeams":[{"id":"team","name":"Team","members":leads_only}]})
                .to_string()],
            )?;
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
            for index in 0..(tasks / TASKS_PER_HISTORY_ROW).max(EVENT_WINDOW_ROWS + 1) {
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
    let db = store.test_reader().expect("reader");
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
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}
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
        // Archive and restore also settle workspace cleanup; stop and delete
        // run the production effect adapter, which releases deleted work.
        let result = if matches!(action, Action::Archive { .. } | Action::Restore { .. }) {
            execute(std::sync::Arc::clone(&store), &action, SYNTHETIC_NOW_SECS).await
        } else {
            agent_lifecycle_runtime::execute(
                std::sync::Arc::clone(&store),
                &action,
                SYNTHETIC_NOW_SECS,
                std::sync::Arc::new(ProductionLifecyclePorts::new(
                    std::sync::Arc::clone(&store),
                    std::sync::Arc::new(Notify::new()),
                    std::sync::Arc::new(Notify::new()),
                    std::sync::Arc::new(NoBroker),
                    None,
                )),
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
    let scratch = std::env::temp_dir();
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

/// Manual measurement, not a check: a delete on a state shaped like the live
/// one (fat agent records, nearly every agent already a tombstone). Run with
/// `--ignored --nocapture` and `CODEX_RUNTIME_STAGE_TIMINGS=1`.
#[tokio::test]
#[ignore = "manual delete latency benchmark on a live-shaped synthetic state"]
async fn delete_latency_on_live_shaped_state() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}
    let scratch = std::env::temp_dir();
    let root = tempfile::tempdir_in(&scratch).expect("root");
    let store = synthetic_store(root.path(), BENCHMARK_TEAMS_ROWS, BENCHMARK_TASKS_ROWS).await;
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.pad',\
                 substr(replace(hex(zeroblob(?1)),'0','x'),1,?1))",
                [LIVE_AGENT_RECORD_PAD_BYTES as i64],
            )?;
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) \
                 WHERE json_extract(record,'$.rootId')>?1",
                [format!("lead-{:04}", LIVE_UNDELETED_LEADS_ROWS)],
            )?;
            Ok(())
        })
        .await
        .expect("live-shaped rows");
    let started = std::time::Instant::now();
    agent_lifecycle_runtime::execute(
        std::sync::Arc::clone(&store),
        &Action::Delete {
            target: "lead-0011".to_owned(),
        },
        SYNTHETIC_NOW_SECS,
        std::sync::Arc::new(ProductionLifecyclePorts::new(
            std::sync::Arc::clone(&store),
            std::sync::Arc::new(Notify::new()),
            std::sync::Arc::new(Notify::new()),
            std::sync::Arc::new(NoBroker),
            None,
        )),
    )
    .await
    .expect("delete");
    eprintln!(
        "benchmark live_shaped_delete_ms={}",
        started.elapsed().as_millis()
    );
}
