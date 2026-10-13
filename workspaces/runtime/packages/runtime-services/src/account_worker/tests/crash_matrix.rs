//! Crash every before/after bridge boundary, cloning one Python-written database.
use super::*;
use std::sync::atomic::Ordering;
type SavedState = (StoreBridge, String, i64, PathBuf);
#[derive(Default)]
pub(super) struct Context {
    pub faults: Arc<Faults>,
    pub state: std::sync::Mutex<Option<SavedState>>,
    pub writes: std::sync::Mutex<Vec<Value>>,
    pub frames: std::sync::Mutex<Vec<Value>>,
    pub inputs: std::sync::Mutex<Vec<String>>,
    pub acknowledged_items: std::sync::Mutex<Vec<String>>,
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
        _ => unreachable!(),
    }
}

async fn execute(index: usize, target: Option<(usize, bool)>) -> Arc<Context> {
    let context = Arc::new(Context::default());
    context.faults.arm(target);
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
    *context.faults.target.lock().unwrap() = None;
    context
}
async fn restart(context: &Context, label: &str) {
    let (bridge, agent, epoch, root) = context.state.lock().unwrap().take().unwrap();
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
    let mut peer = Peer::new();
    let mut worker = peer.worker(bridge.clone()).await;
    let frames = context.frames.lock().unwrap().clone();
    for frame in frames {
        peer.frame(frame).await;
    }
    let marker = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    let mut joined = false;
    // Startup can owe interrupts, but must never resend an unknown input.
    loop {
        tokio::select! {
            ack = peer.acks.recv() => {
                match ack {
                    Some(ack) if ack.0 >= marker => break,
                    Some(_) => {},
                    None => { joined = true; let result = worker.completion().await; assert!(matches!(result, Err(WorkerError::CallbackUnknown)), "{label}: restart exited {result:?}"); break; },
                }
            },
            write = peer.writes.recv() => {
                let Some(wire) = write else { continue; };
                {
                let old = context.writes.lock().unwrap();
                assert!(!old.iter().any(|value| value["id"] == wire.value["id"]), "{label}: operation resent");
                if wire.value["method"] == "turn/start" {
                    assert!(!old.iter().any(|value| value["method"] == "turn/start" && value["params"]["input"] == wire.value["params"]["input"]), "{label}: input resent");
                }
                }
                wire.receipt.send(CommandReceipt::Written { remote_id: None, duplicate: false }).ok().unwrap();
                let result = match wire.value["method"].as_str() {
                    Some("thread/resume") => json!({"thread":{"id":"fake-thread-0"}}),
                    Some("turn/start") => json!({"turn":{"id":"new-turn"}}),
                    _ => json!({}),
                };
                peer.frame(json!({"id":wire.value["id"],"result":result})).await;
            }
        }
    }
    let state = State {
        bridge: bridge.clone(),
        agent,
        epoch,
        root: root.clone(),
        retained: true,
    };
    if !joined {
        let _result = worker.shutdown().await;
    }
    // Check the next actual owner's recovery before initialization admits effects.
    let (release, gate) = oneshot::channel();
    let verification_peer = Peer::gated(Some(gate));
    let mut verification = verification_peer.open_worker(bridge.clone()).await;
    let expected_inputs = context.inputs.lock().unwrap().clone();
    let expected_items = context.acknowledged_items.lock().unwrap().clone();
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
            for id in expected_items {
                let exists: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_items WHERE id=?1)",
                    [id],
                    |row| row.get(0),
                )?;
                assert!(
                    exists,
                    "acknowledged transcript effect missing after restart"
                );
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
    verification.cancel();
    let _released = release.send(());
    let _completion = verification.completion().await;
    std::fs::remove_dir_all(root).unwrap();
}

async fn scenario_matrix(index: usize) -> (usize, f64, f64) {
    let mut cases = 0;
    let mut scenario_seconds = 0.0;
    let mut restart_seconds = 0.0;
    {
        let baseline = execute(index, None).await;
        let calls = baseline.faults.count.load(Ordering::SeqCst);
        assert!(
            calls > 0,
            "scenario {index} has no instrumented bridge calls"
        );
        if let Some((_, _, _, root)) = baseline.state.lock().unwrap().take() {
            std::fs::remove_dir_all(root).unwrap();
        }
        let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
        for call in 0..calls {
            for after in [false, true] {
                if !full
                    && (!after || (call != calls.saturating_sub(3) && !(index == 0 && call == 1)))
                {
                    continue;
                }
                eprintln!("matrix scenario={index} call={call} after={after}");
                let started = std::time::Instant::now();
                let context = execute(index, Some((call, after))).await;
                scenario_seconds += started.elapsed().as_secs_f64();
                let started = std::time::Instant::now();
                tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    restart(
                        &context,
                        &format!("scenario={index} call={call} after={after}"),
                    ),
                )
                .await
                .expect("restart watchdog");
                restart_seconds += started.elapsed().as_secs_f64();
                cases += 1;
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
    // Each scenario owns a paused clock; a deadline advance cannot affect peers.
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..16 {
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
    eprintln!("bridge crash matrix: {cases} restart cases");
}
