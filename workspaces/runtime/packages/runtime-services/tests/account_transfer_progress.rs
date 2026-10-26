//! Transfer progress through production preparation and terminal settlement.
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_services::{
    account_session_runtime::{
        AccountSessionError, AccountSessionFuture, AccountSessionPort, ConnectionScope,
        ProviderRequest, ProviderResponse, RequestCertainty, SessionReceipt,
        TransferProviderRequest,
    },
    account_transfer::Action,
    account_transfer_driver as driver,
    account_worker::preparation::{self, PreparationResources},
    model_catalog::ModelCatalogService,
    scheduler,
};
use studio_runtime_store::{
    AccountTransferRepository, AgentId, AgentRecord, AgentRepository, AgentStatus, RuntimeStore,
};

const OP: &str = "41e0b552-0b0c-4c5c-9c37-2e7483cd05d0";

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let root = tempfile::tempdir().expect("isolated state");
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
    let agent = AgentRecord::from_value(json!({
        "id":"lead","rootId":"lead","epoch":3,"status":"queued","isLead":true,
        "autoWake":true,"accountKey":"source","provider":"claude","model":"claude-opus-4-6",
        "threadId":"source-thread","cwd":root.path(),"name":"Lead","error":null,
        "effort":null,"fastMode":false,"workerDefaults":null,"nativeFailureHold":false
    }))
    .expect("agent");
    store
        .transaction(move |tx| AgentRepository::put(tx, &agent))
        .await
        .expect("seed");
    (root, store)
}

fn agent(store: &RuntimeStore) -> Value {
    AgentRepository::get(
        &store.test_reader().expect("reader"),
        &AgentId::from("lead"),
    )
    .expect("read agent")
    .expect("agent exists")
    .value()
}

fn operation(store: &RuntimeStore) -> Value {
    serde_json::from_str(
        &AccountTransferRepository::get(&store.test_reader().expect("reader"), OP)
            .expect("read transfer")
            .expect("transfer exists"),
    )
    .expect("transfer JSON")
}

async fn request(store: &RuntimeStore) -> Value {
    driver::request(store, OP, "lead", "target", "team", "codex", 10.0)
        .await
        .expect("request transfer")
}

#[derive(Default)]
struct RefusingSession(Mutex<Vec<(String, String)>>);

impl AccountSessionPort for RefusingSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.into(),
            connection: format!("{account}-connection"),
            generation: 1,
        })
    }

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        self.0
            .lock()
            .expect("calls")
            .push((request.account, request.method.as_str().into()));
        Box::pin(async { Err(AccountSessionError::Refused("Fixture history read refused")) })
    }
}

fn resources(root: &std::path::Path, session: Arc<dyn AccountSessionPort>) -> PreparationResources {
    let catalog = ModelCatalogService::new();
    catalog.retain_accounts(&[
        json!({"id":"source","home":root,"provider":"claude"}),
        json!({"id":"target","home":root,"provider":"codex"}),
    ]);
    let mut resources =
        PreparationResources::bundled(root.display().to_string(), catalog).expect("resources");
    resources.account_session = Some(session);
    resources
}

#[tokio::test]
async fn unknown_transfer_operation_keeps_the_native_admission_hold() {
    let (root, store) = fixture().await;
    store
        .transaction(|tx| {
            let mut row = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
            row.extra.insert("accountTransferId".into(), json!(OP));
            AgentRepository::put(tx, &row)
        })
        .await
        .expect("orphan transfer marker");
    let before = agent(&store);
    let session = Arc::new(RefusingSession::default());
    let error = preparation::prepare_native_thread(
        store.clone(),
        &resources(root.path(), session.clone()),
        "lead",
        "orphan-transfer",
        1,
    )
    .await
    .expect_err("an unknown transfer operation cannot admit a native request");
    assert!(matches!(
        error,
        studio_runtime_services::account_worker::WorkerError::PreparationRefused(ref reason)
            if reason == "thread-preparation/AccountTransfer"
    ));
    assert_eq!(agent(&store), before, "the source state remains held");
    assert!(session.0.lock().expect("calls").is_empty());
}

struct CompletingSession {
    store: Arc<RuntimeStore>,
    native_path: std::path::PathBuf,
    calls: Mutex<Vec<Method>>,
}

impl CompletingSession {
    fn receipt(account: &str, operation: String, method: Method, result: Value) -> SessionReceipt {
        SessionReceipt {
            scope: RefusingSession::default()
                .current_scope(account)
                .expect("scope"),
            operation,
            ticket: Some(RequestTicket {
                generation: 1,
                id: RequestId::String(format!("native-{}", method.as_str())),
                method,
            }),
            certainty: RequestCertainty::Written,
            result: Some(ProviderResponse::Reply(result)),
            not_sent_reason: None,
            replayed: false,
        }
    }
}

impl AccountSessionPort for CompletingSession {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        RefusingSession::default().current_scope(account)
    }

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            self.calls.lock().expect("calls").push(request.method);
            let result = match request.method {
                Method::ModelList => {
                    json!({"data":[{"model":"gpt-6-astra","isDefault":true,"supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}],"nextCursor":null})
                }
                Method::ThreadRead => {
                    json!({"thread":{"id":"source-thread","status":{"type":"notLoaded"},"path":self.native_path}})
                }
                Method::ThreadTurnsList => json!({"data":[],"nextCursor":null}),
                Method::ThreadFork => {
                    let member = &operation(&self.store)["members"]["lead"];
                    assert_eq!(member["phase"], "lazy_submitted");
                    assert_eq!(member["providerIntent"]["operation"], request.operation);
                    json!({"thread":{"id":"destination-thread"}})
                }
                method => {
                    panic!("unexpected ordinary provider request before adoption: {method:?}")
                }
            };
            Ok(Self::receipt(
                &request.account,
                request.operation,
                request.method,
                result,
            ))
        })
    }

    fn transfer_provider_request<'a>(
        &'a self,
        request: TransferProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            assert_eq!(request.transfer_id, OP);
            assert_eq!(request.member, "lead");
            assert_eq!(request.source_account, "source");
            self.calls.lock().expect("calls").push(request.method);
            assert_eq!(request.method, Method::ThreadQueueList);
            Ok(Self::receipt(
                &request.source_account,
                request.operation,
                request.method,
                json!({"data":[],"nextCursor":null}),
            ))
        })
    }
}

#[tokio::test]
async fn lazy_history_receipt_is_adopted_before_ordinary_preparation_returns() {
    let (root, store) = fixture().await;
    let native_path = root.path().join("sessions/source.jsonl");
    std::fs::create_dir_all(native_path.parent().expect("sessions")).expect("source directory");
    std::fs::write(&native_path, b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"source-thread\"}}\n{\"type\":\"turn\"}\n").expect("source history");
    store
        .transaction(|tx| {
            let mut row = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
            row.extra.insert("provider".into(), json!("codex"));
            row.extra.insert("model".into(), json!("gpt-6-astra"));
            row.extra.insert("nativeEffort".into(), Value::Null);
            AgentRepository::put(tx, &row)
        })
        .await
        .expect("Codex source");
    request(&store).await;
    let session = Arc::new(CompletingSession {
        store: store.clone(),
        native_path,
        calls: Mutex::default(),
    });
    let resources = resources(root.path(), session.clone());
    resources.model_catalog.retain_accounts(&[
        json!({"id":"source","home":root.path(),"provider":"codex"}),
        json!({"id":"target","home":root.path(),"provider":"codex"}),
    ]);
    resources
        .model_catalog
        .set_account_session(Some(session.clone()));
    let _catalog = resources
        .model_catalog
        .wait_for_admission_catalog("target", &resources.account_home, "codex")
        .await;
    preparation::prepare_native_thread(store.clone(), &resources, "lead", "ordinary", 1)
        .await
        .expect("ordinary preparation after adoption");
    let record = agent(&store);
    assert_eq!(record["threadId"], "destination-thread");
    assert!(record.get("accountTransferId").is_none());
    assert!(record.get("lazyAccountTransfer").is_none());
    let member = &operation(&store)["members"]["lead"];
    assert_eq!(member["phase"], "completed");
    assert_eq!(
        member["providerReceipts"]
            .as_array()
            .expect("receipts")
            .len(),
        1
    );
    let calls = session.calls.lock().expect("calls").clone();
    assert_eq!(
        calls
            .iter()
            .filter(|method| **method == Method::ThreadFork)
            .count(),
        1
    );
    preparation::prepare_native_thread(store.clone(), &resources, "lead", "ordinary-next", 1)
        .await
        .expect("next ordinary preparation");
    assert_eq!(
        *session.calls.lock().expect("calls"),
        calls,
        "admission never repeats a saved transfer"
    );
}

#[tokio::test]
async fn lazy_first_native_thread_preparation_moves_history_or_reports_error() {
    let (root, store) = fixture().await;
    assert_eq!(request(&store).await["members"]["lead"]["phase"], "lazy");
    let view = studio_runtime_services::sync_views::agent_entity_view(
        &agent(&store),
        &studio_runtime_services::sync_views::AgentViewContext {
            project_server_id: None,
            launcher_alive: true,
            empty_lead: false,
            has_question: false,
            has_approval: false,
            latest_work_result_file: None,
            command_progress: vec![],
            stall: None,
        },
    )
    .expect("agent view");
    assert_eq!(view["startWait"]["reason"], "account_transfer");
    let session = Arc::new(RefusingSession::default());
    let result = preparation::prepare_native_thread(
        store.clone(),
        &resources(root.path(), session.clone()),
        "lead",
        "first-thread",
        1,
    )
    .await;
    assert!(
        result.is_err(),
        "no ordinary native request can pass a refused transfer"
    );
    assert_eq!(
        *session.0.lock().expect("calls"),
        [("target".into(), "model/list".into())]
    );
    let member = &operation(&store)["members"]["lead"];
    assert_eq!(
        member["phase"], "blocked",
        "the lazy hold must end with an error"
    );
    assert!(
        member["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
}

#[tokio::test]
async fn waiting_member_never_releases_while_busy() {
    for (status, in_flight) in [
        (AgentStatus::Running, false),
        (AgentStatus::Starting, false),
        (AgentStatus::Approval, false),
        (AgentStatus::Queued, true),
    ] {
        let (root, store) = fixture().await;
        store
            .transaction(move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
                row.status = status;
                row.extra.insert("inFlight".into(), json!(in_flight));
                AgentRepository::put(tx, &row)
            })
            .await
            .expect("busy");
        assert_eq!(request(&store).await["members"]["lead"]["phase"], "waiting");
        let before = operation(&store);
        let session = Arc::new(RefusingSession::default());
        assert!(
            preparation::prepare_native_thread(
                store.clone(),
                &resources(root.path(), session.clone()),
                "lead",
                "busy-thread",
                1,
            )
            .await
            .is_err()
        );
        assert_eq!(operation(&store), before);
        assert_eq!(agent(&store)["accountKey"], "source");
        assert!(session.0.lock().expect("calls").is_empty());
    }
}

#[tokio::test]
async fn waiting_member_advances_on_next_preparation_without_a_wake() {
    let (root, store) = fixture().await;
    store
        .transaction(|tx| {
            let mut row = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
            row.status = AgentStatus::Running;
            row.extra.insert("inFlight".into(), json!(true));
            AgentRepository::put(tx, &row)
        })
        .await
        .expect("busy");
    assert_eq!(request(&store).await["members"]["lead"]["phase"], "waiting");
    store
        .transaction(|tx| {
            let mut row = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
            row.status = AgentStatus::Queued;
            row.extra.insert("inFlight".into(), json!(false));
            AgentRepository::put(tx, &row)
        })
        .await
        .expect("turn ended");
    let session = Arc::new(RefusingSession::default());
    let _result = preparation::prepare_native_thread(
        store.clone(),
        &resources(root.path(), session.clone()),
        "lead",
        "next-thread",
        1,
    )
    .await;
    assert_eq!(agent(&store)["accountKey"], "target");
    assert_eq!(operation(&store)["members"]["lead"]["phase"], "blocked");
    assert_eq!(session.0.lock().expect("calls").len(), 1);
}

#[tokio::test]
async fn waiting_member_releases_in_the_terminal_transaction_and_replay_is_exact() {
    let (_root, store) = fixture().await;
    store.transaction(|tx| {
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('input','lead','user','go','pending',1,3)", [])?;
        Ok(())
    }).await.expect("input");
    let intent = scheduler::claim_intent(&store, "lead", 3, "attempt", 1.0)
        .await
        .expect("claim")
        .expect("intent");
    assert!(
        scheduler::mark_submitted_for_provider(&store, &intent, "ticket", 1, "connection")
            .await
            .expect("submit")
    );
    assert_eq!(
        scheduler::settle_started_provider(&store, &intent, "ticket", 1, "turn")
            .await
            .expect("start"),
        Some(false)
    );
    assert_eq!(request(&store).await["members"]["lead"]["phase"], "waiting");
    let params = json!({"threadId":"source-thread","turn":{"id":"turn","status":"completed"}});
    assert!(
        scheduler::settle_terminal_attempt(&store, &intent.identity(), &params)
            .await
            .expect("complete")
    );
    // The account worker stages completion policy in the terminal transaction.
    store
        .transaction_with(|tx| {
            studio_runtime_services::continuation_runtime::stage(
                tx,
                studio_runtime_services::continuation_runtime::Completion::TerminalTurn {
                    agent_id: "lead",
                    turn: &json!({"id":"turn","status":"completed"}),
                    known: true,
                    now: 11.0,
                    limits: None,
                },
            )
        })
        .await
        .expect("completion policy");
    assert_eq!(agent(&store)["accountKey"], "target");
    assert_eq!(operation(&store)["members"]["lead"]["phase"], "lazy");
    let saved_agent = agent(&store);
    let saved_operation = operation(&store);
    assert!(
        scheduler::settle_terminal_attempt(&store, &intent.identity(), &params)
            .await
            .expect("replay")
    );
    let replay = store
        .transaction_with(|tx| {
            studio_runtime_services::continuation_runtime::stage(
                tx,
                studio_runtime_services::continuation_runtime::Completion::TerminalTurn {
                    agent_id: "lead",
                    turn: &json!({"id":"turn","status":"completed"}),
                    known: true,
                    now: 12.0,
                    limits: None,
                },
            )
        })
        .await
        .expect("replay policy");
    assert!(!replay.changed());
    assert_eq!(agent(&store), saved_agent);
    assert_eq!(operation(&store), saved_operation);
}

#[tokio::test]
async fn cancel_restores_source_fields_with_null_and_absent_values_exactly() {
    let (_root, store) = fixture().await;
    let before = agent(&store);
    assert_eq!(request(&store).await["members"]["lead"]["phase"], "lazy");
    store
        .transaction(|tx| {
            let mut row = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
            row.status = AgentStatus::Failed;
            row.extra
                .insert("error".into(), json!("Transfer history is unavailable"));
            row.extra.insert("nativeFailureHold".into(), json!(true));
            AgentRepository::put(tx, &row)
        })
        .await
        .expect("failed preparation");
    driver::action(&store, OP, Action::Cancel, 11.0)
        .await
        .expect("cancel");
    let mut after = agent(&store);
    // The completed transfer card is the only retained change.
    after
        .as_object_mut()
        .expect("agent object")
        .remove("accountTransfer");
    assert_eq!(after, before);
}
