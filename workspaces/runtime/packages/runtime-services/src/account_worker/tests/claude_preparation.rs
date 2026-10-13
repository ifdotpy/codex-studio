use super::*;

#[tokio::test(start_paused = true)]
async fn claude_turn_freezes_exact_input_before_native_write() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |s| {
            scheduler::lifecycle_transaction_with(s, |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                row.extra.insert("provider".into(), json!("claude"));
                row.extra.insert("nativeEffort".into(), json!("medium"));
                row.extra.insert("claudeOptions".into(), json!({}));
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    peer.claude = true;
    let mut worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    let record = state.record().await;
    assert_eq!(
        record["startAttempt"]["claudeInputRequest"]["input"],
        start["params"]["input"]
    );
    assert_eq!(record["startAttempt"]["connectionId"], "1");
    assert!(start["params"]["dynamicTools"].is_array());
    assert!(start["params"].get("cyberAccessProgram").is_none());
    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}
