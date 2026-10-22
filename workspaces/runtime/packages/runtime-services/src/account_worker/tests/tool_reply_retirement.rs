//! A managed tool call keeps no reply row: its durable tool result rebuilds
//! the answer. No row can leak on any write outcome, and no count of rows can
//! turn a committed tool call into a reported failure.
#![cfg(unix)]
use super::*;

/// The former per-account reply cap (deleted with B1); a call past it must
/// still succeed.
const FORMER_REPLY_CAP: usize = 128;

struct ImmediateStatus;

impl crate::provider_callbacks::tools::StatusWatchPort for ImmediateStatus {
    fn status<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _now: f64,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, crate::managed_host_tools::Error>> {
        Box::pin(async { Ok(json!({"status":"answered"})) })
    }

    fn rules<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _request_id: &'a str,
        _now: f64,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            crate::provider_callbacks::tools::RulesActionResult,
            crate::managed_host_tools::Error,
        >,
    > {
        Box::pin(async {
            Ok(crate::provider_callbacks::tools::RulesActionResult {
                value: Value::Null,
                cancel_monitor_ids: Vec::new(),
            })
        })
    }
}

/// All provider callback rows and callback reply links.
async fn reply_rows(state: &State) -> (i64, i64) {
    state
        .bridge
        .run(|store| async move {
            let connection = store.test_reader()?;
            let count = |sql: &str| connection.query_row(sql, [], |row| row.get::<_, i64>(0));
            Ok((
                count(
                    "SELECT count(*) FROM runtime_operation_receipts WHERE signature='provider-callback'",
                )?,
                count(
                    "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:callback:%:reply'",
                )?,
            ))
        })
        .await
        .expect("reply row counts")
}

pub(super) async fn running_tool_turn(
    state: &State,
    peer: &mut Peer,
    provider: studio_runtime_providers::ProviderKind,
) -> AccountHandle {
    let worker = provider_worker(state, peer, provider).await;
    let start = state.start(peer, &worker).await;
    peer.accepted(&start).await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store.transaction_with::<_, StoreError>(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.connectionId','1') WHERE id=?1",
                    [agent],
                )?;
                Ok(())
            }).await?;
            Ok(())
        })
        .await
        .expect("bind the live connection");
    state.bridge.set_managed_tool_runtime(
        crate::provider_callbacks::ManagedToolRuntime::from_ports_for_test(
            crate::provider_callbacks::ManagedToolRuntimePorts {
                status_watch: Some(Arc::new(ImmediateStatus)),
                child_creation: None,
                monitor_create: None,
                interrupt: None,
                monitor_input: None,
                monitor_cancel: None,
                work_action: None,
                late: Default::default(),
            },
        ),
    );
    worker
}

/// Sends one tool call and returns its callback write; it must carry the
/// tool's real result.
async fn tool_call(peer: &mut Peer, index: usize) -> (u64, Wire) {
    let id = format!("tool-call-{index}");
    let cursor = peer
        .frame(json!({
            "id":id,
            "method":"item/tool/call",
            "params":{
                "threadId":"fake-thread-0",
                "turnId":"turn",
                "callId":format!("status-{index}"),
                "tool":"orchestration_status",
                "arguments":{}
            }
        }))
        .await;
    let callback = peer.next_write().await;
    assert_eq!(callback.value["id"], id.as_str());
    assert_eq!(
        callback.value["result"]["success"], true,
        "tool call {index} must succeed: {}",
        callback.value
    );
    assert!(
        callback.value["result"].to_string().contains("answered"),
        "tool call {index} carries its real result: {}",
        callback.value
    );
    (cursor, callback)
}

async fn answered_tool_call(state: &State, peer: &mut Peer, index: usize) {
    let (cursor, callback) = tool_call(peer, index).await;
    callback
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .expect("callback write receipt receiver");
    peer.acknowledged(cursor).await;
    callbacks_retired(state).await;
}

/// Former leak: 128 unwritten replies of the live turn made the next call
/// report "Too many unresolved provider replies" after its effect committed.
async fn scenario_prepared_replies_of_the_live_turn_do_not_fail_a_tool_call(
    provider: studio_runtime_providers::ProviderKind,
) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = running_tool_turn(&state, &mut peer, provider).await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    for index in 0..FORMER_REPLY_CAP {
                        let identity = crate::provider_callbacks::ReplyIdentity {
                            context: crate::provider_callbacks::CallbackContext {
                                agent: agent.clone(),
                                account: "default".into(),
                                connection: "1".into(),
                                generation: 1,
                                epoch: 0,
                                thread: "fake-thread-0".into(),
                                turn: "turn".into(),
                            },
                            rpc_id: studio_runtime_providers::RequestId::Number(index as u64),
                            attempt: format!("attempt-{index}"),
                        };
                        let value = json!({
                            "identity":identity, "signature":"signature",
                            "body":{"result":{"success":true,"contentItems":[]}},
                            "delivery":"prepared", "toolRequest":format!("leaked-{index}"),
                        });
                        tx.execute(
                            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'provider-callback',?2)",
                            [crate::provider_callbacks::reply_key(&identity), value.to_string()],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("seed prepared replies of the live turn");
    for index in 0..=FORMER_REPLY_CAP {
        answered_tool_call(&state, &mut peer, index).await;
    }
    assert_eq!(
        reply_rows(&state).await,
        (FORMER_REPLY_CAP as i64, 0),
        "tool calls add no reply row or link"
    );
    worker.shutdown().await.expect("worker shutdown");
}

/// No write outcome (written, unknown, not sent, closed connection) leaves a
/// reply row behind: none is ever created.
async fn scenario_no_write_outcome_leaves_a_reply_row(
    provider: studio_runtime_providers::ProviderKind,
) {
    for outcome in ["written", "unknown", "not-sent", "closed"] {
        let state = State::new().await;
        let mut peer = Peer::new();
        let mut worker = running_tool_turn(&state, &mut peer, provider).await;
        let (_cursor, callback) = tool_call(&mut peer, 0).await;
        assert_eq!(
            reply_rows(&state).await,
            (0, 0),
            "{outcome}: before the receipt"
        );
        match outcome {
            "written" => drop(callback.receipt.send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })),
            "unknown" => drop(callback.receipt.send(CommandReceipt::Unknown)),
            "not-sent" => drop(callback.receipt.send(CommandReceipt::NotSent)),
            _ => drop(callback.receipt),
        }
        // An uncertain callback write ends the worker with that error.
        let shutdown = worker.shutdown().await;
        if outcome == "written" {
            shutdown.expect("worker shutdown");
        }
        assert_eq!(
            reply_rows(&state).await,
            (0, 0),
            "{outcome}: after the receipt"
        );
    }
}

#[tokio::test]
async fn codex_prepared_replies_of_the_live_turn_do_not_fail_a_tool_call() {
    scenario_prepared_replies_of_the_live_turn_do_not_fail_a_tool_call(
        studio_runtime_providers::ProviderKind::Codex,
    )
    .await;
}

#[tokio::test]
async fn claude_prepared_replies_of_the_live_turn_do_not_fail_a_tool_call() {
    scenario_prepared_replies_of_the_live_turn_do_not_fail_a_tool_call(
        studio_runtime_providers::ProviderKind::Claude,
    )
    .await;
}

#[tokio::test]
async fn codex_no_write_outcome_leaves_a_reply_row() {
    scenario_no_write_outcome_leaves_a_reply_row(studio_runtime_providers::ProviderKind::Codex)
        .await;
}

#[tokio::test]
async fn claude_no_write_outcome_leaves_a_reply_row() {
    scenario_no_write_outcome_leaves_a_reply_row(studio_runtime_providers::ProviderKind::Claude)
        .await;
}

/// With no reply row, only the callback record stops a second send: a replay
/// of a tool call whose write ended Unknown writes nothing to the provider and
/// keeps the record as uncertain.
async fn scenario_replay_after_an_unknown_write_never_sends_the_reply_again(
    provider: studio_runtime_providers::ProviderKind,
) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = running_tool_turn(&state, &mut peer, provider).await;
    let (cursor, callback) = tool_call(&mut peer, 0).await;
    let frame = json!({
        "id":"tool-call-0",
        "method":"item/tool/call",
        "params":{
            "threadId":"fake-thread-0",
            "turnId":"turn",
            "callId":"status-0",
            "tool":"orchestration_status",
            "arguments":{}
        }
    });
    callback
        .receipt
        .send(CommandReceipt::Unknown)
        .ok()
        .expect("callback write receipt receiver");
    let completion = tokio::time::timeout(std::time::Duration::from_secs(120), worker.completion())
        .await
        .expect("callback completion deadline");
    assert!(
        matches!(completion, Err(WorkerError::CallbackUnknown)),
        "{completion:?}"
    );
    let mut replay = Peer::new();
    let worker = provider_worker(&state, &mut replay, provider).await;
    replay.cursor = peer.cursor - 1;
    replay.frame(frame).await;
    worker_barrier(&worker).await;
    let mut sent = Vec::new();
    while let Ok(write) = replay.writes.try_recv() {
        sent.push(write.value);
    }
    assert!(
        sent.iter().all(|write| write["id"] != "tool-call-0"),
        "the reply is never written twice: {sent:?}"
    );
    assert_eq!(reply_rows(&state).await, (0, 0));
    let record = state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    Ok(["accepted", "submitted", "unknown"].map(|phase| {
                        store::receipt(tx, &format!("callback:default:1:{cursor}"), phase)
                            .expect("callback phase")
                            .is_some()
                    }))
                })
                .await
        })
        .await
        .unwrap();
    assert_eq!(record, [true; 3], "the uncertain record stays held");
}

#[tokio::test]
async fn codex_replay_after_an_unknown_write_never_sends_the_reply_again() {
    scenario_replay_after_an_unknown_write_never_sends_the_reply_again(
        studio_runtime_providers::ProviderKind::Codex,
    )
    .await;
}

#[tokio::test]
async fn claude_replay_after_an_unknown_write_never_sends_the_reply_again() {
    scenario_replay_after_an_unknown_write_never_sends_the_reply_again(
        studio_runtime_providers::ProviderKind::Claude,
    )
    .await;
}
