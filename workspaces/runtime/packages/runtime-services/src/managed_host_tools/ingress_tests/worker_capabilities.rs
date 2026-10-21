use super::*;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository};

#[tokio::test]
async fn capability_profile_spawn_persists_limits_and_refuses_widening_or_unsupported_fields() {
    let (_directory, store, context) = fixture().await;
    store.transaction_with::<_, StoreError>(|tx| {
        let mut lead = AgentRepository::get(tx, &AgentId::from(LEAD))?.unwrap().value();
        lead["provider"] = json!("claude");
        lead["workerDefaults"] = json!({"model":"claude-haiku-4-5","delegationDepth":2});
        AgentRepository::put(tx, &AgentRecord::from_value(lead).unwrap())?;
        let profile: studio_runtime_store::ProfileRecord = serde_json::from_value(json!({
            "id":"limited","name":"Limited","role":"implementer","model":"claude-haiku-4-5","effort":null,"instructions":"Check the source",
            "allowedTools":["Read*"],"disallowedTools":["Bash"],"mcpServers":["docs"],"skills":["research"],"maxTurns":3
        })).unwrap();
        studio_runtime_store::ProfileRepository::save(tx, &profile)
    }).await.unwrap();
    let parent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from(LEAD))
        .unwrap()
        .unwrap()
        .value();
    let actor = Actor::from_record(&parent, LEAD, context.epoch).unwrap();
    let base = json!({"name":"Limited","prompt":"Read the source","workspace":"shared","profile_id":"limited"});
    for (field, bad) in [
        ("allowedTools", json!(["Write"])),
        ("mcpServers", json!(["other"])),
        ("skills", json!(["other"])),
        ("maxTurns", json!(4)),
    ] {
        let mut spec = base.clone();
        spec[field] = bad;
        let result = crate::managed_child_create::spawn_agents(
            &store,
            &actor,
            &json!({"agents":[spec]}),
            &format!("widen-{field}"),
            20.0,
            &CreationEnvironment,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains(field));
    }
    let mut spec = base.clone();
    spec["allowedTools"] = json!(["Read"]);
    spec["disallowedTools"] = json!([]);
    spec["mcpServers"] = json!([]);
    spec["skills"] = json!([]);
    spec["maxTurns"] = json!(1);
    let args = json!({"agents":[spec]});
    let spawned = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &args,
        "limited-spawn",
        20.0,
        &CreationEnvironment,
    )
    .await
    .unwrap();
    let id = AgentId::from(spawned.value["agents"][0]["id"].as_str().unwrap());
    let child = AgentRepository::get(&store.test_reader().unwrap(), &id)
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(
        child["capabilityProfile"],
        json!({"allowedTools":["Read"],"disallowedTools":["Bash"],"mcpServers":[],"skills":[],"maxTurns":1})
    );
    assert_eq!(child["profileInstructions"], "Check the source");
    let replay = crate::managed_child_create::spawn_agents(
        &store,
        &actor,
        &args,
        "limited-spawn",
        21.0,
        &CreationEnvironment,
    )
    .await
    .unwrap();
    assert_eq!(replay.value["agents"], spawned.value["agents"]);

    for field in ["allowedTools", "disallowedTools", "skills"] {
        let spec = json!({"name":"Unsupported","prompt":"Task","workspace":"shared","model":"gpt-6-astra",field:[]});
        let result = crate::managed_child_create::spawn_agents(
            &store,
            &actor,
            &json!({"agents":[spec]}),
            &format!("codex-{field}"),
            22.0,
            &CreationEnvironment,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains(field));
    }
}
