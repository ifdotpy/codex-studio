//! Preparation refuses changed authority and holds input until native state settles.
use super::*;

async fn change_agent(state: &State, key: &'static str, value: Value) {
    let id = state.agent.clone();
    state
        .bridge
        .worker_run("preparation-review", move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                    row.extra.insert(key.into(), value);
                    AgentRepository::put(tx, &row)?;
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
}

pub(super) async fn scenario_completed_repair_and_missing_complaint_deliver() {
    let state = State::new().await;
    change_agent(&state, "contextRepair", json!({"phase":"completed"})).await;
    state.bridge.run(|store| async move { store.transaction_with(move |tx| {
        tx.execute("UPDATE runtime_events SET kind='complaint',text=?1 WHERE id='annotation:input'",
            [json!({"complaints":[{"complaint_id":"missing","text":"important evidence"}]}).to_string()])?;
        Ok::<_, WorkerError>(())
    }).await }).await.unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let turn = state.start(&mut peer, &worker).await;
    assert!(
        turn["params"]["input"]
            .to_string()
            .contains("important evidence")
    );
    peer.accepted(&turn).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_repair_and_missing_complaint_deliver() {
    scenario_completed_repair_and_missing_complaint_deliver().await;
}

#[tokio::test]
async fn compiler_dependencies_and_late_holds_are_checked_before_commit() {
    for mutation in [
        "owner_deleted",
        "owner_moved",
        "asset_path",
        "mode_revision",
        "plan",
        "complaint",
        "nativeThreadBlock",
        "contextRepairWait",
        "contextRepair",
        "accountTransferId",
    ] {
        let state = State::new().await;
        let id = state.agent.clone();
        let epoch = state.epoch;
        state.bridge.run(move |store| async move {
            let setup_id = id.clone();
            store.transaction_with(move |tx| {
                let owner = AgentRepository::get(tx, &AgentId::from(setup_id.as_str()))?.unwrap().value();
                let mut owner = owner.clone();
                owner["id"] = json!("asset-owner");
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES('asset-owner',?1)", [owner.to_string()])?;
                tx.execute("INSERT INTO runtime_assets(id,record) VALUES('asset',?1)",
                    [json!({"id":"asset","agentId":"asset-owner","image":false,"path":"/attachment","name":"evidence.txt","size":4}).to_string()])?;
                tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES('annotation:input',?1) ON CONFLICT(id) DO UPDATE SET record=excluded.record", [json!({"assets":["asset"]}).to_string()])?;
                Ok::<(), WorkerError>(())
            })
            .await?;
            let intent = store::claim(Arc::clone(&store), "default", &id, epoch).await?.unwrap();
            let prepared = preparation::prepare(
                Arc::clone(&store),
                &test_resources()?,
                &intent,
                Method::TurnStart,
                1,
            )
            .await?;
            let mutation_id = id.clone();
            let mutation_name = mutation;
            let prepared_thread_id = intent.thread_id.clone();
            store.transaction_with(move |tx| {
                match mutation_name {
                    "owner_deleted" => { tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id='asset-owner'", [])?; }
                    "owner_moved" => { tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','foreign') WHERE id='asset-owner'", [])?; }
                    "asset_path" => { tx.execute("UPDATE runtime_assets SET record=json_set(record,'$.path','/changed') WHERE id='asset'", [])?; }
                    "plan" => { tx.execute("INSERT INTO runtime_plans(id,record) VALUES(?1,?2)", rusqlite::params![mutation_id,json!({"id":mutation_id,"rootId":mutation_id,"native":{"plan":[{"step":"new plan", "status":"pending"}]}}).to_string()])?; }
                    "complaint" => { tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('new',?1)", [json!({"id":"new","leadId":mutation_id,"recipient":"lead","status":"open","responses":[],"author":mutation_id,"text":"new question","created":1,"version":1}).to_string()])?; }
                    _ => {
                        let mut row = AgentRepository::get(tx, &AgentId::from(mutation_id.as_str()))?.unwrap();
                        let (key, value) = match mutation_name {
                            "mode_revision" => ("agentModeRevision",json!(999)),
                            "nativeThreadBlock" => (mutation_name,json!({"threadId":prepared_thread_id,"error":{}})),
                            "contextRepairWait" => (mutation_name,json!({"error":"wait"})),
                            "contextRepair" => (mutation_name,json!({"phase":"unknown"})),
                            _ => (mutation_name,json!("transfer")),
                        };
                        row.extra.insert(key.into(),value);
                        AgentRepository::put(tx,&row)?;
                    }
                }
                Ok::<(), WorkerError>(())
            })
            .await?;
            let pending = Pending {intent:prepared.intent,method:"turn/start".into(),operation:"test".into(),thread:prepared.thread,compiled:prepared.compiled};
            let error = scheduler::lifecycle_transaction_with(store.as_ref(), move |tx| preparation::commit(tx,&pending,&prepared.guard)).await.unwrap_err();
            assert!(matches!(error, WorkerError::PreparationRefused(_)), "{mutation}: {error}");
            let db = store.test_reader()?;
            let row=AgentRepository::get(&db,&AgentId::from(id.as_str()))?.unwrap().value();
            assert_eq!(row["startAttempt"]["submitted"],false);
            assert_ne!(row["startAttempt"]["settingsFixed"],true);
            let count:i64=db.query_row("SELECT count(*) FROM runtime_items WHERE json_extract(record,'$.role')='user'",[],|r|r.get(0))?;
            assert_eq!(count,0,"{mutation}: no transcript before commit");
            Ok::<_,WorkerError>(())
        }).await.unwrap_or_else(|e|panic!("{mutation}: {e}"));
    }
}

#[tokio::test]
async fn context_repair_holds_and_complaint_ownership_preserve_input() {
    for phase in [
        "completed",
        "unchanged",
        "preparing",
        "submitted",
        "unknown",
        "ready",
    ] {
        let state = State::new().await;
        change_agent(&state, "contextRepair", json!({"phase":phase})).await;
        let id = state.agent.clone();
        let epoch = state.epoch;
        state
            .bridge
            .run(move |store| async move {
                let intent = store::claim(Arc::clone(&store), "default", &id, epoch).await?;
                let result = match intent {
                    Some(intent) => Some(
                        preparation::prepare(
                            Arc::clone(&store),
                            &test_resources().unwrap(),
                            &intent,
                            Method::TurnStart,
                            1,
                        )
                        .await,
                    ),
                    None => None,
                };
                assert_eq!(
                    result.is_some_and(|r| r.is_ok()),
                    matches!(phase, "completed" | "unchanged"),
                    "{phase}"
                );
                Ok::<(), WorkerError>(())
            })
            .await
            .unwrap();
    }
    for (payload, foreign, retained) in [
        ("malformed", false, true),
        ("missing", false, true),
        ("owned", false, false),
        ("owned", true, true),
    ] {
        let state = State::new().await;
        let id = state.agent.clone();
        let epoch = state.epoch;
        state.bridge.run(move |store| async move {
            let write_id = id.clone();
            let write_payload = payload;
            store.transaction_with(move |tx| {
                let body=if write_payload=="malformed" {"important evidence".into()} else {json!({"complaints":[{"complaint_id":write_payload,"text":"important evidence"}]}).to_string()};
                tx.execute("UPDATE runtime_events SET kind='complaint',text=?1 WHERE id='annotation:input'",[body])?;
                if write_payload=="owned" {tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('owned',?1)",[json!({"id":"owned","leadId":if foreign {"other"}else {&write_id},"recipient":"lead","status":"open","responses":[],"author":write_id,"text":"owned body","created":1,"version":1}).to_string()])?;}
                Ok::<(), WorkerError>(())
            }).await?;
            let intent=store::claim(Arc::clone(&store),"default",&id,epoch).await?.unwrap();
            let prepared=preparation::prepare(Arc::clone(&store),&test_resources()?,&intent,Method::TurnStart,1).await?;
            assert_eq!(prepared.params["input"].to_string().contains("important evidence"),retained,"{payload}/{foreign}");
            Ok(())
        }).await.unwrap();
    }
}

/// A release inspection that found native work (`blocked`) never unsubscribed,
/// so the thread stayed loaded. Such an agent prepares normally; only a
/// pending tool reset or an unsettled release on the current thread and
/// connection keeps the input queued.
#[tokio::test]
async fn native_release_phases_hold_only_unsettled_releases() {
    const GENERATION: u64 = 1;
    for (name, connection, held) in [
        ("blocked", "earlier-connection", false),
        ("unknown", "1", true),
    ] {
        let state = State::new().await;
        change_agent(
            &state,
            "nativeRelease",
            json!({
                "phase":name, "threadId":"fake-thread-0", "connectionId":connection
            }),
        )
        .await;
        let id = state.agent.clone();
        let epoch = state.epoch;
        let result = state
            .bridge
            .run(move |store| async move {
                let intent = store::claim(Arc::clone(&store), "default", &id, epoch)
                    .await?
                    .unwrap();
                Ok::<_, WorkerError>(
                    preparation::prepare_turn_with_connection_id(
                        Arc::clone(&store),
                        &test_resources()?,
                        &intent,
                        GENERATION,
                        "transport-connection",
                    )
                    .await
                    .map(|_| ()),
                )
            })
            .await
            .unwrap();
        match result {
            Ok(()) => assert!(!held, "{name}: expected a hold, Rust prepared"),
            Err(WorkerError::PreparationRefused(reason))
                if reason == "thread-preparation/NativeRelease" =>
            {
                assert!(held, "{name}: expected preparation, Rust held")
            }
            Err(error) => panic!("{name}: {error}"),
        }
    }
}

pub(super) async fn scenario_late_context_change_refuses_submission() {
    let state = State::new().await;
    let id = state.agent.clone();
    let epoch = state.epoch;
    let prepared = state
        .bridge
        .worker_run("preparation-review", move |store| async move {
            let intent = store::claim(Arc::clone(&store), "default", &id, epoch)
                .await?
                .unwrap();
            preparation::prepare(
                Arc::clone(&store),
                &test_resources()?,
                &intent,
                Method::TurnStart,
                1,
            )
            .await
        })
        .await
        .unwrap();
    change_agent(&state, "agentModeRevision", json!(444)).await;
    Box::pin(
        state
            .bridge
            .worker_run("preparation-review", move |store| async move {
                let operation =
                    format!("account:default:{}:turn/start", prepared.intent.attempt_id);
                let request = RecoveredRequest {
                    ticket: studio_runtime_providers::RequestTicket {
                        id: RequestId::String(operation.clone()),
                        generation: 1,
                        method: Method::TurnStart,
                    },
                    operation_id: OperationId(operation.clone()),
                    thread_id: prepared.intent.thread_id.clone(),
                    lightweight: false,
                    deadline_ms: 60_000,
                    unknown: false,
                    remote_id: None,
                };
                let context = Pending {
                    intent: prepared.intent,
                    method: "turn/start".into(),
                    operation,
                    thread: prepared.thread,
                    compiled: prepared.compiled,
                };
                assert!(
                    !store::submitted(
                        store.as_ref(),
                        context,
                        "default",
                        request,
                        store::SubmissionEvidence {
                            turn: None,
                            submission: prepared.submission,
                            guard: Some(prepared.guard),
                            connection_id: "test-connection".into(),
                            effect: None,
                        },
                    )
                    .await?
                );
                Ok(())
            }),
    )
    .await
    .unwrap();
    // Nothing was sent: the input returns to the queue, and the agent is
    // queued to prepare its turn again from the current state.
    assert_eq!(state.event_status("input").await, "pending");
    let record = state.record().await;
    assert_eq!(record["status"], "queued", "{record}");
    assert!(record["error"].is_null(), "{record}");
    assert!(record.get("startAttempt").is_none(), "{record}");
}
#[tokio::test]
async fn late_context_change_refuses_submission() {
    Box::pin(scenario_late_context_change_refuses_submission()).await;
}

#[tokio::test]
async fn missing_native_effort_uses_shared_catalog_published_after_construction() {
    let state = State::new().await;
    let service = crate::model_catalog::ModelCatalogService::new();
    let resources =
        preparation::PreparationResources::bundled(String::new(), service.clone()).unwrap();
    crate::model_catalog::tests::publish_fixture(
        &service,
        "default",
        "",
        json!({"data":[{
            "model":"fake-model", "defaultReasoningEffort":"high",
            "supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"}]
        }]}),
    );
    let id = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            let update_id = id.clone();
            store
                .transaction_with(move |tx| {
                    let mut agent =
                        AgentRepository::get(tx, &AgentId::from(update_id.as_str()))?.unwrap();
                    agent.extra.remove("nativeEffort");
                    AgentRepository::put(tx, &agent)
                })
                .await?;
            let intent = store::claim(Arc::clone(&store), "default", &id, epoch)
                .await?
                .unwrap();
            let prepared = preparation::prepare(
                Arc::clone(&store),
                &resources,
                &intent,
                Method::TurnStart,
                1,
            )
            .await?;
            assert_eq!(prepared.params["effort"], "medium");
            Ok(())
        })
        .await
        .unwrap();
    service.shutdown().await.unwrap();
}

pub(super) async fn scenario_sibling_completion_does_not_refuse_non_lead() {
    let state = State::new().await;
    let id = state.agent.clone();
    let epoch = state.epoch;
    let prepared = state
        .bridge
        .worker_run("preparation-review", move |store| async move {
            let record_id = id.clone();
            store
                .transaction_with(move |tx| {
                    let original = AgentRepository::get(tx, &AgentId::from(record_id.as_str()))?
                        .unwrap()
                        .value();
                    for (key, lead) in [
                        ("root", true),
                        ("sibling", false),
                        (record_id.as_str(), false),
                    ] {
                        let mut record = original.clone();
                        record["id"] = json!(key);
                        record["rootId"] = json!("root");
                        record["isLead"] = json!(lead);
                        record["parentId"] = if lead { Value::Null } else { json!("root") };
                        record["role"] = json!(if lead { "orchestrator" } else { "implementer" });
                        record["worktreeReady"] = json!(true);
                        let record = serde_json::from_value(record).unwrap();
                        AgentRepository::put(tx, &record)?;
                    }
                    Ok::<(), WorkerError>(())
                })
                .await?;
            let intent = store::claim(Arc::clone(&store), "default", &id, epoch)
                .await?
                .unwrap();
            preparation::prepare(
                Arc::clone(&store),
                &test_resources()?,
                &intent,
                Method::TurnStart,
                1,
            )
            .await
        })
        .await
        .unwrap();
    state
        .bridge
        .worker_run("preparation-review", |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut sibling = AgentRepository::get(tx, &AgentId::from("sibling"))?.unwrap();
                    sibling.status = studio_runtime_store::AgentStatus::Completed;
                    AgentRepository::put(tx, &sibling)?;
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
    Box::pin(
        state
            .bridge
            .worker_run("preparation-review", move |store| async move {
                let operation =
                    format!("account:default:{}:turn/start", prepared.intent.attempt_id);
                let request = RecoveredRequest {
                    ticket: studio_runtime_providers::RequestTicket {
                        id: RequestId::String(operation.clone()),
                        generation: 1,
                        method: Method::TurnStart,
                    },
                    operation_id: OperationId(operation.clone()),
                    thread_id: prepared.intent.thread_id.clone(),
                    lightweight: false,
                    deadline_ms: 60_000,
                    unknown: false,
                    remote_id: None,
                };
                let context = Pending {
                    intent: prepared.intent,
                    method: "turn/start".into(),
                    operation,
                    thread: prepared.thread,
                    compiled: prepared.compiled,
                };
                assert!(
                    store::submitted(
                        store.as_ref(),
                        context,
                        "default",
                        request,
                        store::SubmissionEvidence {
                            turn: None,
                            submission: prepared.submission,
                            guard: Some(prepared.guard),
                            connection_id: "test-connection".into(),
                            effect: None,
                        },
                    )
                    .await?
                );
                Ok(())
            }),
    )
    .await
    .unwrap();
    assert_eq!(state.event_status("input").await, "dispatching");
    assert_eq!(state.record().await["startAttempt"]["submitted"], true);
}
#[tokio::test]
async fn sibling_completion_does_not_refuse_non_lead() {
    Box::pin(scenario_sibling_completion_does_not_refuse_non_lead()).await;
}

#[tokio::test]
async fn cold_and_expired_catalogs_hold_the_attempt_without_failing_it() {
    for expired in [false, true] {
        let state = State::new().await;
        let id = state.agent.clone();
        state
            .bridge
            .run(move |store| async move {
                store
                    .transaction_with(move |tx| {
                        let mut row =
                            AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                        row.extra.remove("nativeEffort");
                        AgentRepository::put(tx, &row)?;
                        Ok::<_, WorkerError>(())
                    })
                    .await
            })
            .await
            .unwrap();
        let catalog = crate::model_catalog::ModelCatalogService::new();
        let page = json!({"data":[{"model":"fake-model","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}]});
        if expired {
            crate::model_catalog::tests::publish_fixture(&catalog, "default", "", page.clone());
            crate::model_catalog::tests::expire_fixture(&catalog);
        }
        let resources =
            preparation::PreparationResources::bundled(String::new(), catalog.clone()).unwrap();
        let mut peer = Peer::new();
        let mut worker = peer
            .open_worker_with_resources(state.bridge.clone(), resources.clone())
            .await;
        let mut ready = peer.ready.clone();
        while !*ready.borrow_and_update() {
            ready.changed().await.unwrap();
        }
        state.wake(&worker).await;
        worker_barrier(&worker).await;
        let held = state.record().await;
        assert_eq!(
            held["startAttempt"]["prepareError"],
            "model-catalog/refresh-pending"
        );
        assert_eq!(held["startAttempt"]["submitted"], false);
        assert_eq!(held["autoWake"], true);
        assert_ne!(held["status"], "failed");
        assert_eq!(state.event_status("input").await, "reserved");
        assert!(peer.writes.try_recv().is_err());
        // Refresh initiation and automatic wake are step 2. Prove that a fresh
        // observation can resume this exact held attempt without a new claim.
        crate::model_catalog::tests::publish_fixture(&catalog, "default", "", page);
        let id = state.agent.clone();
        let epoch = state.epoch;
        state
            .bridge
            .run(move |store| async move {
                let intent = {
                    let db = store.test_reader()?;
                    let read_tx = db.unchecked_transaction()?;
                    let event = studio_runtime_store::QueueEventRepository::get(
                        &db,
                        &studio_runtime_store::QueueEventId::from("annotation:input"),
                    )?
                    .unwrap();
                    let metadata =
                        studio_runtime_store::NotificationDispatchRepository::event_metadata(
                            &read_tx,
                            "annotation:input",
                        )?
                        .unwrap_or_else(|| json!({}));
                    TurnIntent {
                        agent_id: id,
                        epoch,
                        attempt_id: held["startAttempt"]["id"].as_str().unwrap().into(),
                        account_key: "default".into(),
                        provider: studio_runtime_providers::ProviderKind::Codex,
                        thread_id: held["threadId"].as_str().map(str::to_owned),
                        cwd: held["cwd"].as_str().map(str::to_owned),
                        model: held["model"].as_str().map(str::to_owned),
                        events: vec![crate::scheduler::TurnEvent {
                            id: event.id.0,
                            kind: event.kind,
                            text: event.text,
                            created: event.created,
                            epoch: event.epoch,
                            error: event.error,
                            metadata,
                        }],
                        radio_authorized: false,
                    }
                };
                assert!(matches!(
                    store::preparation_step(
                        &store,
                        &resources,
                        &intent,
                        preparation::admit(Arc::clone(&store), &resources, &intent)
                    )
                    .await?,
                    store::PreparationStep::Ready(_)
                ));
                Ok(())
            })
            .await
            .unwrap();
        worker.shutdown().await.unwrap();
        catalog.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn continuation_gate_refuses_a_preexisting_admission_hold() {
    let state = State::new().await;
    let id = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            let intent = store::claim(Arc::clone(&store), "default", &id, epoch)
                .await?
                .unwrap();
            let update_agent_id = id.clone();
            store
                .transaction_with(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(update_agent_id.as_str()))?
                            .unwrap();
                    row.extra
                        .insert("accountTransferId".into(), json!("transfer"));
                    AgentRepository::put(tx, &row)
                })
                .await?;
            let error = preparation::prepare(
                Arc::clone(&store),
                &test_resources()?,
                &intent,
                Method::TurnStart,
                1,
            )
            .await
            .err()
            .expect("turn admission hold");
            assert_eq!(error.to_string(), "thread-preparation/AccountTransfer");
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn radio_authorized_preparation_uses_the_radio_receipt_gate() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            let mut intent = store::claim(Arc::clone(&store), "default", &agent_id, epoch)
                .await?
                .unwrap();
            store
                .transaction_with(move |tx| {
                    tx.execute(
                        "UPDATE runtime_events SET kind='radio_turn' WHERE id='annotation:input'",
                        [],
                    )?;
                    Ok::<(), WorkerError>(())
                })
                .await?;
            intent.events[0].kind = "radio_turn".into();
            intent.radio_authorized = true;
            let error = preparation::prepare(
                Arc::clone(&store),
                &test_resources()?,
                &intent,
                Method::TurnStart,
                1,
            )
            .await
            .err()
            .expect("radio turns require a current active room receipt");
            assert!(
                error.to_string().contains("radio/turn-selection-changed"),
                "radio-specific authorization should reject the missing receipt: {error}"
            );
            assert!(
                !error.to_string().contains("RadioUnsupported"),
                "radio-authorized turns must bypass the generic radio refusal"
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn bundled_delivery_tools_reach_current_actors_and_preserve_historical_catalogs() {
    let state = State::new().await;
    let resources = preparation::PreparationResources::bundled(
        String::new(),
        crate::model_catalog::ModelCatalogService::new(),
    )
    .unwrap();
    let mut actor = state.record().await;
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    for source in ["current", "legacy", "frozen"] {
                        actor["nativeToolSource"] = json!(if source == "legacy" {
                            "legacy"
                        } else {
                            "current"
                        });
                        if source == "frozen" {
                            actor["frozenNativeParams"] =
                                json!({"dynamicTools":resources.legacy_tools});
                        }
                        let definitions =
                            preparation::actor_tool_definitions(tx, &resources, &actor)?;
                        for name in ["orchestration_message", "orchestration_send"] {
                            let tool = definitions
                                .as_array()
                                .unwrap()
                                .iter()
                                .find(|tool| tool["name"] == name)
                                .expect("delivery tool is exposed to actor");
                            let moment = tool.pointer("/inputSchema/properties/delivery/enum");
                            if source == "current" {
                                assert_eq!(
                                    moment,
                                    Some(&json!([
                                        studio_runtime_schema::DeliveryMoment::CurrentTurn.as_str(),
                                        studio_runtime_schema::DeliveryMoment::NextTurn.as_str()
                                    ]))
                                );
                            } else if name == "orchestration_message" {
                                assert!(moment.is_none(), "historical catalog is immutable");
                            } else {
                                assert_eq!(
                                    moment,
                                    Some(&json!(["queue", "steer"])),
                                    "historical catalog is immutable"
                                );
                            }
                        }
                    }
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
}
