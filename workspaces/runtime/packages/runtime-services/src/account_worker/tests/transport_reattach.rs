//! A provider connection closed while the backend kept running, and the next
//! account worker reached the same provider generation, which a supervisor
//! kept. Generation reconciliation adopts the disconnected turn as running
//! again. When that turn ended while no worker was attached, its end was not
//! read: the agent stayed `running`, and busy-turn input was written to a turn
//! that the provider had already finished (22 October 2026). The worker must
//! read that turn in native history once and settle an ended turn, without
//! sending anything again.
use super::*;

/// A running turn on provider generation 1 whose connection closed while the
/// backend kept running. Its first worker already ran, so the next worker
/// follows a provider disconnect, not a backend restart.
async fn disconnected_running_turn(state: &State) {
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .expect("fixture agent");
                agent.status = studio_runtime_store::AgentStatus::Running;
                agent.extra.insert("turnId".into(), json!("old-turn"));
                agent.extra.insert("inFlight".into(), json!(true));
                agent.extra.insert(
                    "startAttempt".into(),
                    json!({
                        "id":"old-attempt", "accountKey":"default", "epoch":agent.epoch,
                        "threadId":"fake-thread-0", "turnId":"old-turn", "submitted":true,
                        "connectionGeneration":1, "events":["annotation:input"],
                        "activeAtReservation":false, "settingsFixed":true
                    }),
                );
                AgentRepository::put(tx, &agent)?;
                tx.execute(
                    "UPDATE runtime_events SET status='delivered',turn_id='old-turn' \
                     WHERE id='annotation:input'",
                    [],
                )?;
                let interrupted =
                    SchedulerRepository::disconnect_account(tx, "default", 1, 1.0, false)?;
                assert_eq!(
                    interrupted,
                    vec![agent_id.clone()],
                    "the turn is disconnected"
                );
                Ok::<(), WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
    state
        .bridge
        .recovered_accounts
        .lock()
        .unwrap()
        .insert("default".into());
}

/// Answer the native history reads of `old-turn` with `status` and an idle
/// thread.
async fn reply_turn_history(peer: &mut Peer, status: &str) {
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), peer.writes.recv())
        .await
        .expect("the reconnect reads native history for the adopted turn")
        .expect("provider session remains open");
    assert_eq!(first.value["method"], "thread/read", "{}", first.value);
    let _ignored = first.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    peer.reply(
        &first.value,
        json!({"thread":{"id":"fake-thread-0","status":{"type":"notLoaded"}}}),
    )
    .await;
    for method in ["thread/turns/list", "thread/read"] {
        let request = peer.write(method).await;
        let result = if method == "thread/read" {
            json!({"thread":{"id":"fake-thread-0","status":{"type":"notLoaded"}}})
        } else {
            json!({"data":[{"id":"old-turn","threadId":"fake-thread-0","status":status,
                "items":[{"id":"answer","type":"agentMessage","text":"ended while detached"}]}]})
        };
        peer.reply(&request, result).await;
    }
}

#[tokio::test]
async fn a_reattached_turn_that_ended_while_detached_is_settled_from_native_history() {
    let state = State::new().await;
    disconnected_running_turn(&state).await;
    // The supervisor kept the provider: the next worker reaches generation 1.
    let mut peer = Peer::identity("default", 1, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    reply_turn_history(&mut peer, "completed").await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["status"], "completed", "{record}");
    assert_eq!(record["lastCompletedTurn"], "old-turn", "{record}");
    assert_eq!(record["lastAnswer"], "ended while detached", "{record}");
    assert_eq!(state.event_status("input").await, "delivered");
    assert!(
        peer.writes.try_recv().is_err(),
        "settling the ended turn sends nothing"
    );
    worker.shutdown().await.unwrap();
}

/// A reattached turn that is still running keeps running: the one read of this
/// connection records the check and sends nothing.
#[tokio::test]
async fn a_reattached_turn_still_running_only_records_the_check() {
    let state = State::new().await;
    disconnected_running_turn(&state).await;
    let mut peer = Peer::identity("default", 1, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    answer_adopted_turn_check(&mut peer).await;
    worker_barrier(&worker).await;
    let record = state.record().await;
    assert_eq!(record["status"], "running", "{record}");
    assert_eq!(record["turnId"], "old-turn", "{record}");
    assert_eq!(record["inFlight"], true, "{record}");
    assert!(record["connectionCheck"].is_object(), "{record}");
    assert!(
        peer.writes.try_recv().is_err(),
        "a running turn gets one read"
    );
    worker.shutdown().await.unwrap();
}
