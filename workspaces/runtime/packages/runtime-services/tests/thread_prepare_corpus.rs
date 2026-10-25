//! Thread preparation holds, refusal boundaries, and scoped receipt settlement.
use serde_json::{Value, json};
use studio_runtime_services::thread_prepare::*;

fn baseline() -> Value {
    serde_json::from_str(include_str!("thread_prepare/snapshot.json")).expect("snapshot")
}
fn merge(target: &mut Value, patch: &Value) {
    // A presence-aware field becoming absent discards its former payload.
    if patch.get("state") == Some(&json!("absent")) {
        *target = patch.clone();
        return;
    }
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            if let Some(old) = target.get_mut(key) {
                merge(old, value);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = patch.clone();
    }
}
fn snapshot(value: &Value) -> Snapshot {
    serde_json::from_value(value.clone()).expect("typed snapshot")
}
fn attempt(outcome: Outcome) -> PreparedAttempt {
    match outcome {
        Outcome::Start(value) | Outcome::Resume(value) => *value,
        other => panic!("expected preparation, got {other:?}"),
    }
}
fn receipt(a: &PreparedAttempt) -> ProviderReceipt {
    serde_json::from_value(json!({"identity":a.receipt_identity(),"thread_id":"thread","fields":{"model":"native-model"},"reattached":false})).expect("receipt")
}
#[test]
fn every_hold_releases_and_accepts_without_consuming_input() {
    let base = snapshot(&baseline());
    for reason in [
        HoldReason::WorkspacePending,
        HoldReason::ConnectionUnavailable,
        HoldReason::ContextRepair,
        HoldReason::NativeRelease,
        HoldReason::AccountTransfer,
        HoldReason::PreparationPending,
        HoldReason::Inactive,
    ] {
        let mut s = base.clone();
        match reason {
            HoldReason::WorkspacePending => s.workspace = WorkspaceReadiness::Pending,
            HoldReason::ConnectionUnavailable => s.account.available = false,
            HoldReason::PreparationPending => {
                s.pending = Some(PendingPreparation {
                    agent_id: s.actor.id.clone(),
                    epoch: s.actor.epoch,
                    account_key: s.account.key.clone(),
                    request_digest: String::new(),
                    attempt_id: "old".into(),
                    connection_id: s.account.connection_id.clone(),
                    thread_id: None,
                })
            }
            HoldReason::Inactive => s.closed = true,
            _ => s.admission = Some(reason),
        }
        let unchanged = s.clone();
        assert_eq!(plan(&s, "attempt", "ticket"), Outcome::Hold(reason));
        assert_eq!(s, unchanged);
        // Adapter observation (ready workspace/account, resolved repair/release/
        // transfer, replacement connection, reopened runtime) clears the gate.
        let mut ready = base.clone();
        if reason == HoldReason::PreparationPending {
            ready.pending = s.pending;
            ready.account.connection_id = "replacement".into();
        }
        let prepared = attempt(plan(&ready, "attempt", "ticket"));
        ready.actor.prepare_attempt = Some(prepared.attempt_id.clone());
        assert!(
            matches!(
                reduce_receipt(
                    &prepared,
                    &ready,
                    Some(&receipt(&prepared)),
                    None,
                    json!(1000)
                ),
                ReceiptOutcome::Accepted { .. }
            ),
            "{reason:?}"
        );
    }
}

#[test]
fn named_refusals_have_no_intents_and_preserve_snapshot() {
    for refusal in [
        Refusal::ImageWorkspace,
        Refusal::VmWorkspace,
        Refusal::NativeCatalogMutation,
        Refusal::PortableHistoryImport,
        Refusal::FrozenImport,
        Refusal::BrowserRegistration,
        Refusal::Daybreak,
    ] {
        let mut s = snapshot(&baseline());
        match refusal {
            Refusal::ImageWorkspace => s.workspace = WorkspaceReadiness::Image,
            Refusal::VmWorkspace => s.workspace = WorkspaceReadiness::Vm,
            _ => s.unsupported = Some(refusal),
        }
        let before = s.clone();
        assert_eq!(plan(&s, "attempt", "ticket"), Outcome::Refuse(refusal));
        assert_eq!(turn_params(&s, &TurnInput::default()), Err(refusal));
        assert_eq!(s, before);
    }
}

#[test]
fn unknown_then_late_receipt_and_replaced_connection_are_safe() {
    let mut s = snapshot(&baseline());
    let old = attempt(plan(&s, "old", "ticket"));
    s.actor.prepare_attempt = Some("old".into());
    s.pending = Some(PendingPreparation {
        agent_id: s.actor.id.clone(),
        epoch: s.actor.epoch,
        account_key: s.account.key.clone(),
        request_digest: String::new(),
        attempt_id: "old".into(),
        connection_id: s.account.connection_id.clone(),
        thread_id: None,
    });
    assert_eq!(
        reduce_receipt(&old, &s, None, None, json!(0)),
        ReceiptOutcome::Unknown
    );
    assert_eq!(
        plan(&s, "new", "ticket"),
        Outcome::Hold(HoldReason::PreparationPending)
    );
    assert!(matches!(
        reduce_receipt(&old, &s, Some(&receipt(&old)), None, json!(0)),
        ReceiptOutcome::Accepted { .. }
    ));
    s.account.connection_id = "replacement".into();
    let new = attempt(plan(&s, "new", "ticket"));
    assert_eq!(new.operation_id, "prepare:agent:new");
    s.actor.prepare_attempt = Some("new".into());
    assert_eq!(
        reduce_receipt(&old, &s, Some(&receipt(&old)), None, json!(0)),
        ReceiptOutcome::Stale(StaleReason::Connection)
    );
    assert!(matches!(
        reduce_receipt(&new, &s, Some(&receipt(&new)), None, json!(0)),
        ReceiptOutcome::Accepted { .. }
    ));
    s.actor.thread_id = Some("thread".into());
    s.catalog.source = CatalogSource::Legacy;
    s.pending = Some(PendingPreparation {
        agent_id: s.actor.id.clone(),
        epoch: s.actor.epoch,
        account_key: s.account.key.clone(),
        request_digest: String::new(),
        attempt_id: "resume-id".into(),
        connection_id: "old-connection".into(),
        thread_id: Some("thread".into()),
    });
    let mut previous = s.clone();
    previous.pending = None;
    let saved = attempt(plan(&previous, "resume-id", "ticket"));
    s.pending.as_mut().expect("pending resume").request_digest = saved.request_digest;
    let resume = attempt(plan(&s, "unused", "ticket"));
    assert_eq!(resume.operation_id, "prepare:agent:resume-id");
    assert_eq!(resume.method, Method::Resume);
    s.settings.fast_mode = !s.settings.fast_mode;
    let changed = attempt(plan(&s, "changed-params", "ticket"));
    assert_eq!(changed.operation_id, "prepare:agent:changed-params");
    assert_ne!(changed.request_digest, resume.request_digest);
    s.actor.prepare_attempt = Some(changed.attempt_id.clone());
    assert_eq!(
        reduce_receipt(&resume, &s, Some(&receipt(&resume)), None, json!(0)),
        ReceiptOutcome::Stale(StaleReason::Settings)
    );
    assert!(matches!(
        reduce_receipt(&changed, &s, Some(&receipt(&changed)), None, json!(0)),
        ReceiptOutcome::Accepted { .. }
    ));
}

#[test]
fn saved_catalog_identity_and_loaded_epoch_are_not_silently_replaced() {
    let mut s = snapshot(&baseline());
    s.actor.thread_id = Some("thread".into());
    s.catalog.source = CatalogSource::Legacy;
    s.catalog.saved_thread_id = Some("thread".into());
    s.catalog.saved_digest = Some("old digest".into());
    let a = attempt(plan(&s, "attempt", "ticket"));
    assert!(!a.params.contains_key("dynamicTools"));
    // Context repair moves the agent to a new thread and keeps the catalog
    // record of the old one (agent fbcb2568). Python resumes the current
    // thread with the catalog that `nativeToolSource` selects.
    s.catalog.saved_thread_id = Some("thread-before-context-repair".into());
    let resumed = attempt(plan(&s, "attempt", "ticket"));
    assert_eq!(resumed.method, Method::Resume);
    assert_eq!(resumed.params["threadId"], "thread");
    assert!(!resumed.params.contains_key("dynamicTools"));
    s.catalog.source = CatalogSource::Current;
    assert_eq!(
        plan(&s, "attempt", "ticket"),
        Outcome::Refuse(Refusal::CatalogIdentity)
    );
    s.catalog.source = CatalogSource::Legacy;
    s.catalog.saved_thread_id = Some("thread".into());
    let settings = settings_digest(&attempt(plan(&s, "attempt", "ticket")).params);
    s.loaded = Some(LoadedIdentity {
        agent_id: s.actor.id.clone(),
        epoch: s.actor.epoch - 1,
        account_key: s.account.key.clone(),
        connection_id: s.account.connection_id.clone(),
        thread_id: "thread".into(),
        settings_digest: settings,
    });
    assert!(matches!(plan(&s, "attempt", "ticket"), Outcome::Resume(_)));
    s.loaded.as_mut().expect("loaded").epoch = s.actor.epoch;
    assert!(matches!(plan(&s, "attempt", "ticket"), Outcome::Ready(_)));
    // Changed thread settings need the resume that applies them.
    s.settings.profile_instructions.push_str("changed");
    assert!(matches!(plan(&s, "attempt", "ticket"), Outcome::Resume(_)));
}

#[test]
fn receipt_guards_all_frozen_execution_settings() {
    let base = snapshot(&baseline());
    let prepared = attempt(plan(&base, "attempt", "ticket"));
    let changes = [
        json!({"is_lead":false}),
        json!({"needs_title":true}),
        json!({"claude_options":{"permissionMode":"plan"}}),
        json!({"fast_mode":true}),
        json!({"native_effort":null}),
        json!({"yolo_mode":true}),
        json!({"daybreak_enabled":{"state":"present","value":true}}),
        json!({"cyber_access_program":{"state":"present","value":null}}),
    ];
    for change in changes {
        let mut value = baseline();
        merge(&mut value["settings"], &change);
        let mut current = snapshot(&value);
        current.actor.prepare_attempt = Some("attempt".into());
        assert_eq!(
            reduce_receipt(
                &prepared,
                &current,
                Some(&receipt(&prepared)),
                None,
                json!(0)
            ),
            ReceiptOutcome::Stale(StaleReason::Settings)
        );
    }
}

#[test]
fn receipt_and_pending_ownership_are_bound_to_exact_attempt() {
    let base = snapshot(&baseline());
    let prepared = attempt(plan(&base, "attempt", "ticket"));
    let mut current = base.clone();
    current.actor.prepare_attempt = Some("attempt".into());
    for (key, value) in [
        ("operation_id", json!("foreign")),
        ("transport_ticket", json!("foreign")),
        ("agent_id", json!("foreign")),
        ("epoch", json!(8)),
        ("account_key", json!("foreign")),
        ("connection_id", json!("foreign")),
    ] {
        let mut foreign = json!(receipt(&prepared));
        foreign["identity"][key] = value;
        foreign["thread_id"] = json!("foreign-thread");
        let foreign = serde_json::from_value(foreign).expect("foreign receipt");
        assert_eq!(
            reduce_receipt(&prepared, &current, Some(&foreign), None, json!(1000)),
            ReceiptOutcome::Stale(StaleReason::ReceiptIdentity),
            "{key}"
        );
    }
    // A reissued resume can share the operation ID, never its transport ticket.
    let second = attempt(plan(&base, "attempt", "new-ticket"));
    assert_eq!(
        reduce_receipt(
            &second,
            &current,
            Some(&receipt(&prepared)),
            None,
            json!(1000)
        ),
        ReceiptOutcome::Stale(StaleReason::ReceiptIdentity)
    );
    for (key, value) in [
        ("agent_id", json!("foreign")),
        ("epoch", json!(8)),
        ("account_key", json!("foreign")),
    ] {
        let mut pending = json!({"agent_id":"agent","epoch":7,"account_key":"account","request_digest":"digest","attempt_id":"attempt","connection_id":"connection","thread_id":null});
        pending[key] = value;
        let mut s = base.clone();
        s.pending = Some(serde_json::from_value(pending).expect("pending"));
        assert_eq!(
            plan(&s, "new", "new-ticket"),
            Outcome::Refuse(Refusal::PreparationIdentity),
            "{key}"
        );
    }
    let mut foreign = current.clone();
    foreign.actor.id = "foreign".into();
    assert_eq!(
        reduce_receipt(
            &prepared,
            &foreign,
            Some(&receipt(&prepared)),
            None,
            json!(1000)
        ),
        ReceiptOutcome::Stale(StaleReason::Agent)
    );
}

#[test]
fn malformed_stored_catalog_family_never_panics() {
    for tool in ["orchestration_move", "orchestration_agent_manage"] {
        for invalid in [
            Value::Null,
            json!(false),
            json!(42),
            json!("scalar"),
            json!([]),
        ] {
            let mut value = baseline();
            value["settings"]["is_lead"] = json!(false);
            let definition = value["catalog"]["definitions"]
                .as_array_mut()
                .expect("tools")
                .iter_mut()
                .find(|t| t["name"] == tool)
                .expect("tool");
            definition["inputSchema"]["properties"] = invalid;
            let s = snapshot(&value);
            assert_eq!(
                plan(&s, "attempt", "ticket"),
                Outcome::Refuse(Refusal::MalformedCatalog)
            );
        }
    }
    for invalid in [
        Value::Null,
        json!(false),
        json!(42),
        json!("scalar"),
        json!([]),
    ] {
        let mut value = baseline();
        value["catalog"]["definitions"][0]["inputSchema"]["properties"]["accept_cache_loss"] =
            invalid;
        assert_eq!(
            plan(&snapshot(&value), "attempt", "ticket"),
            Outcome::Refuse(Refusal::MalformedCatalog)
        );
    }
    // Typed outer boundaries reject malformed maps before planning.
    for path in ["inputSchema", "description", "name"] {
        let mut value = baseline();
        value["catalog"]["definitions"][0][path] = json!(false);
        assert!(serde_json::from_value::<Snapshot>(value).is_err(), "{path}");
    }
    let mut s = snapshot(&baseline());
    s.actor.thread_id = Some(String::new());
    assert_eq!(
        plan(&s, "attempt", "ticket"),
        Outcome::Refuse(Refusal::EmptyThreadIdentity)
    );
}

#[test]
fn only_a_claude_worker_turns_user_mcp_servers_off() {
    for (parent, provider, expected) in [
        (Some("lead"), "claude", Some(json!(false))),
        (None, "claude", None),
        (Some("lead"), "codex", None),
    ] {
        let mut value = baseline();
        value["actor"]["parent_id"] = json!(parent);
        value["account"]["provider"] = json!(provider);
        let prepared = attempt(plan(&snapshot(&value), "attempt", "ticket"));
        assert_eq!(
            prepared.params.get("userMcp"),
            expected.as_ref(),
            "{provider} {parent:?}"
        );
    }
}

#[test]
fn claude_worker_cache_scope_follows_agent_authority_on_start_and_resume() {
    for lead in [true, false] {
        for thread in [None, Some("thread")] {
            let mut value = baseline();
            value["account"]["provider"] = json!("claude");
            value["settings"]["is_lead"] = json!(lead);
            value["actor"]["thread_id"] = json!(thread);
            value["actor"]["native_tool_source"] = json!("current");
            value["settings"]["claude_options"] = json!({"mcpAutoBackground":false});
            let prepared = attempt(plan(&snapshot(&value), "attempt", "ticket"));
            assert_eq!(prepared.params["managedWorker"], json!(!lead));
            assert_eq!(prepared.params["claude"]["mcpAutoBackground"], false);
        }
    }
}
