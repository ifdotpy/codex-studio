use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::SystemTime,
};
use studio_runtime_schema::persistence::{
    OutboxKind, OutboxStatus, REQUEST_RESULT_TTL_MS, RequestIdentity,
};
use studio_runtime_services::title_generation::{
    TitleError, TitleReceipt, TitleRequest, TitleRequestPort, TitleResult, WriteOutcome,
    admit_python_request, generate, generate_admitted, progress, settle_catalog_failure,
    validate_dispatch,
};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, outbox::Outbox};
use tokio::sync::{mpsc, oneshot};

async fn fixture() -> (tempfile::TempDir, RuntimeStore) {
    let root = tempfile::tempdir().expect("title state directory");
    crate::media_auth_fixtures::write_state(
        root.path(),
        include_str!("title_generation/initial-state.json"),
    );
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("fixture state opens");
    (root, store)
}
fn catalog() -> studio_runtime_schema::model_catalog::ModelCatalog {
    studio_runtime_schema::model_catalog::ModelCatalog::parse(json!({"data":[{"model":"gpt-6-mini","supportedReasoningEfforts":[{"reasoningEffort":"low"}]}]})).expect("valid fixture catalog")
}
fn request_id() -> String {
    RequestIdentity::issue_root()
}
fn deadline() -> SystemTime {
    SystemTime::now() + std::time::Duration::from_secs(60)
}
fn agent(store: &RuntimeStore) -> Value {
    let raw: String = store
        .test_reader()
        .expect("reader")
        .query_row(
            "SELECT record FROM runtime_agents WHERE id='title-agent'",
            [],
            |row| row.get(0),
        )
        .expect("agent");
    serde_json::from_str(&raw).expect("record")
}
fn row_count(store: &RuntimeStore, table: &str) -> i64 {
    store
        .test_reader()
        .expect("reader")
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}
fn reply(store: &RuntimeStore, id: &str) -> Value {
    serde_json::to_value(
        progress(&store.test_reader().expect("reader"), "title-agent", id)
            .expect("progress")
            .expect("known request"),
    )
    .expect("reply")
}
fn receipt(request: &TitleRequest, outcome: WriteOutcome, title: Option<String>) -> TitleReceipt {
    TitleReceipt {
        request_id: request.request_id.clone(),
        agent_id: request.agent_id.clone(),
        epoch: request.epoch,
        account_key: request.account_key.clone(),
        outcome,
        title,
    }
}
struct FixedPort {
    requests: Mutex<Vec<TitleRequest>>,
    replies: Mutex<VecDeque<(WriteOutcome, Option<String>)>>,
}
impl FixedPort {
    fn new(replies: Vec<(WriteOutcome, Option<&str>)>) -> Self {
        Self {
            requests: Mutex::new(vec![]),
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|(outcome, title)| (outcome, title.map(str::to_owned)))
                    .collect(),
            ),
        }
    }
    fn calls(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }
}
impl TitleRequestPort for FixedPort {
    fn request<'a>(
        &'a self,
        request: TitleRequest,
    ) -> futures_util::future::BoxFuture<'a, TitleReceipt> {
        Box::pin(async move {
            let (outcome, title) = self
                .replies
                .lock()
                .expect("replies")
                .pop_front()
                .expect("no unexpected dispatch");
            let reply = receipt(&request, outcome, title);
            self.requests.lock().expect("requests").push(request);
            reply
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
        let receiver = self
            .reply
            .lock()
            .expect("reply")
            .take()
            .expect("single dispatch");
        Box::pin(async move {
            self.started.send(request).expect("started");
            receiver.await.expect("provider receipt")
        })
    }
}

#[tokio::test]
async fn title_admission_owns_one_root_and_unresolved_outbox() {
    let (_root, store) = fixture().await;
    let id = request_id();
    let (pending, start) = admit_python_request(&store, "title-agent", &id)
        .await
        .expect("admit");
    assert!(start);
    assert_eq!(
        pending,
        json!({"id":"title-agent","request_id":id,"status":"pending"})
    );
    let job = agent(&store)["titleJob"].clone();
    let root = job["rootRequestId"]
        .as_str()
        .expect("provider root admitted once");
    RequestIdentity::new("provider", &[], root, &[]).expect("v7 provider root");
    assert_eq!(row_count(&store, "runtime_outbox"), 1);
    assert_eq!(
        row_count(&store, "request_results"),
        0,
        "pending work is not a completed response"
    );
    let retry = admit_python_request(&store, "title-agent", &id)
        .await
        .expect("retry");
    assert_eq!(retry, (pending, true));
    assert_eq!(agent(&store)["titleJob"], job);
    let rows = Outbox::pending::<Value>(
        &store.test_reader().expect("reader"),
        OutboxKind::TitleGeneration,
        i64::MAX,
        10,
    )
    .expect("pending selector");
    assert!(
        rows.is_empty(),
        "only the owner explicitly dispatches a title"
    );
}

#[tokio::test]
async fn unknown_survives_restart_expiry_and_a_new_current_job() {
    let (root, store) = fixture().await;
    let id = request_id();
    let port = FixedPort::new(vec![(WriteOutcome::Unknown, None)]);
    assert_eq!(
        generate(&store, &port, "title-agent", &catalog(), &id, deadline())
            .await
            .expect("unknown"),
        TitleResult::HeldUnknown
    );
    assert_eq!(port.calls(), 1);
    let old_job = agent(&store)["titleJob"].clone();
    let new_id = request_id();
    admit_python_request(&store, "title-agent", &new_id)
        .await
        .expect("new current job");
    assert_eq!(agent(&store)["titleJob"]["requestId"], new_id);
    assert_eq!(row_count(&store, "runtime_outbox"), 2);
    let expired = chrono::Utc::now().timestamp_millis() - REQUEST_RESULT_TTL_MS - 1;
    let old_id = id.clone();
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_outbox SET payload=json_set(payload,'$.admittedRequest.created',?1) WHERE request_id=?2",rusqlite::params![expired,old_id])?;
        Ok(())
    }).await.expect("elapsed admission time");
    drop(store);
    let reopened = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    let never_send = FixedPort::new(vec![]);
    assert_eq!(
        generate_admitted(
            &reopened,
            &never_send,
            "title-agent",
            &catalog(),
            &id,
            deadline()
        )
        .await
        .expect("held"),
        TitleResult::HeldUnknown
    );
    settle_catalog_failure(&reopened, "title-agent", &id)
        .await
        .expect("catalog cannot settle unknown");
    assert_eq!(reply(&reopened, &id)["status"], "pending");
    assert_eq!(never_send.calls(), 0);
    let rows = Outbox::for_owner::<Value>(
        &reopened.test_reader().expect("reader"),
        studio_runtime_schema::persistence::OwnerKind::Agent,
        "title-agent",
        OutboxKind::TitleGeneration,
    )
    .expect("obligations");
    let row = rows
        .iter()
        .find(|row| row.request_id.as_deref() == Some(&id))
        .expect("old job retained");
    assert_eq!(row.status, OutboxStatus::Unknown);
    assert_eq!(row.next_at, None);
    assert_eq!(row.payload["rootRequestId"], old_job["rootRequestId"]);
    assert_eq!(row_count(&reopened, "request_results"), 0);
}

#[tokio::test]
async fn not_sent_retry_preserves_root_and_exact_reply_after_later_changes() {
    let (root, store) = fixture().await;
    let id = request_id();
    let port = FixedPort::new(vec![
        (WriteOutcome::NotSent, None),
        (WriteOutcome::Written, Some("Useful title")),
    ]);
    assert_eq!(
        generate(&store, &port, "title-agent", &catalog(), &id, deadline())
            .await
            .expect("not sent"),
        TitleResult::RetryableNotSent
    );
    assert_eq!(
        generate(&store, &port, "title-agent", &catalog(), &id, deadline())
            .await
            .expect("retry"),
        TitleResult::Applied {
            title: "Useful title".into()
        }
    );
    let requests = port.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].root_request_id, requests[1].root_request_id);
    assert_eq!(requests[0].admitted_request, requests[1].admitted_request);
    let expected =
        json!({"id":"title-agent","request_id":id,"status":"applied","name":"Useful title"});
    assert_eq!(reply(&store, &id), expected);
    assert_eq!(row_count(&store, "runtime_outbox"), 0);
    assert_eq!(row_count(&store, "request_results"), 1);
    store.transaction(|tx| {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','Later title','$.model','different model') WHERE id='title-agent'",[])?;
        tx.execute("UPDATE runtime_items SET record=json_set(record,'$.text','later prompt')",[])?;
        Ok(())
    }).await.expect("later business state");
    drop(store);
    let reopened = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    let no_provider = FixedPort::new(vec![]);
    assert_eq!(
        admit_python_request(&reopened, "title-agent", &id)
            .await
            .expect("replay"),
        (expected, false)
    );
    assert_eq!(
        generate_admitted(
            &reopened,
            &no_provider,
            "title-agent",
            &catalog(),
            &id,
            deadline()
        )
        .await
        .expect("exact replay"),
        TitleResult::Applied {
            title: "Useful title".into()
        }
    );
    assert_eq!(agent(&reopened)["name"], "Later title");
    assert_eq!(no_provider.calls(), 0);
}

#[tokio::test]
async fn changed_input_conflicts_only_before_another_not_sent_dispatch() {
    let (_root, store) = fixture().await;
    let id = request_id();
    let port = FixedPort::new(vec![(WriteOutcome::NotSent, None)]);
    generate(&store, &port, "title-agent", &catalog(), &id, deadline())
        .await
        .expect("not sent");
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_items SET record=json_set(record,'$.text','changed prompt')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("change prompt");
    assert!(matches!(
        generate(&store, &port, "title-agent", &catalog(), &id, deadline()).await,
        Err(TitleError::Conflict)
    ));
    assert_eq!(port.calls(), 1);
}

#[tokio::test]
async fn crash_after_dispatch_admission_never_resends() {
    let (root, store) = fixture().await;
    let id = request_id();
    admit_python_request(&store, "title-agent", &id)
        .await
        .expect("admit");
    store.set_commit_notifier(|| panic!("crash after durable dispatch admission"));
    let port = FixedPort::new(vec![]);
    assert!(
        std::panic::AssertUnwindSafe(generate_admitted(
            &store,
            &port,
            "title-agent",
            &catalog(),
            &id,
            deadline()
        ))
        .catch_unwind()
        .await
        .is_err()
    );
    assert_eq!(port.calls(), 0);
    drop(store);
    let reopened = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    assert_eq!(
        generate_admitted(&reopened, &port, "title-agent", &catalog(), &id, deadline())
            .await
            .expect("held intent"),
        TitleResult::HeldUnknown
    );
    assert_eq!(port.calls(), 0);
}

#[tokio::test]
async fn admission_failures_roll_back_before_provider_dispatch() {
    for table in ["runtime_agents", "runtime_outbox"] {
        let (_root, store) = fixture().await;
        store.transaction(move |tx| {
            let operation=if table=="runtime_agents" {"UPDATE"}else{"INSERT"};
            tx.execute_batch(&format!("CREATE TRIGGER title_admit_fail AFTER {operation} ON {table} BEGIN SELECT RAISE(ABORT,'admission failpoint'); END"))?;Ok(())
        }).await.expect("admission failure");
        let port = FixedPort::new(vec![]);
        assert!(
            generate(
                &store,
                &port,
                "title-agent",
                &catalog(),
                &request_id(),
                deadline()
            )
            .await
            .is_err()
        );
        assert_eq!(port.calls(), 0);
        assert_eq!(row_count(&store, "runtime_outbox"), 0);
        assert_eq!(row_count(&store, "request_results"), 0);
        assert!(agent(&store).get("titleJob").is_none());
    }
}

#[tokio::test]
async fn durable_written_title_recovers_after_final_transaction_failure() {
    for statement in [
        "CREATE TRIGGER title_final_fail BEFORE UPDATE ON runtime_agents WHEN json_extract(NEW.record,'$.name')='Recovered title' BEGIN SELECT RAISE(ABORT,'final failpoint'); END",
        "CREATE TRIGGER title_final_fail AFTER INSERT ON request_results BEGIN SELECT RAISE(ABORT,'final failpoint'); END",
        "CREATE TRIGGER title_final_fail AFTER DELETE ON runtime_outbox BEGIN SELECT RAISE(ABORT,'final failpoint'); END",
        "CREATE TRIGGER title_final_fail BEFORE INSERT ON sync_entities WHEN NEW.collection='agentSummary' AND json_extract(NEW.payload,'$.value.name')='Recovered title' BEGIN SELECT RAISE(ABORT,'final failpoint'); END",
    ] {
        let (root, store) = fixture().await;
        let id = request_id();
        store
            .transaction(move |tx| {
                tx.execute_batch(statement)?;
                Ok(())
            })
            .await
            .expect("final failure");
        let port = FixedPort::new(vec![(WriteOutcome::Written, Some("Recovered title"))]);
        assert!(
            generate(&store, &port, "title-agent", &catalog(), &id, deadline())
                .await
                .is_err(),
            "{statement}"
        );
        assert_eq!(port.calls(), 1);
        assert_eq!(agent(&store)["name"], "Original conversation name");
        assert_eq!(agent(&store)["titleJob"]["status"], "written");
        assert_eq!(row_count(&store, "request_results"), 0);
        settle_catalog_failure(&store, "title-agent", &id)
            .await
            .expect("catalog preserves written title");
        assert_eq!(reply(&store, &id)["status"], "pending");
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER title_final_fail")?;
                Ok(())
            })
            .await
            .expect("remove failure");
        drop(store);
        let reopened = RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart");
        let never_send = FixedPort::new(vec![]);
        assert_eq!(
            generate_admitted(
                &reopened,
                &never_send,
                "title-agent",
                &catalog(),
                &id,
                deadline()
            )
            .await
            .expect("recover written"),
            TitleResult::Applied {
                title: "Recovered title".into()
            }
        );
        assert_eq!(never_send.calls(), 0);
        assert_eq!(row_count(&reopened, "runtime_outbox"), 0);
        assert_eq!(row_count(&reopened, "request_results"), 1);
    }
}

#[tokio::test]
async fn catalog_failure_settles_only_unsent_and_invalid_titles_complete() {
    let (_root, store) = fixture().await;
    let id = request_id();
    admit_python_request(&store, "title-agent", &id)
        .await
        .expect("admit");
    settle_catalog_failure(&store, "title-agent", &id)
        .await
        .expect("catalog failure");
    assert_eq!(
        reply(&store, &id),
        json!({"id":"title-agent","request_id":id,"status":"failed","error":"The title could not be generated. Try /rename again."})
    );
    let second = request_id();
    let port = FixedPort::new(vec![(WriteOutcome::Written, Some("bad\ntitle"))]);
    assert!(matches!(
        generate(
            &store,
            &port,
            "title-agent",
            &catalog(),
            &second,
            deadline()
        )
        .await,
        Err(TitleError::InvalidTitle)
    ));
    assert_eq!(reply(&store, &second)["status"], "failed");
    assert_eq!(row_count(&store, "runtime_outbox"), 0);
    assert_eq!(row_count(&store, "request_results"), 2);
}

#[tokio::test]
async fn late_name_epoch_or_deletion_changes_discard_without_renaming() {
    for change in [
        json!({"name":"Manual title","manualName":true}),
        json!({"name":"Tool title"}),
        json!({"epoch":2}),
        json!({"deletedAt":1}),
    ] {
        let (_root, store) = fixture().await;
        let id = request_id();
        let (started, mut requests) = mpsc::unbounded_channel();
        let (send, receive) = oneshot::channel();
        let port = Arc::new(GatedPort {
            started,
            reply: Mutex::new(Some(receive)),
        });
        let store = Arc::new(store);
        let (task_store, task_port, task_id) = (Arc::clone(&store), Arc::clone(&port), id.clone());
        let task = tokio::spawn(async move {
            generate(
                &task_store,
                task_port.as_ref(),
                "title-agent",
                &catalog(),
                &task_id,
                deadline(),
            )
            .await
        });
        let request = requests.recv().await.expect("provider request");
        let db = store.test_reader().expect("owner proof");
        validate_dispatch(&db, &request).expect("exact pending job");
        for changed in ["root", "prompt", "epoch"] {
            let mut invalid = request.clone();
            match changed {
                "root" => invalid.root_request_id = request_id(),
                "prompt" => invalid.prompt.push_str(" changed"),
                "epoch" => invalid.epoch += 1,
                _ => unreachable!(),
            }
            assert!(matches!(
                validate_dispatch(&db, &invalid),
                Err(TitleError::Conflict)
            ));
        }
        drop(db);

        store
            .transaction(move |tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_agents WHERE id='title-agent'",
                    [],
                    |row| row.get(0),
                )?;
                let mut record: Value = serde_json::from_str(&raw)?;
                record
                    .as_object_mut()
                    .expect("record")
                    .extend(change.as_object().expect("change").clone());
                AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
                Ok(())
            })
            .await
            .expect("concurrent writer");
        let name = agent(&store)["name"].clone();
        send.send(receipt(
            &request,
            WriteOutcome::Written,
            Some("Generated title".into()),
        ))
        .expect("receipt");
        assert_eq!(
            task.await.expect("join").expect("discard"),
            TitleResult::Discarded
        );
        assert_eq!(agent(&store)["name"], name);
        assert_eq!(reply(&store, &id)["status"], "failed");
        assert_eq!(row_count(&store, "runtime_outbox"), 0);
    }
}

#[tokio::test]
async fn unknown_provider_and_unadmitted_opaque_ids_never_dispatch() {
    let (_root, store) = fixture().await;
    let port = FixedPort::new(vec![]);
    assert!(
        admit_python_request(&store, "title-agent", "opaque-new-id")
            .await
            .is_err()
    );
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.provider','future')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("unknown provider");
    assert!(matches!(
        generate(
            &store,
            &port,
            "title-agent",
            &catalog(),
            &request_id(),
            deadline()
        )
        .await,
        Err(TitleError::InvalidStoredObject { .. })
    ));
    assert_eq!(port.calls(), 0);
    assert_eq!(row_count(&store, "runtime_outbox"), 0);
    assert_eq!(row_count(&store, "request_results"), 0);
}

#[tokio::test]
async fn expired_not_sent_job_never_authorizes_a_new_provider_attempt() {
    let (_root, store) = fixture().await;
    let id = request_id();
    let port = FixedPort::new(vec![(WriteOutcome::NotSent, None)]);
    generate(&store, &port, "title-agent", &catalog(), &id, deadline())
        .await
        .expect("not sent");
    let expired = chrono::Utc::now().timestamp_millis() - REQUEST_RESULT_TTL_MS - 1;
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_outbox SET payload=json_set(payload,'$.admittedRequest.created',?1)", [expired])?;
        Ok(())
    }).await.expect("elapsed admission time");
    assert!(matches!(
        generate_admitted(&store, &port, "title-agent", &catalog(), &id, deadline()).await,
        Err(TitleError::RequestResult(_))
    ));
    assert_eq!(port.calls(), 1);
    assert_eq!(reply(&store, &id)["status"], "pending");
    assert_eq!(row_count(&store, "runtime_outbox"), 1);
}

#[tokio::test]
async fn mismatched_provider_evidence_is_held_without_redispatch() {
    let (_root, store) = fixture().await;
    let id = request_id();
    let (started, mut requests) = mpsc::unbounded_channel();
    let (send, receive) = oneshot::channel();
    let port = Arc::new(GatedPort {
        started,
        reply: Mutex::new(Some(receive)),
    });
    let store = Arc::new(store);
    let (task_store, task_port, task_id) = (Arc::clone(&store), Arc::clone(&port), id.clone());
    let task = tokio::spawn(async move {
        generate(
            &task_store,
            task_port.as_ref(),
            "title-agent",
            &catalog(),
            &task_id,
            deadline(),
        )
        .await
    });
    let request = requests.recv().await.expect("provider request");
    let mut wrong = receipt(
        &request,
        WriteOutcome::Written,
        Some("Unproven title".into()),
    );
    wrong.epoch += 1;
    send.send(wrong).expect("mismatched evidence");
    assert_eq!(
        task.await.expect("join").expect("held"),
        TitleResult::HeldUnknown
    );
    assert!(matches!(
        validate_dispatch(&store.test_reader().expect("reader"), &request),
        Err(TitleError::Conflict)
    ));
    let never_send = FixedPort::new(vec![]);
    assert_eq!(
        generate_admitted(
            &store,
            &never_send,
            "title-agent",
            &catalog(),
            &id,
            deadline()
        )
        .await
        .expect("held replay"),
        TitleResult::HeldUnknown
    );
    assert_eq!(never_send.calls(), 0);
    assert_eq!(row_count(&store, "request_results"), 0);
}
