use super::*;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository};

async fn spawn_permission_case(
    provider: &str,
    mode: Option<&str>,
    yolo: bool,
    role: &str,
    override_mode: Option<Value>,
    expected: &str,
    nested: bool,
) {
    let (directory, store, context) = fixture().await;
    let mut parent = {
        let read = store.test_reader().unwrap();
        AgentRepository::get(&read, &AgentId::from(if nested { WORKER } else { LEAD }))
            .unwrap()
            .unwrap()
            .value()
    };
    parent["provider"] = json!(provider);
    parent["yoloMode"] = json!(yolo);
    parent["claudeOptions"] = mode
        .map(|mode| json!({"permissionMode": mode, "thinking": false}))
        .unwrap_or(json!({}));
    let saved = parent.clone();
    store
        .transaction_with::<_, StoreError>(move |tx| {
            let mut root = AgentRepository::get(tx, &AgentId::from(LEAD))?
                .unwrap()
                .value();
            root["workerDefaults"] = json!({"model": "claude-haiku-4-5", "delegationDepth": 2});
            if !nested {
                root = saved.clone();
                root["workerDefaults"] = json!({"model": "claude-haiku-4-5", "delegationDepth": 2});
            } else {
                AgentRepository::put(tx, &AgentRecord::from_value(saved).unwrap())?;
            }
            AgentRepository::put(tx, &AgentRecord::from_value(root).unwrap())
        })
        .await
        .unwrap();
    let parent = AgentRepository::get(
        &store.test_reader().unwrap(),
        &AgentId::from(parent["id"].as_str().unwrap()),
    )
    .unwrap()
    .unwrap()
    .value();
    let actor = Actor::from_record(&parent, parent["id"].as_str().unwrap(), context.epoch).unwrap();
    let mut spec =
        json!({"name": "Child", "prompt": "Do the task", "role": role, "workspace": "shared"});
    if role == "reviewer" {
        spec.as_object_mut().unwrap().remove("workspace");
    }
    if let Some(mode) = override_mode {
        spec["permission_mode"] = mode;
    }
    let args = json!({"agents": [spec]});
    let spawned = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &args,
        "claude-permissions",
        20.0,
        &CreationEnvironment,
    )
    .await;
    if expected.is_empty() {
        assert!(spawned.is_err(), "Invalid permission modes must fail");
        let child_id =
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, b"claude-permissions:0").to_string();
        assert!(
            AgentRepository::get(
                &store.test_reader().unwrap(),
                &AgentId::from(child_id.as_str())
            )
            .unwrap()
            .is_none()
        );
        return;
    }
    let spawned = spawned.unwrap();
    let id = AgentId::from(spawned.value["agents"][0]["id"].as_str().unwrap());
    let child = AgentRepository::get(&store.test_reader().unwrap(), &id)
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(child["claudeOptions"]["permissionMode"], expected);
    assert_eq!(child["yoloMode"], expected == "bypassPermissions");
    assert!(
        child["claudeOptions"].get("thinking").is_none(),
        "Only the permission mode is inherited"
    );
    let replay = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &args,
        "claude-permissions",
        21.0,
        &CreationEnvironment,
    )
    .await
    .unwrap();
    assert_eq!(replay.value["agents"], spawned.value["agents"]);
    drop(store);
    let reopened = RuntimeStore::open(Some(directory.path())).await.unwrap();
    let restored = AgentRepository::get(&reopened.test_reader().unwrap(), &id)
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(restored["claudeOptions"]["permissionMode"], expected);
}

#[tokio::test]
async fn claude_child_inherits_auto_plan_accept_edits_and_default() {
    for mode in [
        "auto",
        "plan",
        "acceptEdits",
        "default",
        "bypassPermissions",
    ] {
        spawn_permission_case(
            "claude",
            Some(mode),
            mode == "bypassPermissions",
            "implementer",
            None,
            mode,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn claude_child_uses_immediate_parent_instead_of_root() {
    spawn_permission_case(
        "claude",
        Some("plan"),
        false,
        "implementer",
        None,
        "plan",
        true,
    )
    .await;
}

#[tokio::test]
async fn claude_child_reviewer_stays_plan_even_with_bypass_override() {
    spawn_permission_case(
        "claude",
        Some("bypassPermissions"),
        true,
        "reviewer",
        Some(json!("bypassPermissions")),
        "plan",
        false,
    )
    .await;
}

#[tokio::test]
async fn claude_child_cannot_escalate_to_bypass() {
    for mode in ["auto", "plan", "acceptEdits", "default"] {
        spawn_permission_case(
            "claude",
            Some(mode),
            true,
            "implementer",
            Some(json!("bypassPermissions")),
            mode,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn claude_child_accepts_explicit_mode_override() {
    spawn_permission_case(
        "claude",
        Some("auto"),
        false,
        "implementer",
        Some(json!("acceptEdits")),
        "acceptEdits",
        false,
    )
    .await;
}

#[tokio::test]
async fn claude_child_maps_legacy_and_codex_yolo() {
    for provider in ["claude", "codex"] {
        for (yolo, expected) in [(false, "default"), (true, "bypassPermissions")] {
            spawn_permission_case(provider, None, yolo, "implementer", None, expected, false).await;
        }
    }
}

#[tokio::test]
async fn claude_child_rejects_invalid_override_without_child_record() {
    for mode in [json!("unknown"), json!(true), Value::Null] {
        spawn_permission_case(
            "claude",
            Some("auto"),
            false,
            "implementer",
            Some(mode),
            "",
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn claude_child_reads_current_parent_permission_at_commit() {
    let (_directory, store, context) = fixture().await;
    let mut parent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from(LEAD))
        .unwrap()
        .unwrap()
        .value();
    parent["provider"] = json!("claude");
    parent["claudeOptions"] = json!({"permissionMode": "bypassPermissions"});
    parent["workerDefaults"] = json!({"model": "claude-haiku-4-5"});
    let actor = Actor::from_record(&parent, LEAD, context.epoch).unwrap();
    parent["claudeOptions"]["permissionMode"] = json!("plan");
    parent["yoloMode"] = json!(false);
    store
        .transaction_with::<_, StoreError>(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(parent).unwrap())
        })
        .await
        .unwrap();
    let spawned = crate::managed_child_create::spawn_agents(&store, &actor,
        &json!({"agents": [{"name": "Child", "prompt": "Do the task", "workspace": "shared", "permission_mode": "bypassPermissions"}]}),
        "current-permissions", 20.0, &CreationEnvironment).await.unwrap();
    let id = AgentId::from(spawned.value["agents"][0]["id"].as_str().unwrap());
    let child = AgentRepository::get(&store.test_reader().unwrap(), &id)
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(child["claudeOptions"]["permissionMode"], "plan");
    assert_eq!(child["yoloMode"], false);
}
