use crate::prepared_python_support as prepared_python;

use futures_util::FutureExt;
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use studio_runtime_services::title_generation::{
    TitleReceipt, TitleRequest, TitleRequestPort, TitleResult, WriteOutcome, admit_python_request,
    generate, generate_admitted, settle_catalog_failure,
};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
use tokio::sync::{mpsc, oneshot};

fn template(root: &Path) {
    let schema: Value = serde_json::from_str(include_str!(
        "../../runtime-store/tests/fixtures/live-state-schema.json"
    ))
    .expect("schema fixture");
    for filename in ["canvas.sqlite3", "analytics.sqlite3"] {
        let database = Connection::open(root.join(filename)).expect("template database");
        database
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; BEGIN;")
            .expect("template durability");
        let mut objects = schema[filename]["sqlite_master"]
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
                .contains(&object["name"].as_str().expect("object name"))
            })
            .collect::<Vec<_>>();
        objects.sort_by_key(|object| {
            (
                match object["type"].as_str() {
                    Some("table") => 0,
                    Some("index") => 1,
                    Some("view") => 2,
                    _ => 3,
                },
                object["name"].as_str(),
            )
        });
        for object in objects {
            database
                .execute_batch(object["sql"].as_str().expect("object DDL"))
                .expect("template DDL");
        }
        database.execute_batch("COMMIT").expect("template commit");
    }
}

#[allow(clippy::disallowed_methods)]
async fn fixture() -> (tempfile::TempDir, RuntimeStore) {
    let root = tempfile::tempdir().expect("tmpfs state directory");
    template(root.path());
    let canvas = root.path().join("canvas.sqlite3");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let output = Command::new(python)
        .arg("-B")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/title_generation/write_state.py"))
        .arg(canvas)
        .output()
        .expect("Python writes fixture records");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("Python-written state opens");
    (root, store)
}

fn catalog() -> Value {
    json!({"data":[{"model":"gpt-6-mini","supportedReasoningEfforts":[{"reasoningEffort":"low"}]}]})
}
fn deadline() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
}

struct FixedPort {
    calls: AtomicUsize,
    replies: Mutex<Vec<TitleReceipt>>,
}
impl FixedPort {
    fn new(replies: Vec<TitleReceipt>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            replies: Mutex::new(replies),
        }
    }
}
impl TitleRequestPort for FixedPort {
    fn request<'a>(
        &'a self,
        _request: TitleRequest,
    ) -> futures_util::future::BoxFuture<'a, TitleReceipt> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies.lock().expect("reply lock").remove(0)
        })
    }
}

struct GatedPort {
    started: mpsc::UnboundedSender<TitleRequest>,
    reply: Mutex<Option<oneshot::Receiver<TitleReceipt>>>,
}
impl TitleRequestPort for GatedPort {
    fn request<'a>(
        &'a self,
        request: TitleRequest,
    ) -> futures_util::future::BoxFuture<'a, TitleReceipt> {
        let started = self.started.clone();
        let reply = self
            .reply
            .lock()
            .expect("reply lock")
            .take()
            .expect("single request");
        Box::pin(async move {
            started
                .send(request)
                .expect("request entered provider port");
            reply.await.expect("provider receipt")
        })
    }
}

fn receipt(request: &TitleRequest, outcome: WriteOutcome, title: Option<&str>) -> TitleReceipt {
    TitleReceipt {
        request_id: request.request_id.clone(),
        agent_id: request.agent_id.clone(),
        epoch: request.epoch,
        account_key: request.account_key.clone(),
        outcome,
        title: title.map(str::to_owned),
    }
}

fn agent_and_projection(store: &RuntimeStore) -> (String, Vec<Value>) {
    let connection = store.test_reader().expect("reader");
    let agent = connection
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='title-agent'",
            [],
            |row| row.get(0),
        )
        .expect("agent row");
    let projection = connection
        .prepare("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities WHERE collection='agent' AND id='title-agent'")
        .expect("projection query")
        .query_map([], |row| {
            Ok(json!([row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,String>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,i64>(5)?]))
        })
        .expect("projection rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("projection values");
    (agent, projection)
}

fn persisted_snapshot(store: &RuntimeStore) -> Value {
    let connection = store.test_reader().expect("snapshot reader");
    let agent: String = connection
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='title-agent'",
            [],
            |row| row.get(0),
        )
        .expect("persisted agent");
    let mut events_statement = connection
        .prepare("SELECT * FROM runtime_events ORDER BY id")
        .expect("events query");
    let event_columns = events_statement
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let events = events_statement
        .query_map([], |row| {
            let mut event = serde_json::Map::new();
            for (index, name) in event_columns.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(value) => json!(value),
                    rusqlite::types::ValueRef::Real(value) => json!(value),
                    rusqlite::types::ValueRef::Text(value) => {
                        json!(String::from_utf8_lossy(value).to_string())
                    }
                    rusqlite::types::ValueRef::Blob(value) => json!(
                        value
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                    ),
                };
                event.insert(name.clone(), value);
            }
            Ok(Value::Object(event))
        })
        .expect("event rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("event values");
    let projection = connection.prepare("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities WHERE collection='agent' AND id='title-agent' ORDER BY id")
        .expect("projection query")
        .query_map([], |row| Ok(json!([row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, i64>(5)?])))
        .expect("projection rows").collect::<Result<Vec<_>, _>>().expect("projection values");
    let receipts = connection
        .prepare("SELECT id,signature,result FROM runtime_operation_receipts ORDER BY id")
        .expect("receipt query")
        .query_map([], |row| {
            Ok(json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?
            ]))
        })
        .expect("receipt rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("receipt values");
    json!({"agent":serde_json::from_str::<Value>(&agent).expect("agent JSON"),"events":events,"projection":projection,"receipts":receipts})
}

#[tokio::test]
async fn admitted_title_catalog_failure_settles_python_receipt_without_renaming() {
    let (_root, store) = fixture().await;
    let before = persisted_snapshot(&store);
    let (pending, start) = admit_python_request(&store, "title-agent", "catalog-unavailable")
        .await
        .expect("admit title");
    assert!(start);
    assert_eq!(pending["status"], "pending");

    settle_catalog_failure(&store, "title-agent", "catalog-unavailable")
        .await
        .expect("settle unavailable catalog");
    let connection = store.test_reader().expect("read settled title");
    let result: String = connection
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id='catalog-unavailable'",
            [],
            |row| row.get(0),
        )
        .expect("durable failure receipt");
    drop(connection);
    let result: Value = serde_json::from_str(&result).expect("failure receipt JSON");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["request_id"], "catalog-unavailable");

    let after = persisted_snapshot(&store);
    assert_eq!(after["agent"], before["agent"]);
    assert_eq!(after["events"], before["events"]);
    assert_eq!(after["projection"], before["projection"]);
    let (replayed, retry) = admit_python_request(&store, "title-agent", "catalog-unavailable")
        .await
        .expect("replay");
    assert!(!retry);
    assert_eq!(replayed["status"], "failed");
    assert_eq!(persisted_snapshot(&store), after);
}

#[tokio::test]
async fn catalog_failure_settlement_preserves_a_durable_written_title_for_recovery() {
    let (_root, store) = fixture().await;
    store
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TRIGGER title_apply_fail BEFORE UPDATE ON runtime_agents
                 BEGIN SELECT RAISE(ABORT,'title apply failpoint'); END",
            )?;
            Ok(())
        })
        .await
        .expect("install deterministic final-apply failure");
    let provider = FixedPort::new(vec![TitleReceipt {
        request_id: "written-before-catalog-failure".into(),
        agent_id: "title-agent".into(),
        epoch: 1,
        account_key: "default".into(),
        outcome: WriteOutcome::Written,
        title: Some("Durable generated title".into()),
    }]);
    assert!(
        generate(
            &store,
            &provider,
            "title-agent",
            &catalog(),
            "written-before-catalog-failure",
            deadline()
        )
        .await
        .is_err(),
        "the final title-application transaction fails"
    );

    let written_effect: Value = {
        let connection = store.test_reader().expect("read durable provider effect");
        let raw: String = connection
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id LIKE 'title-generation:%'",
                [],
                |row| row.get(0),
            )
            .expect("written effect receipt");
        serde_json::from_str(&raw).expect("effect JSON")
    };
    assert_eq!(written_effect["state"], "written");
    assert_eq!(written_effect["title"], "Durable generated title");
    assert!(written_effect["fingerprint"].as_str().is_some());

    settle_catalog_failure(&store, "title-agent", "written-before-catalog-failure")
        .await
        .expect("settle catalog failure without losing written result");
    let (public_result, effect_after_settlement) = {
        let connection = store
            .test_reader()
            .expect("read title receipts after settlement");
        let public_raw: String = connection
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id='written-before-catalog-failure'",
                [],
                |row| row.get(0),
            )
            .expect("public receipt");
        let effect_raw: String = connection
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id LIKE 'title-generation:%'",
                [],
                |row| row.get(0),
            )
            .expect("written effect receipt");
        (
            serde_json::from_str::<Value>(&public_raw).expect("public JSON"),
            serde_json::from_str::<Value>(&effect_raw).expect("effect JSON"),
        )
    };
    assert_eq!(public_result["status"], "pending");
    assert_eq!(effect_after_settlement, written_effect);

    store
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER title_apply_fail")?;
            Ok(())
        })
        .await
        .expect("remove deterministic failure");
    let (retry_receipt, should_finish) =
        admit_python_request(&store, "title-agent", "written-before-catalog-failure")
            .await
            .expect("same-identity HTTP retry admission");
    assert!(
        should_finish,
        "written effects must schedule application recovery"
    );
    assert_eq!(retry_receipt["status"], "pending");
    let no_provider = FixedPort::new(Vec::new());
    assert_eq!(
        generate_admitted(
            &store,
            &no_provider,
            "title-agent",
            &catalog(),
            "written-before-catalog-failure",
            deadline()
        )
        .await
        .expect("recover durable written title"),
        TitleResult::Applied {
            title: "Durable generated title".into()
        }
    );
    assert_eq!(no_provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn admitted_title_receipt_replays_in_python_shape_without_second_provider_call() {
    let (_root, store) = fixture().await;
    let port = FixedPort::new(vec![TitleReceipt {
        request_id: "admitted-replay".into(),
        agent_id: "title-agent".into(),
        epoch: 1,
        account_key: "default".into(),
        outcome: WriteOutcome::Written,
        title: Some("Generated once".into()),
    }]);
    let (pending, start) = admit_python_request(&store, "title-agent", "admitted-replay")
        .await
        .expect("durable admission");
    assert!(start);
    assert_eq!(pending["status"], "pending");
    assert_eq!(
        generate_admitted(
            &store,
            &port,
            "title-agent",
            &catalog(),
            "admitted-replay",
            deadline()
        )
        .await
        .expect("provider result"),
        TitleResult::Applied {
            title: "Generated once".into()
        }
    );
    let (replayed, retry) = admit_python_request(&store, "title-agent", "admitted-replay")
        .await
        .expect("poll completed title receipt");
    assert!(!retry);
    assert_eq!(replayed["id"], "title-agent");
    assert_eq!(replayed["request_id"], "admitted-replay");
    assert_eq!(replayed["status"], "applied");
    assert_eq!(replayed["name"], "Generated once");
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn python_pending_without_internal_effect_remains_unchanged_and_is_not_resent() {
    let (_root, store) = fixture().await;
    let python_signature = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(
        br#"{"chat": "title-agent", "name": null, "operation": "rename"}"#,
    ));
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
                rusqlite::params![
                    "legacy-pending",
                    python_signature,
                    json!({"id":"title-agent","request_id":"legacy-pending","status":"pending"})
                        .to_string()
                ],
            )?;
            Ok(())
        })
        .await
        .expect("Python-shaped pending receipt");
    let port = FixedPort::new(Vec::new());
    assert_eq!(
        generate_admitted(
            &store,
            &port,
            "title-agent",
            &catalog(),
            "legacy-pending",
            deadline()
        )
        .await
        .expect("pending receipt with unknown execution state stays held"),
        TitleResult::HeldUnknown,
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    let (replayed, start) = admit_python_request(&store, "title-agent", "legacy-pending")
        .await
        .expect("replay Python pending receipt");
    assert!(!start);
    assert_eq!(replayed["status"], "pending");
    assert_eq!(replayed["request_id"], "legacy-pending");
    assert_eq!(port.calls.load(Ordering::SeqCst), 0);
}

#[allow(clippy::disallowed_methods)]
fn python_persisted_snapshot(root: &Path, scenario: &str) -> Value {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let output = Command::new(python)
        .arg("-B")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/title_generation/persistence_oracle.py"),
        )
        .arg(root.join("canvas.sqlite3"))
        .arg(scenario)
        .output()
        .expect("run Python persisted-state oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python persisted snapshot JSON")
}

async fn write_concurrent_title(store: &RuntimeStore, name: &str, manual: bool) {
    let name = name.to_owned();
    store
        .transaction_with(move |tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_agents WHERE id='title-agent'",
                [],
                |row| row.get(0),
            )?;
            let mut record: Value =
                serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
            record["name"] = json!(name);
            record["needsTitle"] = json!(false);
            if manual {
                record["manualName"] = json!(true);
            }
            let typed: AgentRecord = serde_json::from_value(record.clone())
                .map_err(studio_runtime_store::StoreError::from)?;
            AgentRepository::put(tx, &typed)?;
            studio_runtime_services::agents_create::publish_agent(tx, &record)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("concurrent writer commits");
}

async fn write_deleted(store: &RuntimeStore) {
    store
        .transaction_with(|tx| {
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_agents WHERE id='title-agent'",
                [],
                |row| row.get(0),
            )?;
            let mut record: Value =
                serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
            record["deletedAt"] = json!(1_791_936_000.0);
            let typed: AgentRecord = serde_json::from_value(record.clone())
                .map_err(studio_runtime_store::StoreError::from)?;
            AgentRepository::put(tx, &typed)?;
            studio_runtime_services::agents_create::publish_agent(tx, &record)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("deletion commits");
}

#[tokio::test]
async fn persisted_python_results_match_apply_late_writers_and_refusal() {
    for (scenario, concurrent) in [
        ("apply", None),
        ("manual", Some(("Manual title", true))),
        ("tool", Some(("Tool title", false))),
        ("refusal", None),
    ] {
        let (_root, store) = fixture().await;
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        let port = Arc::new(GatedPort {
            started: started_tx,
            reply: Mutex::new(Some(reply_rx)),
        });
        let task_store = Arc::new(store);
        let future_store = task_store.clone();
        let future_port = port.clone();
        let task = tokio::spawn(async move {
            generate(
                &future_store,
                future_port.as_ref(),
                "title-agent",
                &catalog(),
                "persisted-title",
                deadline(),
            )
            .await
        });
        let request = started_rx.recv().await.expect("provider request pending");
        match concurrent {
            Some((name, manual)) => write_concurrent_title(&task_store, name, manual).await,
            None if scenario == "refusal" => write_deleted(&task_store).await,
            None => {}
        }
        reply_tx
            .send(receipt(
                &request,
                WriteOutcome::Written,
                Some("Generated title"),
            ))
            .expect("provider result");
        task.await
            .expect("title task joins")
            .expect("title service result");
        let rust = persisted_snapshot(&task_store);
        drop(task_store);
        let (python_root, python_store) = fixture().await;
        drop(python_store);
        let python = python_persisted_snapshot(python_root.path(), scenario);
        assert_eq!(rust["receipts"].as_array().map(Vec::len), Some(2));
        assert_eq!(python["receipts"].as_array().map(Vec::len), Some(1));
        let public_receipt = rust["receipts"]
            .as_array()
            .expect("Rust receipts")
            .iter()
            .find(|row| row[0] == "persisted-title")
            .expect("Python-shaped public title receipt");
        let python_receipt = python["receipts"]
            .as_array()
            .expect("Python receipts")
            .first()
            .expect("Python title receipt");
        assert_eq!(public_receipt[0], python_receipt[0]);
        assert_eq!(public_receipt[1], python_receipt[1]);
        assert_eq!(
            serde_json::from_str::<Value>(public_receipt[2].as_str().expect("Rust receipt JSON"))
                .expect("Rust receipt value"),
            serde_json::from_str::<Value>(python_receipt[2].as_str().expect("Python receipt JSON"))
                .expect("Python receipt value")
        );
        let mut rust_without_receipts = rust.clone();
        let mut python_without_receipts = python.clone();
        rust_without_receipts
            .as_object_mut()
            .expect("Rust snapshot")
            .remove("receipts");
        python_without_receipts
            .as_object_mut()
            .expect("Python snapshot")
            .remove("receipts");
        assert_eq!(
            rust_without_receipts, python_without_receipts,
            "agent, events, and projection for {scenario}"
        );
    }
}

#[tokio::test]
async fn applied_request_replays_after_restart_and_rejects_changed_content_without_writes() {
    let (root, store) = fixture().await;
    let port = FixedPort::new(vec![TitleReceipt {
        request_id: "restart-applied".into(),
        agent_id: "title-agent".into(),
        epoch: 1,
        account_key: "default".into(),
        outcome: WriteOutcome::Written,
        title: Some("Persisted title".into()),
    }]);
    assert_eq!(
        generate(
            &store,
            &port,
            "title-agent",
            &catalog(),
            "restart-applied",
            deadline(),
        )
        .await
        .expect("first apply"),
        TitleResult::Applied {
            title: "Persisted title".into()
        }
    );
    drop(store);

    let reopened = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart after applied title");
    let before_replay = persisted_snapshot(&reopened);
    let no_provider = FixedPort::new(Vec::new());
    assert_eq!(
        generate(
            &reopened,
            &no_provider,
            "title-agent",
            &catalog(),
            "restart-applied",
            deadline(),
        )
        .await
        .expect("same identity replay"),
        TitleResult::Applied {
            title: "Persisted title".into()
        }
    );
    assert_eq!(no_provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(persisted_snapshot(&reopened), before_replay);

    reopened
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_items SET record=json_set(record,'$.text','changed after apply') WHERE id='title-item-1'",
                [],
            )?;
            Ok(())
        })
        .await.expect("changed request content persists");
    let before_conflict = persisted_snapshot(&reopened);
    assert!(matches!(
        generate(
            &reopened,
            &no_provider,
            "title-agent",
            &catalog(),
            "restart-applied",
            deadline(),
        )
        .await,
        Err(studio_runtime_services::title_generation::TitleError::Conflict)
    ));
    assert_eq!(no_provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(persisted_snapshot(&reopened), before_conflict);
}

#[tokio::test]
async fn completed_python_receipt_replays_without_request_or_persisted_writes() {
    let (root, store) = fixture().await;
    drop(store);
    let python = python_persisted_snapshot(root.path(), "apply");
    let reopened = RuntimeStore::open(Some(root.path()))
        .await
        .expect("open Python-completed receipt");
    let before = persisted_snapshot(&reopened);
    assert_eq!(before, python, "Python persisted rows including receipt");

    let no_provider = FixedPort::new(Vec::new());
    assert_eq!(
        generate(
            &reopened,
            &no_provider,
            "title-agent",
            &catalog(),
            "persisted-title",
            deadline(),
        )
        .await
        .expect("Python receipt replay"),
        TitleResult::Applied {
            title: "Generated title".into()
        }
    );
    assert_eq!(no_provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        persisted_snapshot(&reopened),
        before,
        "replay does not write agent, events, projection, or receipt"
    );
}

#[tokio::test]
async fn unknown_provider_outcome_is_never_resent_after_replay() {
    let (_root, store) = fixture().await;
    let port = FixedPort::new(vec![TitleReceipt {
        request_id: "title-request".into(),
        agent_id: "title-agent".into(),
        epoch: 1,
        account_key: "default".into(),
        outcome: WriteOutcome::Unknown,
        title: None,
    }]);
    let first = generate(
        &store,
        &port,
        "title-agent",
        &catalog(),
        "title-request",
        deadline(),
    )
    .await
    .expect("first request");
    let second = generate(
        &store,
        &port,
        "title-agent",
        &catalog(),
        "title-request",
        deadline(),
    )
    .await
    .expect("replay");
    assert_eq!(first, TitleResult::HeldUnknown);
    assert_eq!(second, TitleResult::HeldUnknown);
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn late_result_after_manual_or_tool_title_writes_no_agent_or_projection_rows() {
    for (name, manual) in [("manual title", true), ("tool title", false)] {
        let (_root, store) = fixture().await;
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        let port = Arc::new(GatedPort {
            started: started_tx,
            reply: Mutex::new(Some(reply_rx)),
        });
        let task_store = Arc::new(store);
        let future_store = task_store.clone();
        let future_port = port.clone();
        let task = tokio::spawn(async move {
            generate(
                &future_store,
                future_port.as_ref(),
                "title-agent",
                &catalog(),
                "late-title",
                deadline(),
            )
            .await
        });
        let request = started_rx.recv().await.expect("request is pending");
        write_concurrent_title(&task_store, name, manual).await;
        let before_late_result = agent_and_projection(&task_store);
        reply_tx
            .send(receipt(
                &request,
                WriteOutcome::Written,
                Some("Provider title"),
            ))
            .expect("late receipt");
        assert_eq!(
            task.await
                .expect("title task joins")
                .expect("service result"),
            TitleResult::Discarded
        );
        assert_eq!(agent_and_projection(&task_store), before_late_result);
    }
}

#[tokio::test]
async fn proven_not_sent_can_retry_same_identity_once_written() {
    let (_root, store) = fixture().await;
    let port = FixedPort::new(vec![
        TitleReceipt {
            request_id: "title-request".into(),
            agent_id: "title-agent".into(),
            epoch: 1,
            account_key: "default".into(),
            outcome: WriteOutcome::NotSent,
            title: None,
        },
        TitleReceipt {
            request_id: "title-request".into(),
            agent_id: "title-agent".into(),
            epoch: 1,
            account_key: "default".into(),
            outcome: WriteOutcome::Written,
            title: Some("Useful title".into()),
        },
    ]);
    assert_eq!(
        generate(
            &store,
            &port,
            "title-agent",
            &catalog(),
            "title-request",
            deadline()
        )
        .await
        .expect("not sent"),
        TitleResult::RetryableNotSent
    );
    assert_eq!(
        generate(
            &store,
            &port,
            "title-agent",
            &catalog(),
            "title-request",
            deadline()
        )
        .await
        .expect("retry"),
        TitleResult::Applied {
            title: "Useful title".into()
        }
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 2);
    let (record, projection) = agent_and_projection(&store);
    assert_eq!(
        serde_json::from_str::<Value>(&record).expect("record")["name"],
        "Useful title"
    );
    assert_eq!(projection.len(), 1);
}

#[tokio::test]
async fn changed_prompt_under_a_retry_identity_conflicts_without_resending() {
    let (_root, store) = fixture().await;
    let port = FixedPort::new(vec![TitleReceipt {
        request_id: "same-id".into(),
        agent_id: "title-agent".into(),
        epoch: 1,
        account_key: "default".into(),
        outcome: WriteOutcome::NotSent,
        title: None,
    }]);
    assert_eq!(
        generate(
            &store,
            &port,
            "title-agent",
            &catalog(),
            "same-id",
            deadline()
        )
        .await
        .expect("NotSent"),
        TitleResult::RetryableNotSent
    );
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_items SET record=json_set(record,'$.text','changed prompt') WHERE id='title-item-1'", [])?;
        Ok(())
    }).await.expect("Python-written item changes");
    assert!(
        generate(
            &store,
            &port,
            "title-agent",
            &catalog(),
            "same-id",
            deadline()
        )
        .await
        .is_err()
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn committed_intent_is_held_after_restart_before_provider_call() {
    let (root, store) = fixture().await;
    store.set_commit_notifier(|| panic!("crash after durable intent commit"));
    let port = FixedPort::new(Vec::new());
    let interrupted = std::panic::AssertUnwindSafe(generate(
        &store,
        &port,
        "title-agent",
        &catalog(),
        "crash-after-intent",
        deadline(),
    ))
    .catch_unwind()
    .await;
    assert!(interrupted.is_err());
    assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    drop(store);
    let reopened = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart after intent");
    let replay_port = FixedPort::new(Vec::new());
    assert_eq!(
        generate(
            &reopened,
            &replay_port,
            "title-agent",
            &catalog(),
            "crash-after-intent",
            deadline()
        )
        .await
        .expect("held intent"),
        TitleResult::HeldUnknown,
    );
    assert_eq!(replay_port.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failpoints_before_and_after_intent_and_title_projection_writes_roll_back() {
    for timing in ["BEFORE", "AFTER"] {
        let (_root, store) = fixture().await;
        store.transaction(move |tx| {
            tx.execute_batch(&format!("CREATE TRIGGER title_intent_fail {timing} INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT,'title intent failpoint'); END"))?;
            Ok(())
        }).await.expect("intent failpoint");
        let port = FixedPort::new(Vec::new());
        assert!(
            generate(
                &store,
                &port,
                "title-agent",
                &catalog(),
                "intent-fail",
                deadline()
            )
            .await
            .is_err()
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
        let connection = store.test_reader().expect("state read");
        let intents: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts",
                [],
                |row| row.get(0),
            )
            .expect("intent count");
        assert_eq!(intents, 0, "{timing} intent write rolls back");
    }
    // The receipt update fails before the provider call; the title writes fail after it.
    for (table, operation, expected_calls) in [
        ("runtime_agents", "UPDATE", 1),
        ("sync_entities", "INSERT", 1),
        ("runtime_operation_receipts", "UPDATE", 0),
    ] {
        for timing in ["BEFORE", "AFTER"] {
            let (_root, store) = fixture().await;
            store.transaction(move |tx| {
                tx.execute_batch(&format!("CREATE TRIGGER title_write_fail {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'title write failpoint'); END"))?;
                Ok(())
            }).await.expect("title write failpoint");
            let port = FixedPort::new(vec![TitleReceipt {
                request_id: "title-write-fail".into(),
                agent_id: "title-agent".into(),
                epoch: 1,
                account_key: "default".into(),
                outcome: WriteOutcome::Written,
                title: Some("Generated title".into()),
            }]);
            assert!(
                generate(
                    &store,
                    &port,
                    "title-agent",
                    &catalog(),
                    "title-write-fail",
                    deadline()
                )
                .await
                .is_err(),
                "{table} {timing}"
            );
            assert_eq!(
                port.calls.load(Ordering::SeqCst),
                expected_calls,
                "{table} {timing}"
            );
            let (record, projection) = agent_and_projection(&store);
            assert_eq!(
                serde_json::from_str::<Value>(&record).expect("agent")["name"],
                "Original conversation name"
            );
            assert!(projection.is_empty(), "{table} {timing}");
        }
    }
}

#[tokio::test]
async fn durable_written_title_replays_after_failed_final_commit_without_provider_resend() {
    for (table, operation, condition) in [
        ("runtime_agents", "UPDATE", ""),
        ("sync_entities", "INSERT", ""),
        (
            "runtime_operation_receipts",
            "UPDATE",
            " WHEN NEW.id LIKE 'title-generation:%' AND json_extract(NEW.result,'$.state')='applied'",
        ),
    ] {
        for timing in ["BEFORE", "AFTER"] {
            let (root, store) = fixture().await;
            store
                .transaction(move |tx| {
                    tx.execute_batch(&format!("CREATE TRIGGER title_final_fail {timing} {operation} ON {table}{condition} BEGIN SELECT RAISE(ABORT,'title final failpoint'); END"))?;
                    Ok(())
                })
                .await.expect("final-write failpoint");
            let port = FixedPort::new(vec![TitleReceipt {
                request_id: "title-replay".into(),
                agent_id: "title-agent".into(),
                epoch: 1,
                account_key: "default".into(),
                outcome: WriteOutcome::Written,
                title: Some("Recovered title".into()),
            }]);
            assert!(
                generate(
                    &store,
                    &port,
                    "title-agent",
                    &catalog(),
                    "title-replay",
                    deadline()
                )
                .await
                .is_err(),
                "{table} {timing}"
            );
            assert_eq!(port.calls.load(Ordering::SeqCst), 1);
            let (record, projection) = agent_and_projection(&store);
            assert_eq!(
                serde_json::from_str::<Value>(&record).expect("agent")["name"],
                "Original conversation name"
            );
            assert!(projection.is_empty(), "{table} {timing}");
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER title_final_fail")?;
                    Ok(())
                })
                .await
                .expect("remove failure trigger");
            drop(store);
            let reopened = RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart with written title");
            let no_provider = FixedPort::new(Vec::new());
            assert_eq!(
                generate(
                    &reopened,
                    &no_provider,
                    "title-agent",
                    &catalog(),
                    "title-replay",
                    deadline()
                )
                .await
                .expect("replay durable title"),
                TitleResult::Applied {
                    title: "Recovered title".into()
                },
                "{table} {timing}"
            );
            assert_eq!(no_provider.calls.load(Ordering::SeqCst), 0);
            let (record, projection) = agent_and_projection(&reopened);
            assert_eq!(
                serde_json::from_str::<Value>(&record).expect("agent")["name"],
                "Recovered title"
            );
            assert_eq!(projection.len(), 1);
        }
    }
}

#[tokio::test]
async fn python_writer_can_open_new_title_state_before_rust_service() {
    let (_root, store) = fixture().await;
    let (record, projection) = agent_and_projection(&store);
    assert_eq!(
        serde_json::from_str::<Value>(&record).expect("record")["name"],
        "Original conversation name"
    );
    assert!(projection.is_empty());
}
