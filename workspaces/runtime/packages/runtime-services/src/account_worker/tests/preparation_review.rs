//! Review regressions exercise the production adapter against Python-created rows.
use super::*;

async fn change_agent(state: &State, key: &'static str, value: Value) {
    let id = state.agent.clone();
    state
        .bridge
        .worker_run("preparation-review", move |store| {
            store.transaction_with(|tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                row.extra.insert(key.into(), value);
                AgentRepository::put(tx, &row)?;
                Ok::<_, WorkerError>(())
            })
        })
        .await
        .unwrap();
}

pub(super) async fn scenario_completed_repair_and_missing_complaint_deliver() {
    let state = State::new().await;
    change_agent(&state, "contextRepair", json!({"phase":"completed"})).await;
    state.bridge.run(|store| store.transaction_with(|tx| {
        tx.execute("UPDATE runtime_events SET kind='complaint',text=?1 WHERE id='annotation:input'",
            [json!({"complaints":[{"complaint_id":"missing","text":"important evidence"}]}).to_string()])?;
        Ok::<_, WorkerError>(())
    })).await.unwrap();
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

#[tokio::test(start_paused = true)]
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
        state.bridge.run(move |store| {
            store.transaction(|tx| {
                let owner = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap().value();
                let mut owner = owner.clone();
                owner["id"] = json!("asset-owner");
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES('asset-owner',?1)", [owner.to_string()])?;
                tx.execute("INSERT INTO runtime_assets(id,record) VALUES('asset',?1)",
                    [json!({"id":"asset","agentId":"asset-owner","image":false,"path":"/attachment","name":"evidence.txt","size":4}).to_string()])?;
                tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES('annotation:input',?1) ON CONFLICT(id) DO UPDATE SET record=excluded.record", [json!({"assets":["asset"]}).to_string()])?;
                Ok(())
            })?;
            let intent = store::claim(store, "default", &id, epoch)?.unwrap();
            let prepared = preparation::prepare(store, &test_resources()?, &intent, Method::TurnStart, 1)?;
            store.transaction(|tx| {
                match mutation {
                    "owner_deleted" => { tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id='asset-owner'", [])?; }
                    "owner_moved" => { tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.rootId','foreign') WHERE id='asset-owner'", [])?; }
                    "asset_path" => { tx.execute("UPDATE runtime_assets SET record=json_set(record,'$.path','/changed') WHERE id='asset'", [])?; }
                    "plan" => { tx.execute("INSERT INTO runtime_plans(id,record) VALUES(?1,?2)", rusqlite::params![id,json!({"id":id,"rootId":id,"native":{"plan":[{"step":"new plan", "status":"pending"}]}}).to_string()])?; }
                    "complaint" => { tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('new',?1)", [json!({"id":"new","leadId":id,"recipient":"lead","status":"open","responses":[],"author":id,"text":"new question","created":1,"version":1}).to_string()])?; }
                    _ => {
                        let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                        let (key, value) = match mutation {
                            "mode_revision" => ("agentModeRevision",json!(999)),
                            "nativeThreadBlock" => (mutation,json!({"threadId":intent.thread_id,"error":{}})),
                            "contextRepairWait" => (mutation,json!({"error":"wait"})),
                            "contextRepair" => (mutation,json!({"phase":"unknown"})),
                            _ => (mutation,json!("transfer")),
                        };
                        row.extra.insert(key.into(),value);
                        AgentRepository::put(tx,&row)?;
                    }
                }
                Ok(())
            })?;
            let pending = Pending {intent:prepared.intent,method:"turn/start".into(),operation:"test".into(),thread:prepared.thread,compiled:prepared.compiled};
            let error = scheduler::lifecycle_transaction_with(store, |tx| preparation::commit(tx,&pending,&prepared.guard)).unwrap_err();
            assert!(matches!(error, WorkerError::PreparationRefused(_)), "{mutation}: {error}");
            let db = store.read()?;
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
async fn context_repair_phases_and_complaint_ownership_match_python() {
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
            .run(move |store| {
                let intent = store::claim(store, "default", &id, epoch)?;
                let result = intent.as_ref().map(|i| {
                    preparation::prepare(store, &test_resources().unwrap(), i, Method::TurnStart, 1)
                });
                assert_eq!(
                    result.is_some_and(|r| r.is_ok()),
                    matches!(phase, "completed" | "unchanged"),
                    "{phase}"
                );
                Ok(())
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
        state.bridge.run(move |store| {
            store.transaction(|tx| {
                let body=if payload=="malformed" {"important evidence".into()} else {json!({"complaints":[{"complaint_id":payload,"text":"important evidence"}]}).to_string()};
                tx.execute("UPDATE runtime_events SET kind='complaint',text=?1 WHERE id='annotation:input'",[body])?;
                if payload=="owned" {tx.execute("INSERT INTO runtime_complaints(id,record) VALUES('owned',?1)",[json!({"id":"owned","leadId":if foreign {"other"}else {&id},"recipient":"lead","status":"open","responses":[],"author":id,"text":"owned body","created":1,"version":1}).to_string()])?;}
                Ok(())
            })?;
            let intent=store::claim(store,"default",&id,epoch)?.unwrap();
            let prepared=preparation::prepare(store,&test_resources()?,&intent,Method::TurnStart,1)?;
            assert_eq!(prepared.params["input"].to_string().contains("important evidence"),retained,"{payload}/{foreign}");
            Ok(())
        }).await.unwrap();
    }
}

pub(super) async fn scenario_late_context_change_refuses_submission() {
    let state = State::new().await;
    let id = state.agent.clone();
    let epoch = state.epoch;
    let prepared = state
        .bridge
        .worker_run("preparation-review", move |store| {
            let intent = store::claim(store, "default", &id, epoch)?.unwrap();
            preparation::prepare(store, &test_resources()?, &intent, Method::TurnStart, 1)
        })
        .await
        .unwrap();
    change_agent(&state, "agentModeRevision", json!(444)).await;
    state
        .bridge
        .worker_run("preparation-review", move |store| {
            let operation = format!("account:default:{}:turn/start", prepared.intent.attempt_id);
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
            assert!(!store::submitted(
                store,
                &context,
                "default",
                &request,
                None,
                prepared.submission.as_ref(),
                Some(&prepared.guard)
            )?);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(state.event_status("input").await, "pending");
    assert!(
        state.record().await["error"]
            .as_str()
            .unwrap()
            .contains("snapshot-changed")
    );
}
#[tokio::test]
async fn late_context_change_refuses_submission() {
    scenario_late_context_change_refuses_submission().await;
}

#[tokio::test]
async fn missing_native_effort_uses_shared_catalog_published_after_construction() {
    let state = State::new().await;
    let service = crate::model_catalog::ModelCatalogService::new(None);
    let resources = preparation::PreparationResources::bundled(
        String::new(),
        service.clone(),
        test_resources().unwrap().python_executable,
    )
    .unwrap();
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
        .run(move |store| {
            store.transaction(|tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                agent.extra.remove("nativeEffort");
                AgentRepository::put(tx, &agent)
            })?;
            let intent = store::claim(store, "default", &id, epoch)?.unwrap();
            let prepared = preparation::prepare(store, &resources, &intent, Method::TurnStart, 1)?;
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
        .worker_run("preparation-review", move |store| {
            store.transaction(|tx| {
                let original = AgentRepository::get(tx, &AgentId::from(id.as_str()))?
                    .unwrap()
                    .value();
                for (key, lead) in [("root", true), ("sibling", false), (id.as_str(), false)] {
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
                Ok(())
            })?;
            let intent = store::claim(store, "default", &id, epoch)?.unwrap();
            preparation::prepare(store, &test_resources()?, &intent, Method::TurnStart, 1)
        })
        .await
        .unwrap();
    state
        .bridge
        .worker_run("preparation-review", |store| {
            store.transaction_with(|tx| {
                let mut sibling = AgentRepository::get(tx, &AgentId::from("sibling"))?.unwrap();
                sibling.status = studio_runtime_store::AgentStatus::Completed;
                AgentRepository::put(tx, &sibling)?;
                Ok::<_, WorkerError>(())
            })
        })
        .await
        .unwrap();
    state
        .bridge
        .worker_run("preparation-review", move |store| {
            let operation = format!("account:default:{}:turn/start", prepared.intent.attempt_id);
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
            assert!(store::submitted(
                store,
                &context,
                "default",
                &request,
                None,
                prepared.submission.as_ref(),
                Some(&prepared.guard)
            )?);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(state.event_status("input").await, "dispatching");
    assert_eq!(state.record().await["startAttempt"]["submitted"], true);
}
#[tokio::test]
async fn sibling_completion_does_not_refuse_non_lead() {
    scenario_sibling_completion_does_not_refuse_non_lead().await;
}

#[tokio::test(start_paused = true)]
async fn cold_and_expired_catalogs_hold_the_attempt_without_failing_it() {
    for expired in [false, true] {
        let state = State::new().await;
        let id = state.agent.clone();
        state
            .bridge
            .run(move |store| {
                store.transaction_with(|tx| {
                    let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                    row.extra.remove("nativeEffort");
                    AgentRepository::put(tx, &row)?;
                    Ok::<_, WorkerError>(())
                })
            })
            .await
            .unwrap();
        let catalog = crate::model_catalog::ModelCatalogService::new(None);
        let page = json!({"data":[{"model":"fake-model","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}]});
        if expired {
            crate::model_catalog::tests::publish_fixture(&catalog, "default", "", page.clone());
            crate::model_catalog::tests::expire_fixture(&catalog);
        }
        let resources = preparation::PreparationResources::bundled(
            String::new(),
            catalog.clone(),
            test_resources().unwrap().python_executable,
        )
        .unwrap();
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
            .run(move |store| {
                let db = store.read()?;
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
                let intent = TurnIntent {
                    agent_id: id,
                    epoch,
                    attempt_id: held["startAttempt"]["id"].as_str().unwrap().into(),
                    account_key: "default".into(),
                    provider: "codex".into(),
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
                };
                drop(read_tx);
                drop(db);
                assert!(
                    store::preparation_step(store, &intent, || preparation::admit(
                        store, &resources, &intent
                    ))?
                    .is_some()
                );
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
        .run(move |store| {
            let intent = store::claim(store, "default", &id, epoch)?.unwrap();
            store.transaction(|tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                row.extra
                    .insert("accountTransferId".into(), json!("transfer"));
                AgentRepository::put(tx, &row)
            })?;
            let error =
                preparation::prepare(store, &test_resources()?, &intent, Method::TurnStart, 1)
                    .err()
                    .expect("turn admission hold");
            assert_eq!(error.to_string(), "thread-preparation/AccountTransfer");
            Ok(())
        })
        .await
        .unwrap();
}
