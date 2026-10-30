//! Tool results and callback identity fences survive transport retirement.
//! Written responses retire from the outbox; uncertain responses stay held.
//! Outstanding response bodies never turn a committed tool into a refusal.
#![cfg(unix)]
use super::*;
use crate::tool_requests::{self, CallbackTask};
use rusqlite::OptionalExtension;
use studio_runtime_schema::persistence::{
    OutboxKind, OutboxStatus, ToolRequestKind, ToolRequestStatus,
};
use studio_runtime_store::outbox::Outbox;

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
        _action: crate::managed_status_watch::WatchAction,
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
                value: studio_runtime_schema::raw_json::RawJson(serde_json::value::to_raw_value(&Value::Null).expect("JSON")),
                cancel_monitor_ids: Vec::new(),
            })
        })
    }
}

/// Canonical response obligations and accepted callbacks still owing delivery.
async fn delivery_rows(state: &State) -> (i64, i64) {
    state.bridge.run(|store|async move {
        let db=store.test_reader()?;
        Ok((db.query_row("SELECT count(*) FROM runtime_outbox WHERE kind=?1",[OutboxKind::ProviderCallback],|row|row.get(0))?,
            db.query_row("SELECT count(*) FROM runtime_tool_requests WHERE kind=?1 AND status IN (?2,?3,?4) AND json_type(record,'$.payload.accepted')='object'",rusqlite::params![ToolRequestKind::ProviderCallback,ToolRequestStatus::Queued,ToolRequestStatus::Running,ToolRequestStatus::Unknown],|row|row.get(0))?))
    }).await.expect("canonical callback obligations")
}
async fn delivery_states(state: &State) -> Vec<OutboxStatus> {
    state
        .bridge
        .run(|store| async move {
            let db = store.test_reader()?;
            let mut query =
                db.prepare("SELECT status FROM runtime_outbox WHERE kind=?1 ORDER BY id")?;
            Ok(query
                .query_map([OutboxKind::ProviderCallback], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .expect("delivery states")
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
                                stream_id: "default".into(),
                                connection: "1".into(),
                                generation: 1,
                                epoch: 0,
                                thread: "fake-thread-0".into(),
                                turn: "turn".into(),
                            },
                            rpc_id: studio_runtime_providers::RequestId::Number(index as u64),
                            attempt: format!("attempt-{index}"),
                        };
                        let input=json!({"id":index,"method":"item/tool/call","params":{"threadId":"fake-thread-0","turnId":"turn","callId":format!("leaked-{index}"),"tool":"orchestration_status","arguments":{}}});
                        let response=json!({"id":index,"result":{"success":true,"contentItems":[]}});
                        let studio_runtime_providers::DecodedJournalPayload::Frame(input)=studio_runtime_providers::decode_journal_value(1,1,&input).payload else { panic!("input frame") };
                        let studio_runtime_providers::DecodedJournalPayload::Frame(response)=studio_runtime_providers::decode_journal_value(1,1,&response).payload else { panic!("response frame") };
                        let reply=crate::provider_callbacks::save_frame_response(tx,&identity.context,&input,&response).map_err(|error|StoreError::SchemaMismatch(error.to_string()))?;
                        let key=crate::provider_callbacks::callback_origin_key(&reply.context,&reply.rpc_id);
                        let mut obligation=Outbox::get::<crate::provider_callbacks::CallbackResponse>(tx,&key)?.expect("prepared response");
                        obligation.payload.tool_request=Some(format!("leaked-{index}"));
                        Outbox::put(tx,&obligation)?;
                    }
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("seed prepared replies of the live turn");
    answered_tool_call(&state, &mut peer, FORMER_REPLY_CAP).await;
    assert_eq!(
        delivery_rows(&state).await,
        (FORMER_REPLY_CAP as i64, 0),
        "the written tool response retires; existing pending obligations remain"
    );
    worker.shutdown().await.expect("worker shutdown");
}

/// A written answer retires its obligation. Unknown and lost outcomes remain
/// held; a proven NotSent permits only a new delivery of the saved tool result.
async fn scenario_callback_delivery_preserves_uncertainty(
    provider: studio_runtime_providers::ProviderKind,
) {
    for outcome in [
        Some(WriteOutcome::Written),
        Some(WriteOutcome::Unknown),
        Some(WriteOutcome::NotSent),
        None,
    ] {
        let state = State::new().await;
        let mut peer = Peer::new();
        let mut worker = running_tool_turn(&state, &mut peer, provider).await;
        let (cursor, callback) = tool_call(&mut peer, 0).await;
        let original = callback.value.clone();
        assert_eq!(
            delivery_rows(&state).await,
            (1, 1),
            "{outcome:?}: one canonical response before its receipt"
        );
        assert_eq!(delivery_states(&state).await, [OutboxStatus::Submitted]);
        match outcome {
            Some(WriteOutcome::Written) => drop(callback.receipt.send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            })),
            Some(WriteOutcome::Unknown) => drop(callback.receipt.send(CommandReceipt::Unknown)),
            Some(WriteOutcome::NotSent) => {
                drop(callback.receipt.send(CommandReceipt::NotSent));
                let retry = peer.next_write().await;
                assert_eq!(
                    retry.value, original,
                    "NotSent reuses the exact completed tool result"
                );
                drop(retry.receipt.send(CommandReceipt::Written {
                    remote_id: None,
                    duplicate: false,
                }));
            }
            None => drop(callback.receipt),
        }
        let shutdown = worker.shutdown().await;
        if matches!(outcome, Some(WriteOutcome::Written | WriteOutcome::NotSent)) {
            shutdown.expect("written callback shutdown");
            assert_eq!(
                delivery_rows(&state).await,
                (0, 0),
                "written response body retires"
            );
        } else {
            assert!(
                matches!(shutdown, Err(WorkerError::CallbackUnknown)),
                "{outcome:?}: {shutdown:?}"
            );
            assert_eq!(
                delivery_rows(&state).await,
                (1, 1),
                "{outcome:?}: uncertainty is retained"
            );
            assert_eq!(delivery_states(&state).await, [OutboxStatus::Unknown]);
        }
        let task = tool_requests::get_callback::<
            crate::account_worker::callback_record::CallbackObligation,
        >(
            &state.bridge.store.test_reader().unwrap(),
            &format!("callback:default:1:{cursor}"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            task.status,
            if matches!(outcome, Some(WriteOutcome::Written | WriteOutcome::NotSent)) {
                ToolRequestStatus::Completed
            } else {
                ToolRequestStatus::Unknown
            }
        );
        let db = state.bridge.store.test_reader().unwrap();
        let raw: Option<String> = db
            .query_row(
                "SELECT record FROM runtime_tool_requests WHERE id=?1",
                [&task.payload.root_request_id],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        if matches!(outcome, Some(WriteOutcome::Written | WriteOutcome::NotSent)) {
            assert!(
                raw.is_none(),
                "a delivered read has no retained callback owner"
            );
            let retained: i64 = db.query_row(
                "SELECT count(*) FROM runtime_tool_requests WHERE kind='managed_tool' AND json_extract(record,'$.callId')='status-0'",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(retained, 0, "the delivered read owner is removed");
        } else {
            let owner: CallbackTask<crate::provider_callbacks::CallbackOwner> =
                serde_json::from_str(&raw.expect("uncertain owner is retained")).unwrap();
            let token = owner.payload.admission.as_ref().unwrap();
            let saved = studio_runtime_store::request_results::RequestResults::replay(
                &db,
                token.id(),
                token.signature(),
                chrono::Utc::now().timestamp_millis(),
            )
            .unwrap();
            assert_eq!(owner.status, ToolRequestStatus::Unknown);
            assert!(
                saved.is_none(),
                "uncertain delivery has no fabricated completed response"
            );
            let same_body: bool = db.query_row(
                "SELECT json_extract(payload,'$.body.result')=json_extract(?1,'$.result') FROM runtime_outbox WHERE kind=?2 AND status=?3",
                rusqlite::params![original.to_string(), OutboxKind::ProviderCallback, OutboxStatus::Unknown],
                |row| row.get(0),
            ).unwrap();
            assert!(
                same_body,
                "uncertain delivery retains the exact pending reply"
            );
        }
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
async fn codex_callback_delivery_preserves_uncertainty() {
    scenario_callback_delivery_preserves_uncertainty(studio_runtime_providers::ProviderKind::Codex)
        .await;
}

#[tokio::test]
async fn claude_callback_delivery_preserves_uncertainty() {
    scenario_callback_delivery_preserves_uncertainty(
        studio_runtime_providers::ProviderKind::Claude,
    )
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
    assert_eq!(delivery_rows(&state).await, (1, 1));
    assert_eq!(delivery_states(&state).await, [OutboxStatus::Unknown]);
    let db = state.bridge.store.test_reader().unwrap();
    let task = tool_requests::get_callback::<
        crate::account_worker::callback_record::CallbackObligation,
    >(&db, &format!("callback:default:1:{cursor}"))
    .unwrap()
    .unwrap();
    assert_eq!(
        task.status,
        ToolRequestStatus::Unknown,
        "the uncertain obligation stays held"
    );
    assert!(
        task.payload.submitted.is_some(),
        "the exact response provenance stays with its owner"
    );
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
