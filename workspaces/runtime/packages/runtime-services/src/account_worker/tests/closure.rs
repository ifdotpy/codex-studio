//! Closure regressions exercise the worker, including fresh store ownership.
use super::*;
use studio_runtime_store::path_identity::CanonicalPath;

const FLUSH_OPERATION_PROGRESS_GUARD: std::time::Duration = std::time::Duration::from_secs(10);

async fn recover_after_flush_failure(state: &State) {
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state
        .bridge
        .run(|store| async move {
            store
                .transaction_with(move |tx| {
                    tx.execute_batch("DROP TRIGGER IF EXISTS no_flush")?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();

    // A fresh bridge performs process startup recovery, not just lease recovery.
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let peer = Peer::new();
    let mut replacement = peer.worker(bridge).await;
    replacement.shutdown().await.unwrap();
}

async fn item_text(state: &State, item_id: &str) -> String {
    let pattern = format!("%:{item_id}");
    state
        .bridge
        .run(move |store| async move {
            Ok(store.test_reader()?.query_row(
                "SELECT json_extract(record,'$.text') FROM runtime_items WHERE id LIKE ?1",
                [pattern],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap()
}

pub(super) async fn pending_text_recovery(stopped: bool) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
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
    // In the stopped case, the interrupt reply is not terminal; the submitted
    // turn remains open for a final provider delta until turn/completed is journaled.
    state.bridge.run(|store| async move { store.transaction_with(move |tx| {
        tx.execute_batch("CREATE TRIGGER no_flush BEFORE INSERT ON runtime_items BEGIN SELECT RAISE(ABORT,'flush unavailable'); END;")?;
        Ok(())
    }).await }).await.unwrap();
    let cursor = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"pending-recovery","delta":"acknowledged full answer"}})).await;
    peer.acknowledged(cursor).await;
    assert!(worker.shutdown().await.is_err());
    if crash_matrix::MATRIX
        .try_with(|context| {
            context
                .faults
                .fired
                .load(std::sync::atomic::Ordering::SeqCst)
        })
        .unwrap_or(false)
    {
        // Leave the database exactly at the named bridge boundary. The matrix
        // owns trigger removal and the first subsequent startup recovery.
        return;
    }
    recover_after_flush_failure(&state).await;
    assert_eq!(
        item_text(&state, "pending-recovery").await,
        "acknowledged full answer"
    );
}

#[tokio::test]
async fn stopped_pending_text_survives_fresh_process_recovery() {
    pending_text_recovery(true).await;
}
#[tokio::test]
async fn pending_text_survives_fresh_process_recovery() {
    pending_text_recovery(false).await;
}

#[tokio::test]
async fn elapsed_flush_failure_recovers_acknowledged_pending_text() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;

    *state.bridge.faults.pause_operation.lock().unwrap() = Some("flush_streams".into());
    let cursor = peer
        .frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"elapsed-flush-recovery","delta":"acknowledged before elapsed flush"}}))
        .await;
    peer.acknowledged(cursor).await;

    let flush_paused = state.bridge.faults.paused.notified();
    tokio::pin!(flush_paused);
    tokio::time::pause();
    tokio::time::advance(STREAM_CADENCE).await;
    tokio::time::resume();
    tokio::time::timeout(FLUSH_OPERATION_PROGRESS_GUARD, flush_paused)
        .await
        .expect("elapsed flush reaches the named operation gate");

    state
        .bridge
        .run(|store| async move {
            store
                .transaction_with(move |tx| {
                    tx.execute_batch("CREATE TRIGGER no_flush BEFORE INSERT ON runtime_items BEGIN SELECT RAISE(ABORT,'flush unavailable'); END;")?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_one();
    let completion = tokio::time::timeout(FLUSH_OPERATION_PROGRESS_GUARD, worker.completion())
        .await
        .expect("forced flush failure completes the worker");
    assert!(completion.is_err(), "failed stream flush exits the worker");

    recover_after_flush_failure(&state).await;
    assert_eq!(
        item_text(&state, "elapsed-flush-recovery").await,
        "acknowledged before elapsed flush"
    );
}

#[tokio::test]
async fn journal_error_does_not_delete_an_acknowledged_pending_batch() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let cursor = peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"pending-error","delta":"must remain durable"}})).await;
    peer.acknowledged(cursor).await;
    state.bridge.run(|store| async move { store.transaction_with(move |tx| {
        tx.execute_batch("CREATE TRIGGER journal_error BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'account-worker:journal:%' BEGIN SELECT RAISE(ABORT,'journal unavailable'); END;")?;
        Ok(())
    }).await }).await.unwrap();
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
        .run(|store| async move {
            store
                .transaction_with(move |tx| {
                    tx.execute_batch("DROP TRIGGER journal_error")?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let peer = Peer::new();
    let mut replacement = peer.worker(bridge).await;
    replacement.shutdown().await.unwrap();
    let text = state.bridge.run(|store| async move {
        Ok(store.test_reader()?.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE id LIKE '%:pending-error'", [], |row| row.get::<_, String>(0))?)
    }).await.unwrap();
    assert_eq!(text, "must remain durable");
}

pub(super) async fn scenario_retained_submit_and_settlement_keep_one_replay_identity() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    // The submitted commit retires the Submit effect that owed it, so no
    // effect outlives the durable submission.
    let receipts = |pattern: &'static str| {
        state.bridge.run(move |store| async move {
            Ok(store.test_reader()?.query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE ?1",
                [pattern],
                |row| row.get::<_, i64>(0),
            )?)
        })
    };
    assert_eq!(receipts("account-worker:effect:%").await.unwrap(), 0);
    assert_eq!(
        receipts("account-worker:account:%:turn/start:submitted")
            .await
            .unwrap(),
        1
    );
    peer.accepted(&start).await;
    worker_barrier(&worker).await;
    let count = state.bridge.run(|store| async move { Ok(store.test_reader()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:effect:%' OR id LIKE 'account-worker:account:%:submitted'",[],|row|row.get::<_,i64>(0))?) }).await.unwrap();
    assert_eq!(count, 0, "settlement retires the submission guard");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn elapsed_flush_deadline_wins_over_replenished_events() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let cursor=peer.frame(json!({"method":"item/agentMessage/delta","params":{"threadId":"fake-thread-0","turnId":"turn","itemId":"timer","delta":"below threshold"}})).await;
    peer.acknowledged(cursor).await;
    let permit = state.bridge.faults.operation_gate.write().await;
    // Keep the event lane ready while time passes. Its first DB job is blocked.
    for _ in 0..8 {
        peer.frame(json!({"method":"test/barrier","params":{}}))
            .await;
    }
    tokio::time::pause();
    tokio::time::advance(STREAM_CADENCE).await;
    drop(permit);
    tokio::time::resume();
    let mut count = 0;
    let mut acknowledged = cursor;
    loop {
        acknowledged = peer.acknowledged_at_least(acknowledged + 1).await;
        // Order the read after the worker's next serve cycle, which flushes a due deadline.
        worker_barrier(&worker).await;
        count += 1;
        let flushed = state
            .bridge
            .run(|store| async move {
                Ok(store.test_reader()?.query_row(
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
        state
            .bridge
            .faults
            .startup_recovery_done
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let mut replacement = peer.worker(bridge).await;
        if generation > 2 {
            // The restart marker records its claimed automatic check, so a later
            // worker finishes its startup pass without reading again.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while !state
                .bridge
                .faults
                .startup_recovery_done
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                assert!(
                    peer.writes.try_recv().is_err(),
                    "generation {generation} repeated the restart history check"
                );
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "generation {generation} startup pass did not finish"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        if generation == 2 {
            // Each replacement is a new backend process, so the interrupted turn
            // is a restart turn. The first worker makes its one automatic native
            // history check; the provider writes the read and never answers.
            let read = peer.next_write().await;
            assert_eq!(read.value["method"], "thread/read");
            let _written = read.receipt.send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            });
        }
        let marker = peer
            .frame(json!({"method":"test/barrier","params":{}}))
            .await;
        peer.acknowledged(marker).await;
        replacement.shutdown().await.unwrap();
        let (text, receipts, retained, checkpoints) = state.bridge.run(|store| async move {
            let db=store.test_reader()?;
            let text=db.query_row("SELECT json_extract(record,'$.text') FROM runtime_items WHERE id LIKE '%:generation'",[],|r|r.get::<_,String>(0))?;
            let receipts=db.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:%' AND id NOT LIKE 'account-worker:retained:%'",[],|r|r.get::<_,i64>(0))?;
            let retained=db.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:retained:%'",[],|r|r.get::<_,i64>(0))?;
            let checkpoints=db.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:cursor:default:%:ack'",[],|r|r.get::<_,i64>(0))?;
            Ok((text,receipts,retained,checkpoints))
        }).await.unwrap();
        assert_eq!(text, "old generation text");
        assert!(
            receipts <= 4,
            "generation churn retained {receipts} worker receipts"
        );
        // The one automatic restart check is a retained read (submitted,
        // written, result). Retained receipts are durable by design, and the
        // submission pins its generation's checkpoint (see `retire_generations`).
        // Churn adds nothing more: later workers do not repeat the check.
        assert_eq!(
            retained, 3,
            "generation {generation} retained {retained} history read receipts"
        );
        assert!(
            checkpoints <= 2,
            "generation churn retained {checkpoints} cursor checkpoints"
        );
    }
}

#[tokio::test]
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
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut record =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    record
                        .extra
                        .insert("accountKey".into(), json!("destination"));
                    AgentRepository::put(tx, &record)?;
                    Ok(())
                })
                .await
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

#[tokio::test]
async fn retained_submit_and_settlement_keep_one_replay_identity() {
    scenario_retained_submit_and_settlement_keep_one_replay_identity().await;
}

#[tokio::test]
async fn old_generation_pending_text_is_materialized_and_receipts_retire() {
    scenario_old_generation_pending_text_is_materialized_and_receipts_retire().await;
}

#[tokio::test]
async fn written_callback_replay_compacts_its_journal_without_resending() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.named_target.lock().unwrap() = Some((
        "callback_written".into(),
        0,
        Some("callback:default:1:1".into()),
        true,
    ));
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
    let rows = state.bridge.run(|store| async move { Ok(store.test_reader()?.query_row("SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:journal:%' OR id LIKE 'account-worker:callback:%'",[],|row|row.get::<_,i64>(0))?) }).await.unwrap();
    assert_eq!(
        rows, 0,
        "Written replay advances and compacts the journal prefix"
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn publication_uses_the_identity_snapshot_loaded_before_serving() {
    let state = State::new().await;
    let root = state.bridge.store.root().to_path_buf();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker/tests/publication-identity",
        move || -> std::io::Result<()> {
            let directory = root.join("multi-server");
            std::fs::create_dir_all(&directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            }
            std::fs::write(directory.join("identity.json"), b"invalid identity")
        },
    )
    .await
    .unwrap()
    .unwrap();
    state
        .bridge
        .run(|store| async move {
            assert!(crate::notification_dispatch::PublicationContext::load(store.as_ref()).is_ok());
            let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let transaction_entered = Arc::clone(&entered);
            scheduler::lifecycle_transaction(store.as_ref(), move |_| {
                transaction_entered.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await
            .expect("projection keeps the cached absent identity");
            assert!(
                entered.load(std::sync::atomic::Ordering::SeqCst),
                "the cached absent identity remains usable"
            );
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
            .prepare_run("blocked-filesystem", move |_| async move {
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/account_worker/tests/blocked-filesystem",
                    move || {
                        entered.send(()).unwrap();
                        blocked.recv().unwrap();
                        Ok::<(), WorkerError>(())
                    },
                )
                .await
                .map_err(|_| WorkerError::TaskFailed)?
            })
            .await
    });
    observed.await.unwrap();
    let access = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.bridge.run(|store| async move {
            Ok(store.test_reader()?.query_row(
                "SELECT COUNT(*) FROM runtime_agents",
                [],
                |row| row.get::<_, i64>(0),
            )?)
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
    tokio::select! {
        () = crashed() => return,
        paused = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            state.bridge.faults.paused.notified(),
        ) => paused.unwrap(),
    }
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
    let committed = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            if state
                .bridge
                .faults
                .committed
                .lock()
                .unwrap()
                .iter()
                .any(|(name, _, _)| name == "stop")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    });
    tokio::select! {
        () = crashed() => return,
        committed = committed => committed.expect("stop commits while filesystem preparation remains paused"),
    }
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

#[tokio::test]
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
            "Provider thread resume failed (thread refused); input remains pending."
        );
        assert_eq!(held["error"], held["startOutcomeHold"]["reason"]);
    }
    assert_ne!(state.event_status("input").await, "delivered");
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_generation_thread_receipt_remains_unknown() {
    unresolved_thread_receipt_survives_replacement(false).await;
}
#[tokio::test]
async fn recovered_thread_rejection_replaces_receipt_pending_hold() {
    unresolved_thread_receipt_survives_replacement(true).await;
}

/// 2026-10-21: two agents stopped with "Provider thread resume failed (the
/// reply does not match the prepared thread)", and the record did not say
/// which check failed. The stored error names the failed check.
#[tokio::test]
async fn a_rejected_thread_reply_names_the_failed_check() {
    for (thread, check) in [
        (
            json!({"id":"another-thread"}),
            "the reply thread another-thread is not the prepared thread fake-thread-0",
        ),
        (json!({}), "the reply has no thread ID"),
    ] {
        let state = State::new().await;
        let mut peer = Peer::new();
        let mut worker = peer.worker(state.bridge.clone()).await;
        state.wake(&worker).await;
        let resume = peer.write("thread/resume").await;
        let reply = peer
            .frame(json!({"id":resume["id"],"result":{"thread":thread}}))
            .await;
        peer.acknowledged(reply).await;
        worker_barrier(&worker).await;
        let held = state.record().await;
        assert_eq!(
            held["error"],
            format!(
                "Provider thread resume failed (the reply does not match the prepared thread: {check}); input remains pending."
            )
        );
        assert!(
            peer.writes.try_recv().is_err(),
            "a rejected thread reply sends no input"
        );
        assert_ne!(state.event_status("input").await, "delivered");
        worker.shutdown().await.unwrap();
    }
}

/// Fixture state in a canonical folder, so the stored cwd text differs from
/// the canonical cwd only where a test makes it differ.
async fn canonical_state() -> State {
    let base = test_root();
    std::fs::create_dir_all(&base).expect("test root");
    State::new_at(
        CanonicalPath::existing(&base)
            .expect("canonical test root")
            .into_path_buf(),
    )
    .await
}

/// Set the stored cwd of the fixture agent.
async fn store_cwd(state: &State, cwd: String) {
    let id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                    row.extra.insert("cwd".into(), json!(cwd));
                    AgentRepository::put(tx, &row)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
}

/// 2026-10-21 07:59 to 08:39 UTC: four worktree agents failed every thread
/// resume with "the reply does not match the prepared thread". Each stored
/// cwd ended in a separator (`.worktrees/codex-agents/<id>/`, written before
/// #387). Preparation sends the canonical cwd, which has no trailing
/// separator. Before #386 the receipt compared it as text with the stored
/// cwd, so the reducer returned Stale(Cwd) for a thread in the same folder.
/// Live records keep the separator, so the receipt must accept them.
#[tokio::test]
async fn a_stored_cwd_with_a_trailing_separator_resumes_its_thread() {
    let state = canonical_state().await;
    let canonical = state.root.to_str().expect("UTF-8 root").to_owned();
    store_cwd(&state, format!("{canonical}/")).await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    assert_eq!(
        resume["params"]["cwd"], canonical,
        "preparation sends the canonical cwd"
    );
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    worker_barrier(&worker).await;
    assert_eq!(
        state.record().await["error"],
        Value::Null,
        "the reply of the same folder is accepted"
    );
    let start = peer.write("turn/start").await;
    assert_eq!(
        start["params"]["cwd"], canonical,
        "the turn runs in the canonical cwd"
    );
    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}

/// A cwd that moved to another folder after the resume request was written
/// still makes the reply stale, and the stored error names that check.
#[tokio::test]
async fn a_cwd_moved_to_another_folder_refuses_the_thread_reply() {
    let state = canonical_state().await;
    let moved = state.root.join("moved");
    std::fs::create_dir(&moved).expect("other folder");
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    let resume = peer.write("thread/resume").await;
    store_cwd(&state, moved.to_str().expect("UTF-8 folder").to_owned()).await;
    let reply = peer
        .frame(json!({"id":resume["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    peer.acknowledged(reply).await;
    worker_barrier(&worker).await;
    assert_eq!(
        state.record().await["error"],
        "Provider thread resume failed (the reply does not match the prepared thread: stale Cwd); input remains pending."
    );
    assert!(
        peer.writes.try_recv().is_err(),
        "a stale thread reply sends no input"
    );
    assert_ne!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_catalog_refreshes_and_wakes_without_an_external_command() {
    let state = State::new().await;
    let id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                    row.extra.remove("nativeEffort");
                    AgentRepository::put(tx, &row)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    let resources = test_resources().unwrap();
    let catalog = resources.model_catalog.clone();
    let catalog_pages = crate::model_catalog::tests::serve_pages(
        &catalog,
        crate::model_catalog::tests::AccountSessionPages::gated(vec![
            json!({"data":[{"model":"fake-model","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}],"nextCursor":null}),
        ]),
    );
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
    // The pending preparation starts the refresh itself and wakes on its result.
    // The refresh reads the catalog over the account's retained session.
    catalog_pages.release();
    let turn = peer.write_turn_start().await;
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
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
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
                .await
        })
        .await
        .unwrap();
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    assert_eq!(
        state.record().await["startAttempt"]["prepareError"],
        "workspace-readiness/pending"
    );
    assert_eq!(
        state.record().await["lastWorkspaceWait"]["attemptId"],
        state.record().await["startAttempt"]["id"]
    );
    assert!(state.record().await.get("preparationWake").is_none());
    worker.task.abort();
    assert!(worker.completion().await.is_err());
    state
        .bridge
        .run(|store| async move {
            store
                .transaction_with(move |tx| {
                    let mut owner =
                        AgentRepository::get(tx, &AgentId::from("workspace-owner"))?.unwrap();
                    owner.extra.remove("workspaceOperation");
                    AgentRepository::put(tx, &owner)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    let mut bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
    bridge.faults = state.bridge.faults.clone();
    let mut peer = Peer::new();
    let mut replacement = peer.worker(bridge).await;
    let turn = peer.write_turn_start().await;
    peer.accepted(&turn).await;
    assert_eq!(state.event_status("input").await, "delivered");
    assert!(
        state.record().await["startAttempt"]
            .get("prepareError")
            .is_none()
    );
    assert!(state.record().await.get("lastWorkspaceWait").is_none());
    replacement.shutdown().await.unwrap();
}
/// One agent's preparation never blocks another's: while A is held inside
/// its workspace pass, B is prepared and written to the provider.
#[tokio::test]
async fn agent_preparations_run_concurrently() {
    let state = State::new().await;
    let a = state.agent.clone();
    let b = "concurrent-neighbor".to_owned();
    let neighbor = b.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row = AgentRepository::get(tx, &AgentId::from(a.as_str()))?.unwrap();
                    row.id = AgentId::from(neighbor.as_str());
                    row.extra
                        .insert("threadId".into(), json!("neighbor-thread"));
                    AgentRepository::put(tx, &row)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    state.enqueue_for(&b, "neighbor-input").await;
    let mut peer = Peer::new();
    let worker = peer.worker(state.bridge.clone()).await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("workspace".into());
    state.wake(&worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("first preparation reaches its workspace pass");
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    worker
        .send(AccountCommand::Wake {
            agent_id: b.clone(),
            epoch: state.epoch,
        })
        .await
        .unwrap();
    let neighbor = peer.write("thread/resume").await;
    assert_eq!(neighbor["params"]["threadId"], "neighbor-thread");
    state.bridge.faults.release.notify_one();
    let held = peer.write("thread/resume").await;
    assert_ne!(held["id"], neighbor["id"]);
    drop(worker);
}

/// A workspace-pending preparation wakes on the blocker's commit in the same
/// worker: no timer, no restart, and one prepareError write per blocker.
#[tokio::test]
async fn workspace_pending_wakes_when_the_blocking_owner_commits() {
    let state = State::new().await;
    let id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
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
                .await
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    let held = state.record().await;
    assert_eq!(
        held["startAttempt"]["prepareError"],
        "workspace-readiness/pending"
    );
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(3600)).await;
    tokio::time::resume();
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err(), "time alone never retries");
    assert_eq!(
        state.record().await["lastWorkspaceWait"]["at"],
        held["lastWorkspaceWait"]["at"],
        "a repeated attempt with the same blocker does not rewrite it"
    );
    state
        .bridge
        .run(|store| async move {
            store
                .transaction_with(move |tx| {
                    let mut owner =
                        AgentRepository::get(tx, &AgentId::from("workspace-owner"))?.unwrap();
                    owner.extra.remove("workspaceOperation");
                    AgentRepository::put(tx, &owner)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
    let turn = peer.write_turn_start().await;
    peer.accepted(&turn).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn workspace_retry_wakes_after_restart() {
    scenario_workspace_retry_wakes_after_restart().await;
}

#[tokio::test]
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
