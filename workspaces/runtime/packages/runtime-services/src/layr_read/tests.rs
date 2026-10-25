#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, ManagedToolPortsConfig, ManagedToolRuntimePorts,
    dispatch_with_managed_tools,
};
use std::sync::Mutex;
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
use studio_runtime_store::RuntimeStore;

const LEAD: &str = "6f5d1c44-63c1-4a8e-9a39-0b9f6f2f8c11";
const REVIEWER: &str = "2b0e7c1a-2d55-4a51-8f8e-55ad3c0f6d12";
const UNREADY: &str = "0c1d2e3f-4a5b-4c6d-9e8f-f6e5d4c3b2a1";
const NATIVE: &str = "9a4c3b1d-1e2f-4c5d-8e7f-a1b2c3d4e5f6";

fn agent(id: &str, index: usize, mode: &str, role: &str, ready: bool) -> Value {
    let root = if mode == "vm" { LEAD } else { id };
    json!({
        "id": id, "rootId": root, "parentId": (root != id).then_some(root),
        "name": format!("agent {index}"), "isLead": root == id, "role": role,
        "accountKey": "default", "provider": "codex", "model": "gpt-6-astra",
        "status": "running", "autoWake": true, "epoch": 3, "turnEpoch": 3,
        "threadId": format!("thread-{index}"), "turnId": format!("turn-{index}"),
        "executionMode": mode, "layrReady": ready, "cwd": "/line",
        "created": 1_792_510_147.0 + index as f64, "prompt": "fixture",
        "agentMode": "multi", "concurrency": 8, "maxAgents": 16,
    })
}

fn agents() -> Vec<Value> {
    vec![
        agent(LEAD, 0, "vm", "orchestrator", true),
        agent(REVIEWER, 1, "vm", "reviewer", true),
        agent(UNREADY, 2, "vm", "reviewer", false),
        agent(NATIVE, 3, "native", "reviewer", true),
    ]
}

/// One `layr.exec` answer of the fake guest.
#[derive(Clone)]
enum Answer {
    Result(Value),
    Lost,
}

/// One guest request: method, parameters, request ID and wait.
type GuestCall = (String, Value, Option<String>, Duration);

/// The guest side: it records every request.
struct FakeGuest {
    answer: Answer,
    calls: Mutex<Vec<GuestCall>>,
}

impl GuestRpc for FakeGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: Duration,
        on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        self.calls.lock().unwrap().push((
            method.to_owned(),
            params.clone(),
            request_id.map(str::to_owned),
            timeout,
        ));
        match &self.answer {
            Answer::Result(result) => {
                on_frame(json!({"id": request_id, "result": result}))?;
                Ok(result.clone())
            }
            Answer::Lost => Err(GuestError::Transport {
                message: "connection closed before a result".into(),
                uncertain: true,
            }),
        }
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: Arc<RuntimeStore>,
    guest: Arc<FakeGuest>,
}

impl Fixture {
    async fn new(answer: Answer) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
        store
            .transaction_with(|tx| -> Result<(), studio_runtime_store::StoreError> {
                for record in agents() {
                    studio_runtime_store::AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(record).unwrap(),
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        Self {
            _directory: directory,
            store,
            guest: Arc::new(FakeGuest {
                answer,
                calls: Mutex::new(Vec::new()),
            }),
        }
    }

    fn ports(&self, attached: bool) -> ManagedToolRuntimePorts {
        let late = crate::provider_callbacks::tools::LatePorts::default();
        if attached {
            let guest = Arc::clone(&self.guest) as Arc<dyn GuestRpc>;
            assert!(late.layr_read.set(guest).is_ok());
        }
        ManagedToolRuntimePorts {
            status_watch: None,
            child_creation: None,
            monitor_create: None,
            interrupt: None,
            monitor_input: None,
            monitor_cancel: None,
            work_action: None,
            late,
        }
    }

    /// Send one `layr_read` call of `actor` and return the tool result.
    async fn call_with(&self, actor: &str, call_id: &str, args: Value, attached: bool) -> Value {
        let record = agents()
            .into_iter()
            .find(|record| record["id"] == actor)
            .unwrap();
        let context = CallbackContext {
            agent: actor.to_owned(),
            account: "default".into(),
            connection: "connection".into(),
            generation: 7,
            epoch: 3,
            thread: record["threadId"].as_str().unwrap().to_owned(),
            turn: record["turnId"].as_str().unwrap().to_owned(),
        };
        let frame = Frame::Request(RequestFrame {
            id: RequestId::String(format!("rpc-{call_id}")),
            method: Method::ItemToolCall,
            params: Some(json!({
                "threadId": context.thread, "turnId": context.turn,
                "callId": call_id, "tool": TOOL, "arguments": args,
            })),
            fields: Default::default(),
        });
        let ports = crate::provider_callbacks::managed_tool_ports_with_runtime(
            &self.store,
            &context,
            &frame,
            ManagedToolPortsConfig {
                wake: Arc::new(tokio::sync::Notify::new()),
                monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
                runtime_ports: self.ports(attached),
                preparation: None,
            },
        )
        .await
        .unwrap();
        let decision = dispatch_with_managed_tools(
            Arc::clone(&self.store),
            &context,
            &frame,
            20.0,
            Arc::new(ports),
        )
        .await
        .unwrap();
        let CallbackDecision::Answer(reply) = decision else {
            panic!("no tool answer: {decision:?}");
        };
        let ResponseBody::Result(result) = reply.body else {
            panic!("not a result");
        };
        result
    }

    async fn call(&self, actor: &str, call_id: &str, args: Value) -> Value {
        self.call_with(actor, call_id, args, true).await
    }

    fn calls(&self) -> Vec<GuestCall> {
        self.guest.calls.lock().unwrap().clone()
    }
}

fn text(result: &Value) -> String {
    result["contentItems"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn encoded(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// Python `codex_vm_agents.run_layr_read`: a VM reviewer reads layr outside its
/// sandbox through one guest `layr.exec` request, whose identity comes from
/// the agent and the tool request. Before this owner, Rust answered the tool
/// as unsupported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vm_reviewer_reads_layr_through_one_exact_guest_request() {
    let fixture = Fixture::new(Answer::Result(json!({
        "exitCode": 0, "stdout": encoded("diff text\n"), "stderr": encoded("note"),
        "reason": "", "pid": 41,
    })))
    .await;
    let args = json!({"args": ["--line", "main", "diff", "s1", "s2"]});
    let first = fixture.call(REVIEWER, "read-1", args.clone()).await;
    assert_eq!(first["success"], true, "{first}");
    let answer: Value = serde_json::from_str(&text(&first)).unwrap();
    assert_eq!(
        answer,
        json!({"exitCode": 0, "stdout": "diff text\n", "stderr": "note"})
    );
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1);
    let (method, params, request_id, timeout) = &calls[0];
    assert_eq!(method, "layr.exec");
    assert_eq!(
        *params,
        json!({"agentId": REVIEWER, "argv": ["layr", "--line", "main", "diff", "s1", "s2"]})
    );
    // Python `dynamic` keys the tool request as `<thread>:<call>`.
    let identity = "layr-read:35f1f22133704ced9123decbce90efded6d277fd58e50b9e2af4cc3a2974b275";
    assert_eq!(request_id.as_deref(), Some(identity));
    assert_eq!(*timeout, Duration::from_secs(330));

    // A retry of the same tool request returns the first answer; the
    // command does not run again.
    let retry = fixture.call(REVIEWER, "read-1", args).await;
    assert_eq!(retry, first);
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guest_reason_reaches_the_reviewer() {
    let fixture = Fixture::new(Answer::Result(json!({
        "exitCode": null, "stdout": "",
        "stderr": base64::engine::general_purpose::STANDARD.encode([0xff_u8, b'x']),
        "reason": "timeout",
    })))
    .await;
    let result = fixture
        .call(REVIEWER, "read-2", json!({"args": ["status"]}))
        .await;
    let answer: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(
        answer,
        json!({"exitCode": null, "stdout": "", "stderr": "\u{fffd}x", "reason": "timeout"})
    );
}

/// A lost guest answer keeps its uncertainty: the reviewer gets no second
/// command under a new identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_guest_answer_stays_unknown() {
    let fixture = Fixture::new(Answer::Lost).await;
    let args = json!({"args": ["log"]});
    let first = fixture.call(REVIEWER, "read-3", args.clone()).await;
    assert!(
        text(&first).contains("Linux guest layr.exec failed"),
        "{first}"
    );
    let retry = fixture.call(REVIEWER, "read-3", args).await;
    assert_eq!(retry, first);
    assert_eq!(fixture.calls().len(), 1);
}

/// Python allowed only a VM reviewer with a ready line; any other caller gets
/// a refusal, and nothing reaches the guest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_ready_vm_reviewer_reads_layr() {
    let fixture = Fixture::new(Answer::Result(json!({"exitCode": 0}))).await;
    for (index, actor) in [LEAD, UNREADY, NATIVE].into_iter().enumerate() {
        let result = fixture
            .call(
                actor,
                &format!("refused-{index}"),
                json!({"args": ["status"]}),
            )
            .await;
        assert_eq!(result["success"], false, "{result}");
        assert!(text(&result).contains(REFUSAL), "{actor}: {result}");
    }
    let refused = fixture
        .call(REVIEWER, "refused-try", json!({"args": ["try", "sh"]}))
        .await;
    assert!(text(&refused).contains("layr_read runs only"), "{refused}");
    let unattached = fixture
        .call_with(REVIEWER, "unattached", json!({"args": ["status"]}), false)
        .await;
    assert!(text(&unattached).contains(DEPENDENCY), "{unattached}");
    assert!(fixture.calls().is_empty());
}

/// The exact allowlist and argument rules of Python `run_layr_read`.
#[test]
fn layr_read_keeps_the_python_argument_rules() {
    let accepted = |args: Value| command(&json!({"args": args}));
    for args in [
        json!(["status"]),
        json!(["show", "s1:src/main.rs"]),
        json!(["--line", "main", "log"]),
        json!(["--line", "a", "--line", "b", "ls-files"]),
        json!(["blame", "--", "x"]),
        json!(["grep", "-n", "x"]),
        json!(["merge-base", "a", "b"]),
        json!(["rev-parse", "HEAD"]),
        json!(["diff", "x".repeat(4096)]),
        Value::Array(std::iter::repeat_n(json!("status"), 64).collect()),
    ] {
        assert_eq!(
            accepted(args.clone()),
            Ok(serde_json::from_value(args).unwrap())
        );
    }
    let shape = "Supply args as 1 to 64 strings";
    let allowlist = "layr_read runs only blame, diff, grep, log, ls-files, merge-base, \
                     rev-parse, show, status, after optional --line <line>";
    for (args, refusal) in [
        (json!([]), shape),
        (json!("status"), shape),
        (json!(["status", 1]), shape),
        (json!(["show", "a\u{0}b"]), shape),
        (json!(["diff", "x".repeat(4097)]), shape),
        (
            Value::Array(std::iter::repeat_n(json!("status"), 65).collect()),
            shape,
        ),
        (json!(["try", "sh"]), allowlist),
        (json!(["--project", "p", "status"]), allowlist),
        (json!(["-C", "/", "status"]), allowlist),
        (json!(["--line"]), allowlist),
        (json!(["--line", "main"]), allowlist),
        (json!(["commit"]), allowlist),
    ] {
        assert_eq!(accepted(args.clone()), Err(refusal.to_owned()), "{args}");
    }
    assert_eq!(command(&json!({})), Err(shape.to_owned()));
}
