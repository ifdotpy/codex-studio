use super::*;

#[cfg(unix)]
pub(super) fn claude_resources(state: &State) -> preparation::PreparationResources {
    let mut resources = super::test_resources().unwrap();
    let home = state.root.join("claude-home");
    let registry = state.root.join("accounts/registry.json");
    let binary = state.root.join("fake-claude");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    std::fs::write(
        &binary,
        "#!/bin/sh\nprintf '%s' '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"fixture@example.test\",\"subscriptionType\":\"max\"}'\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = "claude:fixture@example.test";
    std::fs::write(
        &registry,
        json!({
            "version": 1,
            "defaultAccountKey": "default",
            "accounts": {"default": {
                "id": "default", "home": home, "label": "Fixture Claude",
                "source": "Claude Code", "status": "ready", "provider": "claude",
                "email": "fixture@example.test", "accountId": identity,
                "_credentialIdentity": identity, "plan": "max",
                "claudeOptions": {"binaryPath": binary, "configDir": home, "customModels": []}
            }},
            "logins": {}, "deleteReceipts": {}
        })
        .to_string(),
    )
    .unwrap();
    let driver = Arc::new(crate::account_driver::AccountDriver::open(&registry).unwrap());
    resources.claude_account_pin = driver.internal_record("default").unwrap();
    resources.account_driver = Some(driver);
    resources
}

#[cfg(unix)]
#[tokio::test]
async fn claude_turn_freezes_exact_input_before_native_write() {
    let state = State::new().await;
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                row.extra.insert("provider".into(), json!("claude"));
                row.extra.insert("nativeEffort".into(), json!("medium"));
                row.extra.insert("claudeOptions".into(), json!({}));
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    peer.claude = true;
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), claude_resources(&state))
        .await;
    let start = state.start(&mut peer, &worker).await;
    let record = state.record().await;
    assert_eq!(
        record["startAttempt"]["claudeInputRequest"]["input"],
        start["params"]["input"]
    );
    assert_eq!(
        record["startAttempt"]["connectionId"],
        worker.scope.connection
    );
    assert!(start["params"]["dynamicTools"].is_array());
    assert!(start["params"].get("cyberAccessProgram").is_none());
    peer.accepted(&start).await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}
