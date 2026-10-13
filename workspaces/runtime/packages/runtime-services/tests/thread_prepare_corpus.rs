//! Complete provider requests and receipt patches against the real Python seam.
#[path = "support/prepared_python.rs"]
mod prepared_python;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_services::thread_prepare::*;

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new() -> Self {
        let python = prepared_python::prepared_python(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let mut child = Command::new(python)
            .arg("-B")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/thread_prepare/oracle.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("oracle starts");
        Self {
            input: child.stdin.take().expect("stdin"),
            output: BufReader::new(child.stdout.take().expect("stdout")),
            child,
        }
    }
    fn call(&mut self, value: Value) -> Value {
        writeln!(self.input, "{value}").expect("oracle input");
        self.input.flush().expect("flush");
        let mut line = String::new();
        self.output.read_line(&mut line).expect("oracle output");
        let value: Value = serde_json::from_str(&line).expect("oracle JSON");
        assert!(value.get("error").is_none(), "oracle failed: {value}");
        value["result"].clone()
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let _ignored = writeln!(self.input, "{{\"command\":\"close\"}}");
        let _ignored = self.input.flush();
        self.child.wait().expect("reap oracle");
    }
}
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
    serde_json::from_value(json!({"identity":a.receipt_identity(),"thread_id":"thread","fields":{"model":"native-model"},"reattached":false,"claude_read_submitted":false})).expect("receipt")
}
fn apply(value: &mut Value, patch: &AgentPatch) {
    let object = value.as_object_mut().expect("record");
    for (key, value) in &patch.set {
        object.insert(key.clone(), value.clone());
    }
    for key in &patch.remove {
        object.remove(key);
    }
}
fn equal(name: &str, path: &str, actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(
                a.keys().collect::<Vec<_>>(),
                b.keys().collect::<Vec<_>>(),
                "{name}: {path} keys"
            );
            for (key, value) in a {
                equal(name, &format!("{path}.{key}"), value, &b[key]);
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{name}: {path} length");
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                equal(name, &format!("{path}[{index}]"), a, b);
            }
        }
        _ => assert_eq!(actual, expected, "{name}: {path}"),
    }
}

#[test]
fn python_thread_preparation_corpus() {
    let all: Vec<Value> =
        serde_json::from_str(include_str!("thread_prepare/cases.json")).expect("cases");
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let mut oracle = Oracle::new();
    let mut counts = BTreeMap::new();
    for case in all.into_iter().filter(|c| full || c["fast"] == true) {
        let name = case["name"].as_str().expect("name");
        let mut value = baseline();
        merge(&mut value, &case["snapshot"]);
        if value["pending"].is_object() {
            let mut previous = value.clone();
            previous["pending"] = Value::Null;
            if let Some(change) = case.get("saved_params") {
                merge(&mut previous, change);
            }
            let saved = attempt(plan(&snapshot(&previous), "previous", "ticket"));
            value["pending"]["request_digest"] = json!(saved.request_digest);
        }
        let s = snapshot(&value);
        let planned = plan(&s, "attempt", "ticket");
        if let Some(expected_id) = case.get("expected_attempt_id") {
            let a = attempt(planned.clone());
            assert_eq!(
                json!(a.attempt_id),
                *expected_id,
                "{name}: independent attempt identity"
            );
        }
        let mut current_value = value.clone();
        current_value["actor"]["prepare_attempt"] = json!("attempt");
        if let Some(change) = case.get("current") {
            merge(&mut current_value, change);
        }
        let mut command = json!({"snapshot":value,"current":current_value,"receipt":case.get("receipt"),"close":case.get("close"),"turn_input":case.get("turn_input").cloned().unwrap_or(json!({})),"claude_working":case.get("claude_working")});
        // Resume retries retain a durable identity (explicit task contract).
        // The oracle's supplied uid is the same external attempt identity.
        if let Outcome::Resume(a) = &planned
            && a.attempt_id != "attempt"
        {
            command["attempt_id"] = json!(a.attempt_id);
            command["current"]["actor"]["prepare_attempt"] = json!(a.attempt_id);
            current_value["actor"]["prepare_attempt"] = json!(a.attempt_id);
        }
        command["stored"] = json!({"preparedContext":{"epoch":["old-thread",1],"versions":{"roleSkill":"old-role"}},"nativeToolCatalog":{"threadId":"thread","digest":"saved-digest"}});
        let expected = oracle.call(command);
        equal(
            name,
            "turn",
            &json!(
                turn_params(
                    &s,
                    &serde_json::from_value(case.get("turn_input").cloned().unwrap_or(json!({})))
                        .expect("input")
                )
                .expect("turn overrides")
            ),
            &expected["turn"],
        );
        match planned {
            Outcome::Ready(_) => assert_eq!(expected["outcome"], "ready", "{name}"),
            Outcome::Hold(HoldReason::PreparationPending) => {
                assert_eq!(expected["outcome"], "hold", "{name}")
            }
            Outcome::Start(a) | Outcome::Resume(a) => {
                equal(name, "method", &json!(a.method), &expected["outcome"]);
                equal(name, "params", &json!(a.params), &expected["params"]);
                equal(
                    name,
                    "context_versions",
                    &json!(a.context_versions),
                    &expected["context_versions"],
                );
                equal(
                    name,
                    "catalog_digest",
                    &json!(catalog_digest(&tool_definitions(&s).expect("catalog"))),
                    &expected["catalog_digest"],
                );
                let mut actual = expected["original"].clone();
                apply(&mut actual, &a.before_submit);
                equal(name, "before", &actual, &expected["before"]);
                equal(
                    name,
                    "release association",
                    &json!(a.native_release_id),
                    &expected["native_release_id"],
                );
                assert_eq!(
                    expected["validation_calls"],
                    json!(usize::from(!s.actor.native_effort_present)),
                    "{name}: validation"
                );
                // Apply only external observations to our independently patched row.
                for (key, value) in expected["external_delta"]
                    .as_object()
                    .expect("external delta")
                {
                    actual
                        .as_object_mut()
                        .expect("row")
                        .insert(key.clone(), value.clone());
                }
                for key in expected["external_remove"]
                    .as_array()
                    .expect("external removals")
                {
                    actual
                        .as_object_mut()
                        .expect("row")
                        .remove(key.as_str().expect("key"));
                }
                if let Some(key) = case.get("removed_setting").and_then(Value::as_str) {
                    assert!(actual.get(key).is_none(), "{name}: setting must be absent");
                }
                equal(name, "receipt_before", &actual, &expected["receipt_before"]);
                if case.get("close").is_some() {
                    current_value["actor"]["native_release"] =
                        actual.get("nativeRelease").cloned().unwrap_or(Value::Null);
                }
                let artifacts: Vec<&str> = a
                    .artifacts
                    .iter()
                    .map(|a| match a {
                        ArtifactIntent::RememberRole { text, .. } => text.as_str(),
                    })
                    .collect();
                equal(name, "artifacts", &json!(artifacts), &expected["artifacts"]);
                let r: Option<ProviderReceipt> =
                    case.get("receipt").filter(|v| !v.is_null()).map(|v| {
                        let mut v = v.clone();
                        v["identity"] = json!(a.receipt_identity());
                        serde_json::from_value(v).expect("typed receipt")
                    });
                let close: Option<CloseNotice> = case
                    .get("close")
                    .map(|v| serde_json::from_value(v.clone()).expect("close"));
                let current = snapshot(&current_value);
                let outcome =
                    reduce_receipt(&a, &current, r.as_ref(), close.as_ref(), json!(1000.0));
                match outcome {
                    ReceiptOutcome::Accepted { patch, loaded } => {
                        assert_eq!(expected["settlement"], "accepted", "{name}");
                        apply(&mut actual, &patch);
                        equal(name, "after", &actual, &expected["after"]);
                        assert_eq!(loaded.thread_id, actual["threadId"], "{name}");
                        assert_eq!(expected["loaded"], true, "{name}");
                    }
                    ReceiptOutcome::Unknown => {
                        assert_eq!(expected["settlement"], "unknown", "{name}")
                    }
                    ReceiptOutcome::Stale(_) => {
                        assert_eq!(expected["settlement"], "stale", "{name}");
                        equal(
                            name,
                            "no-write-on-stale",
                            &expected["receipt_before"],
                            &expected["after"],
                        );
                    }
                }
            }
            other => panic!("unexpected {name}: {other:?}"),
        }
        *counts
            .entry(case["family"].as_str().expect("family").to_owned())
            .or_insert(0) += 1;
    }
    eprintln!("thread preparation corpus full={full}: {counts:?}");
    assert!(
        counts.len() >= 10,
        "all original families have fast witnesses"
    );
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
        Refusal::NativeReview,
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
    s.catalog.saved_thread_id = Some("wrong".into());
    assert_eq!(
        plan(&s, "attempt", "ticket"),
        Outcome::Refuse(Refusal::CatalogIdentity)
    );
    s.catalog.saved_thread_id = Some("thread".into());
    s.loaded = Some(LoadedIdentity {
        agent_id: s.actor.id.clone(),
        epoch: s.actor.epoch - 1,
        account_key: s.account.key.clone(),
        connection_id: s.account.connection_id.clone(),
        thread_id: "thread".into(),
    });
    assert!(matches!(plan(&s, "attempt", "ticket"), Outcome::Resume(_)));
    s.loaded.as_mut().expect("loaded").epoch = s.actor.epoch;
    assert!(matches!(plan(&s, "attempt", "ticket"), Outcome::Ready(_)));
}

#[test]
fn oracle_guard_blocks_content_metadata_processes_and_network() {
    let python = prepared_python::prepared_python(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
    let result = Command::new(python)
        .arg("-B")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/thread_prepare/oracle.py"))
        .arg("--guard-test")
        .output()
        .expect("guard test");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
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
    let mut oracle = Oracle::new();
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
            assert_eq!(
                oracle.call(json!({"snapshot":value,"tool_probe":true}))["malformed"],
                true
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
        assert_eq!(
            oracle.call(json!({"snapshot":value,"tool_probe":true}))["malformed"],
            true
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
fn oracle_boundary_guards_detect_enclosing_control_flow_changes() {
    let python = prepared_python::prepared_python(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
    let result = Command::new(python)
        .arg("-B")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/thread_prepare/oracle.py"))
        .arg("--boundary-test")
        .output()
        .expect("boundary test");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
