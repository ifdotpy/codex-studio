//! A claimed managed tool request always ends terminal with `finished` set:
//! proven pre-write refusals as not applied, possible effects as unknown, and
//! a replay with the same identity returns the settled result without dispatch.

use super::*;
use futures_util::future::BoxFuture;

#[derive(Clone, Copy)]
enum SpawnScript {
    /// Refuse before any write.
    Refuse,
    /// Refuse before any write because no fresh model catalog was readable.
    CatalogUnavailable,
    /// Commit the first child row, then fail (image provisioning shape).
    FailAfterChild,
    /// Report a writer failure that may hide a commit.
    StoreFailure,
    /// Never finish, so the callback future can be dropped mid-dispatch.
    Hang,
    /// Wait for `release`, then report a written child with a wake signal.
    Release,
}

struct ScriptedChildCreation {
    script: SpawnScript,
    calls: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ScriptedChildCreation {
    fn new(script: SpawnScript) -> Arc<Self> {
        Arc::new(Self {
            script,
            calls: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
}

impl crate::provider_callbacks::RuntimeChildCreationPort for ScriptedChildCreation {
    fn spawn<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        _: &'a Value,
        request_id: &'a str,
        _: f64,
    ) -> BoxFuture<'a, Result<OperationResult, Error>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            match self.script {
                SpawnScript::Refuse => Err(Error::Invalid("Supply 1 to 64 agents")),
                SpawnScript::CatalogUnavailable => Err(Error::Unavailable(
                    crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
                )),
                SpawnScript::StoreFailure => Err(Error::Store(StoreError::InvalidStoredObject {
                    table: "runtime_agents",
                    id: "writer".into(),
                })),
                SpawnScript::FailAfterChild => {
                    let mut child = actor.record().value();
                    child["id"] = json!(crate::managed_child_create::child_id(request_id, 0));
                    let child = studio_runtime_store::AgentRecord::from_value(child).unwrap();
                    store
                        .transaction_with::<_, StoreError>(move |tx| {
                            studio_runtime_store::AgentRepository::put(tx, &child)
                        })
                        .await?;
                    Err(Error::Unavailable("workspace-state/image-provisioning"))
                }
                SpawnScript::Hang => std::future::pending::<Result<OperationResult, Error>>().await,
                SpawnScript::Release => {
                    self.release.notified().await;
                    Ok(OperationResult {
                        value: json!({"requestId":request_id,"agents":[]}),
                        signals: vec![crate::managed_host_tools::WakeSignal::new("account-worker")],
                    })
                }
            }
        })
    }

    fn review<'a>(
        &'a self,
        _: &'a RuntimeStore,
        _: &'a Actor,
        _: &'a Value,
        _: &'a str,
        _: f64,
    ) -> BoxFuture<'a, Result<OperationResult, Error>> {
        Box::pin(async { Err(Error::Invalid("Native review requires a Codex agent")) })
    }
}

struct UnusedInterrupt;

impl crate::provider_callbacks::RuntimeInterruptPort for UnusedInterrupt {
    fn stop_by_agent<'a>(
        &'a self,
        _: Arc<RuntimeStore>,
        _: &'a Actor,
        _: &'a str,
        _: &'a str,
        _: f64,
    ) -> BoxFuture<'a, Result<crate::managed_host_tools::DispatchedOperation, Error>> {
        unreachable!("a refused interrupt never reaches the lifecycle port")
    }
}

async fn ports_for(
    store: &Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &Frame,
    creation: &Arc<ScriptedChildCreation>,
) -> Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>> {
    ports_with_wake(
        store,
        context,
        frame,
        creation,
        Arc::new(tokio::sync::Notify::new()),
    )
    .await
}

async fn ports_with_wake(
    store: &Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &Frame,
    creation: &Arc<ScriptedChildCreation>,
    wake: Arc<tokio::sync::Notify>,
) -> Arc<dyn crate::managed_host_tools::Ports<Prepared = ()>> {
    let creation: Arc<dyn crate::provider_callbacks::RuntimeChildCreationPort> = creation.clone();
    shared_ports(
        crate::provider_callbacks::managed_tool_ports_with_runtime(
            store.as_ref(),
            context,
            frame,
            ManagedToolPortsConfig {
                wake,
                monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
                runtime_ports: ManagedToolRuntimePorts {
                    status_watch: None,
                    child_creation: Some(creation),
                    monitor_create: None,
                    interrupt: Some(Arc::new(UnusedInterrupt)),
                    monitor_input: None,
                    monitor_cancel: None,
                    work_action: None,
                    late: Default::default(),
                },
                preparation: None,
            },
        )
        .await
        .unwrap(),
    )
}

fn request_row(store: &RuntimeStore, call_id: &str) -> Value {
    studio_runtime_store::system_reads::all(&store.test_reader().unwrap(), "runtime_tool_requests")
        .unwrap()
        .into_iter()
        .find(|row| row["callId"] == call_id)
        .unwrap()
}

fn saved_result(store: &RuntimeStore, id: &str) -> Option<Value> {
    studio_runtime_store::system_reads::get(
        &store.test_reader().unwrap(),
        "runtime_tool_results",
        id,
    )
    .unwrap()
}

fn assert_terminal(store: &RuntimeStore, call_id: &str, outcome: &str, certainty: Option<&str>) {
    let row = request_row(store, call_id);
    assert_eq!(row["stage"], "failed", "{call_id}: {row}");
    assert_eq!(row["outcome"], outcome, "{call_id}: {row}");
    assert!(row["finished"].is_number(), "{call_id}: {row}");
    assert_eq!(row["result"]["success"], false, "{call_id}: {row}");
    if let Some(certainty) = certainty {
        assert_eq!(row["deliveryCertainty"], certainty, "{call_id}: {row}");
    }
    assert_eq!(
        saved_result(store, row["id"].as_str().unwrap()).as_ref(),
        Some(&row["result"]),
        "{call_id}: a replay answers from the saved result"
    );
}

async fn replay_returns_saved_result(
    store: &Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &Frame,
    creation: &Arc<ScriptedChildCreation>,
    call_id: &str,
) {
    let calls = creation.calls.load(Ordering::SeqCst);
    let before = request_row(store, call_id);
    let replay = dispatch_with_managed_tools(
        Arc::clone(store),
        context,
        frame,
        40.0,
        ports_for(store, context, frame, creation).await,
    )
    .await
    .unwrap();
    let CallbackDecision::Answer(reply) = replay else {
        panic!("{call_id}: replay answers")
    };
    let ResponseBody::Result(body) = reply.body else {
        panic!("{call_id}: replay is a tool result")
    };
    assert_eq!(body["success"], false, "{call_id}");
    assert_eq!(
        creation.calls.load(Ordering::SeqCst),
        calls,
        "{call_id}: replay never dispatches again"
    );
    assert_eq!(request_row(store, call_id), before, "{call_id}");
}

#[tokio::test]
async fn proven_pre_write_refusals_settle_not_applied() {
    let (_root, store, context) = fixture().await;
    let creation = ScriptedChildCreation::new(SpawnScript::Refuse);
    for (call_id, tool, args) in [
        (
            "settle-spawn-refused",
            "orchestration_spawn",
            json!({"agents":[]}),
        ),
        (
            "settle-review-refused",
            "orchestration_review",
            json!({"target":{"type":"uncommittedChanges"}}),
        ),
        (
            "settle-interrupt-refused",
            "orchestration_interrupt",
            json!({}),
        ),
    ] {
        let frame = call(&context, call_id, tool, args);
        let decision = dispatch_with_managed_tools(
            Arc::clone(&store),
            &context,
            &frame,
            20.0,
            ports_for(&store, &context, &frame, &creation).await,
        )
        .await
        .unwrap();
        assert!(matches!(decision, CallbackDecision::Answer(_)), "{call_id}");
        assert_terminal(&store, call_id, "not_applied", Some("not_sent"));
        replay_returns_saved_result(&store, &context, &frame, &creation, call_id).await;
    }
}

#[tokio::test]
async fn transient_catalog_refusal_settles_not_applied_with_retry_guidance() {
    let (_root, store, context) = fixture().await;
    let creation = ScriptedChildCreation::new(SpawnScript::CatalogUnavailable);
    let call_id = "settle-spawn-catalog";
    let frame = call(
        &context,
        call_id,
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}],"request_id":"catalog-try-1"}),
    );
    let decision = dispatch_with_managed_tools(
        Arc::clone(&store),
        &context,
        &frame,
        20.0,
        ports_for(&store, &context, &frame, &creation).await,
    )
    .await
    .unwrap();
    assert!(matches!(decision, CallbackDecision::Answer(_)));
    assert_terminal(&store, call_id, "not_applied", Some("not_sent"));
    let row = request_row(&store, call_id);
    let text = row["result"]["contentItems"][0]["text"]
        .as_str()
        .expect("refusal text");
    assert!(
        text.contains("temporarily unavailable") && text.contains("new request_id"),
        "{text}"
    );
    replay_returns_saved_result(&store, &context, &frame, &creation, call_id).await;
}

#[tokio::test]
async fn possible_effects_settle_unknown_and_never_redispatch() {
    for (call_id, script) in [
        ("settle-spawn-after-child", SpawnScript::FailAfterChild),
        ("settle-spawn-store-failure", SpawnScript::StoreFailure),
    ] {
        let (_root, store, context) = fixture().await;
        let creation = ScriptedChildCreation::new(script);
        let frame = call(
            &context,
            call_id,
            "orchestration_spawn",
            json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
        );
        let first = dispatch_with_managed_tools(
            Arc::clone(&store),
            &context,
            &frame,
            20.0,
            ports_for(&store, &context, &frame, &creation).await,
        )
        .await;
        assert!(first.is_err(), "{call_id}: the caller still sees the error");
        assert_terminal(&store, call_id, "unknown", Some("unknown"));
        replay_returns_saved_result(&store, &context, &frame, &creation, call_id).await;
    }
}

#[tokio::test]
async fn rolled_back_local_execution_settles_not_applied() {
    let (_root, store, context) = fixture().await;
    let agent_id = context.agent.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute(
                "INSERT INTO runtime_work(id,record) VALUES('settle-corrupt-work',?1)",
                [
                    json!({"id":"settle-corrupt-work","rootId":agent_id,"title":17,"status":"ready"})
                        .to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let frame = call(
        &context,
        "settle-local-refused",
        "orchestration_search",
        json!({"query":"fixture"}),
    );
    let ports = shared_ports(
        crate::provider_callbacks::managed_tool_ports(
            &store,
            &context,
            &frame,
            Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .unwrap(),
    );
    let first =
        dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports.clone())
            .await;
    assert!(
        first.is_err(),
        "the caller still sees the stored-object error"
    );
    assert_terminal(
        &store,
        "settle-local-refused",
        "not_applied",
        Some("not_sent"),
    );
    let replay = dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 21.0, ports)
        .await
        .unwrap();
    assert!(matches!(replay, CallbackDecision::Answer(_)));
}

#[tokio::test]
async fn dropped_dispatch_settles_unknown() {
    let (_root, store, context) = fixture().await;
    let creation = ScriptedChildCreation::new(SpawnScript::Hang);
    let call_id = "settle-spawn-dropped";
    let frame = call(
        &context,
        call_id,
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
    );
    let ports = ports_for(&store, &context, &frame, &creation).await;
    let entered = creation.entered.notified();
    let task = tokio::spawn({
        let store = Arc::clone(&store);
        let context = context.clone();
        let frame = frame.clone();
        async move { dispatch_with_managed_tools(store, &context, &frame, 20.0, ports).await }
    });
    entered.await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // The guard settles on the writer; wait for that commit, bounded.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while request_row(&store, call_id)["stage"] == "running" {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_terminal(&store, call_id, "unknown", Some("unknown"));
    replay_returns_saved_result(&store, &context, &frame, &creation, call_id).await;
}

#[tokio::test]
async fn startup_recovery_settles_stuck_rows_and_unblocks_context_repair() {
    let (root, store, context) = fixture().await;
    let creation = ScriptedChildCreation::new(SpawnScript::Hang);
    let call_id = "settle-startup-stuck";
    let frame = call(
        &context,
        call_id,
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
    );
    // Seed the live symptom: a claimed spawn whose process died mid-dispatch
    // (no runtime to run the guard), plus a reserved row never claimed.
    let running = {
        let ports = ports_for(&store, &context, &frame, &creation).await;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let creation_for_dispatch = Arc::clone(&creation);
        let store_for_dispatch = Arc::clone(&store);
        let context_for_dispatch = context.clone();
        let frame_for_dispatch = frame.clone();
        std::thread::spawn(move || {
            runtime.block_on(async {
                let entered = creation_for_dispatch.entered.notified();
                let dispatch = dispatch_with_managed_tools(
                    store_for_dispatch,
                    &context_for_dispatch,
                    &frame_for_dispatch,
                    20.0,
                    ports,
                );
                tokio::select! {
                    _ = dispatch => unreachable!("the scripted spawn never finishes"),
                    () = entered => {}
                }
                // The dropped dispatch can only queue its guard on this
                // runtime, which is never driven again: a dead process.
            });
            drop(runtime);
        })
        .join()
        .unwrap();
        request_row(&store, call_id)
    };
    assert_eq!(
        (running["stage"].as_str(), running["outcome"].as_str()),
        (Some("running"), Some("pending"))
    );
    let mut queued = running.clone();
    queued["id"] = json!(format!("{}-queued", running["id"].as_str().unwrap()));
    queued["callId"] = json!("settle-startup-queued");
    queued["stage"] = json!("queued");
    queued.as_object_mut().unwrap().remove("started");
    let queued_id = queued["id"].as_str().unwrap().to_owned();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            tx.execute(
                "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",
                params![queued_id, queued.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    drop(store);

    let restarted = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    assert_eq!(
        crate::provider_callbacks::recover_tool_requests_on_startup(&restarted, 30.0)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        crate::provider_callbacks::recover_tool_requests_on_startup(&restarted, 31.0)
            .await
            .unwrap(),
        0,
        "a second startup pass finds nothing"
    );
    let stuck = request_row(&restarted, call_id);
    assert_eq!(
        (stuck["stage"].as_str(), stuck["outcome"].as_str()),
        (Some("interrupted"), Some("unknown"))
    );
    assert_eq!(stuck["finished"], 30.0);
    assert_eq!(
        saved_result(&restarted, stuck["id"].as_str().unwrap()).as_ref(),
        Some(&stuck["result"])
    );
    let queued = request_row(&restarted, "settle-startup-queued");
    assert_eq!(
        (queued["stage"].as_str(), queued["outcome"].as_str()),
        (Some("cancelled"), Some("not_applied"))
    );
    assert_eq!(queued["finished"], 30.0);
    let blocking: i64 = restarted
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_tool_requests WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.stage') IN ('queued','running')",
            [&context.agent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(blocking, 0, "context repair no longer waits for these rows");
    replay_returns_saved_result(&restarted, &context, &frame, &creation, call_id).await;
    assert_eq!(creation.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn connection_recovery_leaves_an_in_flight_dispatch_to_record_its_real_outcome() {
    let (_root, store, context) = fixture().await;
    let creation = ScriptedChildCreation::new(SpawnScript::Release);
    let call_id = "settle-spawn-in-flight";
    let frame = call(
        &context,
        call_id,
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
    );
    let wake = Arc::new(tokio::sync::Notify::new());
    let ports = ports_with_wake(&store, &context, &frame, &creation, Arc::clone(&wake)).await;
    let entered = creation.entered.notified();
    let task = tokio::spawn({
        let store = Arc::clone(&store);
        let context = context.clone();
        let frame = frame.clone();
        async move { dispatch_with_managed_tools(store, &context, &frame, 20.0, ports).await }
    });
    entered.await;

    // The connection is lost while the claimed spawn is still running here.
    crate::provider_callbacks::recover_connection(
        &store,
        &context.account,
        &context.connection,
        25.0,
    )
    .await
    .unwrap();
    let running = request_row(&store, call_id);
    assert_eq!(
        (running["stage"].as_str(), running["outcome"].as_str()),
        (Some("running"), Some("pending")),
        "recovery leaves an in-process dispatch to its owner"
    );
    assert_eq!(saved_result(&store, running["id"].as_str().unwrap()), None);

    let woken = wake.notified();
    creation.release.notify_one();
    let decision = task.await.unwrap().unwrap();
    let CallbackDecision::Answer(reply) = decision else {
        panic!("the dispatch answers")
    };
    let ResponseBody::Result(body) = reply.body else {
        panic!("the dispatch answers with a tool result")
    };
    assert_eq!(body["success"], true, "the real outcome reaches the caller");
    tokio::time::timeout(std::time::Duration::from_secs(10), woken)
        .await
        .expect("the real outcome's wake signal is released");
    let settled = request_row(&store, call_id);
    assert_eq!(settled["result"]["success"], true, "{settled}");
    assert_eq!(settled["deliveryCertainty"], "written", "{settled}");
    assert_eq!(
        saved_result(&store, settled["id"].as_str().unwrap()).as_ref(),
        Some(&settled["result"])
    );

    // Once the owner is gone, the settled row is no longer recovery's work.
    crate::provider_callbacks::recover_connection(
        &store,
        &context.account,
        &context.connection,
        30.0,
    )
    .await
    .unwrap();
    assert_eq!(request_row(&store, call_id), settled);
    assert_eq!(creation.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn connection_recovery_settles_a_claim_without_an_in_process_owner() {
    let (_root, store, context) = fixture().await;
    let creation = ScriptedChildCreation::new(SpawnScript::Hang);
    let call_id = "settle-spawn-ownerless";
    let frame = call(
        &context,
        call_id,
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
    );
    // Seed a running claim whose dispatcher is gone without its drop guard.
    let ports = ports_for(&store, &context, &frame, &creation).await;
    let entered = creation.entered.notified();
    let dispatch = dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports);
    tokio::select! {
        _ = dispatch => unreachable!("the scripted spawn never finishes"),
        () = entered => {}
    }
    // Let the drop guard settle first, then reopen the claim as a dispatcher
    // that vanished without one (the shape left by a failed settlement).
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while request_row(&store, call_id)["stage"] == "running" {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let id = request_row(&store, call_id)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let row_id = id.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let mut row =
                studio_runtime_store::system_reads::get(tx, "runtime_tool_requests", &row_id)?
                    .unwrap();
            row["stage"] = json!("running");
            row["outcome"] = json!("pending");
            row.as_object_mut().unwrap().remove("finished");
            row.as_object_mut().unwrap().remove("result");
            row.as_object_mut().unwrap().remove("deliveryCertainty");
            tx.execute(
                "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                params![row.to_string(), row_id],
            )?;
            tx.execute("DELETE FROM runtime_tool_results WHERE id=?1", [&row_id])?;
            Ok(())
        })
        .await
        .unwrap();
    crate::provider_callbacks::recover_connection(
        &store,
        &context.account,
        &context.connection,
        25.0,
    )
    .await
    .unwrap();
    let stuck = request_row(&store, call_id);
    assert_eq!(
        (stuck["stage"].as_str(), stuck["outcome"].as_str()),
        (Some("interrupted"), Some("unknown"))
    );
    assert_eq!(saved_result(&store, &id).as_ref(), Some(&stuck["result"]));
}

#[tokio::test]
async fn failed_finalize_still_records_the_real_operation_and_wakes_its_owner() {
    let (_root, store, context) = fixture().await;
    let creation = ScriptedChildCreation::new(SpawnScript::Release);
    let call_id = "settle-spawn-finalize-failed";
    let frame = call(
        &context,
        call_id,
        "orchestration_spawn",
        json!({"agents":[{"name":"Worker","prompt":"Task"}]}),
    );
    let wake = Arc::new(tokio::sync::Notify::new());
    let ports = ports_with_wake(&store, &context, &frame, &creation, Arc::clone(&wake)).await;
    let entered = creation.entered.notified();
    let task = tokio::spawn({
        let store = Arc::clone(&store);
        let context = context.clone();
        let frame = frame.clone();
        async move { dispatch_with_managed_tools(store, &context, &frame, 20.0, ports).await }
    });
    entered.await;
    // A newer supervisor generation takes the row over mid-dispatch, so the
    // finalize transaction refuses this caller after the operation ran.
    let id = request_row(&store, call_id)["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let row_id = id.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let mut row =
                studio_runtime_store::system_reads::get(tx, "runtime_tool_requests", &row_id)?
                    .unwrap();
            row["supervisor"]["generation"] = json!(8);
            tx.execute(
                "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                params![row.to_string(), row_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let woken = wake.notified();
    creation.release.notify_one();
    assert!(
        task.await.unwrap().is_err(),
        "the caller sees the finalize refusal"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), woken)
        .await
        .expect("the recorded operation's wake signal is released");
    let settled = request_row(&store, call_id);
    assert_eq!(settled["result"]["success"], true, "{settled}");
    assert_eq!(settled["deliveryCertainty"], "written", "{settled}");
    assert!(settled["finished"].is_number(), "{settled}");
    assert_eq!(saved_result(&store, &id).as_ref(), Some(&settled["result"]));
    assert_eq!(creation.calls.load(Ordering::SeqCst), 1);
}
