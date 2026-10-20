//! A managed tool reply that the worker wrote leaves no outstanding provider
//! callback row, so the account's reply limit counts only real in-flight calls.
use super::*;

/// The account's reply limit (provider_callbacks/model.rs MAX_PENDING).
const REPLY_LIMIT: usize = 128;

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

/// (outstanding replies, all provider callback rows, reply links)
async fn reply_rows(state: &State) -> (i64, i64, i64) {
    state
        .bridge
        .run(|store| async move {
            let connection = store.read()?;
            let count = |sql: &str| connection.query_row(sql, [], |row| row.get::<_, i64>(0));
            Ok((
                count(
                    "SELECT count(*) FROM runtime_operation_receipts WHERE signature='provider-callback' AND json_extract(result,'$.delivery') IN ('prepared','submitted','unknown')",
                )?,
                count(
                    "SELECT count(*) FROM runtime_operation_receipts WHERE signature='provider-callback'",
                )?,
                count(
                    "SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:callback:%:reply'",
                )?,
            ))
        })
        .await
        .unwrap()
}

async fn running_tool_turn(state: &State, peer: &mut Peer) -> AccountHandle {
    let worker = peer.worker(state.bridge.clone()).await;
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
        .unwrap();
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

async fn answered_tool_call(peer: &mut Peer, index: usize) {
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
    callback
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .expect("callback write receipt receiver");
    peer.acknowledged(cursor).await;
}

#[tokio::test]
async fn written_tool_replies_leave_no_outstanding_reply_past_the_account_limit() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = running_tool_turn(&state, &mut peer).await;
    // The call after the limit must succeed: no written reply stays outstanding.
    for index in 0..=REPLY_LIMIT {
        answered_tool_call(&mut peer, index).await;
    }
    assert_eq!(
        reply_rows(&state).await,
        (0, 0, 0),
        "written tool calls leave no reply row or link"
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn leaked_replies_of_a_settled_turn_do_not_block_a_new_tool_call() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = running_tool_turn(&state, &mut peer).await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    for index in 0..REPLY_LIMIT {
                        let identity = crate::provider_callbacks::ReplyIdentity {
                            context: crate::provider_callbacks::CallbackContext {
                                agent: agent.clone(),
                                account: "default".into(),
                                connection: "1".into(),
                                generation: 1,
                                epoch: 0,
                                thread: "fake-thread-0".into(),
                                turn: "earlier-turn".into(),
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
        .unwrap();
    assert_eq!(
        reply_rows(&state).await.0,
        REPLY_LIMIT as i64,
        "the account starts at its reply limit"
    );
    answered_tool_call(&mut peer, 0).await;
    assert_eq!(reply_rows(&state).await, (0, 0, 0));
    worker.shutdown().await.unwrap();
}
