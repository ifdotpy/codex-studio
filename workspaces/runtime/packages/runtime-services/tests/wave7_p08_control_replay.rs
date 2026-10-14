#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_providers::Method;
use studio_runtime_services::{
    claude_controls::{ControlRequest, service::*, store::StoredControls},
    claude_runtime::*,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};

struct RetainedOwner {
    calls: Mutex<Vec<ClaudeControlCall>>,
    cut: Method,
    after: bool,
    reached: tokio::sync::Notify,
}
impl ClaudeSessionPort for RetainedOwner {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new("account".into(), "connection".into(), 1).expect("binding")
    }
    fn request(&self, call: ClaudeControlCall) -> ControlFuture<'_, ClaudeControlReceipt> {
        Box::pin(async move {
            let cut = call.identity.method == self.cut;
            if cut && !self.after {
                self.reached.notify_one();
                std::future::pending::<()>().await;
            }
            self.calls.lock().expect("calls").push(call.clone());
            if cut && self.after {
                self.reached.notify_one();
                std::future::pending::<()>().await;
            }
            let result = match call.identity.method {
                Method::ClaudeState => {
                    json!({"tasks":[],"turns":[{"id":"one","status":"completed"},{"id":"two","status":"completed"},{"id":"three","status":"completed"}]})
                }
                Method::ThreadQueueList => json!({"data":[]}),
                Method::ThreadRollback => json!({"threadId":"thread","nativeId":"restored"}),
                Method::ClaudeSettings => json!({"settings":{"permissionMode":"plan"}}),
                _ => panic!("unexpected control"),
            };
            Ok(ClaudeControlReceipt {
                identity: call.identity,
                result,
            })
        })
    }
}
#[tokio::test]
async fn restart_at_each_control_transport_boundary_never_resends_an_uncertain_mutation() {
    let root = tempfile::tempdir().expect("scratch");
    let template = root.path().join("template");
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&template).expect("template");
    std::fs::create_dir(&cwd).expect("cwd");
    let output = std::process::Command::new(python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    ))
    .args([
        "-B",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/claude_runtime/controls_oracle.py"
        ),
        "--store-template",
    ])
    .arg(&template)
    .arg("--cwd")
    .arg(&cwd)
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (action, mutation) in [
        ("settings", Method::ClaudeSettings),
        ("rollback", Method::ThreadRollback),
    ] {
        let methods = if action == "rollback" {
            vec![Method::ClaudeState, Method::ThreadQueueList, mutation]
        } else {
            vec![Method::ClaudeState, mutation]
        };
        for (index, cut) in methods.into_iter().enumerate() {
            for after in [false, true] {
                let state = root.path().join(format!("{action}-{index}-{after}"));
                std::fs::create_dir(&state).expect("state");
                for file in ["canvas.sqlite3", "analytics.sqlite3"] {
                    std::fs::copy(template.join(file), state.join(file)).expect("clone");
                }
                let store = Arc::new(RuntimeStore::open(Some(&state)).expect("store"));
                let owner = Arc::new(RetainedOwner {
                    calls: Mutex::new(vec![]),
                    cut,
                    after,
                    reached: tokio::sync::Notify::new(),
                });
                let controls = Controls::new(
                    Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
                    Arc::new(RetainedClaudeSession::new(owner.clone())),
                );
                let request = || {
                    ControlRequest::from_value(&json!({"id":"agent","action":action,"request_id":"control-one","turn_id":"two","settings":{"permissionMode":"plan"}})).expect("request")
                };
                tokio::select! {
                    _ = owner.reached.notified() => {},
                    result = controls.execute(request()) => panic!("unexpected completion: {result:?}"),
                }
                drop(controls);
                drop(store);
                let prior_effects = owner
                    .calls
                    .lock()
                    .expect("calls")
                    .iter()
                    .filter(|call| call.identity.method == mutation)
                    .count();
                let recovered = Arc::new(RetainedOwner {
                    calls: Mutex::new(vec![]),
                    cut: Method::Initialize,
                    after: false,
                    reached: tokio::sync::Notify::new(),
                });
                let store = Arc::new(RuntimeStore::open(Some(&state)).expect("restart"));
                let controls = Controls::new(
                    Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
                    Arc::new(RetainedClaudeSession::new(recovered.clone())),
                );
                let result = controls.execute(request()).await;
                let held = cut == mutation;
                assert_eq!(
                    result.is_err(),
                    held,
                    "{action}/{cut:?}/{after}: {result:?}"
                );
                let calls = recovered.calls.lock().expect("calls");
                assert!(
                    prior_effects
                        + calls
                            .iter()
                            .filter(|call| call.identity.method == mutation)
                            .count()
                        <= 1
                );
                for call in calls.iter() {
                    assert_eq!(call.identity.scope.agent, "agent");
                    assert_eq!(call.identity.scope.epoch, 2);
                    assert_eq!(
                        call.identity.scope.request_id.as_deref(),
                        Some("control-one")
                    );
                }
                let agent =
                    AgentRepository::get(&store.read().expect("read"), &AgentId("agent".into()))
                        .expect("read")
                        .expect("agent")
                        .value();
                assert_eq!(
                    agent["workspaceOperation"],
                    if held {
                        json!("branch_recovery")
                    } else {
                        Value::Null
                    }
                );
            }
        }
    }
}
