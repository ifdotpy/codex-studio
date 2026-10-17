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
        AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse, RequestCertainty,
        SessionReceipt,
    },
    lifecycle_effects::ProductionLifecyclePorts as ProductionLifecycleAdapter,
};
use studio_runtime_services::{
    agent_lifecycle_runtime::{
        self, LifecycleEffect, LifecycleEffectKind, LifecyclePorts, UnavailableLifecyclePorts,
    },
    agent_management::{Action, ManagementError, Records},
};
use studio_runtime_store::{RuntimeStore, Transaction, agent_management};
use tokio::sync::Notify;

const COMPOSED_DELETE_EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
async fn seed(store: &RuntimeStore, records: Records) {
    store.transaction(move |tx| {
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
    }).await.expect("Python rows seed");
}

/// Runs the production adapter through its public async lifecycle entry point.
/// Keeping the store and ports together makes fixture calls use the same
/// ownership boundary as the runtime server.
struct ProductionLifecyclePorts {
    store: Arc<RuntimeStore>,
    adapter: Arc<ProductionLifecycleAdapter>,
}

impl ProductionLifecyclePorts {
    fn new(
        store: Arc<RuntimeStore>,
        monitor_wake: Arc<Notify>,
        lifecycle_wake: Arc<Notify>,
        interrupt_broker: Arc<dyn AccountSessionPort>,
        voice_provider: Option<Arc<dyn studio_runtime_services::voice_runtime::VoiceRequestPort>>,
    ) -> Self {
        Self {
            adapter: Arc::new(ProductionLifecycleAdapter::new(
                Arc::clone(&store),
                monitor_wake,
                lifecycle_wake,
                interrupt_broker,
                voice_provider,
            )),
            store,
        }
    }

    async fn execute(&self, action: &Action, now: f64) -> Result<Value, ManagementError> {
        agent_lifecycle_runtime::execute(Arc::clone(&self.store), action, now, self.adapter.clone())
            .await
    }

    async fn committed(&self) -> Result<(), ManagementError> {
        LifecyclePorts::committed(&*self.adapter).await
    }
}

#[tokio::test]
async fn composed_stop_dispatches_active_voice_session_after_commit() {
    use studio_runtime_services::voice_runtime::{
        Method, VoiceConnectionScope, VoiceFuture, VoiceReply, VoiceRequestPort,
        VoiceRequestReceipt, VoiceWriteCertainty,
    };

    struct RecordingVoice {
        calls: std::sync::atomic::AtomicUsize,
        called: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }
    impl VoiceRequestPort for RecordingVoice {
        fn prepare<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
            _: &'a str,
            _: i64,
        ) -> VoiceFuture<'a, Result<VoiceConnectionScope, VoiceWriteCertainty>> {
            self.signal_dispatch();
            Box::pin(async { Err(VoiceWriteCertainty::NotSent) })
        }

        fn request<'a>(
            &'a self,
            _: VoiceConnectionScope,
            _: String,
            _: Method,
            _: Value,
            _: std::time::Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            self.signal_dispatch();
            Box::pin(async {
                VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                }
            })
        }

        fn reply<'a>(
            &'a self,
            _: VoiceConnectionScope,
            _: String,
            _: VoiceReply,
            _: std::time::Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            Box::pin(async {
                VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                }
            })
        }
    }
    impl RecordingVoice {
        fn signal_dispatch(&self) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(called) = self.called.lock().expect("voice signal lock").take() {
                called.send(()).unwrap_or(());
            }
        }
    }
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "running-False")
        .expect("Python stop case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    )
    .await;
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.threadId','voice-thread','$.accountKey','voice-account') WHERE id='lead'",
                [],
            )?;
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES('stop-active-voice','lead',1,'voice-thread','voice-account','6','ready')",
                [],
            )?;
            Ok(())
        }).await
        .expect("voice row seed");
    let (called_tx, called_rx) = tokio::sync::oneshot::channel();
    let voice = Arc::new(RecordingVoice {
        calls: std::sync::atomic::AtomicUsize::new(0),
        called: Mutex::new(Some(called_tx)),
    });
    let voice_port: Arc<dyn VoiceRequestPort> = voice.clone();
    let ports = ProductionLifecyclePorts::new(
        Arc::clone(&store),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        Some(voice_port),
    );
    ports
        .execute(
            &Action::Stop {
                target: "lead".to_owned(),
                descendants: false,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            case["now"].as_f64().expect("stop timestamp"),
        )
        .await
        .expect("stop commits and dispatches voice cleanup");
    tokio::time::timeout(std::time::Duration::from_secs(10), called_rx)
        .await
        .expect("voice cleanup dispatches after stop commit")
        .expect("voice provider sends its dispatch signal");
    assert!(
        voice.calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "active voice stop is dispatched after the lifecycle commit"
    );
    let pending_intents = store
        .read()
        .expect("read voice stop receipt")
        .query_row(
            "SELECT COUNT(*) FROM runtime_operation_receipts WHERE signature='lifecycle.voice-stop.v1' \
             AND json_extract(result,'$.agent')='lead' AND json_extract(result,'$.status')='pending'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count durable voice intents");
    assert_eq!(pending_intents, 1, "NotSent keeps its durable retry intent");
}

#[tokio::test]
async fn production_stop_cancels_usage_and_capacity_continuations_in_store() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "idle-False")
        .expect("Python idle stop case");
    let mut records: Records = serde_json::from_value(case["records"].clone()).expect("records");
    let lead = records
        .tables
        .get_mut("runtime_agents")
        .expect("runtime agent table")
        .iter_mut()
        .find(|agent| agent["id"] == "lead")
        .expect("lead record");
    lead["usageResume"] = json!({"id":"resume-stop","status":"scheduled","dueAt":200.0});
    lead["capacityRetry"] = json!({"id":"retry-stop","status":"scheduled","dueAt":200.0});

    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(&store, records).await;
    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES(?1,?2,?3)",
                params![
                    "resume-stop",
                    "lead",
                    json!({"id":"resume-stop","status":"scheduled","dueAt":200.0}).to_string()
                ],
            )?;
            tx.execute(
                "INSERT INTO runtime_capacity_retries(id,agent,record) VALUES(?1,?2,?3)",
                params![
                    "retry-stop",
                    "lead",
                    json!({"id":"retry-stop","status":"scheduled","dueAt":200.0}).to_string()
                ],
            )?;
            Ok(())
        })
        .await
        .expect("continuation rows");
    let ports = ProductionLifecyclePorts::new(
        Arc::clone(&store),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        None,
    );

    ports
        .execute(
            &Action::Stop {
                target: "lead".to_owned(),
                descendants: false,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            case["now"].as_f64().expect("stop timestamp"),
        )
        .await
        .expect("stop commits and cancels continuations");

    let db = store.read().expect("read cancelled continuations");
    let usage: String = db
        .query_row(
            "SELECT record FROM runtime_usage_resumes WHERE id='resume-stop'",
            [],
            |row| row.get(0),
        )
        .expect("usage row");
    let capacity: String = db
        .query_row(
            "SELECT record FROM runtime_capacity_retries WHERE id='retry-stop'",
            [],
            |row| row.get(0),
        )
        .expect("capacity row");
    assert_eq!(
        serde_json::from_str::<Value>(&usage).expect("usage JSON")["status"],
        "cancelled"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&capacity).expect("capacity JSON")["status"],
        "finished"
    );
}

#[tokio::test]
async fn production_delete_releases_deleted_worker_work_item() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "delete")
        .expect("Python delete case");
    let mut records: Records = serde_json::from_value(case["records"].clone()).expect("records");
    records.tables.insert(
        "runtime_work".to_owned(),
        vec![json!({
            "id":"deleted-worker-task", "rootId":"lead", "owner":"worker",
            "status":"ready", "title":"Return task", "description":"",
            "created":1.0, "updated":1.0, "version":1, "dependencies":[],
            "results":[], "decisions":[], "createdBy":"lead"
        })],
    );

    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(&store, records).await;
    let ports = ProductionLifecyclePorts::new(
        Arc::clone(&store),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        None,
    );

    ports
        .execute(
            &Action::Delete {
                target: "lead".to_owned(),
            },
            case["now"].as_f64().expect("delete timestamp"),
        )
        .await
        .expect("delete commits and releases worker work");

    let db = store.read().expect("read released work");
    let raw: String = db
        .query_row(
            "SELECT record FROM runtime_work WHERE id='deleted-worker-task'",
            [],
            |row| row.get(0),
        )
        .expect("work row");
    let work: Value = serde_json::from_str(&raw).expect("work JSON");
    assert!(work["owner"].is_null());
    assert_eq!(work["status"], "ready");
    assert_eq!(work["version"], 2);
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

    async fn settle_deleted(&self, store: &RuntimeStore) {
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
            .await
            .expect("post-commit delete reconciliation");
    }
}
impl LifecyclePorts for InterruptFixture {
    fn stage_voice_delete(&self, _: &Transaction<'_>, _: &str) -> Result<(), ManagementError> {
        Ok(())
    }

    fn committed(
        &self,
    ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        Box::pin(async { Ok(()) })
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

async fn clone_store(base: &Path, root: &Path) -> RuntimeStore {
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        fs::copy(base.join(name), root.join(name)).expect("clone schema");
    }
    RuntimeStore::open(Some(root))
        .await
        .expect("open cloned fixture")
}

fn assert_records(store: &RuntimeStore, case: &Value) {
    let saved = state(store);
    for (table, expected) in case["expected"]["records"]["tables"]
        .as_object()
        .expect("tables")
    {
        let mut actual = saved["tables"][table].clone();
        let mut expected = expected.clone();
        if table == "runtime_agents" {
            for rows in [&mut actual, &mut expected] {
                if let Some(rows) = rows.as_array_mut() {
                    for row in rows {
                        if let Some(record) = row.as_object_mut() {
                            record.remove("workspaceCleanupIntent");
                        }
                    }
                }
            }
        }
        assert_eq!(actual, expected, "{} {table}", case["name"]);
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

#[tokio::test]
async fn python_stop_corpus_complete_records_and_exact_descendants() {
    let cases = cases();
    assert_eq!(cases.len(), 21);
    let base = tempfile::tempdir().expect("template");
    template(base.path());
    for case in cases {
        let root = tempfile::tempdir().expect("case");
        let store = Arc::new(clone_store(base.path(), root.path()).await);
        seed(
            &store,
            serde_json::from_value(case["records"].clone()).expect("Python rows"),
        )
        .await;
        let actions = case["actions"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![case["action"].clone()]);
        let ports = Arc::new(InterruptFixture::for_case(&case));
        let mut result = Value::Null;
        for action in actions {
            let action: Action = serde_json::from_value(action).expect("action");
            result = agent_lifecycle_runtime::execute(
                Arc::clone(&store),
                &action,
                1234567.5,
                ports.clone(),
            )
            .await
            .expect("stop commits");
        }
        ports.settle_deleted(&store).await;
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
        drop(db);
        assert_records(&store, &case);
        drop(store);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart");
        assert_records(&store, &case);
    }
}

#[tokio::test]
async fn lifecycle_store_failpoints_and_missing_consumer_leave_no_writes() {
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unknown-True")
        .expect("unknown corpus");
    let base = tempfile::tempdir().expect("template");
    template(base.path());
    let store = RuntimeStore::open(Some(base.path())).await.expect("open");
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    )
    .await;
    // The clones below copy only the database files; the seed must not stay in the WAL.
    store
        .checkpoint_wal_passive()
        .await
        .expect("seed database checkpoint");
    drop(store);
    for (table, operation) in [
        ("runtime_agents", "UPDATE"),
        ("runtime_requests", "UPDATE"),
        ("runtime_events", "UPDATE"),
        ("sync_entities", "INSERT"),
    ] {
        for boundary in ["BEFORE", "AFTER"] {
            let root = tempfile::tempdir().expect("case");
            let store = Arc::new(clone_store(base.path(), root.path()).await);
            let before = state(&store);
            store.transaction(move |tx| { tx.execute_batch(&format!("CREATE TRIGGER failpoint {boundary} {operation} ON {table} BEGIN SELECT RAISE(ABORT, 'stop failure'); END"))?; Ok(()) }).await.expect("failpoint");
            let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
            assert!(
                agent_lifecycle_runtime::execute(
                    Arc::clone(&store),
                    &action,
                    1234567.5,
                    Arc::new(InterruptFixture::default()),
                )
                .await
                .is_err()
            );
            assert_eq!(state(&store), before, "{boundary} {table}");
            drop(store);
            let store = Arc::new(
                RuntimeStore::open(Some(root.path()))
                    .await
                    .expect("restart"),
            );
            assert_eq!(state(&store), before);
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER failpoint")?;
                    Ok(())
                })
                .await
                .expect("remove fault");
            let refusal = agent_lifecycle_runtime::execute(
                Arc::clone(&store),
                &action,
                1234567.5,
                Arc::new(UnavailableLifecyclePorts),
            )
            .await;
            assert!(matches!(
                refusal,
                Err(ManagementError::Unavailable(
                    "account-worker/agent-interrupt"
                ))
            ));
            assert_eq!(state(&store), before);
            agent_lifecycle_runtime::execute(
                Arc::clone(&store),
                &action,
                1234567.5,
                Arc::new(InterruptFixture::default()),
            )
            .await
            .expect("recover");
            assert_records(&store, &case);
        }
    }
}

#[tokio::test]
async fn production_interrupt_is_durable_post_commit_and_retry_coalesces_after_rollback() {
    struct Broker {
        store: Arc<RuntimeStore>,
        scope: ConnectionScope,
        calls: Mutex<Vec<ProviderRequest>>,
    }
    impl AccountSessionPort for Broker {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            (account == self.scope.account).then(|| self.scope.clone())
        }

        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            SessionReceipt,
        > {
            Box::pin(async move {
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

                    replayed: false,
                })
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
            let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
            seed(
                &store,
                serde_json::from_value(case["records"].clone()).expect("Python rows"),
            )
            .await;
            let before = state(&store);
            store
            .transaction(move |tx| {
                tx.execute_batch(&format!(
                    "CREATE TRIGGER interrupt_projection_fail {boundary} INSERT ON sync_entities BEGIN SELECT RAISE(ABORT,'projection failure'); END"
                ))?;
                Ok(())
            }).await
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
                None,
            );
            let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
            assert!(
                ports
                    .execute(&action, case["now"].as_f64().expect("time"))
                    .await
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
                .await
                .expect("remove failpoint");

            assert_eq!(
                ports
                    .execute(&action, case["now"].as_f64().expect("time"))
                    .await
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
            drop(reader);
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

#[tokio::test]
async fn production_child_notification_matches_python_corpus() {
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "stop-worker-notification")
        .expect("Python child-stop corpus case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    )
    .await;
    let ports = ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        None,
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert_eq!(
        ports
            .execute(&action, case["now"].as_f64().expect("time"))
            .await
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

#[tokio::test]
async fn production_restart_scan_settles_durable_deleted_agent_state() {
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
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(&store, records).await;

    // Constructing the production bundle represents process restart. The
    // tombstone and stale activity are the durable cleanup intent.
    let ports = ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        None,
    );
    ports.committed().await.expect("restart reconciliation");
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

#[tokio::test]
async fn composed_delete_stages_voice_sessions_before_rows_are_removed_and_stops_after_commit() {
    use studio_runtime_services::voice_runtime::{
        Method, VoiceConnectionScope, VoiceFuture, VoiceReply, VoiceRequestPort,
        VoiceRequestReceipt, VoiceWriteCertainty,
    };

    struct RecordingVoice {
        store: Arc<RuntimeStore>,
        calls: Mutex<Vec<String>>,
        call_times: Mutex<Vec<(String, tokio::time::Instant)>>,
        completion_times: Mutex<Vec<(String, tokio::time::Instant)>>,
        dispatched_after_commit: Mutex<Vec<bool>>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
        completion_changed: Notify,
    }
    impl VoiceRequestPort for RecordingVoice {
        fn prepare<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
            _: &'a str,
            _: i64,
        ) -> VoiceFuture<'a, Result<VoiceConnectionScope, VoiceWriteCertainty>> {
            Box::pin(async { Err(VoiceWriteCertainty::NotSent) })
        }

        fn request<'a>(
            &'a self,
            _: VoiceConnectionScope,
            operation_id: String,
            _: Method,
            _: Value,
            _: std::time::Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            let deleted = self
                .store
                .read()
                .and_then(|db| {
                    db.query_row(
                        "SELECT COUNT(*) FROM voice_sessions WHERE id='delete-unknown-voice'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(studio_runtime_store::StoreError::from)
                })
                .is_ok_and(|count| count == 0);
            self.dispatched_after_commit
                .lock()
                .expect("dispatched lock")
                .push(deleted);
            self.calls
                .lock()
                .expect("calls lock")
                .push(operation_id.clone());
            let completion_operation_id = operation_id.clone();
            self.call_times
                .lock()
                .expect("call times lock")
                .push((operation_id, tokio::time::Instant::now()));
            let completed = &self.completed;
            let completion_changed = &self.completion_changed;
            let completion_times = &self.completion_times;
            Box::pin(async move {
                completion_times
                    .lock()
                    .expect("completion times lock")
                    .push((completion_operation_id, tokio::time::Instant::now()));
                completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                completion_changed.notify_one();
                VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                }
            })
        }

        fn reply<'a>(
            &'a self,
            _: VoiceConnectionScope,
            _: String,
            _: VoiceReply,
            _: std::time::Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            Box::pin(async {
                VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                }
            })
        }
    }

    struct NoBroker;
    impl AccountSessionPort for NoBroker {}

    async fn wait_for_completed_requests(voice: &RecordingVoice, expected: usize) {
        tokio::time::timeout(COMPOSED_DELETE_EVENT_TIMEOUT, async {
            loop {
                let changed = voice.completion_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if voice.completed.load(std::sync::atomic::Ordering::SeqCst) >= expected {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("voice request completion arrives");
    }

    async fn wait_for_voice_commits(
        committed_requests: &std::sync::atomic::AtomicUsize,
        changed: &Notify,
        expected: usize,
    ) {
        tokio::time::timeout(COMPOSED_DELETE_EVENT_TIMEOUT, async {
            loop {
                let notification = changed.notified();
                tokio::pin!(notification);
                notification.as_mut().enable();
                if committed_requests.load(std::sync::atomic::Ordering::SeqCst) >= expected {
                    return;
                }
                notification.await;
            }
        })
        .await
        .expect("voice result commit follows completed request");
    }

    fn read_delete_receipt(store: &RuntimeStore) -> Value {
        let reader = store.read().expect("read deletion receipt");
        let raw: String = reader
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id LIKE 'agent-delete-voice:lead:%'",
                [],
                |row| row.get(0),
            )
            .expect("durable voice deletion intent");
        serde_json::from_str(&raw).expect("deletion receipt JSON")
    }

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "delete")
        .expect("Python delete case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    )
    .await;
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.threadId','voice-thread','$.accountKey','voice-account') WHERE id='lead'",
                [],
            )?;
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES('delete-unknown-voice','lead',1,'voice-thread','voice-account','6','unknown')",
                [],
            )?;
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                 VALUES('delete-retry-voice','lead',2,'voice-thread','voice-account','6','active')",
                [],
            )?;
            let actor_epoch = tx.query_row(
                "SELECT json_extract(record,'$.epoch') FROM runtime_agents WHERE id='lead'",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let stop_operation = "voice:stop:delete-unknown-voice";
            let retained_id = format!(
                "account-worker:retained:voice-account:{}:submitted",
                studio_runtime_store::sha256_hex(stop_operation.as_bytes())
            );
            let submitted = json!({
                "call": {
                    "operation": stop_operation,
                    "method": "thread/realtime/stop",
                    "actor": {"agent": "lead", "epoch": actor_epoch},
                    "scope": {"account": "voice-account", "connection": "6", "generation": 6},
                    "params": {"threadId": "voice-thread"}
                }
            });
            tx.execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'retained',?2)",
                rusqlite::params![retained_id, submitted.to_string()],
            )?;
            Ok(())
        }).await
        .expect("voice row seed");
    let voice = Arc::new(RecordingVoice {
        store: Arc::clone(&store),
        calls: Mutex::new(Vec::new()),
        call_times: Mutex::new(Vec::new()),
        completion_times: Mutex::new(Vec::new()),
        dispatched_after_commit: Mutex::new(Vec::new()),
        completed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        completion_changed: Notify::new(),
    });
    let voice_port: Arc<dyn VoiceRequestPort> = voice.clone();
    let ports = ProductionLifecyclePorts::new(
        Arc::clone(&store),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        Some(voice_port),
    );

    let committed_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let committed_requests_for_observer = Arc::clone(&committed_requests);
    let completed_requests_for_observer = Arc::clone(&voice.completed);
    let commit_changed = Arc::new(Notify::new());
    let commit_changed_for_observer = Arc::clone(&commit_changed);
    store.add_commit_observer(move || {
        committed_requests_for_observer.fetch_max(
            completed_requests_for_observer.load(std::sync::atomic::Ordering::SeqCst),
            std::sync::atomic::Ordering::SeqCst,
        );
        commit_changed_for_observer.notify_one();
    });

    let dispatch_started = tokio::time::Instant::now();
    ports
        .execute(
            &Action::Delete {
                target: "lead".to_owned(),
            },
            case["now"].as_f64().expect("delete timestamp"),
        )
        .await
        .expect("delete commits and dispatches voice stop");
    for completed in 1..=8 {
        wait_for_completed_requests(&voice, completed).await;
    }
    wait_for_voice_commits(&committed_requests, &commit_changed, 8).await;

    // Keep the retained stop Unknown while the active session reaches its own
    // retry cap, then advance the held window to 30s without a SQLite write in
    // flight. The later NotSent commit must make the held stop dispatchable
    // before the original 35s deadline.
    let elapsed = dispatch_started.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(30));
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(30) - elapsed).await;
    let held_receipt = read_delete_receipt(&store);
    assert_eq!(
        held_receipt["voiceEndHeldUncertain"][0]["sessionId"], "delete-unknown-voice",
        "the held stop remains uncertain at the 30s checkpoint"
    );
    {
        let calls = voice.calls.lock().expect("calls lock");
        assert_eq!(
            calls
                .iter()
                .filter(|operation| {
                    operation.as_str() == "voice:delete-stop:delete-retry-voice"
                })
                .count(),
            8,
            "the active session reaches its own bounded retry budget"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|operation| {
                    operation.as_str() == "voice:delete-stop:delete-unknown-voice"
                })
                .count(),
            0,
            "an Unknown retained stop remains held through its deadline"
        );
    }
    assert_eq!(
        voice
            .calls
            .lock()
            .expect("calls lock")
            .iter()
            .filter(|operation| operation.as_str() == "voice:delete-stop:delete-retry-voice")
            .count(),
        8,
        "the active session exhausts its own bounded retry budget"
    );
    let stop_operation = "voice:stop:delete-unknown-voice";
    let retained_id = format!(
        "account-worker:retained:voice-account:{}:result",
        studio_runtime_store::sha256_hex(stop_operation.as_bytes())
    );
    tokio::time::resume();
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'retained',?2)",
                rusqlite::params![retained_id, json!({"result":"NotSent","cursor":2}).to_string()],
            )?;
            Ok(())
        }).await
        .expect("ordinary stop settles as proven not sent after deletion");
    for completed in 9..=16 {
        wait_for_completed_requests(&voice, completed).await;
    }
    wait_for_voice_commits(&committed_requests, &commit_changed, 16).await;
    assert!(
        tokio::time::Instant::now().duration_since(dispatch_started)
            >= std::time::Duration::from_secs(35)
    );
    wait_for_completed_requests(&voice, 16).await;
    assert_eq!(
        voice.completed.load(std::sync::atomic::Ordering::SeqCst),
        16,
        "every dispatched voice request future completed"
    );

    let calls = voice.calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 16);
    assert_eq!(
        calls
            .iter()
            .filter(|operation| operation.as_str() == "voice:delete-stop:delete-retry-voice")
            .count(),
        8
    );
    assert_eq!(
        calls
            .iter()
            .filter(|operation| operation.as_str() == "voice:delete-stop:delete-unknown-voice")
            .count(),
        8
    );
    drop(calls);
    let first_held_call_at = voice
        .call_times
        .lock()
        .expect("call times lock")
        .iter()
        .find_map(|(operation, at)| {
            (operation == "voice:delete-stop:delete-unknown-voice").then_some(*at)
        })
        .expect("held stop call follows its 35s deadline");
    assert!(
        first_held_call_at.duration_since(dispatch_started) < std::time::Duration::from_secs(35),
        "a retained stop dispatches when its receipt becomes NotSent before the held deadline"
    );
    let last_held_completion_at = voice
        .completion_times
        .lock()
        .expect("completion times lock")
        .iter()
        .filter_map(|(operation, at)| {
            (operation == "voice:delete-stop:delete-unknown-voice").then_some(*at)
        })
        .max()
        .expect("held stop has completed retries");
    assert!(
        last_held_completion_at.duration_since(dispatch_started)
            >= std::time::Duration::from_secs(35),
        "the held session retains its own retry budget beyond the original deadline"
    );
    let dispatched_after_commit = voice
        .dispatched_after_commit
        .lock()
        .expect("dispatch boundary lock");
    assert_eq!(dispatched_after_commit.len(), 16);
    assert!(dispatched_after_commit.iter().all(|dispatched| *dispatched));
    let reader = store.read().expect("read committed delete");
    let saved: String = reader
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id LIKE 'agent-delete-voice:lead:%'",
            [],
            |row| row.get(0),
        )
        .expect("durable voice deletion intent");
    let saved: Value = serde_json::from_str(&saved).expect("intent JSON");
    assert_eq!(
        saved["voiceEndResults"]["delete-retry-voice"], "not_sent",
        "active retry receipts are durable"
    );
    assert_eq!(
        saved["voiceEndResults"]["delete-unknown-voice"], "not_sent",
        "held retry receipts are durable"
    );
    let pending_session_ids = saved["voiceEndSessions"]
        .as_array()
        .expect("pending deletion sessions")
        .iter()
        .filter_map(|session| session["sessionId"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(pending_session_ids.len(), 2);
    assert!(pending_session_ids.contains("delete-retry-voice"));
    assert!(pending_session_ids.contains("delete-unknown-voice"));
    assert_eq!(saved["voiceEndHeldUncertain"], json!([]));
    assert_eq!(
        reader
            .query_row(
                "SELECT COUNT(*) FROM voice_sessions WHERE agent='lead'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("voice rows deleted"),
        0
    );
}

#[tokio::test]
async fn production_interrupt_requires_in_flight_active_turn() {
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
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(&store, records).await;
    let ports = ProductionLifecyclePorts::new(
        store.clone(),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(NoBroker),
        None,
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    ports
        .execute(&action, case["now"].as_f64().expect("time"))
        .await
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

#[tokio::test]
async fn production_unknown_interrupt_is_never_resent_on_repeated_stop() {
    struct UnknownBroker {
        scope: ConnectionScope,
        calls: Mutex<Vec<ProviderRequest>>,
    }
    impl AccountSessionPort for UnknownBroker {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            (account == self.scope.account).then(|| self.scope.clone())
        }

        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            SessionReceipt,
        > {
            Box::pin(async move {
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
                    replayed: false,
                })
            })
        }
    }

    let case = cases()
        .into_iter()
        .find(|case| case["name"] == "running-True")
        .expect("Python running stop case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("Python rows"),
    )
    .await;
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
        None,
    );
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    ports
        .execute(&action, case["now"].as_f64().expect("time"))
        .await
        .expect("first stop commits with unknown interrupt");
    let first = state(&store)["tables"]["runtime_agents"][0]["startAttempt"]["interrupt"].clone();
    assert_eq!(first["outcome"], "unknown");
    ports
        .execute(&action, case["now"].as_f64().expect("time"))
        .await
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

#[tokio::test]
async fn deletion_requires_release_only_for_releasable_deleted_owner_work() {
    let mut case = cases()
        .into_iter()
        .find(|c| c["name"] == "delete-worker-review-work")
        .expect("review work corpus case");
    case["records"]["tables"]["runtime_work"][0]["status"] = json!("ready");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    )
    .await;
    let before = state(&store);
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(matches!(
        agent_lifecycle_runtime::execute(
            Arc::clone(&store),
            &action,
            1234567.5,
            Arc::new(UnavailableLifecyclePorts),
        )
        .await,
        Err(ManagementError::Unavailable("work-driver/deletion-release"))
    ));
    assert_eq!(state(&store), before);
}

#[tokio::test]
async fn effect_adapter_failure_rolls_back_its_own_writes() {
    struct Fails;
    impl LifecyclePorts for Fails {
        fn committed(
            &self,
        ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
            Box::pin(async { panic!("must not wake after rollback") })
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
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    )
    .await;
    let before = state(&store);
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(
        agent_lifecycle_runtime::execute(Arc::clone(&store), &action, 1234567.5, Arc::new(Fails),)
            .await
            .is_err()
    );
    drop(store);
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    assert_eq!(state(&store), before);
}

#[tokio::test]
async fn successful_adapter_cannot_resurrect_stopped_agent() {
    struct Resumes;
    impl LifecyclePorts for Resumes {
        fn committed(
            &self,
        ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
            Box::pin(async { panic!("must not wake after rollback") })
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
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    )
    .await;
    let before = state(&store);
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(matches!(
        agent_lifecycle_runtime::execute(
            Arc::clone(&store),
            &action,
            1234567.5,
            Arc::new(Resumes),
        )
        .await,
        Err(ManagementError::Invalid(
            "Lifecycle adapter changed stop identity"
        ))
    ));
    drop(store);
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    assert_eq!(state(&store), before);
}

#[tokio::test]
async fn successful_adapter_cannot_replace_attempt_or_provider_identity() {
    struct ReplacesIdentity(&'static str);
    impl LifecyclePorts for ReplacesIdentity {
        fn committed(
            &self,
        ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
            Box::pin(async { panic!("must not wake after rollback") })
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
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
        seed(
            &store,
            serde_json::from_value(case["records"].clone()).expect("rows"),
        )
        .await;
        let before = state(&store);
        let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
        assert!(matches!(
            agent_lifecycle_runtime::execute(
                Arc::clone(&store),
                &action,
                1234567.5,
                Arc::new(ReplacesIdentity(field)),
            )
            .await,
            Err(ManagementError::Invalid(
                "Lifecycle adapter changed stop identity"
            ))
        ));
        assert_eq!(state(&store), before, "{field}");
    }
}

#[tokio::test]
async fn lost_wake_preserves_committed_stop_for_restart() {
    struct LostWake;
    impl LifecyclePorts for LostWake {
        fn stage(
            &self,
            tx: &Transaction<'_>,
            effect: &LifecycleEffect,
        ) -> Result<(), ManagementError> {
            InterruptFixture::default().stage(tx, effect)
        }
        fn committed(
            &self,
        ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
            Box::pin(async { Err(ManagementError::Invalid("injected lost wake")) })
        }
    }
    let case = cases()
        .into_iter()
        .find(|c| c["name"] == "unknown-True")
        .expect("case");
    let root = tempfile::tempdir().expect("root");
    template(root.path());
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(
        &store,
        serde_json::from_value(case["records"].clone()).expect("rows"),
    )
    .await;
    let action: Action = serde_json::from_value(case["action"].clone()).expect("action");
    assert!(matches!(
        agent_lifecycle_runtime::execute(
            Arc::clone(&store),
            &action,
            1234567.5,
            Arc::new(LostWake),
        )
        .await,
        Err(ManagementError::Store(_))
    ));
    drop(store);
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    assert_records(&store, &case);
}
