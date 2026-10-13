//! Crash every before/after bridge boundary, cloning one Python-written database.
use super::*;
use std::sync::atomic::Ordering;
type SavedState = (StoreBridge, String, i64, PathBuf);
#[derive(Default)]
pub(super) struct Context {
    pub faults: Arc<Faults>,
    pub state: std::sync::Mutex<Option<SavedState>>,
    pub writes: std::sync::Mutex<Vec<Value>>,
    pub frames: std::sync::Mutex<BTreeMap<(u64, u64), Value>>,
    pub acknowledged: std::sync::Mutex<BTreeMap<u64, u64>>,
    pub generation: std::sync::atomic::AtomicU64,
    pub inputs: std::sync::Mutex<Vec<String>>,
    committed: std::sync::Mutex<Vec<(String, usize)>>,
    pub agent_seed: std::sync::Mutex<Option<studio_runtime_store::AgentRecord>>,
}
tokio::task_local! { pub(super) static MATRIX: Arc<Context>; }
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
        9 => scenario_unsupported_approval_is_answered_after_commit().await,
        10 => scenario_unsupported_question_is_answered_after_commit().await,
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
        21 => preparation_review::scenario_late_context_change_refuses_submission().await,
        22 => preparation_review::scenario_sibling_completion_does_not_refuse_non_lead().await,
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

async fn execute(index: usize, target: Option<(String, usize, bool)>) -> Arc<Context> {
    let context = Arc::new(Context::default());
    context.faults.arm(None);
    *context.faults.named_target.lock().unwrap() = target.clone();
    let result = tokio::spawn(MATRIX.scope(context.clone(), async move {
        tokio::time::timeout(std::time::Duration::from_secs(300), scenario(index))
            .await
            .expect("scenario watchdog")
    }))
    .await;
    if target.is_none() {
        result.expect("baseline scenario");
    }
    if target.is_some() {
        assert!(
            context.faults.fired.load(Ordering::SeqCst),
            "scenario {index} did not reach injection {target:?}"
        );
    }
    if target.is_some() {
        *context.committed.lock().unwrap() = context.faults.committed.lock().unwrap().clone();
        context.faults.arm(None);
    }
    context
}
async fn restart(context: &Context, label: &str) {
    let (bridge, agent, epoch, root) = context.state.lock().unwrap().take().unwrap();
    bridge.run(|store| store.transaction_with(|tx| {
        tx.execute_batch("DROP TRIGGER IF EXISTS no_flush; DROP TRIGGER IF EXISTS journal_error; DROP TRIGGER IF EXISTS retain_effect;")?;
        Ok(())
    })).await.unwrap();
    // The failed worker must release ownership before the replacement starts.
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            if !bridge.accounts.lock().unwrap().contains("default") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old account owner exits");
    let generation = context.generation.load(Ordering::SeqCst).max(1);
    let mut peer = Peer::identity("default", generation, None);
    let mut worker = peer.worker(bridge.clone()).await;
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
    // Startup can owe interrupts, but must never resend an unknown input.
    loop {
        tokio::select! {
            done = &mut barrier_done, if marker_seen => { done.expect("replacement reaches command barrier after all owed effects"); break; },
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
                let result = match wire.value["method"].as_str() {
                    Some("thread/resume" | "thread/start") => json!({"thread":{"id":"fake-thread-0"}}),
                    Some("turn/start") => json!({"turn":{"id":"new-turn"}}),
                    _ => json!({}),
                };
                peer.frame(json!({"id":wire.value["id"],"result":result})).await;
            }
        }
    }
    let stop_committed = context
        .committed
        .lock()
        .unwrap()
        .iter()
        .any(|(name, _)| name == "stop");
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
        let held_unknown =
            attempt["interrupt"]["outcome"].as_str() == Some("unknown") && tracked_turn;
        assert!(
            replacement_interrupted || held_unknown,
            "{label}: committed stop was neither sent once nor held Unknown with its turn tracked"
        );
    }
    if !joined {
        let _result = worker.shutdown().await;
    }
    // A second initialized recovery must retain unknown operations without resend.
    let mut verification_peer = Peer::identity("default", generation, None);
    verification_peer.cursor = marker;
    let mut verification = verification_peer.worker(bridge.clone()).await;
    worker_barrier(&verification).await;
    assert!(
        verification_peer.writes.try_recv().is_err(),
        "{label}: second recovery resent an unresolved operation"
    );
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
    state
        .bridge
        .run(move |store| {
            let db = store.read()?;
            for id in expected_inputs {
                let status: String = db.query_row(
                    "SELECT status FROM runtime_events WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                assert_ne!(status, "reserved", "input left reserved after recovery");
            }
            for (item, expected) in expected_items {
                let actual: String = db.query_row("SELECT COALESCE((SELECT body FROM runtime_item_fulltext WHERE id=i.id),json_extract(i.record,'$.text')) FROM runtime_items i WHERE id LIKE ?1", [format!("%:{item}")], |row|row.get(0))?;
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
    let record = state.record().await;
    assert_ne!(
        record["status"], "starting",
        "{label}: stranded reservation"
    );
    assert_ne!(
        state.event_status("input").await,
        "reserved",
        "{label}: reserved forever"
    );
    prove_ready_for_new_turn(context, &state, &mut verification_peer, &verification).await;
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
        .run(move |store| {
            store.transaction_with(|tx| {
                AgentRepository::put(tx, &seed)?;
                Ok(())
            })
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

// Expectations come from the delivered protocol, never the store under test.
fn expected_text(context: &Context) -> BTreeMap<String, String> {
    let acknowledged = context.acknowledged.lock().unwrap();
    let frames = context.frames.lock().unwrap();
    let mut text = BTreeMap::<String, String>::new();
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
                text.entry(id.into())
                    .or_default()
                    .push_str(params["delta"].as_str().expect("delta text"));
            }
            Some("item/completed") if params["item"]["type"] == "agentMessage" => {
                let item = &params["item"];
                let value = item["text"].as_str().unwrap_or("");
                if !value.is_empty() {
                    text.insert(item["id"].as_str().unwrap().into(), value.into());
                }
            }
            _ => {}
        }
    }
    text
}

async fn scenario_matrix(index: usize) -> (usize, f64, f64) {
    let mut cases = 0;
    let mut scenario_seconds = 0.0;
    let mut restart_seconds = 0.0;
    {
        let baseline = execute(index, None).await;
        let operations = baseline.faults.operations.lock().unwrap().clone();
        let calls = operations.len();
        assert!(
            calls > 0,
            "scenario {index} has no instrumented bridge calls"
        );
        if let Some((_, _, _, root)) = baseline.state.lock().unwrap().take() {
            std::fs::remove_dir_all(root).unwrap();
        }
        let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
        let mut covered = std::collections::BTreeSet::new();
        for (call, (name, occurrence)) in operations.iter().enumerate() {
            for after in [false, true] {
                if !full
                    && (!after
                        || (call != calls.saturating_sub(3) && !(index == 0 && name == "claim")))
                {
                    continue;
                }
                eprintln!(
                    "matrix scenario={index} operation={name} occurrence={occurrence} after={after}"
                );
                let started = std::time::Instant::now();
                let context = execute(index, Some((name.clone(), *occurrence, after))).await;
                scenario_seconds += started.elapsed().as_secs_f64();
                let started = std::time::Instant::now();
                tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    restart(
                        &context,
                        &format!("scenario={index} operation={name} occurrence={occurrence} after={after}"),
                    ),
                )
                .await
                .expect("restart watchdog");
                restart_seconds += started.elapsed().as_secs_f64();
                covered.insert((name.clone(), *occurrence, after));
                cases += 1;
            }
        }
        if full {
            for (name, occurrence) in operations {
                for after in [false, true] {
                    assert!(
                        covered.contains(&(name.clone(), occurrence, after)),
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
    let (watchdog_done, watchdog_wait) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let seconds = if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
            600
        } else {
            120
        };
        if matches!(
            watchdog_wait.recv_timeout(std::time::Duration::from_secs(seconds)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            eprintln!("bridge crash matrix exceeded its wall-clock watchdog");
            std::process::abort();
        }
    });
    // Each scenario owns a paused clock; a deadline advance cannot affect peers.
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..31 {
        if std::env::var("MATRIX_SCENARIO")
            .ok()
            .is_some_and(|value| value != index.to_string())
        {
            continue;
        }
        let permit = slots.clone().acquire_owned().await.unwrap();
        tasks.spawn_blocking(move || {
            let _permit = permit;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
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
    watchdog_done.send(()).expect("watchdog alive");
    watchdog.join().expect("watchdog joins");
    eprintln!("bridge crash matrix: {cases} restart cases");
}
