#[path = "support/prepared_python.rs"]
mod prepared_python;

use futures_util::FutureExt;
use rusqlite::Connection;
use serde_json::{Value, json};
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
    TitleReceipt, TitleRequest, TitleRequestPort, TitleResult, WriteOutcome, generate,
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

fn fixture() -> (tempfile::TempDir, RuntimeStore) {
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
    let store = RuntimeStore::open(Some(root.path())).expect("Python-written state opens");
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
    let connection = store.read().expect("reader");
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

fn write_concurrent_title(store: &RuntimeStore, name: &str, manual: bool) {
    store
        .transaction_with(|tx| {
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
        .expect("concurrent writer commits");
}

#[tokio::test]
async fn unknown_provider_outcome_is_never_resent_after_replay() {
    let (_root, store) = fixture();
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
        let (_root, store) = fixture();
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
        write_concurrent_title(&task_store, name, manual);
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
    let (_root, store) = fixture();
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
    let (_root, store) = fixture();
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
    }).expect("Python-written item changes");
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
    let (root, store) = fixture();
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
    let reopened = RuntimeStore::open(Some(root.path())).expect("restart after intent");
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
        let (_root, store) = fixture();
        store.transaction(|tx| {
            tx.execute_batch(&format!("CREATE TRIGGER title_intent_fail {timing} INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT,'title intent failpoint'); END"))?;
            Ok(())
        }).expect("intent failpoint");
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
        let connection = store.read().expect("state read");
        let intents: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts",
                [],
                |row| row.get(0),
            )
            .expect("intent count");
        assert_eq!(intents, 0, "{timing} intent write rolls back");
    }
    for (table, operation) in [
        ("runtime_agents", "UPDATE"),
        ("sync_entities", "INSERT"),
        ("runtime_operation_receipts", "UPDATE"),
    ] {
        for timing in ["BEFORE", "AFTER"] {
            let (_root, store) = fixture();
            store.transaction(|tx| {
                tx.execute_batch(&format!("CREATE TRIGGER title_write_fail {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'title write failpoint'); END"))?;
                Ok(())
            }).expect("title write failpoint");
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
            assert_eq!(port.calls.load(Ordering::SeqCst), 1);
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
            " WHEN json_extract(NEW.result,'$.state')='applied'",
        ),
    ] {
        for timing in ["BEFORE", "AFTER"] {
            let (root, store) = fixture();
            store
                .transaction(|tx| {
                    tx.execute_batch(&format!("CREATE TRIGGER title_final_fail {timing} {operation} ON {table}{condition} BEGIN SELECT RAISE(ABORT,'title final failpoint'); END"))?;
                    Ok(())
                })
                .expect("final-write failpoint");
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
                .expect("remove failure trigger");
            drop(store);
            let reopened =
                RuntimeStore::open(Some(root.path())).expect("restart with written title");
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

#[test]
fn python_writer_can_open_new_title_state_before_rust_service() {
    let (_root, store) = fixture();
    let (record, projection) = agent_and_projection(&store);
    assert_eq!(
        serde_json::from_str::<Value>(&record).expect("record")["name"],
        "Original conversation name"
    );
    assert!(projection.is_empty());
}
