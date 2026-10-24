//! A `turn/start` outcome became unknown (live: its reply did not arrive
//! before its deadline), and Studio waited to check the input in native
//! history. The provider connection then closed while the backend kept
//! running. The disconnect can leave a submitted input with no confirmed turn
//! identity. The next account worker checks native history, keeps the input
//! uncertain until the turn is terminal, and never sends the input again.
use super::*;

/// Submit the input on provider generation 1, settle its start as unknown as
/// the worker does when the `turn/start` reply deadline elapses, and close that
/// provider connection while the backend runs. The account's next connection
/// reaches provider `generation`: a new process, or the same one that a
/// supervisor kept. Returns that provider and its worker.
///
/// The start and disconnect run through the store, as in the restart-held
/// test: a worker that waits out the real reply deadline would move the test
/// clock, and the next worker's native history read compares its deadline
/// with that clock.
async fn unknown_start_then_disconnect(state: &State, generation: u64) -> (Peer, AccountHandle) {
    // The account's first worker of this backend has run, so the next worker
    // follows a provider disconnect and not a backend restart.
    let first = Peer::new();
    let mut worker = first.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    worker.shutdown().await.unwrap();
    state
        .bridge
        .faults
        .startup_recovery_done
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let agent = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            let intent = store::claim(Arc::clone(&store), "default", &agent, epoch)
                .await?
                .unwrap();
            let operation = format!("account:default:{}:turn/start", intent.attempt_id);
            let request = RecoveredRequest {
                ticket: studio_runtime_providers::RequestTicket {
                    id: RequestId::String(operation.clone()),
                    generation: 1,
                    method: Method::TurnStart,
                },
                operation_id: OperationId(operation.clone()),
                thread_id: intent.thread_id.clone(),
                lightweight: false,
                deadline_ms: 60_000,
                unknown: false,
                remote_id: None,
            };
            let prepared = preparation::prepare(
                Arc::clone(&store),
                &test_resources()?,
                &intent,
                Method::TurnStart,
                1,
            )
            .await?;
            assert!(
                store::submitted(
                    &store,
                    Pending {
                        thread: prepared.thread,
                        compiled: prepared.compiled,
                        intent: prepared.intent,
                        method: "turn/start".into(),
                        operation
                    },
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
            crate::scheduler::settle_start_outcome(
                &store,
                &intent,
                crate::scheduler::TurnStart::Unknown,
            )
            .await
            .unwrap();
            crate::scheduler::lifecycle_transaction(&store, |tx| {
                store::disconnect_provider_generation(tx, "default", 1, false)
            })
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(state.event_status("input").await, "uncertain");
    let next = Peer::identity("default", generation, None);
    let replacement = next.worker(state.bridge.clone()).await;
    (next, replacement)
}

#[tokio::test]
async fn a_disconnect_after_an_unknown_start_checks_native_history_once() {
    for generation in [1, 2] {
        for status in ["inProgress", "completed"] {
            applied_unknown_start_is_kept_after_a_disconnect(generation, status).await;
        }
    }
}

async fn applied_unknown_start_is_kept_after_a_disconnect(generation: u64, status: &str) {
    let state = State::new().await;
    let (mut next, mut replacement) = unknown_start_then_disconnect(&state, generation).await;
    let history = tokio::select! {
        history = next.write("thread/turns/list") => history,
        () = worker_barrier(&replacement) => {
            let record = state.record().await;
            panic!("generation {generation}: the unknown start was never checked in native history: {record}")
        }
    };
    // Native history identifies the input, but only a terminal turn proves
    // that this worker can settle it without adopting a live stream.
    next.reply(
        &history,
        json!({"data":[{"id":"native-turn","threadId":"fake-thread-0","status":status,
            "items":[],"clientUserMessageId":"annotation:input"}]}),
    )
    .await;
    worker_barrier(&replacement).await;
    let record = state.record().await;
    if status == "completed" {
        assert_eq!(
            record["lastCompletedTurn"], "native-turn",
            "generation {generation}, status {status}: {record}"
        );
        assert_eq!(
            state.event_status("input").await,
            "delivered",
            "generation {generation}, status {status}"
        );
    } else {
        assert_eq!(
            record["restartRecovery"]["stage"], "held",
            "generation {generation}, status {status}: {record}"
        );
        assert_eq!(
            record["contextRepairWait"]["events"],
            json!(["annotation:input"])
        );
        assert!(
            record["turnId"].is_null(),
            "generation {generation}: {record}"
        );
        assert_eq!(
            state.event_status("input").await,
            "uncertain",
            "generation {generation}, status {status}"
        );
    }
    while let Ok(wire) = next.writes.try_recv() {
        assert_ne!(
            wire.value["method"], "turn/start",
            "an input whose outcome was unknown is never sent again"
        );
    }
    replacement.shutdown().await.unwrap();
}

/// Absence from native history is not proof that the provider never applied
/// the input: it stays `uncertain`, the wait stays visible, and nothing is sent.
#[tokio::test]
async fn a_disconnected_unknown_start_absent_from_history_stays_uncertain() {
    let state = State::new().await;
    let (mut next, mut replacement) = unknown_start_then_disconnect(&state, 2).await;
    let mut read = false;
    loop {
        let wire = tokio::select! {
            wire = next.next_write() => wire,
            () = worker_barrier(&replacement) => break,
        };
        let request = wire.value;
        let _ignored = wire.receipt.send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        });
        let result = match request["method"].as_str() {
            Some("thread/turns/list") => {
                read = true;
                json!({"data":[]})
            }
            Some("thread/read") => {
                json!({"thread":{"id":"fake-thread-0","status":{"type":"idle"}}})
            }
            _ => panic!("absent input must not be sent again: {request}"),
        };
        next.reply(&request, result).await;
    }
    assert!(read, "the replacement worker reads native history");
    assert_eq!(state.event_status("input").await, "uncertain");
    let record = state.record().await;
    assert!(
        record["error"].as_str().is_some_and(
            |error| error.starts_with("Context repair waits for a confirmed input receipt")
        ),
        "{record}"
    );
    replacement.shutdown().await.unwrap();
}
