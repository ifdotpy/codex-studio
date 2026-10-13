//! Closure regressions exercise the worker, including fresh store ownership.
use super::*;

pub(super) async fn pending_text_recovery(stopped: bool) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let cursor = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"pending-recovery","delta":"acknowledged full answer"}})).await;
    peer.acknowledged(cursor).await;
    state.bridge.run(|store| store.transaction_with(|tx| {
        tx.execute_batch("CREATE TRIGGER no_flush BEFORE INSERT ON runtime_items BEGIN SELECT RAISE(ABORT,'flush unavailable'); END;")?;
        Ok(())
    })).await.unwrap();
    if stopped {
        worker
            .send(AccountCommand::Stop {
                agent_id: state.agent.clone(),
            })
            .await
            .unwrap();
        let interrupt = peer.write("turn/interrupt").await;
        peer.reply(&interrupt, json!({})).await;
    }
    assert!(worker.shutdown().await.is_err());
    state
        .bridge
        .run(|store| {
            store.transaction_with(|tx| {
                tx.execute_batch("DROP TRIGGER no_flush")?;
                Ok(())
            })
        })
        .await
        .unwrap();
    // A fresh bridge performs process startup recovery, not just lease recovery.
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let peer = Peer::new();
    let mut replacement = peer.worker(bridge).await;
    replacement.shutdown().await.unwrap();
    let text = state.bridge.run(|store| {
        Ok(store.read()?.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE id LIKE '%:pending-recovery'", [], |row| row.get::<_, String>(0))?)
    }).await.unwrap();
    assert_eq!(text, "acknowledged full answer");
}

#[tokio::test(start_paused = true)]
async fn stopped_pending_text_survives_fresh_process_recovery() {
    pending_text_recovery(true).await;
}
#[tokio::test(start_paused = true)]
async fn pending_text_survives_fresh_process_recovery() {
    pending_text_recovery(false).await;
}

#[tokio::test(start_paused = true)]
async fn journal_error_does_not_delete_an_acknowledged_pending_batch() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let cursor = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"pending-error","delta":"must remain durable"}})).await;
    peer.acknowledged(cursor).await;
    state.bridge.run(|store| store.transaction_with(|tx| {
        tx.execute_batch("CREATE TRIGGER journal_error BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'account-worker:journal:%' BEGIN SELECT RAISE(ABORT,'journal unavailable'); END;")?;
        Ok(())
    })).await.unwrap();
    peer.frame(json!({"method":"test/barrier","params":{}}))
        .await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
            .await
            .unwrap()
            .is_err()
    );
    state
        .bridge
        .run(|store| {
            store.transaction_with(|tx| {
                tx.execute_batch("DROP TRIGGER journal_error")?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let peer = Peer::new();
    let mut replacement = peer.worker(bridge).await;
    replacement.shutdown().await.unwrap();
    let text = state.bridge.run(|store| {
        Ok(store.read()?.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE id LIKE '%:pending-error'", [], |row| row.get::<_, String>(0))?)
    }).await.unwrap();
    assert_eq!(text, "must remain durable");
}

pub(super) async fn scenario_retained_submit_and_settlement_keep_one_replay_identity() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("complete_effect".into());
    state.bridge.run(|store| store.transaction_with(|tx| {
        tx.execute_batch("CREATE TRIGGER retain_effect BEFORE DELETE ON runtime_operation_receipts WHEN OLD.id LIKE 'account-worker:effect:%' BEGIN SELECT RAISE(ABORT,'effect retirement unavailable'); END;")?;
        Ok(())
    })).await.unwrap();
    let start = state.start(&mut peer, &worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.bridge.faults.paused.notified(),
    )
    .await
    .unwrap();
    let accepted = json!({"id":start["id"],"result":{"turn":{"id":"turn"}}});
    peer.frame(accepted.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
            .await
            .unwrap()
            .is_err()
    );
    state
        .bridge
        .run(|store| {
            store.transaction_with(|tx| {
                tx.execute_batch("DROP TRIGGER retain_effect")?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    peer.cursor = 1;
    let mut replacement = peer.worker(state.bridge.clone()).await;
    let cursor = peer.frame(accepted).await;
    peer.acknowledged(cursor).await;
    worker_barrier(&replacement).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "retained Submit must not re-admit accepted input"
    );
    let count = state.bridge.run(|store|Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:effect:%' OR id LIKE 'account-worker:account:%:submitted'",[],|row|row.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(count, 0, "settlement retires effect and guard together");
    replacement.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn elapsed_flush_deadline_wins_over_replenished_events() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let cursor=peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"timer","delta":"below threshold"}})).await;
    peer.acknowledged(cursor).await;
    let permit = state.bridge.slot.clone().acquire_owned().await.unwrap();
    // Keep the event lane ready while time passes. Its first DB job is blocked.
    for _ in 0..8 {
        peer.frame(json!({"method":"test/barrier","params":{}}))
            .await;
    }
    tokio::time::advance(STREAM_CADENCE).await;
    drop(permit);
    let mut count = 0;
    loop {
        peer.acknowledged(cursor + 1 + count).await;
        count += 1;
        let flushed = state
            .bridge
            .run(|store| {
                Ok(store.read()?.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_items WHERE id LIKE '%:timer')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        if flushed {
            break;
        }
        assert!(
            count <= 2,
            "ready event traffic postponed elapsed flush deadline"
        );
        peer.frame(json!({"method":"test/barrier","params":{}}))
            .await;
    }
    worker.shutdown().await.unwrap();
}

pub(super) async fn scenario_old_generation_pending_text_is_materialized_and_receipts_retire() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let cursor = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"generation","delta":"old generation text"}})).await;
    peer.acknowledged(cursor).await;
    worker.task.abort();
    assert!(worker.completion().await.is_err());
    let end = if MATRIX.try_with(|_| ()).is_ok() {
        3
    } else {
        18
    };
    for generation in 2..end {
        let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
        bridge.faults = state.bridge.faults.clone();
        let mut peer = Peer::identity("default", generation, None);
        let mut replacement = peer.worker(bridge).await;
        let marker = peer
            .frame(json!({"method":"test/barrier","params":{}}))
            .await;
        peer.acknowledged(marker).await;
        replacement.shutdown().await.unwrap();
        let (text, receipts) = state.bridge.run(|store| {
            let db=store.read()?;
            let text=db.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE id LIKE '%:generation'",[],|r|r.get::<_,String>(0))?;
            let receipts=db.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:%'",[],|r|r.get::<_,i64>(0))?;
            Ok((text,receipts))
        }).await.unwrap();
        assert_eq!(text, "old generation text");
        assert!(
            receipts <= 4,
            "generation churn retained {receipts} worker receipts"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn competing_workers_transfer_claim_to_destination_session() {
    let state = State::new().await;
    let mut old_peer = Peer::new();
    let mut old = old_peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("claim".into());
    state.wake(&old).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.bridge.faults.paused.notified(),
    )
    .await
    .unwrap();
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| {
                let mut record = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                record
                    .extra
                    .insert("accountKey".into(), json!("destination"));
                AgentRepository::put(tx, &record)?;
                Ok(())
            })
        })
        .await
        .unwrap();
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    let mut destination_peer = Peer::identity("destination", 1, None);
    let resources = test_resources().unwrap();
    crate::model_catalog::tests::publish_fixture(
        &resources.model_catalog,
        "destination",
        "",
        json!({"data":[{"model":"fake-model","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}]}),
    );
    let mut destination = destination_peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    state.bridge.faults.release.notify_one();
    let start = state.start(&mut destination_peer, &destination).await;
    assert!(
        start["id"]
            .as_str()
            .unwrap()
            .starts_with("account:destination:")
    );
    destination_peer.accepted(&start).await;
    worker_barrier(&old).await;
    assert!(old_peer.writes.try_recv().is_err());
    destination_peer.completed().await;
    old.shutdown().await.unwrap();
    destination.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_submit_and_settlement_keep_one_replay_identity() {
    scenario_retained_submit_and_settlement_keep_one_replay_identity().await;
}

#[tokio::test(start_paused = true)]
async fn old_generation_pending_text_is_materialized_and_receipts_retire() {
    scenario_old_generation_pending_text_is_materialized_and_receipts_retire().await;
}

#[tokio::test(start_paused = true)]
async fn written_callback_replay_compacts_its_journal_without_resending() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.named_target.lock().unwrap() = Some(("callback_written".into(), 0, true));
    let frame = json!({"id":"written-callback","method":"item/tool/call","params":{}});
    peer.frame(frame.clone()).await;
    let wire = peer.next_write().await;
    wire.receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
            .await
            .unwrap(),
        Err(WorkerError::TaskFailed)
    ));
    state.bridge.faults.arm(None);
    let mut peer = Peer::new();
    let mut replacement = peer.worker(state.bridge.clone()).await;
    let cursor = peer.frame(frame).await;
    peer.acknowledged(cursor).await;
    worker_barrier(&replacement).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "Written answer must not be sent twice"
    );
    let rows = state.bridge.run(|store| Ok(store.read()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:journal:%' OR id LIKE 'account-worker:callback:%'",[],|row|row.get::<_,i64>(0))?)).await.unwrap();
    assert_eq!(
        rows, 0,
        "Written replay advances and compacts the journal prefix"
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_publication_identity_refuses_before_entering_writer() {
    let state = State::new().await;
    state
        .bridge
        .run(|store| {
            let directory = store.root().join("multi-server");
            std::fs::create_dir_all(&directory).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            std::fs::write(directory.join("identity.json"), b"invalid identity").unwrap();
            let entered = std::cell::Cell::new(false);
            let result = scheduler::lifecycle_transaction(store, |_| {
                entered.set(true);
                Ok(())
            });
            assert!(result.is_err());
            assert!(
                !entered.get(),
                "identity must be resolved before the writer begins"
            );
            assert!(crate::notification_dispatch::PublicationContext::load(store.root()).is_err());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn filesystem_preparation_does_not_own_database_permit() {
    let state = State::new().await;
    let bridge = state.bridge.clone();
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let job = tokio::spawn(async move {
        bridge
            .prepare_run("blocked-filesystem", move |_| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            })
            .await
    });
    observed.await.unwrap();
    let access = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.bridge.run(|store| {
            Ok(store
                .read()?
                .query_row("SELECT COUNT(*) FROM runtime_agents", [], |row| {
                    row.get::<_, i64>(0)
                })?)
        }),
    )
    .await;
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    assert!(
        access
            .expect("another account can use the store during filesystem work")
            .unwrap()
            > 0
    );
}

pub(super) async fn scenario_preparation_drains_journal_and_stop() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("workspace".into());
    state.wake(&worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.bridge.faults.paused.notified(),
    )
    .await
    .unwrap();
    let marker = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    peer.acknowledged(marker).await;
    worker
        .send(AccountCommand::Stop {
            agent_id: state.agent.clone(),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            if state
                .bridge
                .faults
                .committed
                .lock()
                .unwrap()
                .iter()
                .any(|(name, _)| name == "stop")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stop commits while filesystem preparation remains paused");
    assert!(state.record().await["epoch"].as_i64().unwrap() > state.epoch);
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_one();
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "revoked preparation must never admit provider input"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn preparation_drains_journal_and_stop() {
    scenario_preparation_drains_journal_and_stop().await;
}

pub(super) async fn unresolved_thread_receipt_survives_replacement(rejected: bool) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    worker_barrier(&worker).await;
    worker.task.abort();
    assert!(worker.completion().await.is_err());
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut peer = Peer::identity("default", if rejected { 1 } else { 2 }, None);
    let mut replacement = peer.worker(bridge).await;
    let held = state.record().await;
    assert_eq!(
        held["startOutcomeHold"]["reason"],
        "thread-preparation/receipt-pending"
    );
    assert_eq!(held["startOutcomeHold"]["connectionGeneration"], 1);
    assert_eq!(held["startOutcomeHold"]["operationId"], resume["id"]);
    state.wake(&replacement).await;
    worker_barrier(&replacement).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "unknown prior thread request must prevent input replay"
    );
    if rejected {
        let cursor = peer
            .frame(json!({"id":resume["id"], "error":{"code":-32000,"message":"thread refused"}}))
            .await;
        peer.acknowledged(cursor).await;
        let held = state.record().await;
        assert_eq!(
            held["startOutcomeHold"]["reason"],
            "Provider thread resume failed; input remains pending."
        );
        assert_eq!(held["error"], held["startOutcomeHold"]["reason"]);
    }
    assert_ne!(state.event_status("input").await, "delivered");
    replacement.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn old_generation_thread_receipt_remains_unknown() {
    unresolved_thread_receipt_survives_replacement(false).await;
}
#[tokio::test(start_paused = true)]
async fn recovered_thread_rejection_replaces_receipt_pending_hold() {
    unresolved_thread_receipt_survives_replacement(true).await;
}

#[tokio::test(start_paused = true)]
async fn cold_catalog_refreshes_and_wakes_without_an_external_command() {
    let state = State::new().await;
    let id = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                row.extra.remove("nativeEffort");
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let resources = test_resources().unwrap();
    let catalog = resources.model_catalog.clone();
    let mut catalog_peer = Peer::new();
    crate::model_catalog::tests::set_refresh_factory(&catalog, catalog_peer.factory.clone());
    let mut peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    let held = state.record().await;
    assert_eq!(
        held["startAttempt"]["prepareError"],
        "model-catalog/refresh-pending"
    );
    tokio::time::advance(PREPARATION_RETRY).await;
    let list = catalog_peer.write("model/list").await;
    // The one-off exchange does not require journal ACK for its final response.
    catalog_peer.frame(json!({"id":list["id"],"result":{"data":[{"model":"fake-model","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}],"nextCursor":null}})).await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let turn = peer.write("turn/start").await;
    assert_eq!(
        state.record().await["startAttempt"]["id"],
        held["startAttempt"]["id"]
    );
    peer.accepted(&turn).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
    catalog.shutdown().await.unwrap();
}

pub(super) async fn scenario_workspace_retry_wakes_after_restart() {
    let state = State::new().await;
    let id = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| {
                let mut owner = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                owner.id = AgentId::from("workspace-owner");
                owner.status = studio_runtime_store::AgentStatus::Waiting;
                owner.extra.insert("autoWake".into(), json!(false));
                owner
                    .extra
                    .insert("workspaceOperation".into(), json!("capture"));
                AgentRepository::put(tx, &owner)?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    assert_eq!(
        state.record().await["preparationWake"]["reason"],
        "workspace-readiness/pending"
    );
    worker.task.abort();
    assert!(worker.completion().await.is_err());
    state
        .bridge
        .run(|store| {
            store.transaction_with(|tx| {
                let mut owner =
                    AgentRepository::get(tx, &AgentId::from("workspace-owner"))?.unwrap();
                owner.extra.remove("workspaceOperation");
                AgentRepository::put(tx, &owner)?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut peer = Peer::new();
    let mut replacement = peer.worker(bridge).await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let turn = peer.write("turn/start").await;
    peer.accepted(&turn).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert!(state.record().await.get("preparationWake").is_none());
    replacement.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn workspace_retry_wakes_after_restart() {
    scenario_workspace_retry_wakes_after_restart().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_surfaces_unconfirmed_provider_cleanup() {
    let state = State::new().await;
    let peer =
        Peer::identity_with_cleanup("default", 1, None, Some("termination unconfirmed".into()));
    let mut worker = peer.worker(state.bridge.clone()).await;
    assert!(matches!(
        worker.shutdown().await,
        Err(WorkerError::CleanupUnconfirmed(_))
    ));
}
