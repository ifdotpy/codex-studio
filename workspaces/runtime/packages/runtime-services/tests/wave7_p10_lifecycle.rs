#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};
use studio_runtime_providers::{RequestId, RequestTicket};
use studio_runtime_services::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    lifecycle_effects::ProductionLifecyclePorts,
};
use studio_runtime_services::{
    agent_lifecycle_runtime::{
        self, LifecycleEffect, LifecycleEffectKind, LifecyclePorts, UnavailableLifecyclePorts,
    },
    agent_management::{Action, ManagementError, Records},
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
                } else if table == "voice_sessions" {
                    tx.execute(
                        "INSERT INTO voice_sessions(id,agent,created,ended,state) VALUES(?1,?2,1,?3,?4)",
                        params![row["id"].as_str(), row["agent"].as_str(), row["ended"].as_f64(), row["state"].as_str()],
                    )?;
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

fn parsed_child_event_rows(rows: &Value) -> Value {
    let mut rows = rows.clone();
    for row in rows.as_array_mut().expect("event rows") {
        if row
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("child-stop:"))
            && let Some(text) = row.get("text").and_then(Value::as_str)
        {
            row["text"] = serde_json::from_str(text).expect("child event JSON payload");
        }
    }
    rows
}

// A fixture for the transaction boundary only. Provider transport is owned by
// account_worker, whose direct Stop receipt tests run in the crate suite.
struct InterruptFixture {
    notification_payloads: BTreeMap<String, String>,
    pending_deleted: Mutex<Vec<String>>,
}
impl Default for InterruptFixture {
    fn default() -> Self {
        Self {
            notification_payloads: BTreeMap::new(),
            pending_deleted: Mutex::new(Vec::new()),
        }
    }
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
            pending_deleted: Mutex::new(Vec::new()),
        }
    }

    fn settle_deleted(&self, store: &RuntimeStore) {
        let pending = std::mem::take(&mut *self.pending_deleted.lock().expect("delete queue"));
        store
            .transaction(|tx| {
                for id in pending {
                    if let Some(mut agent) = studio_runtime_store::AgentRepository::get(
                        tx,
                        &studio_runtime_store::AgentId::from(id.as_str()),
                    )? {
                        if agent.extra.get("deletedAt").is_none_or(Value::is_null) {
                            continue;
                        }
                        let attempt = agent.extra.get("startAttempt").unwrap_or(&Value::Null);
                        let needs_settlement = agent.extra.get("inFlight").and_then(Value::as_bool)
                            == Some(true)
                            || agent.extra.get("turnId").is_some_and(|turn| {
                                turn.as_str().is_some_and(|value| !value.is_empty())
                            })
                            || attempt.get("activeAtReservation").and_then(Value::as_bool)
                                == Some(true)
                            || agent
                                .extra
                                .get("status")
                                .and_then(Value::as_str)
                                .is_some_and(|status| {
                                    ["running", "starting", "approval"].contains(&status)
                                });
                        if !needs_settlement {
                            continue;
                        }
                        agent.extra.insert("inFlight".to_owned(), json!(false));
                        agent.extra.insert("turnId".to_owned(), Value::Null);
                        agent.extra.insert("activity".to_owned(), Value::Null);
                        agent.extra.insert("activeTools".to_owned(), json!([]));
                        if let Some(attempt) = agent.extra.get_mut("startAttempt")
                            && attempt.get("activeAtReservation") == Some(&json!(true))
                        {
                            attempt["activeAtReservation"] = json!(false);
                        }
                        studio_runtime_store::AgentRepository::put(tx, &agent)?;
                    }
                }
                Ok(())
            })
            .expect("post-commit delete reconciliation");
    }
}
impl LifecyclePorts for InterruptFixture {
    fn committed(&self) -> Result<(), ManagementError> {
        Ok(())
    }
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        if effect.kind() == LifecycleEffectKind::FinalizeDeleted {
            let id = effect.before()["id"]
                .as_str()
                .expect("deleted agent identity");
            self.pending_deleted
                .lock()
                .map_err(|_| ManagementError::Invalid("Lifecycle deletion queue unavailable"))?
                .push(id.to_owned());
            return Ok(());
        }
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
            let parent = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(parent_id),
            )?
            .expect("parent agent");
            let text = self
                .notification_payloads
                .get(&id)
                .map(String::as_str)
                .unwrap_or("{}");
            studio_runtime_services::event_write::enqueue_recovery_event(
                tx,
                &parent,
                id,
                "child_result",
                text.to_owned(),
                effect.at(),
            )
            .expect("enqueue child notification through the production event writer");
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
    assert_eq!(cases.len(), 21);
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
        ports.settle_deleted(&store);
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
fn production_interrupt_is_durable_post_commit_and_retry_coalesces_after_rollback() {
    struct Broker {
        store: Arc<RuntimeStore>,
        scope: ConnectionScope,
        calls: Mutex<Vec<ProviderRequest>>,
    }
    impl AccountSessionPort for Broker {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            (account == self.scope.account).then(|| self.scope.clone())
        }

        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> Result<SessionReceipt, AccountSessionError> {
            assert_eq!(request.expected_scope.as_ref(), Some(&self.scope));
            let reader = self
                .store
                .read()
                .expect("intent is committed before dispatch");
            let raw: String = reader
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id='lead'",
                    [],
                    |row| row.get(0),
                )
                .expect("stopped agent row");
            let agent: Value = serde_json::from_str(&raw).expect("agent record");
            assert_eq!(agent["startAttempt"]["interrupt"]["outcome"], "unknown");
            assert_eq!(
                agent["startAttempt"]["interrupt"]["requestTicket"],
                request.operation
            );
            self.calls.lock().expect("calls lock").push(request.clone());
            Ok(SessionReceipt {
                scope: self.scope.clone(),
                operation: request.operation.clone(),
                ticket: Some(RequestTicket {
                    generation: self.scope.generation,
                    id: RequestId::String(request.operation),
                    method: request.method,
                }),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(json!({"ok":true}))),
                not_sent_reason: None,
            })
        }
    }

    let corpus = cases();
    for case_name in ["running-True", "delete-running"] {
        let case = corpus
            .iter()
            .find(|case| case["name"] == case_name)
            .expect("Python active stop/delete corpus case");
        for boundary in ["BEFORE", "AFTER"] {
            let root = tempfile::tempdir().expect("root");
            template(root.path());
            let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open"));
            seed(
                &store,
                serde_json::from_value(case["records"].clone()).expect("Python rows"),
            );
            let before = state(&store);
            store
            .transaction(|tx| {
                tx.execute_batch(&format!(
                    "CREATE TRIGGER interrupt_projection_fail {boundary} INSERT ON sync_entities BEGIN SELECT RAISE(ABORT,'projection failure'); END"
                ))?;
                Ok(())
            })
            .expect("install projection failpoint");
            let broker = Arc::new(Broker {
                store: store.clone(),
                scope: ConnectionScope {
                    account: "fixture-account".into(),
                    connection: "fixture-connection".into(),
                    generation: 7,
                },
                calls: Mutex::new(Vec::new()),
            });
            let ports = ProductionLifecyclePorts::new(
                store.clone(),
                Arc::new(Notify::new()),
                Arc::new(Notify::new()),
                broker.clone(),
            );
            let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
            assert!(
                ports
                    .execute(&action, case["now"].as_f64().expect("time"))
                    .is_err()
            );
            assert_eq!(
                state(&store),
                before,
                "rollback at {boundary} sync projection"
            );
            assert!(broker.calls.lock().expect("calls lock").is_empty());
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER interrupt_projection_fail")?;
                    Ok(())
                })
                .expect("remove failpoint");

            assert_eq!(
                ports
                    .execute(&action, case["now"].as_f64().expect("time"))
                    .expect("retry"),
                case["expected"]["result"]
            );
            let calls = broker.calls.lock().expect("calls lock");
            assert_eq!(
                calls.len(),
                1,
                "one provider dispatch after {boundary} rollback"
            );
            assert_eq!(
                calls[0].actor.as_ref().map(|actor| actor.agent.as_str()),
                Some("lead")
            );
            assert_eq!(calls[0].actor.as_ref().map(|actor| actor.epoch), Some(3));
            drop(calls);
            let reader = store.read().expect("settled receipt");
            let raw: String = reader
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id='lead'",
                    [],
                    |row| row.get(0),
                )
                .expect("agent row");
            let agent: Value = serde_json::from_str(&raw).expect("agent record");
            assert_eq!(agent["startAttempt"]["interrupt"]["outcome"], "accepted");
            let actual = state(&store);
            for (table, expected) in case["expected"]["records"]["tables"]
                .as_object()
                .expect("Python output tables")
            {
                let mut actual_rows = actual["tables"][table].clone();
                if table == "runtime_agents" {
                    for row in actual_rows.as_array_mut().expect("agent rows") {
                        if let Some(attempt) = row.get_mut("startAttempt") {
                            attempt
                                .as_object_mut()
                                .expect("attempt object")
                                .remove("interrupt");
                        }
                    }
                }
                assert_eq!(actual_rows, *expected, "Python effect corpus rows: {table}");
            }
        }
    }
}

#[test]
fn production_child_notification_matches_python_corpus() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "stop-worker-notification")
        .expect("Python child-stop corpus case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    );
    let ports = ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert_eq!(
        ports
            .execute(&action, case["now"].as_f64().expect("time"))
            .expect("local stop and durable parent event"),
        case["expected"]["result"]
    );
    // The oracle mirrors Runtime.enqueue's auto-wake state transition. Event
    // payloads are JSON values in both implementations; serializer whitespace
    // and map-key order are not part of the event contract.
    let actual = state(&store);
    for (table, expected) in case["expected"]["records"]["tables"]
        .as_object()
        .expect("Python output tables")
    {
        if table == "runtime_events" {
            assert_eq!(
                parsed_child_event_rows(&actual["tables"][table]),
                parsed_child_event_rows(expected),
                "Python child notification event rows"
            );
        } else {
            assert_eq!(
                &actual["tables"][table], expected,
                "Python event corpus: {table}"
            );
        }
    }
    let actual_sync = actual["sync"]
        .as_array()
        .expect("sync rows")
        .iter()
        .filter(|row| row[0] != "workspace")
        .map(|row| json!([row[0], row[1], row[4], row[5]]))
        .collect::<Vec<_>>();
    let mut expected_sync = case["expected"]["syncEntities"]
        .as_array()
        .expect("Python sync rows")
        .iter()
        .map(|row| json!([row[0], row[1], row[2], row[3]]))
        .collect::<Vec<_>>();
    expected_sync.sort_by_key(|row| (row[0].to_string(), row[1].to_string()));
    assert_eq!(
        actual_sync, expected_sync,
        "Python notification projections"
    );
}

#[test]
fn production_restart_scan_settles_durable_deleted_agent_state() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "delete-running")
        .expect("active delete corpus case");
    let mut records: Records =
        serde_json::from_value(case["records"].clone()).expect("Python rows");
    let deleted = records
        .tables
        .get_mut("runtime_agents")
        .expect("agent rows")
        .iter_mut()
        .find(|agent| agent["id"] == "worker")
        .expect("deleted worker");
    deleted["deletedAt"] = json!(case["now"]);
    deleted["status"] = json!("running");
    deleted["inFlight"] = json!(true);
    deleted["turnId"] = json!("turn-running");
    deleted["activity"] = json!({"at":case["now"]});
    deleted["activeTools"] = json!([{"name":"tool"}]);
    deleted["startAttempt"]["activeAtReservation"] = json!(true);

    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open"));
    seed(&store, records);

    // Constructing the production bundle represents process restart. The
    // tombstone and stale activity are the durable cleanup intent.
    let ports = ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
    );
    ports.committed().expect("restart reconciliation");
    let settled_state = state(&store);
    let settled = settled_state["tables"]["runtime_agents"]
        .as_array()
        .expect("agent rows")
        .iter()
        .find(|agent| agent["id"] == "worker")
        .expect("settled worker");
    assert_eq!(settled["status"], "paused");
    assert_eq!(settled["inFlight"], false);
    assert_eq!(settled["turnId"], Value::Null);
    assert_eq!(settled["activity"], Value::Null);
    assert_eq!(settled["activeTools"], json!([]));
    assert_eq!(settled["startAttempt"]["activeAtReservation"], false);
}

#[test]
fn production_interrupt_requires_in_flight_active_turn() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "running-True")
        .expect("active stop corpus case");
    let mut records: Records =
        serde_json::from_value(case["records"].clone()).expect("Python rows");
    let lead = records
        .tables
        .get_mut("runtime_agents")
        .expect("agent rows")
        .iter_mut()
        .find(|agent| agent["id"] == "lead")
        .expect("lead agent");
    lead["inFlight"] = json!(false);
    lead["turnId"] = json!("turn-stale");

    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open"));
    seed(&store, records);
    let ports = ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    ports
        .execute(&action, case["now"].as_f64().expect("time"))
        .expect("stop");
    let stopped_state = state(&store);
    let agent = stopped_state["tables"]["runtime_agents"]
        .as_array()
        .expect("agent rows")
        .iter()
        .find(|agent| agent["id"] == "lead")
        .expect("stopped lead");
    assert_eq!(agent["startAttempt"].get("interrupt"), None);
}

#[test]
fn production_unknown_interrupt_is_never_resent_on_repeated_stop() {
    struct UnknownBroker {
        scope: ConnectionScope,
        calls: Mutex<Vec<ProviderRequest>>,
    }
    impl AccountSessionPort for UnknownBroker {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            (account == self.scope.account).then(|| self.scope.clone())
        }

        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> Result<SessionReceipt, AccountSessionError> {
            self.calls
                .lock()
                .expect("broker calls")
                .push(request.clone());
            Ok(SessionReceipt {
                scope: self.scope.clone(),
                operation: request.operation,
                ticket: None,
                certainty: RequestCertainty::Unknown,
                result: None,
                not_sent_reason: None,
            })
        }
    }

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "running-True")
        .expect("Python running stop case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    );
    let broker = Arc::new(UnknownBroker {
        scope: ConnectionScope {
            account: "fixture-account".into(),
            connection: "fixture-connection".into(),
            generation: 7,
        },
        calls: Mutex::new(Vec::new()),
    });
    let ports = ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        broker.clone(),
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    ports
        .execute(&action, case["now"].as_f64().expect("time"))
        .expect("first stop commits with unknown interrupt");
    let first = state(&store)["tables"]["runtime_agents"][0]["startAttempt"]["interrupt"].clone();
    assert_eq!(first["outcome"], "unknown");
    ports
        .execute(&action, case["now"].as_f64().expect("time"))
        .expect("repeat stop commits without another interrupt");
    let calls = broker.calls.lock().expect("broker calls");
    assert_eq!(calls.len(), 1);
    drop(calls);
    let reader = store.read().expect("reader");
    let raw: String = reader
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='lead'",
            [],
            |row| row.get(0),
        )
        .expect("agent row");
    let agent: Value = serde_json::from_str(&raw).expect("agent");
    assert_eq!(agent["startAttempt"]["interrupt"], first);
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
