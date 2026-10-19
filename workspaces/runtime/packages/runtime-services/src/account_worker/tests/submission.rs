//! The submitted commit runs in the agent's own task: one agent's held commit
//! neither writes early nor holds back another agent's write.
use super::*;

async fn scenario_held_commit_does_not_hold_back_another_agent(
    provider: studio_runtime_providers::ProviderKind,
) {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = provider_worker(&state, &mut peer, provider).await;
    let (first, second) = (state.agent.clone(), "second-agent".to_owned());
    let copy = second.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(first.as_str()))?.unwrap();
                row.id = AgentId::from(copy.as_str());
                row.root_id = AgentId::from(copy.as_str());
                row.extra.insert("threadId".into(), json!("fake-thread-1"));
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    state.enqueue_for(&second, "second-input").await;
    *state.bridge.faults.pause_operation.lock().unwrap() = Some("submitted".into());
    state.wake(&worker).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.bridge.faults.paused.notified(),
    )
    .await
    .expect("the first agent's submitted commit is held");
    *state.bridge.faults.pause_operation.lock().unwrap() = None;
    // FULL commit before write: nothing of the held request reached the provider.
    assert!(peer.writes.try_recv().is_err());
    worker
        .send(AccountCommand::Wake {
            agent_id: second.clone(),
            epoch: state.epoch,
        })
        .await
        .unwrap();
    let resume = peer.write("thread/resume").await;
    assert_eq!(resume["params"]["threadId"], "fake-thread-1", "{resume}");
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-1"}}))
        .await;
    let turn = peer.write("turn/start").await;
    assert!(
        turn["params"]["input"].to_string().contains("second-input"),
        "{turn}"
    );
    state.bridge.faults.release.notify_one();
    // The held request is written once, after its commit returns.
    let resume = peer.write("thread/resume").await;
    assert_eq!(resume["params"]["threadId"], "fake-thread-0", "{resume}");
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn codex_held_commit_does_not_hold_back_another_agent() {
    scenario_held_commit_does_not_hold_back_another_agent(
        studio_runtime_providers::ProviderKind::Codex,
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn claude_held_commit_does_not_hold_back_another_agent() {
    scenario_held_commit_does_not_hold_back_another_agent(
        studio_runtime_providers::ProviderKind::Claude,
    )
    .await;
}
