#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_services::claude_controls::{
    ControlError, ControlRequest, service::*, store::StoredControls,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};
struct Native {
    calls: Mutex<Vec<String>>,
    unknown: bool,
    generation: std::sync::atomic::AtomicU64,
}
impl ControlSession for Native {
    fn binding(&self) -> ControlBinding {
        ControlBinding::new(
            "account".into(),
            "connection".into(),
            self.generation.load(std::sync::atomic::Ordering::SeqCst),
        )
        .expect("valid fixture")
    }
    fn call(&self, method: &str, _params: Value) -> ControlFuture<'_, Value> {
        let method = method.to_owned();
        Box::pin(async move {
            self.calls
                .lock()
                .expect("valid fixture")
                .push(method.clone());
            Ok(match method.as_str() {
                "claude/state" => {
                    json!({"tasks":[],"turns":[{"id":"one","status":"completed"},{"id":"two","status":"completed"},{"id":"three","status":"completed"}]})
                }
                "thread/queue/list" => json!({"data":[]}),
                "thread/rollback" => {
                    if self.unknown {
                        return Err(ControlError::Unknown);
                    }
                    json!({"threadId":"thread","nativeId":"restored"})
                }
                "claude/commands" => json!({"commands":["/fixture"]}),
                "claude/stopTask" => json!({"stopped":true}),
                _ => unreachable!(),
            })
        })
    }
}
#[tokio::test]
async fn stored_control_replay_and_unknown_never_repeat_native_write() {
    let root = tempfile::tempdir().expect("valid fixture");
    let template = root.path().join("template");
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&template).expect("valid fixture");
    std::fs::create_dir(&cwd).expect("valid fixture");
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
    .expect("valid fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let corpus: Value = serde_json::from_slice(&output.stdout).expect("Python corpus");
    for unknown in [false, true] {
        let state = root.path().join(format!("state-{unknown}"));
        std::fs::create_dir(&state).expect("valid fixture");
        for file in ["canvas.sqlite3", "analytics.sqlite3"] {
            std::fs::copy(template.join(file), state.join(file)).expect("valid fixture");
        }
        let store = Arc::new(RuntimeStore::open(Some(&state)).expect("valid fixture"));
        let native = Arc::new(Native {
            calls: Mutex::new(vec![]),
            unknown,
            generation: std::sync::atomic::AtomicU64::new(1),
        });
        let adapter = Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {})));
        let service = Controls::new(adapter.clone(), native.clone());
        for row in corpus["reads"].as_array().expect("read corpus") {
            assert_eq!(
                service
                    .read_or_stop(&row["agent"], &row["body"], None)
                    .await
                    .expect("read action"),
                row["expected"]
            );
        }

        let request = || {
            ControlRequest::from_value(&json!({"id":"agent","action":"rollback","request_id":"control-one","turn_id":"two"})).expect("valid fixture")
        };
        let first = service.execute(request()).await;
        assert_eq!(
            first.is_err(),
            unknown,
            "{first:?}: {:?}",
            native.calls.lock().expect("fixture calls")
        );
        let replay = Controls::new(adapter, native.clone())
            .execute(request())
            .await;
        assert_eq!(replay.is_err(), unknown);
        assert_eq!(
            native
                .calls
                .lock()
                .expect("valid fixture")
                .iter()
                .filter(|m| m.as_str() == "thread/rollback")
                .count(),
            1
        );
        let agent = AgentRepository::get(
            &store.read().expect("valid fixture"),
            &AgentId("agent".into()),
        )
        .expect("valid fixture")
        .expect("valid fixture")
        .value();
        assert_eq!(agent["status"], "paused");
        assert_eq!(
            agent["workspaceOperation"],
            if unknown {
                json!("branch_recovery")
            } else {
                Value::Null
            }
        );
        let item: String = store
            .read()
            .expect("valid fixture")
            .query_row("SELECT record FROM runtime_items WHERE id='two'", [], |r| {
                r.get(0)
            })
            .expect("valid fixture");
        assert_eq!(
            serde_json::from_str::<Value>(&item)
                .expect("valid fixture")
                .get("afterRestore")
                .is_some(),
            !unknown
        );
    }
}

struct Cut {
    inner: Arc<StoredControls>,
    step: &'static str,
    after: bool,
    reached: tokio::sync::Notify,
}
impl Cut {
    async fn at(&self, step: &str, after: bool) {
        if self.step == step && self.after == after {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
    }
}
impl ControlStorage for Cut {
    fn reserve(
        &self,
        request: ControlRequest,
        binding: ControlBinding,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("reserve", false).await;
            let result = self.inner.reserve(request, binding).await?;
            self.at("reserve", true).await;
            Ok(result)
        })
    }
    fn submitted(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        request: ControlRequest,
        native: Value,
        queue: Value,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("submitted", false).await;
            let result = self
                .inner
                .submitted(expected, request, native, queue)
                .await?;
            self.at("submitted", true).await;
            Ok(result)
        })
    }
    fn provider(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        result: Value,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        Box::pin(async move {
            self.at("provider", false).await;
            let result = self.inner.provider(expected, result).await?;
            self.at("provider", true).await;
            Ok(result)
        })
    }
    fn complete(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        request: ControlRequest,
    ) -> ControlFuture<'_, Value> {
        Box::pin(async move {
            self.at("complete", false).await;
            let result = self.inner.complete(expected, request).await?;
            self.at("complete", true).await;
            Ok(result)
        })
    }
    fn failed(
        &self,
        expected: studio_runtime_services::claude_controls::ControlSnapshot,
        uncertain: bool,
    ) -> ControlFuture<'_, ()> {
        Box::pin(async move {
            self.at("failed", false).await;
            self.inner.failed(expected, uncertain).await?;
            self.at("failed", true).await;
            Ok(())
        })
    }
}
#[tokio::test]
async fn durable_control_store_boundaries_resume_or_hold_without_duplicate_write() {
    let root = tempfile::tempdir().expect("valid fixture");
    let template = root.path().join("template");
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&template).expect("valid fixture");
    std::fs::create_dir(&cwd).expect("valid fixture");
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
    .expect("valid fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    for step in ["reserve", "submitted", "provider", "complete", "failed"] {
        if !full && !["submitted", "provider"].contains(&step) {
            continue;
        }
        for after in [false, true] {
            let state = root.path().join(format!("{step}-{after}"));
            std::fs::create_dir(&state).expect("valid fixture");
            for name in ["canvas.sqlite3", "analytics.sqlite3"] {
                std::fs::copy(template.join(name), state.join(name)).expect("valid fixture");
            }
            let store = Arc::new(RuntimeStore::open(Some(&state)).expect("valid fixture"));
            let native = Arc::new(Native {
                calls: Mutex::new(vec![]),
                unknown: step == "failed",
                generation: std::sync::atomic::AtomicU64::new(1),
            });
            let adapter = Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {})));
            let cut = Arc::new(Cut {
                inner: adapter.clone(),
                step,
                after,
                reached: tokio::sync::Notify::new(),
            });
            let service = Controls::new(cut.clone(), native.clone());
            let request = || {
                ControlRequest::from_value(&json!({"id":"agent","action":"rollback","request_id":"control-one","turn_id":"two"})).expect("valid fixture")
            };
            tokio::select! { _=cut.reached.notified()=>{}, result=service.execute(request())=>panic!("{step}/{after}: unexpected completion {}",result.is_ok()) }
            drop(service);
            drop(cut);
            drop(adapter);
            drop(store);
            let store = Arc::new(RuntimeStore::open(Some(&state)).expect("valid fixture"));
            let service = Controls::new(
                Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
                native.clone(),
            );
            if step == "provider" && after {
                native
                    .generation
                    .store(2, std::sync::atomic::Ordering::SeqCst);
            }
            let outcome = service.execute(request()).await;
            let held = (step == "submitted" && after)
                || (step == "provider" && !after)
                || step == "failed";
            assert_eq!(outcome.is_err(), held, "{step}/{after}: {outcome:?}");
            assert!(
                native
                    .calls
                    .lock()
                    .expect("valid fixture")
                    .iter()
                    .filter(|m| m.as_str() == "thread/rollback")
                    .count()
                    <= 1,
                "{step}/{after}"
            );
            let agent = AgentRepository::get(
                &store.read().expect("valid fixture"),
                &AgentId("agent".into()),
            )
            .expect("valid fixture")
            .expect("valid fixture")
            .value();
            assert_eq!(
                agent["workspaceOperation"],
                if held {
                    json!("branch_recovery")
                } else {
                    Value::Null
                },
                "{step}/{after}"
            );
        }
    }
}
