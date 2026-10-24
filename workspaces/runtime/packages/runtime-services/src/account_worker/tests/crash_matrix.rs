//! Crash every before/after bridge boundary, cloning one Python-written database.
use super::*;
use std::sync::atomic::Ordering;
type SavedState = (StoreBridge, String, i64, PathBuf);
const SCENARIO_COUNT: usize = 31;
const FAST_SCENARIOS: [usize; 4] = [0, 8, 23, 30];
#[derive(Default)]
pub(super) struct Context {
    pub faults: Arc<Faults>,
    pub state: std::sync::Mutex<Option<SavedState>>,
    pub writes: std::sync::Mutex<Vec<Value>>,
    pub frames: std::sync::Mutex<BTreeMap<(u64, u64), Value>>,
    pub acknowledged: std::sync::Mutex<BTreeMap<u64, u64>>,
    pub generation: std::sync::atomic::AtomicU64,
    pub inputs: std::sync::Mutex<Vec<String>>,
    committed: std::sync::Mutex<Vec<(String, usize, Option<String>)>>,
    pub agent_seed: std::sync::Mutex<Option<studio_runtime_store::AgentRecord>>,
    pub provider_write: tokio::sync::Notify,
}
tokio::task_local! { pub(super) static MATRIX: Arc<Context>; }

async fn read_on_pool<T, F>(store: Arc<RuntimeStore>, operation: F) -> Result<T, WorkerError>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> Result<T, WorkerError> + Send + 'static,
{
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_crash_matrix_read",
        move || {
            let connection = store.test_reader()?;
            operation(&connection)
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)?
}

async fn scenario(index: usize) {
    match index {
        0 => scenario_normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement().await,
        1 => scenario_interrupt_before_start_reply_uses_observed_turn_and_revokes_epoch().await,
        2 => scenario_shutdown_full_queue_and_silent_provider_is_bounded().await,
        3 => scenario_deadline_unknown_is_not_resent_and_late_reply_refines_it().await,
        4 => scenario_provider_exit_mid_turn_retains_unknown_without_retry().await,
        5 => scenario_duplicate_stale_and_completion_before_reply_preserve_order().await,
        6 => scenario_cancel_queued_input_prevents_provider_admission().await,
        7 => scenario_generation_closed_holds_the_old_submission().await,
        8 => scenario_restart_after_submitted_before_written_recovers_unknown_without_resend().await,
        9 => scenario_native_approval_without_a_thread_is_refused_after_commit().await,
        10 => scenario_native_question_without_a_thread_is_refused_after_commit().await,
        11 => scenario_unsupported_dynamic_tool_is_answered_after_commit().await,
        12 => scenario_foreign_account_stop_cannot_revoke_agent().await,
        13 => scenario_callback_unknown_write_is_not_resent_on_replay().await,
        14 => scenario_a_new_stop_can_retry_a_definitely_rejected_interrupt().await,
        15 => scenario_stream_base_survives_replaced_visible_item().await,
        16 => scenario_new_thread_freezes_settings_and_accepts_context().await,
        17 => scenario_child_result_materializes_before_submission().await,
        18 => scenario_workspace_refusal_retains_queue().await,
        19 => scenario_workspace_pending_reuses_reserved_attempt().await,
        20 => preparation_review::scenario_completed_repair_and_missing_complaint_deliver().await,
        21 => Box::pin(preparation_review::scenario_late_context_change_refuses_submission()).await,
        22 => Box::pin(preparation_review::scenario_sibling_completion_does_not_refuse_non_lead()).await,
        23 => closure::pending_text_recovery(true).await,
        24 => closure::pending_text_recovery(false).await,
        25 => closure::scenario_retained_submit_and_settlement_keep_one_replay_identity().await,
        26 => closure::scenario_old_generation_pending_text_is_materialized_and_receipts_retire().await,
        27 => closure::scenario_preparation_drains_journal_and_stop().await,
        28 => closure::unresolved_thread_receipt_survives_replacement(false).await,
        29 => closure::unresolved_thread_receipt_survives_replacement(true).await,
        30 => closure::scenario_workspace_retry_wakes_after_restart().await,
        _ => unreachable!(),
    }
}

async fn execute(
    index: usize,
    target: Option<(String, usize, Option<String>, bool)>,
) -> Option<Arc<Context>> {
    let context = Arc::new(Context::default());
    context.faults.arm(None);
    *context.faults.named_target.lock().unwrap() = target.clone();
    let result = tokio::spawn(MATRIX.scope(context.clone(), async move {
        Box::pin(tokio::time::timeout(
            std::time::Duration::from_secs(300),
            scenario(index),
        ))
        .await
        .expect("scenario watchdog")
    }))
    .await;
    if target.is_none() {
        result.expect("baseline scenario");
    }
    if let Some((name, occurrence, identity, _)) = &target
        && !context.faults.fired.load(Ordering::SeqCst)
    {
        assert!(
            *occurrence > 0 || timing_dependent(name, identity.as_deref()),
            "scenario {index} did not reach injection {target:?}"
        );
        if let Some((_, _, _, root)) = context.state.lock().unwrap().take() {
            std::fs::remove_dir_all(root).unwrap();
        }
        return None;
    }
    if target.is_some() {
        *context.committed.lock().unwrap() = context.faults.committed.lock().unwrap().clone();
        context.faults.arm(None);
    }
    Some(context)
}
/// Boundaries whose first occurrence may be missed: retained-session
/// requests, the history reads of restart reconciliation, and steer claims.
/// Later occurrences of any boundary may also be missed, because their count
/// varies with task interleaving; every other first occurrence must be reached.
fn timing_dependent(name: &str, identity: Option<&str>) -> bool {
    name.starts_with("retained_")
        || name == "claim_steer"
        || identity.is_some_and(|id| id.contains("request-method=thread/"))
}
/// The fake provider's answer to a request the replacement worker may send.
fn fake_result(request: &Value) -> Value {
    match request["method"].as_str() {
        Some("thread/resume" | "thread/start") => json!({"thread":{"id":"fake-thread-0"}}),
        Some("thread/read") => json!({"thread":{"id":"fake-thread-0","status":{"type":"idle"}}}),
        Some("thread/turns/list") => json!({"data":[]}),
        Some("turn/start") => json!({"turn":{"id":"new-turn"}}),
        _ => json!({}),
    }
}

async fn replacement_worker(peer: &Peer, bridge: StoreBridge) -> AccountHandle {
    assert!(
        !bridge
            .accounts
            .lock()
            .expect("account owners")
            .contains(&peer.account),
        "the previous account owner must exit before replacement"
    );
    // The matrix reuses Faults across workers. Its completion flag must name
    // this first pass, not a previous worker's completed startup.
    bridge
        .faults
        .startup_recovery_done
        .store(false, Ordering::SeqCst);
    peer.worker(bridge).await
}

async fn finish_replacement_recovery(peer: &mut Peer, worker: &AccountHandle, label: &str) {
    let barrier = worker_barrier(worker);
    tokio::pin!(barrier);
    loop {
        tokio::select! {
            () = &mut barrier => break,
            wire = peer.next_write() => {
                assert!(matches!(wire.value["method"].as_str(), Some("thread/read" | "thread/turns/list")),
                    "{label}: replacement recovery resent an unresolved operation: {}", wire.value);
                assert_eq!(wire.value["params"]["threadId"], "fake-thread-0", "{label}: recovery reads only its original thread");
                wire.receipt.send(CommandReceipt::Written { remote_id: None, duplicate: false }).ok().expect("recovery read receipt");
                peer.frame(json!({"id":wire.value["id"],"result":fake_result(&wire.value)})).await;
            }
        }
    }
}

#[tokio::test]
async fn a_replacement_waits_for_its_own_recovery_before_the_health_turn() {
    let context = Arc::new(Context::default());
    MATRIX.scope(context.clone(), async {
        let state = State::new().await;
        let peer = Peer::new();
        let mut worker = peer.worker(state.bridge.clone()).await;
        worker_barrier(&worker).await;
        assert!(context.faults.startup_recovery_done.load(Ordering::SeqCst));
        worker.shutdown().await.expect("original worker shutdown");
        let agent = state.agent.clone();
        let epoch = state.epoch;
        state.bridge.store.transaction(move |tx| {
            let mut owner = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.expect("original actor");
            owner.status = studio_runtime_store::AgentStatus::Queued;
            owner.extra.insert("inFlight".into(), json!(false));
            owner.extra.insert("autoWake".into(), json!(true));
            owner.extra.insert("restartRecovery".into(), json!({"stage":"held","startAttempt":{
                "id":"matrix-held-start","epoch":epoch,"accountKey":"default","threadId":"fake-thread-0",
                "events":["annotation:input"],"submitted":true,"executionOutcome":"unknown",
                "requestTicket":"matrix-held-ticket","connectionGeneration":1,"connectionId":"original"}}));
            owner.extra.insert("contextRepairWait".into(), json!({"scope":"native","events":["annotation:input"],
                "source":{"id":agent,"epoch":epoch,"accountKey":"default","threadId":"fake-thread-0","attemptId":null},
                "error":"Context repair waits for a confirmed input receipt: annotation:input","nextCheckAt":0}));
            owner.extra.remove("startAttempt");
            AgentRepository::put(tx, &owner)?;
            tx.execute("UPDATE runtime_events SET status='uncertain',error='Unknown native input outcome' WHERE id='annotation:input'", [])?;
            Ok(())
        }).await.expect("held original input");
        let mut peer = Peer::new();
        let mut worker = replacement_worker(&peer, state.bridge.clone()).await;
        assert!(!context.faults.startup_recovery_done.load(Ordering::SeqCst),
            "the previous worker's startup result cannot complete its replacement");
        finish_replacement_recovery(&mut peer, &worker, "held input replacement").await;
        assert_eq!(state.event_status("input").await, "uncertain");
        assert_eq!(state.record().await["restartRecovery"]["stage"], "held");
        prove_ready_for_new_turn(&context, &state, &mut peer, &worker).await;
        let starts: Vec<_> = context.writes.lock().expect("writes").iter()
            .filter(|wire| wire["method"] == "turn/start").cloned().collect();
        assert_eq!(starts.len(), 1, "only the separate health input starts");
        assert!(starts[0]["params"]["input"][0]["text"].as_str().expect("health input").contains("health-input"));
        assert_eq!(state.event_status("input").await, "uncertain");
        worker.shutdown().await.expect("replacement shutdown");
        std::fs::remove_dir_all(&state.root).expect("owned fixture cleanup");
    }).await;
}

async fn restart(context: &Context, label: &str) {
    let (bridge, agent, epoch, root) = context.state.lock().unwrap().take().unwrap();
    bridge
        .run(|store| async move {
            store
                .transaction_with(move |tx| {
                    tx.execute_batch("DROP TRIGGER IF EXISTS no_flush; DROP TRIGGER IF EXISTS journal_error; DROP TRIGGER IF EXISTS retain_effect;")?;
                    Ok::<(), WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
    // The failed worker must release ownership before the replacement starts.
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let released = bridge.account_released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !bridge.accounts.lock().unwrap().contains("default") {
                break;
            }
            released.await;
        }
    })
    .await
    .expect("old account owner exits");
    let generation = context.generation.load(Ordering::SeqCst).max(1);
    let mut peer = Peer::identity("default", generation, None);
    let mut worker = replacement_worker(&peer, bridge.clone()).await;
    let frames = context.frames.lock().unwrap().clone();
    for ((frame_generation, cursor), frame) in frames {
        if frame_generation != generation {
            continue;
        }
        peer.cursor = cursor - 1;
        peer.frame(frame).await;
    }
    let marker = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    let mut joined = false;
    let mut replacement_writes = BTreeSet::new();
    let mut replacement_interrupted = false;
    let (barrier_send, mut barrier_done) = oneshot::channel();
    let mut barrier_send = Some(barrier_send);
    let mut marker_seen = false;
    let mut barrier_seen = false;
    // Startup can owe interrupts, but must never resend an unknown input.
    loop {
        if barrier_seen && bridge.faults.startup_recovery_done.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            done = &mut barrier_done, if marker_seen && !barrier_seen => {
                done.expect("replacement reaches command barrier after all owed effects");
                barrier_seen = true;
            },
            () = bridge.faults.startup_recovery.notified(), if barrier_seen => {},
            ack = peer.acks.recv() => {
                if let Some(ack) = &ack {
                    let mut acknowledged = context.acknowledged.lock().unwrap();
                    let highest = acknowledged.entry(generation).or_default();
                    *highest = (*highest).max(ack.0);
                }
                match ack {
                    Some(ack) if ack.0 >= marker => {
                        if let Some(reply) = barrier_send.take() {
                            worker.send(AccountCommand::Barrier(reply)).await.unwrap();
                            marker_seen = true;
                        }
                    },
                    Some(_) => {},
                    None => { joined = true; let result = worker.completion().await; assert!(matches!(result, Err(WorkerError::CallbackUnknown)), "{label}: restart exited {result:?}"); break; },
                }
            },
            write = peer.writes.recv() => {
                let Some(wire) = write else { continue; };
                replacement_interrupted |= wire.value["method"] == "turn/interrupt";
                assert!(replacement_writes.insert(wire.value["id"].to_string()), "{label}: duplicate write within replacement");
                {
                let old = context.writes.lock().unwrap();
                assert!(
                    !old.iter().any(|value| value["id"] == wire.value["id"]),
                    "{label}: operation resent: method={} id={}",
                    wire.value["method"],
                    wire.value["id"]
                );
                if wire.value["method"] == "turn/start" {
                    assert!(!old.iter().any(|value| value["method"] == "turn/start" && value["params"]["input"] == wire.value["params"]["input"]), "{label}: input resent");
                }
                }
                wire.receipt.send(CommandReceipt::Written { remote_id: None, duplicate: false }).ok().unwrap();
                peer.frame(json!({"id":wire.value["id"],"result":fake_result(&wire.value)})).await;
            }
        }
    }
    let stop_committed = context
        .committed
        .lock()
        .unwrap()
        .iter()
        .any(|(name, _, _)| name == "stop");
    let accepted_turn = context
        .frames
        .lock()
        .unwrap()
        .values()
        .any(|frame| frame.pointer("/result/turn/id").and_then(Value::as_str) == Some("turn"));
    let interrupted_before = context
        .writes
        .lock()
        .unwrap()
        .iter()
        .any(|wire| wire["method"] == "turn/interrupt");
    let state = State {
        bridge: bridge.clone(),
        agent,
        epoch,
        root: root.clone(),
        retained: true,
    };
    if stop_committed && accepted_turn && !interrupted_before {
        let record = state.record().await;
        let attempt = &record["startAttempt"];
        let tracked_turn = attempt["turnId"].as_str() == Some("turn")
            || attempt["observedTurnId"].as_str() == Some("turn");
        let interrupt = &attempt["interrupt"];
        let ticket = interrupt["requestTicket"]
            .as_str()
            .expect("unknown interrupt has a ticket")
            .to_owned();
        let interrupt_generation = interrupt["generation"]
            .as_u64()
            .expect("unknown interrupt has a generation");
        let receipt_count = read_on_pool(bridge.store.clone(), move |db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:%:submitted' AND json_extract(result,'$.request.id')=?1 AND json_extract(result,'$.request.generation')=?2 AND json_extract(result,'$.request.method')='turn/interrupt'",
                rusqlite::params![ticket, i64::try_from(interrupt_generation).unwrap()],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
        let held_unknown = interrupt["outcome"].as_str() == Some("unknown")
            && tracked_turn
            && interrupt["threadId"] == "fake-thread-0"
            && interrupt["turnId"] == "turn"
            && interrupt_generation == generation
            && receipt_count == 1;
        assert!(
            replacement_interrupted || held_unknown,
            "{label}: committed stop was neither sent once nor held Unknown with its turn tracked"
        );
    }
    if !joined {
        let _result = worker.shutdown().await;
    }
    // A second initialized recovery must replay unresolved callbacks and retain
    // their Unknown intent without writing a second response. The request's
    // cursor settled with its batch, so only its record holds the uncertainty.
    let frames = context.frames.lock().unwrap().clone();
    let record_phases = |cursor: u64| {
        let store = bridge.store.clone();
        async move {
            read_on_pool(store, move |db| {
                let count = |phase: &str| {
                    db.query_row(
                        "SELECT COUNT(*) FROM runtime_operation_receipts WHERE id=?1",
                        [format!(
                            "account-worker:{}:{phase}",
                            crate::account_worker::callback_record::operation(
                                "default", generation, cursor
                            )
                        )],
                        |row| row.get::<_, i64>(0),
                    )
                };
                Ok((count("accepted")?, count("submitted")?))
            })
            .await
            .unwrap()
        }
    };
    let mut unresolved_callback = None;
    for ((frame_generation, cursor), frame) in frames {
        if frame_generation != generation
            || !frame["id"].is_string()
            || frame["method"].as_str().is_none()
        {
            continue;
        }
        if record_phases(cursor).await == (1, 1) {
            unresolved_callback = Some((cursor, frame));
            break;
        }
    }
    let (mut verification_peer, mut verification) =
        if let Some((callback_cursor, callback_frame)) = unresolved_callback {
            let mut peer = Peer::identity("default", generation, None);
            let worker = replacement_worker(&peer, bridge.clone()).await;
            peer.cursor = callback_cursor - 1;
            let redelivered_cursor = peer.frame(callback_frame).await;
            assert_eq!(
                redelivered_cursor, callback_cursor,
                "replay uses the exact callback cursor"
            );
            finish_replacement_recovery(&mut peer, &worker, label).await;
            assert!(
                peer.writes.try_recv().is_err(),
                "{label}: unresolved callback was redelivered"
            );
            assert_eq!(
                record_phases(callback_cursor).await,
                (1, 1),
                "callback intent remains held as Unknown"
            );
            (peer, worker)
        } else {
            let mut peer = Peer::identity("default", generation, None);
            peer.cursor = marker;
            let worker = replacement_worker(&peer, bridge.clone()).await;
            finish_replacement_recovery(&mut peer, &worker, label).await;
            (peer, worker)
        };
    let expected_inputs = context.inputs.lock().unwrap().clone();
    let expected_items = expected_text(context);
    let assert_label = label.to_owned();
    let terminal = {
        let acknowledged = context.acknowledged.lock().unwrap();
        context
            .frames
            .lock()
            .unwrap()
            .iter()
            .rfind(|((generation, cursor), frame)| {
                *cursor <= acknowledged.get(generation).copied().unwrap_or(0)
                    && frame["method"] == "turn/completed"
                    && frame["params"]["turn"]["id"] == "turn"
            })
            .and_then(|(_, frame)| {
                frame["params"]["turn"]["status"]
                    .as_str()
                    .map(str::to_owned)
            })
    };
    // A reservation held by a pending workspace wait is not stranded.
    let record = state.record().await;
    let workspace_blocked = read_on_pool(state.bridge.store.clone(), |db| {
        Ok(db.query_row(
            "SELECT COUNT(*) FROM runtime_agents WHERE json_extract(record,'$.workspaceOperation') IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )? > 0)
    })
    .await
    .unwrap();
    let workspace_wait = workspace_blocked
        || record["startAttempt"]["prepareError"] == "workspace-readiness/pending";
    read_on_pool(state.bridge.store.clone(), move |db| {
            for id in expected_inputs {
                let status: String = db.query_row(
                    "SELECT status FROM runtime_events WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                assert!(
                    workspace_wait || status != "reserved",
                    "input left reserved after recovery"
                );
            }
            for (item, expected) in expected_items {
                let actual = db.query_row("SELECT COALESCE((SELECT body FROM runtime_item_fulltext WHERE id=i.id),json_extract(i.record,'$.text')) FROM runtime_items i WHERE id LIKE ?1", [format!("%:{item}")], |row|row.get::<_, studio_runtime_store::stored_text::StoredText>(0))?.into_string();
                assert!(
                    actual == expected,
                    "{assert_label}: acknowledged payload text missing after restart for {item}: actual_len={}, expected_len={}, actual_prefix={:?}, expected_prefix={:?}",
                    actual.len(),
                    expected.len(),
                    actual.chars().take(48).collect::<String>(),
                    expected.chars().take(48).collect::<String>()
                );
                if let Some(status) = &terminal {
                    let mark: String = db.query_row("SELECT json_extract(record,'$.turnStatus') FROM runtime_items WHERE id LIKE ?1",[format!("%:{item}")],|row|row.get(0))?;
                    assert_eq!(&mark,status,"completion effects missing after restart");
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    if !workspace_wait {
        assert_ne!(
            record["status"], "starting",
            "{label}: stranded reservation"
        );
        assert_ne!(
            state.event_status("input").await,
            "reserved",
            "{label}: reserved forever"
        );
    }
    // The health agent shares the workspace, so a pending workspace wait blocks it too.
    if !workspace_wait {
        prove_ready_for_new_turn(context, &state, &mut verification_peer, &verification).await;
    }
    verification
        .shutdown()
        .await
        .expect("usable replacement shuts down");
    std::fs::remove_dir_all(root).unwrap();
}

async fn prove_ready_for_new_turn(
    context: &Context,
    state: &State,
    peer: &mut Peer,
    worker: &AccountHandle,
) {
    finish_replacement_recovery(peer, worker, "replacement health").await;
    let mut seed = context
        .agent_seed
        .lock()
        .unwrap()
        .clone()
        .expect("Python-written seed");
    seed.id = AgentId::from("replacement-health");
    seed.root_id = seed.id.clone();
    seed.extra.insert("threadId".into(), json!("health-thread"));
    let epoch = seed.epoch;
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    AgentRepository::put(tx, &seed)?;
                    Ok::<(), WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
    let health = State {
        bridge: state.bridge.clone(),
        agent: "replacement-health".into(),
        epoch,
        root: state.root.clone(),
        retained: true,
    };
    health.enqueue("health-input").await;
    health.wake(worker).await;
    let resume = peer.write("thread/resume").await;
    assert_eq!(resume["params"]["threadId"], "health-thread");
    peer.reply(&resume, json!({"thread":{"id":"health-thread"}}))
        .await;
    let start = peer.write("turn/start").await;
    let cursor = peer
        .frame(json!({"id":start["id"],"result":{"turn":{"id":"health-turn"}}}))
        .await;
    let settlement =
        tokio::time::timeout(std::time::Duration::from_secs(120), peer.acks.recv()).await;
    assert!(
        matches!(settlement, Ok(Some(ack)) if ack.0 == cursor),
        "replacement health start was not settled: {settlement:?}; bridge operations: {:?}",
        context.faults.operations.lock().unwrap()
    );
    let cursor=peer.frame(json!({"method":"turn/completed","params":{"threadId":"health-thread","turn":{"id":"health-turn","status":"completed"}}})).await;
    peer.acknowledged(cursor).await;
    assert_eq!(
        health.record().await["status"],
        "completed",
        "replacement executes a new turn"
    );
}

#[tokio::test]
async fn health_turn_waits_for_pending_recovery_history() {
    let context = Arc::new(Context::default());
    MATRIX.scope(context.clone(), async {
        let state = State::new().await;
        let agent_id = state.agent.clone();
        state.bridge.run(move |store| async move {
            store.transaction_with(move |tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.unwrap();
                let event = "annotation:input";
                let attempt = json!({"id":"restart-attempt","epoch":agent.epoch,"accountKey":"default",
                    "threadId":"fake-thread-0","events":[event],"submitted":true,
                    "executionOutcome":"unknown","created":1.0,"connectionGeneration":1});
                agent.extra.insert("restartRecovery".into(), json!({"stage":"held","startAttempt":attempt}));
                agent.extra.insert("contextRepairWait".into(), json!({"error":"unconfirmed input",
                    "scope":"native","events":[event],"nextCheckAt":0,
                    "source":{"id":agent_id,"epoch":agent.epoch,"accountKey":"default","threadId":"fake-thread-0","attemptId":null}}));
                AgentRepository::put(tx, &agent)?;
                tx.execute("UPDATE runtime_events SET status='uncertain' WHERE id=?1", [event])?;
                Ok::<(), WorkerError>(())
            }).await
        }).await.unwrap();
        let mut peer = Peer::new();
        let mut worker = peer.worker(state.bridge.clone()).await;
        // Force native history ahead of the health wake, without relying on
        // scheduler speed or the channel being momentarily empty.
        context.provider_write.notified().await;
        assert_eq!(context.writes.lock().unwrap().last().unwrap()["method"], "thread/turns/list");
        prove_ready_for_new_turn(&context, &state, &mut peer, &worker).await;
        assert_eq!(state.event_status("input").await, "uncertain", "unknown input is never resent");
        worker.shutdown().await.unwrap();
        std::fs::remove_dir_all(&state.root).unwrap();
    }).await;
}

// Expectations come from the delivered protocol, never the store under test.
fn expected_text(context: &Context) -> BTreeMap<String, String> {
    let acknowledged = context.acknowledged.lock().unwrap();
    let frames = context.frames.lock().unwrap();
    let mut text = BTreeMap::<String, String>::new();
    // An item/started replaces the stored visible text only until the next
    // delta, which rebuilds the stream from its durable base.
    let mut replaced = BTreeMap::<String, String>::new();
    for ((generation, cursor), frame) in frames.iter() {
        if *cursor > acknowledged.get(generation).copied().unwrap_or(0) {
            continue;
        }
        let params = &frame["params"];
        if params["threadId"] != "fake-thread-0"
            || params["turnId"].as_str().is_some_and(|turn| turn != "turn")
        {
            continue;
        }
        match frame["method"].as_str() {
            Some("item/agentMessage/delta") => {
                let id = params["itemId"].as_str().expect("delta item");
                replaced.remove(id);
                text.entry(id.into())
                    .or_default()
                    .push_str(params["delta"].as_str().expect("delta text"));
            }
            Some(method @ ("item/started" | "item/completed"))
                if params["item"]["type"] == "agentMessage" =>
            {
                let item = &params["item"];
                let value = item["text"].as_str().unwrap_or("");
                if !value.is_empty() {
                    let id: String = item["id"].as_str().unwrap().into();
                    if method == "item/started" {
                        replaced.insert(id, value.into());
                    } else {
                        replaced.remove(&id);
                        text.insert(id, value.into());
                    }
                }
            }
            _ => {}
        }
    }
    text.extend(replaced);
    text
}

async fn scenario_matrix(index: usize) -> (usize, f64, f64) {
    let mut cases = 0;
    let mut scenario_seconds = 0.0;
    let mut restart_seconds = 0.0;
    {
        let baseline = execute(index, None).await.expect("baseline runs");
        let operations = baseline.faults.operations.lock().unwrap().clone();
        assert!(
            !operations.is_empty(),
            "scenario {index} has no instrumented bridge calls"
        );
        if let Some((_, _, _, root)) = baseline.state.lock().unwrap().take() {
            std::fs::remove_dir_all(root).unwrap();
        }
        let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
        let mut covered = std::collections::BTreeSet::new();
        let mut skipped = std::collections::BTreeSet::new();
        let mut attempted = 0;
        for (name, occurrence, identity) in &operations {
            for after in [false, true] {
                if !full && !(after && (name == "finish" || (index == 0 && name == "claim"))) {
                    continue;
                }
                eprintln!(
                    "matrix scenario={index} operation={name} occurrence={occurrence} identity={identity:?} after={after}"
                );
                attempted += 1;
                let started = std::time::Instant::now();
                let Some(context) = execute(
                    index,
                    Some((name.clone(), *occurrence, identity.clone(), after)),
                )
                .await
                else {
                    assert!(
                        *occurrence > 0 || timing_dependent(name, identity.as_deref()),
                        "missing required boundary"
                    );
                    skipped.insert((name.clone(), *occurrence, identity.clone(), after));
                    eprintln!(
                        "skipped unfired boundary scenario={index} operation={name} occurrence={occurrence} identity={identity:?} after={after}"
                    );
                    continue;
                };
                scenario_seconds += started.elapsed().as_secs_f64();
                let started = std::time::Instant::now();
                tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    restart(
                        &context,
                        &format!("scenario={index} operation={name} occurrence={occurrence} identity={identity:?} after={after}"),
                    ),
                )
                .await
                .expect("restart watchdog");
                restart_seconds += started.elapsed().as_secs_f64();
                covered.insert((name.clone(), *occurrence, identity.clone(), after));
                cases += 1;
            }
        }
        assert!(cases > 0, "scenario {index} fired no failpoints");
        assert_eq!(attempted, covered.len() + skipped.len());
        eprintln!(
            "matrix coverage scenario={index} attempted={attempted} fired={} skipped={}",
            covered.len(),
            skipped.len()
        );
        if full {
            for (name, occurrence, identity) in operations {
                for after in [false, true] {
                    assert!(
                        covered.contains(&(name.clone(), occurrence, identity.clone(), after))
                            || skipped.contains(&(
                                name.clone(),
                                occurrence,
                                identity.clone(),
                                after
                            )),
                        "missing named boundary"
                    );
                }
            }
        }
    }
    eprintln!(
        "matrix timing scenario={index} cases={cases} replay={scenario_seconds:.3}s restart={restart_seconds:.3}s"
    );
    (cases, scenario_seconds, restart_seconds)
}

#[tokio::test]
async fn every_scenario_recovers_at_every_bridge_boundary() {
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let selected = std::env::var("MATRIX_SCENARIO")
        .ok()
        .map(|value| value.parse::<usize>().expect("MATRIX_SCENARIO is an index"));
    let scenarios: Vec<usize> = if let Some(index) = selected {
        assert!(index < SCENARIO_COUNT, "MATRIX_SCENARIO is out of range");
        vec![index]
    } else if full {
        (0..SCENARIO_COUNT).collect()
    } else {
        FAST_SCENARIOS.into()
    };
    // Each scenario owns its current-thread runtime; native SQLite actor work
    // must run against real Tokio time, and timer-specific scenarios control
    // their own clock only around the deadline assertion.
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = tokio::task::JoinSet::new();
    for index in scenarios {
        let permit = slots.clone().acquire_owned().await.unwrap();
        tasks.spawn_blocking(move || {
            let _permit = permit;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(scenario_matrix(index))
        });
    }
    let mut cases = 0;
    let mut replay = 0.0;
    let mut restart = 0.0;
    while let Some(result) = tasks.join_next().await {
        let (count, scenario_time, restart_time) = result.unwrap();
        cases += count;
        replay += scenario_time;
        restart += restart_time;
    }
    if let Some(path) = std::env::var_os("MATRIX_REPORT") {
        std::fs::write(path, json!({"cases":cases,"scenarioReplaySeconds":replay,"restartSeconds":restart,"secondsPerCase":(replay+restart)/cases as f64}).to_string()).unwrap();
    }
    eprintln!("bridge crash matrix: {cases} restart cases");
}
