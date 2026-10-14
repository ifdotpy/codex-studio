use super::*;
use crate::account_worker::retained::{
    PortError, RequestActor, RequestResult, RetainedSessionPort, SessionRequest,
};
use crate::{
    recovery::{RecoveryAction, RecoveryOutcome},
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
    recovery_store::CompletionWriter,
};
fn request(state: &State, port: &RetainedSessionPort, operation: &str) -> SessionRequest {
    SessionRequest {
        scope: port.scope().clone(),
        expected_scope: None,
        operation: operation.into(),
        actor: Some(RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        }),
        transfer: None,
        deleted_actor_session: None,
        method: Method::ThreadRead,
        params: json!({"threadId":"fake-thread-0","includeTurns":false}),
        deadline: (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std(),
    }
}

async fn recovered_publications(state: &State) -> Vec<(String, String)> {
    state
        .bridge
        .run(|store| {
            let db = store.read()?;
            let mut statement = db.prepare(
                "SELECT json_extract(result,'$.effect.Recovered.identity'), json_extract(result,'$.effect.Recovered.observation.method') FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:default:%:pending' AND json_extract(result,'$.effect.Recovered.identity') IS NOT NULL ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .expect("read recovered publication receipts")
}
#[tokio::test(start_paused = true)]
async fn retained_reply_replays_without_resending_and_rejects_changed_payload() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "history-1");
    let sender = port.clone();
    let first = call.clone();
    let task = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    peer.reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let receipt = task.await.unwrap().unwrap();
    assert_eq!(
        receipt.result,
        RequestResult::Reply(json!({"thread":{"id":"fake-thread-0"}}))
    );
    assert_eq!(
        port.request(call.clone()).await.unwrap().result,
        receipt.result
    );
    let mut altered = call;
    altered.params["limit"] = json!(2);
    assert_eq!(
        port.request(altered).await.unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_request_rejects_a_stale_expected_connection_before_dispatch() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let mut call = request(&state, &port, "expected-scope-mismatch");
    let mut expected = port.scope().clone();
    expected.connection = "stale-connection".into();
    call.expected_scope = Some(expected);
    assert_eq!(
        port.request(call).await.unwrap_err(),
        PortError::ScopeMismatch
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

async fn restart_history_case(outcome: &str) {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    let fail_terminal_settlement = outcome == "terminal_failure";
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .expect("seed agent");
                let event = "annotation:input";
                let attempt = json!({
                    "id":"restart-attempt","epoch":epoch,"accountKey":"default",
                    "threadId":"fake-thread-0","events":[event],"submitted":true,
                    "executionOutcome":"unknown","created":1.0,"connectionGeneration":1
                });
                agent.status = studio_runtime_store::AgentStatus::Queued;
                agent.extra.insert("threadId".into(), json!("fake-thread-0"));
                agent.extra.insert("autoWake".into(), json!(true));
                agent.extra.insert("inFlight".into(), json!(false));
                agent.extra.insert("error".into(), json!("Context repair waits for a confirmed input receipt: annotation:input"));
                agent.extra.insert("restartRecovery".into(), json!({"stage":"held","startAttempt":attempt}));
                agent.extra.insert("contextRepairWait".into(), json!({"error":"Context repair waits for a confirmed input receipt: annotation:input","scope":"native","events":[event],"nextCheckAt":0,"source":{"id":agent_id,"epoch":epoch,"accountKey":"default","threadId":"fake-thread-0","attemptId":null}}));
                agent.extra.remove("startAttempt");
                agent.extra.remove("disconnectRecovery");
                AgentRepository::put(tx, &agent)?;
                tx.execute("UPDATE runtime_events SET status='uncertain',error='Server restarted before delivery acknowledgement' WHERE id=?1", [event])?;
                if fail_terminal_settlement {
                    tx.execute(
                        &format!("CREATE TRIGGER fail_restart_transcript BEFORE INSERT ON runtime_items WHEN NEW.agent='{}' BEGIN SELECT RAISE(ABORT,'restart settlement failpoint'); END", agent_id),
                        [],
                    )?;
                }
                Ok::<(), WorkerError>(())
            })?;
            Ok(())
        })
        .await
        .expect("seed restart wait");

    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let history = peer.write("thread/turns/list").await;
    match outcome {
        "delivered" | "terminal" | "terminal_failure" => {
            let (status, items) = if outcome == "delivered" {
                ("inProgress", json!([]))
            } else {
                (
                    "completed",
                    json!([{"id":"answer","type":"agentMessage","text":"recovered answer"}]),
                )
            };
            peer.reply(
                &history,
                json!({"data":[{"id":"old-turn","threadId":"fake-thread-0","status":status,
                    "items":items,"clientUserMessageId":"annotation:input"}]}),
            )
            .await;
            if outcome == "terminal_failure" {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        if state.record().await["restartRecovery"]["stage"] == "held" {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("failed settlement rolls back to held receipt");
                assert_eq!(state.event_status("input").await, "uncertain");
                let agent = state.agent.clone();
                let item_id = format!("{agent}:answer");
                let transcript = state
                    .bridge
                    .run(move |store| {
                        Ok(studio_runtime_store::TranscriptRepository::item(
                            &store.read()?,
                            &agent,
                            &item_id,
                        )
                        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?)
                    })
                    .await
                    .unwrap();
                assert!(
                    transcript.is_none(),
                    "failed settlement rolls back transcript writes"
                );
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let record = state.record().await;
                        let settled = if outcome == "terminal" {
                            record["lastCompletedTurn"] == "old-turn"
                        } else {
                            record["restartRecovery"]["stage"] == "finished"
                        };
                        if settled {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("delivered receipt committed");
                if outcome == "terminal" {
                    assert_eq!(state.record().await["lastCompletedTurn"], "old-turn");
                    assert_eq!(state.event_status("input").await, "delivered");
                    let agent = state.agent.clone();
                    let item_id = format!("{agent}:answer");
                    let item = state
                        .bridge
                        .run(move |store| {
                            Ok(studio_runtime_store::TranscriptRepository::item(
                                &store.read()?,
                                &agent,
                                &item_id,
                            )
                            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?)
                        })
                        .await
                        .unwrap()
                        .expect("recovered transcript item");
                    assert_eq!(item.record["text"], "recovered answer");
                    assert_eq!(item.record["turnStatus"], "completed");
                    let publications = recovered_publications(&state).await;
                    assert_eq!(publications.len(), 2);
                    assert_eq!(publications[0].1, "item/completed");
                    assert_eq!(publications[1].1, "turn/completed");
                } else {
                    assert_eq!(state.record().await["turnId"], "old-turn");
                }
            }
            assert!(
                peer.writes.try_recv().is_err(),
                "delivered input is never submitted again"
            );
        }
        "not_applied" => {
            peer.reply(
                &history,
                json!({"data":[{
                    "id":"rejected","threadId":"fake-thread-0","status":"failed",
                    "items":[],"clientUserMessageId":"annotation:input","startOutcome":"not_applied"
                }]}),
            )
            .await;
            let thread = peer.write("thread/read").await;
            peer.reply(
                &thread,
                json!({"thread":{"id":"fake-thread-0","status":{"type":"idle"}}}),
            )
            .await;
            let resume = peer.write("thread/resume").await;
            peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
                .await;
            let start = peer.write("turn/start").await;
            peer.accepted(&start).await;
            assert!(peer.writes.try_recv().is_err());
        }
        "unavailable" => {
            let cursor = peer.frame(json!({"id":history["id"],"error":{"code":-32000,"message":"history unavailable"}})).await;
            peer.acknowledged(cursor).await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if state.record().await["restartRecovery"]["stage"] == "held" {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("unknown remains held");
            assert_eq!(state.event_status("input").await, "uncertain");
            assert!(
                peer.writes.try_recv().is_err(),
                "unknown input is never resubmitted"
            );
        }
        _ => unreachable!(),
    }
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn startup_first_pass_reconciles_exact_history_without_replaying_unknown_input() {
    for outcome in [
        "delivered",
        "terminal",
        "terminal_failure",
        "not_applied",
        "unavailable",
    ] {
        restart_history_case(outcome).await;
    }
}

#[tokio::test(start_paused = true)]
async fn restart_terminal_commit_before_wake_converges_on_next_worker_start() {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| {
            store.transaction_with(|tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .expect("seed agent");
                let attempt = json!({
                    "id":"restart-attempt","epoch":epoch,"accountKey":"default",
                    "threadId":"fake-thread-0","events":["annotation:input"],"submitted":true,
                    "executionOutcome":"unknown","created":1.0,"connectionGeneration":1
                });
                agent.status = studio_runtime_store::AgentStatus::Queued;
                agent.extra.insert("threadId".into(), json!("fake-thread-0"));
                agent.extra.insert("autoWake".into(), json!(true));
                agent.extra.insert("inFlight".into(), json!(false));
                agent.extra.insert("error".into(), json!("held"));
                agent.extra.insert("restartRecovery".into(), json!({"stage":"held","startAttempt":attempt}));
                agent.extra.insert("contextRepairWait".into(), json!({
                    "error":"held","scope":"native","events":["annotation:input"],"nextCheckAt":0,
                    "source":{"id":agent_id,"epoch":epoch,"accountKey":"default","threadId":"fake-thread-0","attemptId":null}
                }));
                agent.extra.remove("startAttempt");
                AgentRepository::put(tx, &agent)?;
                tx.execute("UPDATE runtime_events SET status='uncertain',error='Server restarted before delivery acknowledgement' WHERE id='annotation:input'", [])?;
                Ok::<(), WorkerError>(())
            })?;
            Ok(())
        })
        .await
        .unwrap();

    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            let raw: String = store.read()?.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [&agent],
                |row| row.get(0),
            )?;
            let snapshot = RecoverySnapshot::from_record(serde_json::from_str(&raw).unwrap())
                .map_err(|error| StoreError::SchemaMismatch(error.location))
                .map_err(WorkerError::from)?;
            let scope = ConnectionScope {
                account: "default".into(),
                connection: "restart-test".into(),
                generation: 1,
            };
            let turn = json!({
                "id":"old-turn","threadId":"fake-thread-0","status":"completed",
                "items":[{"id":"answer","type":"agentMessage","text":"recovered answer"}],
                "clientUserMessageId":"annotation:input"
            });
            let action = RecoveryAction::AcceptStart { turn };
            let mut completion = super::super::restart_recovery::RestartInputCompletion::default();
            let outcome = store.transaction_with(|tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.contextRepairWait.nextCheckAt',1) WHERE id=?1",
                    [&agent],
                )?;
                let stale_wait = completion
                    .complete(tx, &snapshot, &scope, &action)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                assert!(matches!(stale_wait, RecoveryOutcome::Superseded));
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.contextRepairWait.nextCheckAt',0) WHERE id=?1",
                    [&agent],
                )?;
                tx.execute(
                    "UPDATE runtime_events SET status='pending' WHERE id='annotation:input'",
                    [],
                )?;
                let stale_event = completion
                    .complete(tx, &snapshot, &scope, &action)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                assert!(matches!(stale_event, RecoveryOutcome::Superseded));
                tx.execute(
                    "UPDATE runtime_events SET status='uncertain' WHERE id='annotation:input'",
                    [],
                )?;
                completion
                    .complete(tx, &snapshot, &scope, &action)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))
            })?;
            assert!(matches!(outcome, RecoveryOutcome::Applied(_)));
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(state.record().await["lastCompletedTurn"], "old-turn");
    assert_eq!(state.event_status("input").await, "delivered");
    let agent = state.agent.clone();
    let item_id = format!("{agent}:answer");
    let item = state
        .bridge
        .run(move |store| {
            Ok(
                studio_runtime_store::TranscriptRepository::item(&store.read()?, &agent, &item_id)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?,
            )
        })
        .await
        .unwrap()
        .expect("terminal item is committed with the settlement");
    assert_eq!(item.record["turnStatus"], "completed");
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let publications = recovered_publications(&state).await;
    assert_eq!(publications.len(), 2);
    assert_eq!(publications[0].1, "item/completed");
    assert_eq!(publications[1].1, "turn/completed");
    assert!(
        peer.writes.try_recv().is_err(),
        "committed recovery does not resubmit input on restart"
    );
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_lost_waiter_preserves_unknown_until_exact_late_reply() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "lost-control");
    let sender = port.clone();
    let first = call.clone();
    let task = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    task.abort();
    let _ignored = task.await;
    assert_eq!(
        port.request(call.clone()).await.unwrap().result,
        RequestResult::Unknown
    );
    peer.reply(&wire, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    port.barrier(port.scope(), call.deadline).await.unwrap();
    assert!(matches!(
        port.request(call).await.unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_scope_epoch_and_expired_queue_refuse_before_write() {
    let state = State::new().await;
    let peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let mut call = request(&state, &port, "scope");
    call.scope.generation += 1;
    assert_eq!(port.request(call).await.unwrap_err(), PortError::Identity);
    let mut call = request(&state, &port, "epoch");
    call.actor.as_mut().expect("actor-bound request").epoch += 1;
    assert_eq!(port.request(call).await.unwrap_err(), PortError::Identity);
    let mut call = request(&state, &port, "expired");
    call.deadline = tokio::time::Instant::now().into_std();
    assert_eq!(port.request(call).await.unwrap_err(), PortError::NotSent);
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_authorization_refuses_deleted_agent() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| {
            store.transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id=?1",
                    [agent],
                )?;
                Ok(())
            })?;
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        port.request(request(&state, &port, "deleted-agent"))
            .await
            .unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn deleted_voice_stop_requires_the_exact_committed_session() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let actor = state.agent.clone();
    let epoch = state.epoch;
    let scope = port.scope().clone();
    let receipt_id = format!("agent-delete-voice:{actor}:{epoch}");
    let session = json!({
        "sessionId":"voice-session-7",
        "agentId":actor,
        "epoch":epoch,
        "accountKey":scope.account,
        "threadId":"fake-thread-0",
        "connectionId":scope.connection,
        "generation":scope.generation
    });
    let actor_for_store = actor.clone();
    state.bridge.run(move |store| {
        store.transaction_with(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id=?1",
                [&actor_for_store],
            )?;
            tx.execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'voice-delete',?2)",
                rusqlite::params![receipt_id, json!({"agent":actor_for_store,"epoch":epoch,"voiceEndSessions":[session]}).to_string()],
            )?;
            Ok::<(), studio_runtime_store::StoreError>(())
        })?;
        Ok(())
    }).await.unwrap();

    let mut call = request(&state, &port, "voice:delete-stop:voice-session-7");
    call.method = Method::ThreadRealtimeStop;
    call.params = json!({"threadId":"fake-thread-0"});
    call.expected_scope = Some(scope.clone());
    call.deleted_actor_session = Some("voice-session-7".into());
    let sender = port.clone();
    let task = tokio::spawn(async move { sender.request(call).await });
    let wire = peer.write("thread/realtime/stop").await;
    peer.reply(&wire, json!({})).await;
    assert_eq!(
        task.await.unwrap().unwrap().result,
        RequestResult::Reply(json!({}))
    );
    assert!(peer.writes.try_recv().is_err());

    let mut mismatched = request(&state, &port, "voice:delete-stop:unlisted-session");
    mismatched.method = Method::ThreadRealtimeStop;
    mismatched.params = json!({"threadId":"fake-thread-0"});
    mismatched.expected_scope = Some(scope);
    mismatched.deleted_actor_session = Some("unlisted-session".into());
    assert_eq!(
        port.request(mismatched).await.unwrap_err(),
        PortError::Identity
    );
    assert!(peer.writes.try_recv().is_err());

    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn repeated_retained_history_reads_observe_fresh_provider_state() {
    use crate::{
        account_worker::retained::consumers::HistoryPort,
        recovery_history::{HistoryMethod, HistoryReceipt},
        recovery_runtime::RetainedHistory,
    };

    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let retained = worker.retained_session();
    let scope = retained.scope().clone();
    let first_history = HistoryPort::new(
        retained.clone(),
        RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        tokio::runtime::Handle::current(),
    );
    let first_deadline =
        (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let params = json!({"threadId":"fake-thread-0","includeTurns":false});

    let first_scope = scope.clone();
    let first_params = params.clone();
    let first = tokio::task::spawn_blocking(move || {
        let mut history = first_history;
        history.request(
            &first_scope,
            HistoryMethod::Thread,
            first_params,
            first_deadline,
        )
    });
    let first_wire = peer.write("thread/read").await;
    let first_cursor = peer
        .frame(json!({
            "id":first_wire["id"],
            "result":{"thread":{"id":"fake-thread-0","revision":1}}
        }))
        .await;
    peer.acknowledged_or_worker_exits(first_cursor, &mut worker)
        .await;
    let first_result: HistoryReceipt = first.await.unwrap().unwrap();

    let second_scope = scope.clone();
    let second_params = params.clone();
    let second_deadline =
        (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let second_history = HistoryPort::new(
        retained,
        RequestActor {
            agent: state.agent.clone(),
            epoch: state.epoch,
        },
        tokio::runtime::Handle::current(),
    );
    let second = tokio::task::spawn_blocking(move || {
        let mut history = second_history;
        history.request(
            &second_scope,
            HistoryMethod::Thread,
            second_params,
            second_deadline,
        )
    });
    let mut second = second;
    let second_wire = tokio::select! {
        wire = peer.write("thread/read") => wire,
        _result = &mut second => panic!("repeated history read reused an earlier receipt"),
    };
    assert_eq!(second_wire["method"], "thread/read");
    assert_ne!(first_wire["id"], second_wire["id"]);
    let second_cursor = peer
        .frame(json!({
            "id":second_wire["id"],
            "result":{"thread":{"id":"fake-thread-0","revision":2}}
        }))
        .await;
    peer.acknowledged_or_worker_exits(second_cursor, &mut worker)
        .await;
    let second_result = second.await.unwrap().unwrap();
    assert_eq!(first_result.body["thread"]["revision"], 1);
    assert_eq!(second_result.body["thread"]["revision"], 2);
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_barrier_waits_for_committed_journal_without_owning_bridge() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("journal".into());
    let cursor = peer
        .frame(json!({"method":"test/barrier","params":{}}))
        .await;
    state.bridge.faults.paused.notified().await;
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let waiter = tokio::spawn(async move { port.barrier(port.scope(), deadline).await });
    // The same permit remains available while the journal/barrier is paused.
    state
        .bridge
        .run(|s| {
            let _: i64 = s.read()?.query_row("SELECT 1", [], |r| r.get(0))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!waiter.is_finished());
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_waiters();
    waiter.await.unwrap().unwrap();
    peer.acknowledged(cursor).await;
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn recovery_reconcile_wait_does_not_hold_store_bridge_permit() {
    use crate::{
        recovery::{RecoveryError, RecoveryOutcome},
        recovery_absence::RecoveryObservation,
        recovery_history::ConnectionScope,
        recovery_policy::RecoverySnapshot,
        recovery_runtime::{RecoveryJob, RecoveryRuntime, RetainedRecoveryReader},
        recovery_store::CompletionWriter,
        task_recovery_store::TaskCompletionWriter,
    };

    struct Completion;
    impl CompletionWriter for Completion {
        fn start_held(
            &mut self,
            _: &studio_runtime_store::Transaction<'_>,
            _: &Value,
            _: &str,
            _: &str,
        ) -> Result<(), RecoveryError> {
            Ok(())
        }
        fn complete(
            &mut self,
            _: &studio_runtime_store::Transaction<'_>,
            _: &RecoverySnapshot,
            _: &ConnectionScope,
            _: &crate::recovery::RecoveryAction,
        ) -> Result<RecoveryOutcome, RecoveryError> {
            Ok(RecoveryOutcome::Unconfirmed)
        }
    }
    struct TaskCompletion;
    impl TaskCompletionWriter for TaskCompletion {
        fn resolve_receipt(&mut self, receipt: &Value) -> Result<Value, RecoveryError> {
            Ok(receipt.clone())
        }
        fn complete(
            &mut self,
            _: &crate::task_recovery::TaskRecoverySnapshot,
            _: &ConnectionScope,
            _: Value,
        ) -> Result<bool, RecoveryError> {
            Ok(false)
        }
    }

    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    let mut record = state.record().await;
    record["inFlight"] = json!(true);
    record["status"] = json!("running");
    if let Some(agent) = record.as_object_mut() {
        agent.remove("startAttempt");
        agent.remove("turnId");
    }
    let snapshot = RecoverySnapshot::from_record(record).unwrap();
    let session = worker.retained_session();
    let scope = session.scope().clone();
    let observation = RecoveryObservation {
        scope: scope.clone(),
        at: 100.0,
        supervisor: None,
        quiet: false,
        journal_drained: false,
        account_reserved: true,
        tools_refreshing: false,
        preparation_pending: false,
        completed_turn: None,
    };
    let observation_scope = scope.clone();
    let observation_for_reader = observation.clone();
    let actor = RequestActor {
        agent: state.agent.clone(),
        epoch: state.epoch,
    };
    let store = state.bridge.store.clone();
    let history_port = crate::account_worker::retained::consumers::HistoryPort::new(
        session,
        actor,
        tokio::runtime::Handle::current(),
    );
    let reconcile = tokio::task::spawn_blocking(move || {
        let observation = move |current: &ConnectionScope| {
            (current == &observation_scope).then(|| observation_for_reader.clone())
        };
        let source: &crate::recovery_runtime::ObservationSource<'_> = &observation;
        let mut completion = Completion;
        let mut task_completion = TaskCompletion;
        let mut history = history_port;
        let mut reader = RetainedRecoveryReader::new(
            scope.clone(),
            "fake-thread-0".into(),
            &mut history,
            source,
        )
        .unwrap();
        let mut runtime = RecoveryRuntime {
            store: &store,
            observation: source,
            completion: &mut completion,
            task_completion: &mut task_completion,
        };
        runtime.reconcile(&RecoveryJob::Turn(snapshot), scope, &mut reader)
    });
    let request = peer.next_write().await;
    assert_eq!(request.value["method"], "thread/read");
    assert!(
        request
            .receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })
            .is_ok()
    );
    state
        .bridge
        .run(|store| {
            let _: i64 = store.read()?.query_row("SELECT 1", [], |row| row.get(0))?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!reconcile.is_finished());
    peer.reply(
        &request.value,
        json!({"thread":{"id":"fake-thread-0","status":{"type":"active"}}}),
    )
    .await;
    assert!(matches!(
        reconcile.await.unwrap().unwrap(),
        RecoveryOutcome::Unconfirmed
    ));
    worker.shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn retained_reply_failure_cannot_publish_a_positive_receipt() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "failed-journal");
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.write("thread/read").await;
    state.bridge.run(|s| s.transaction_with(|tx| { tx.execute_batch("CREATE TRIGGER fail_journal BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'account-worker:journal:%' BEGIN SELECT RAISE(ABORT,'journal failed'); END;")?; Ok(()) })).await.unwrap();
    peer.frame(json!({"id":wire["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    assert_eq!(waiter.await.unwrap().unwrap_err(), PortError::Unknown);
    assert!(worker.completion().await.is_err());
    state
        .bridge
        .run(|s| {
            s.transaction_with(|tx| {
                tx.execute_batch("DROP TRIGGER fail_journal")?;
                Ok(())
            })
        })
        .await
        .unwrap();
    // Same generation replay has the same operation receipt but no committed cursor.
    let peer = Peer::new();
    let mut replacement = peer.worker(state.bridge.clone()).await;
    assert_eq!(
        replacement
            .retained_session()
            .request(call)
            .await
            .unwrap()
            .result,
        RequestResult::Unknown
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_committed_callback_keeps_exact_response_and_sends_once() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let frame = Frame::Response {
        id: RequestId::String("native-callback".into()),
        body: ResponseBody::Result(json!({"answer":"approved"})),
        fields: Default::default(),
    };
    let deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    let sender = port.clone();
    let first = frame.clone();
    let task = tokio::spawn(async move {
        sender
            .submit_committed_frame(
                sender.scope(),
                "canonical-operation".into(),
                first,
                deadline,
            )
            .await
    });
    let wire = peer.next_write().await;
    assert_eq!(wire.value["id"], "native-callback");
    assert_eq!(wire.value["result"], json!({"answer":"approved"}));
    assert!(
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: Some(42),
                duplicate: false,
            })
            .is_ok()
    );
    assert_eq!(task.await.unwrap().unwrap(), WriteOutcome::Written);
    assert_eq!(
        port.submit_committed_frame(port.scope(), "canonical-operation".into(), frame, deadline)
            .await
            .unwrap(),
        WriteOutcome::Written
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_deadline_during_intent_never_writes_after_expiry() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("retained_submitted".into());
    let mut call = request(&state, &port, "expires-in-queue");
    call.deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(2)).into_std();
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    state.bridge.faults.paused.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert_eq!(waiter.await.unwrap().unwrap_err(), PortError::Unknown);
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    state.bridge.faults.release.notify_waiters();
    worker_barrier(&worker).await;
    call.deadline = (tokio::time::Instant::now() + std::time::Duration::from_secs(60)).into_std();
    assert_eq!(
        port.request(call).await.unwrap().result,
        RequestResult::NotSent
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn retained_response_before_write_receipt_stays_positive() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let port = worker.retained_session();
    let call = request(&state, &port, "reply-before-write");
    let sender = port.clone();
    let first = call.clone();
    let waiter = tokio::spawn(async move { sender.request(first).await });
    let wire = peer.next_write().await;
    let cursor = peer
        .frame(json!({"id":wire.value["id"],"result":{"thread":{"id":"fake-thread-0"}}}))
        .await;
    assert!(
        wire.receipt
            .send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false
            })
            .is_ok()
    );
    peer.acknowledged(cursor).await;
    assert!(matches!(
        waiter.await.unwrap().unwrap().result,
        RequestResult::Reply(_)
    ));
    assert!(matches!(
        port.request(call).await.unwrap().result,
        RequestResult::Reply(_)
    ));
    worker.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn claude_adapter_uses_retained_scoped_control_and_invalidates_on_stop() {
    use crate::claude_controls::service::{ControlScope, ControlSession};
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let session = std::sync::Arc::new(worker.claude_session().unwrap());
    let scope = ControlScope {
        agent: state.agent.clone(),
        epoch: state.epoch.try_into().unwrap(),
        request_id: Some("stop-task-request".into()),
    };
    let copy = session.clone();
    let call_scope = scope.clone();
    let task = tokio::spawn(async move {
        copy.call_scoped(
            call_scope,
            "claude/stopTask",
            json!({"threadId":"fake-thread-0","taskId":"task"}),
        )
        .await
    });
    let request = peer.write("claude/stopTask").await;
    peer.reply(&request, json!({"status":"stopped"})).await;
    assert_eq!(task.await.unwrap().unwrap(), json!({"status":"stopped"}));
    assert_eq!(
        session
            .call_scoped(
                scope.clone(),
                "claude/stopTask",
                json!({"threadId":"fake-thread-0","taskId":"task"})
            )
            .await
            .unwrap(),
        json!({"status":"stopped"})
    );
    worker.shutdown().await.unwrap();
    assert!(
        session
            .call_scoped(scope, "claude/state", json!({"threadId":"fake-thread-0"}))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn claude_adapter_distinguishes_not_sent_from_unknown() {
    use crate::claude_controls::service::ControlSession;
    use crate::claude_controls::{ControlError, service::ControlScope};
    use crate::claude_runtime::RetainedClaudeSession;

    for (request_id, receipt, expected) in [
        ("not-sent", CommandReceipt::NotSent, "not-sent"),
        ("unknown", CommandReceipt::Unknown, "unknown"),
    ] {
        let state = State::new().await;
        let mut peer = Peer::new();
        peer.claude = true;
        let mut worker = peer.worker(state.bridge.clone()).await;
        let session = RetainedClaudeSession::new(std::sync::Arc::new(
            crate::account_worker::retained::consumers::ClaudePort::new(worker.retained_session())
                .unwrap(),
        ));
        let scope = ControlScope {
            agent: state.agent.clone(),
            epoch: state.epoch.try_into().unwrap(),
            request_id: Some(request_id.into()),
        };
        let task = tokio::spawn(async move {
            session
                .call_scoped(
                    scope,
                    "claude/stopTask",
                    json!({"threadId":"fake-thread-0","taskId":"task"}),
                )
                .await
        });
        let wire = peer.next_write().await;
        assert_eq!(wire.value["method"], "claude/stopTask");
        let _ignored = wire.receipt.send(receipt);
        match expected {
            "not-sent" => assert!(matches!(task.await.unwrap(), Err(ControlError::NotSent))),
            _ => assert!(matches!(task.await.unwrap(), Err(ControlError::Unknown))),
        }
        worker.shutdown().await.unwrap();
    }
}
