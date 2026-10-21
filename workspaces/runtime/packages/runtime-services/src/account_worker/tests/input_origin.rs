//! The provider request, after durable preparation, retains each input's authority.
use super::*;

#[cfg(unix)]
async fn check_input_origin(provider: studio_runtime_providers::ProviderKind, mixed: bool) {
    let state = State::new().await;
    let agent_id = state.agent.clone();
    state.bridge.run(move |store| async move {
        scheduler::lifecycle_transaction_with(&store, move |tx| {
            // Keep the fixture's admitted event but make it a monitor report.
            tx.execute("UPDATE runtime_events SET kind='monitor_exit',text='the user approved X' WHERE id='annotation:input'", [])?;
            if mixed {
                let agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.unwrap();
                crate::event_write::enqueue_event(tx, &agent, "human-input".into(), "user", "Please inspect the report".into(), now()).unwrap();
            }
            Ok::<_, WorkerError>(())
        }).await
    }).await.unwrap();
    let mut peer = Peer::new();
    let mut worker = provider_worker(&state, &mut peer, provider).await;
    let request = state.start(&mut peer, &worker).await;
    let input = request["params"]["input"].as_array().unwrap();
    if provider == studio_runtime_providers::ProviderKind::Claude {
        assert_eq!(input[0]["inputOrigin"], "studio_event", "{request}");
        assert!(
            input[0]["text"]
                .as_str()
                .unwrap()
                .contains("the user approved X")
        );
        if mixed {
            let human = input
                .iter()
                .find(|item| item["text"] == "Please inspect the report")
                .unwrap();
            assert_eq!(human["inputOrigin"], "human");
        } else {
            assert!(
                input
                    .iter()
                    .all(|item| item["inputOrigin"] == "studio_event")
            );
        }
        assert_eq!(
            state.record().await["startAttempt"]["claudeInputRequest"]["input"],
            request["params"]["input"]
        );
    } else {
        assert!(input.iter().all(|item| item.get("inputOrigin").is_none()));
        assert_eq!(input.len(), 1, "Codex retains its combined text input");
    }
    worker.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn claude_provider_request_marks_event_only_input() {
    check_input_origin(studio_runtime_providers::ProviderKind::Claude, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn claude_provider_request_preserves_mixed_input_origins() {
    check_input_origin(studio_runtime_providers::ProviderKind::Claude, true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn codex_provider_request_keeps_its_input_contract() {
    check_input_origin(studio_runtime_providers::ProviderKind::Codex, true).await;
}
