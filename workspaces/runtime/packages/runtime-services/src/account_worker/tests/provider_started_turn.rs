//! A turn that the provider starts on its own binds the idle agent. Claude
//! starts one when a background task of the session finishes after the last
//! turn ended. Before the binding, the store kept the agent `completed` while
//! the turn ran, so each managed tool call of the turn was answered "Managed
//! tool callback has no unique active turn for this account generation"
//! (Lumina lead 841ebc0e, 20 to 21 October 2026: 57 failed calls, every one
//! in such a turn).
use super::tool_reply_retirement::running_tool_turn;
use super::*;

/// The turn id the provider chose for its own turn.
const PROVIDER_TURN: &str = "provider-turn";

async fn frame_acknowledged(peer: &mut Peer, value: Value) {
    let cursor = peer.frame(value).await;
    peer.acknowledged(cursor).await;
}

/// One managed tool call of `turn`; returns the callback reply.
async fn tool_call(peer: &mut Peer, turn: &str) -> Value {
    let cursor = peer
        .frame(json!({
            "id":"provider-turn-call",
            "method":"item/tool/call",
            "params":{
                "threadId":"fake-thread-0",
                "turnId":turn,
                "callId":"provider-turn-status",
                "tool":"orchestration_status",
                "arguments":{}
            }
        }))
        .await;
    let callback = peer.next_write().await;
    assert_eq!(callback.value["id"], "provider-turn-call");
    callback
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .expect("callback write receipt receiver");
    peer.acknowledged(cursor).await;
    callback.value
}

async fn scenario_provider_started_turn_answers_its_tool_calls(
    provider: studio_runtime_providers::ProviderKind,
) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = running_tool_turn(&state, &mut peer, provider).await;
    peer.completed().await;
    assert_eq!(state.record().await["status"], "completed");

    frame_acknowledged(
        &mut peer,
        json!({"method":"turn/started","params":{
            "threadId":"fake-thread-0","turn":{"id":PROVIDER_TURN,"status":"inProgress"}
        }}),
    )
    .await;
    let record = state.record().await;
    assert_eq!(record["status"], "running", "{record}");
    assert_eq!(record["turnId"], PROVIDER_TURN, "{record}");
    assert_eq!(record["inFlight"], true, "{record}");

    let reply = tool_call(&mut peer, PROVIDER_TURN).await;
    assert_eq!(
        reply["result"]["success"], true,
        "a tool call of the provider's own turn is answered: {reply}"
    );
    assert!(reply["result"].to_string().contains("answered"), "{reply}");

    frame_acknowledged(
        &mut peer,
        json!({"method":"turn/completed","params":{
            "threadId":"fake-thread-0","turn":{"id":PROVIDER_TURN,"status":"completed"}
        }}),
    )
    .await;
    let record = state.record().await;
    assert_eq!(record["status"], "completed", "{record}");
    assert_eq!(record["inFlight"], false, "{record}");
    assert_eq!(record["lastCompletedTurn"], PROVIDER_TURN, "{record}");
    worker.shutdown().await.expect("worker shutdown");
}

/// A stopped agent does not take over a turn its provider started.
async fn scenario_stopped_agent_does_not_bind_a_provider_turn(
    provider: studio_runtime_providers::ProviderKind,
) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = running_tool_turn(&state, &mut peer, provider).await;
    peer.completed().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.autoWake',json('false')) WHERE id=?1",
                        [agent],
                    )?;
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("stop the agent");
    frame_acknowledged(
        &mut peer,
        json!({"method":"turn/started","params":{
            "threadId":"fake-thread-0","turn":{"id":PROVIDER_TURN,"status":"inProgress"}
        }}),
    )
    .await;
    let record = state.record().await;
    assert_eq!(record["status"], "completed", "{record}");
    assert_ne!(record["turnId"], PROVIDER_TURN, "{record}");
    let reply = tool_call(&mut peer, PROVIDER_TURN).await;
    assert_eq!(reply["result"]["success"], false, "{reply}");
    worker.shutdown().await.expect("worker shutdown");
}

#[tokio::test]
async fn claude_provider_started_turn_answers_its_tool_calls() {
    scenario_provider_started_turn_answers_its_tool_calls(
        studio_runtime_providers::ProviderKind::Claude,
    )
    .await;
}

#[tokio::test]
async fn codex_provider_started_turn_answers_its_tool_calls() {
    scenario_provider_started_turn_answers_its_tool_calls(
        studio_runtime_providers::ProviderKind::Codex,
    )
    .await;
}

#[tokio::test]
async fn claude_stopped_agent_does_not_bind_a_provider_turn() {
    scenario_stopped_agent_does_not_bind_a_provider_turn(
        studio_runtime_providers::ProviderKind::Claude,
    )
    .await;
}
