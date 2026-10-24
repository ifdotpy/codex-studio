#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, ManagedToolPortsConfig, ManagedToolRuntimePorts,
    dispatch_with_managed_tools,
};
use std::sync::Mutex;
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};

const LEAD: &str = "6f5d1c44-63c1-4a8e-9a39-0b9f6f2f8c11";
const REVIEWER: &str = "2b0e7c1a-2d55-4a51-8f8e-55ad3c0f6d12";
const NATIVE: &str = "9a4c3b1d-1e2f-4c5d-8e7f-a1b2c3d4e5f6";
const NATIVE_CHILD: &str = "0c1d2e3f-4a5b-4c6d-9e8f-f6e5d4c3b2a1";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn agent(id: &str, index: usize, root: &str, mode: &str, role: &str) -> Value {
    json!({
        "id": id, "rootId": root, "parentId": (root != id).then_some(root),
        "name": format!("agent {index}"), "isLead": root == id, "role": role,
        "accountKey": "default", "provider": "codex", "model": "gpt-6-astra",
        "status": "running", "autoWake": true, "epoch": 3, "turnEpoch": 3,
        "threadId": format!("thread-{index}"), "turnId": format!("turn-{index}"),
        "executionMode": mode, "cwd": "/line", "created": 1_792_510_147.0 + index as f64,
        "prompt": "fixture", "agentMode": "multi", "concurrency": 8, "maxAgents": 16,
    })
}

fn agents() -> Vec<Value> {
    vec![
        agent(LEAD, 0, LEAD, "vm", "orchestrator"),
        agent(REVIEWER, 1, LEAD, "vm", "reviewer"),
        agent(NATIVE, 2, NATIVE, "native", "orchestrator"),
        agent(NATIVE_CHILD, 3, NATIVE, "vm", "implementer"),
    ]
}

async fn store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let records = agents();
    store
        .transaction_with(move |tx| -> Result<(), studio_runtime_store::StoreError> {
            for record in &records {
                studio_runtime_store::AgentRepository::put(
                    tx,
                    &studio_runtime_store::AgentRecord::from_value(record.clone()).unwrap(),
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    (directory, store)
}

/// One `host.exec` answer of the fake guest.
#[derive(Clone)]
enum Answer {
    Result {
        frames: Vec<Value>,
        result: Value,
    },
    Lost,
    Rejected {
        code: &'static str,
        detail: &'static str,
    },
}

/// One guest request: method, parameters, request ID and wait.
type GuestCall = (String, Value, Option<String>, Duration);

/// A running command: `host.exec` tells the test that it started, then
/// waits for the release.
type Hold = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

/// The guest side: it records every request and checks that the tool receipt
/// holds the exact guest request before `host.exec` arrives.
struct FakeGuest {
    store: Arc<RuntimeStore>,
    answer: Answer,
    calls: Mutex<Vec<GuestCall>>,
    saved_before_exec: Mutex<Vec<Option<Value>>>,
    hold: Mutex<Option<Hold>>,
}

impl FakeGuest {
    fn new(store: &Arc<RuntimeStore>, answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            store: Arc::clone(store),
            answer,
            calls: Mutex::new(Vec::new()),
            saved_before_exec: Mutex::new(Vec::new()),
            hold: Mutex::new(None),
        })
    }

    /// Keep the next `host.exec` running until the returned sender releases it.
    fn hold(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (started, on_start) = tokio::sync::oneshot::channel();
        let (release, on_release) = std::sync::mpsc::channel();
        *self.hold.lock().unwrap() = Some((started, on_release));
        (on_start, release)
    }

    fn calls(&self, method: &str) -> Vec<(Value, Option<String>, Duration)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.0 == method)
            .map(|call| (call.1.clone(), call.2.clone(), call.3))
            .collect()
    }
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
        if method == "host.configure" {
            return Ok(json!({"configured": true}));
        }
        assert_eq!(method, "host.exec");
        let connection = self.store.test_reader().unwrap();
        let saved = connection
            .query_row(
                "SELECT json_extract(record,'$.hostExecRequest') FROM runtime_tool_requests \
                 WHERE json_extract(record,'$.stage')='running'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .map(|raw| serde_json::from_str(&raw).unwrap());
        self.saved_before_exec.lock().unwrap().push(saved);
        if let Some((started, release)) = self.hold.lock().unwrap().take() {
            started.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(60)).unwrap();
        }
        let id = request_id.unwrap().to_owned();
        match &self.answer {
            Answer::Result { frames, result } => {
                for frame in frames {
                    let mut frame = frame.clone();
                    frame["id"] = json!(id);
                    on_frame(frame)?;
                }
                on_frame(json!({"id": id, "result": result}))?;
                Ok(result.clone())
            }
            Answer::Lost => Err(GuestError::Transport {
                message: "connection closed before a result".into(),
                uncertain: true,
            }),
            Answer::Rejected { code, detail } => Err(GuestError::Rejected {
                code: Some((*code).into()),
                detail: Some((*detail).into()),
                uncertain: false,
            }),
        }
    }
}

#[derive(Default)]
struct FakeService {
    checks: Mutex<usize>,
}

impl HostExecService for FakeService {
    fn ensure(&self) -> BoxFuture<'_, Result<String, String>> {
        *self.checks.lock().unwrap() += 1;
        Box::pin(async { Ok(TOKEN.to_owned()) })
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: Arc<RuntimeStore>,
    guest: Arc<FakeGuest>,
    service: Arc<FakeService>,
}

impl Fixture {
    async fn new(answer: Answer) -> Self {
        let (directory, store) = store().await;
        let guest = FakeGuest::new(&store, answer);
        Self {
            _directory: directory,
            store,
            guest,
            service: Arc::default(),
        }
    }

    fn ports(&self, attached: bool) -> ManagedToolRuntimePorts {
        let late = crate::provider_callbacks::tools::LatePorts::default();
        if attached {
            let runtime = HostExecRuntime::new(
                Arc::clone(&self.guest) as Arc<dyn GuestRpc>,
                Arc::clone(&self.service) as Arc<dyn HostExecService>,
            );
            assert!(late.host_exec.set(runtime).is_ok());
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

    async fn receipt(&self, actor: &str, call_id: &str) -> Value {
        let index = agents()
            .iter()
            .position(|record| record["id"] == actor)
            .unwrap();
        let id = format!("thread-{index}:{call_id}");
        self.store
            .read_async("test", move |tx| {
                studio_runtime_store::system_reads::get(tx, "runtime_tool_requests", &id)
            })
            .await
            .unwrap()
            .unwrap()
    }
}

fn text(result: &Value) -> String {
    result["contentItems"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn operation(actor: &str, request: &str) -> String {
    format!(
        "host-exec:{}",
        studio_runtime_store::sha256_hex(format!("{actor}:{request}").as_bytes())
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vm_agent_runs_one_command_with_its_exact_guest_request() {
    let id = operation(LEAD, "build");
    let fixture = Fixture::new(Answer::Result {
        frames: vec![
            json!({"event": "output", "data": {"text": "hel"}}),
            json!({"event": "host.heartbeat", "data": {"operationId": "x"}}),
            json!({"event": "output", "data": {"text": "lo"}}),
        ],
        result: json!({"operationId": operation(LEAD, "build"), "state": "exited", "exitCode": 0}),
    })
    .await;
    let args = json!({"action": "execute", "command": "make", "request_id": "build", "timeout_seconds": 30});
    let first = fixture.call(LEAD, "call-1", args.clone()).await;
    assert_eq!(first["success"], true, "{first}");
    let value: Value = serde_json::from_str(&text(&first)).unwrap();
    assert_eq!(
        (value["exitCode"].clone(), value["output"].clone()),
        (json!(0), json!("hello"))
    );
    let params = json!({
        "agentId": LEAD, "action": "execute", "argv": ["/bin/zsh", "-lc", "make"],
        "cwd": "/line", "timeoutSeconds": 30, "operationId": id,
    });
    let exec = fixture.guest.calls("host.exec");
    assert_eq!(
        exec,
        vec![(
            params.clone(),
            Some(id.clone()),
            Duration::from_secs_f64(3690.0)
        )]
    );
    // The guest gets the channel token first, under its token identity.
    let configure = fixture.guest.calls("host.configure");
    assert_eq!(
        configure[0],
        (
            json!({"token": TOKEN}),
            Some(format!(
                "host-configure:{}",
                studio_runtime_store::sha256_hex(TOKEN.as_bytes())
            )),
            Duration::from_secs(15)
        )
    );
    assert_eq!(*fixture.service.checks.lock().unwrap(), 1);
    // The exact request was durable before the guest saw it.
    let saved = json!({"requestId": id, "params": params, "timeout": 3690.0});
    assert_eq!(
        fixture.guest.saved_before_exec.lock().unwrap().clone(),
        vec![Some(saved.clone())]
    );
    let receipt = fixture.receipt(LEAD, "call-1").await;
    assert_eq!(receipt["hostExecRequest"], saved);
    assert_eq!(receipt["outcome"], "applied", "{receipt}");
    // A replay of the same call returns the saved answer; the guest sees nothing new.
    let replay = fixture.call(LEAD, "call-1", args).await;
    assert_eq!(text(&replay), text(&first));
    assert_eq!(fixture.guest.calls("host.exec").len(), 1);
}

/// A command can run for hours. Its guest call has its own thread, so the
/// runtime's blocking pool stays free for store reads
/// (`browser_cards::guest_call` follows the same rule).
#[test]
fn a_running_command_leaves_the_blocking_pool_free() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = Arc::new(
            Fixture::new(Answer::Result {
                frames: Vec::new(),
                result: json!({"state": "exited", "exitCode": 0}),
            })
            .await,
        );
        let (started, release) = fixture.guest.hold();
        let call = tokio::spawn({
            let fixture = Arc::clone(&fixture);
            async move {
                fixture
                    .call(LEAD, "call-1", json!({"command": "make"}))
                    .await
            }
        });
        started.await.unwrap();
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            fixture.store.read_async("test", |tx| {
                tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .map_err(studio_runtime_store::StoreError::from)
            }),
        )
        .await;
        release.send(()).unwrap();
        let result = call.await.unwrap();
        assert!(
            matches!(read, Ok(Ok(1))),
            "a store read waited for the running command"
        );
        assert_eq!(result["success"], true, "{result}");
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_guest_answer_is_unknown_and_never_repeats_the_command() {
    let fixture = Fixture::new(Answer::Lost).await;
    let args = json!({"command": "make", "request_id": "build"});
    let first = fixture.call(LEAD, "call-1", args.clone()).await;
    assert_eq!(first["success"], false, "{first}");
    let id = operation(LEAD, "build");
    let note = format!("Operation: {id}. Use action status or list; never repeat the command.");
    assert!(text(&first).contains(&note), "{first}");
    assert_eq!(fixture.receipt(LEAD, "call-1").await["outcome"], "unknown");
    // The same call returns the saved unknown answer without a guest request.
    let replay = fixture.call(LEAD, "call-1", args.clone()).await;
    assert_eq!(text(&replay), text(&first));
    assert_eq!(fixture.guest.calls("host.exec").len(), 1);
    // A new call with the same request_id sends the same guest identity: the
    // guest receipt answers it, and the command does not start again.
    fixture.call(LEAD, "call-2", args).await;
    let exec = fixture.guest.calls("host.exec");
    assert_eq!(exec.len(), 2);
    assert_eq!(exec[0], exec[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_guest_error_reaches_the_caller_with_its_operation() {
    let fixture = Fixture::new(Answer::Rejected {
        code: "busy",
        detail: "A request of this guest runs the command; cancel it or wait",
    })
    .await;
    let result = fixture
        .call(
            LEAD,
            "call-1",
            json!({"command": "make", "request_id": "build"}),
        )
        .await;
    assert_eq!(result["success"], false, "{result}");
    let message = text(&result);
    assert!(
        message.contains("A request of this guest runs the command; cancel it or wait"),
        "{message}"
    );
    assert!(message.contains(&operation(LEAD, "build")), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_send_nothing_to_the_guest_or_the_service() {
    let fixture = Fixture::new(Answer::Lost).await;
    let cases = [
        (
            REVIEWER,
            json!({"command": "make"}),
            "host_exec is unavailable to read-only reviewers",
        ),
        (
            NATIVE,
            json!({"command": "make"}),
            "host_exec is available only to VM agents",
        ),
        (
            NATIVE_CHILD,
            json!({"command": "make"}),
            "host_exec is available only to VM agents",
        ),
        (
            LEAD,
            json!({"action": "run", "command": "make"}),
            "Unknown host_exec action",
        ),
        (LEAD, json!({"action": "execute"}), "Supply a command"),
        (LEAD, json!({"command": ""}), "Supply a command"),
        (LEAD, json!({"command": "a\u{0}b"}), "Supply a command"),
        (
            LEAD,
            json!({"command": "make", "timeout_seconds": 0}),
            "The command timeout must be between 0 and 3600 seconds",
        ),
        (
            LEAD,
            json!({"command": "make", "timeout_seconds": 3600.5}),
            "The command timeout must be between 0 and 3600 seconds",
        ),
        (
            LEAD,
            json!({"command": "make", "timeout_seconds": true}),
            "The command timeout must be between 0 and 3600 seconds",
        ),
        (
            LEAD,
            json!({"action": "status"}),
            "Supply the host_exec operation_id",
        ),
        (
            LEAD,
            json!({"action": "cancel", "operation_id": ""}),
            "Supply the host_exec operation_id",
        ),
    ];
    for (index, (actor, args, message)) in cases.into_iter().enumerate() {
        let call = format!("refused-{index}");
        let result = fixture.call(actor, &call, args).await;
        assert_eq!(result["success"], false, "{result}");
        assert!(text(&result).starts_with(message), "{index}: {result}");
        assert_eq!(
            fixture.receipt(actor, &call).await["outcome"],
            "not_applied"
        );
    }
    assert!(fixture.guest.calls.lock().unwrap().is_empty());
    assert_eq!(*fixture.service.checks.lock().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_its_runtime_the_tool_names_its_dependency() {
    let fixture = Fixture::new(Answer::Lost).await;
    let result = fixture
        .call_with(LEAD, "call-1", json!({"command": "make"}), false)
        .await;
    assert_eq!(result["success"], false, "{result}");
    assert!(
        text(&result).contains("requires host-exec/layr-bridge"),
        "{result}"
    );
    assert!(fixture.guest.calls.lock().unwrap().is_empty());
}

#[test]
fn read_actions_use_their_parameters_and_wait() {
    let status = guest_request(
        LEAD,
        Some(&json!("/line")),
        &json!({"action": "status", "operation_id": "op", "after_seq": 5}),
        "thread:call",
    )
    .unwrap();
    assert_eq!(
        status.params,
        json!({"agentId": LEAD, "action": "status", "operationId": "op", "afterSeq": 5})
    );
    assert_eq!(status.timeout, 60.0);
    assert_eq!(status.request_id, operation(LEAD, "thread:call"));
    let list = guest_request(LEAD, None, &json!({"action": "list"}), "thread:call").unwrap();
    assert_eq!(list.params, json!({"agentId": LEAD, "action": "list"}));
    assert_eq!(list.operation_note(), None);
    // Without timeout_seconds the default applies; a supplied cwd wins.
    let execute = guest_request(
        LEAD,
        Some(&json!("/line")),
        &json!({"command": "make", "cwd": "/line/app"}),
        "thread:call",
    )
    .unwrap();
    assert_eq!(execute.params["timeoutSeconds"], 300);
    assert_eq!(execute.params["cwd"], "/line/app");
    assert_eq!(execute.timeout, 3960.0);
}

#[test]
fn only_vm_writers_get_the_host_exec_definition() {
    use crate::thread_prepare::{
        CatalogFilter, Role, SessionTool, VmRole, filter_tool_definitions,
    };
    let definition = tool_definition().expect("bundled definition");
    assert_eq!(definition.name, TOOL);
    let names = |vm: Option<VmRole>| {
        filter_tool_definitions(
            &[],
            &CatalogFilter {
                provider: studio_runtime_providers::ProviderKind::Codex,
                is_lead: true,
                can_delegate: true,
                federation_enabled: false,
                cwd: "/line",
                session_tool: SessionTool::Move,
                vm,
            },
        )
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>()
    };
    assert_eq!(names(VmRole::of(true, Role::Orchestrator)), vec![TOOL]);
    assert_eq!(names(VmRole::of(true, Role::Implementer)), vec![TOOL]);
    assert_eq!(
        names(VmRole::of(true, Role::Reviewer)),
        vec![crate::layr_read::TOOL]
    );
    assert!(names(VmRole::of(false, Role::Orchestrator)).is_empty());
}

#[tokio::test]
async fn an_interrupted_host_exec_receipt_names_its_operation() {
    let (_directory, store) = store().await;
    let id = operation(LEAD, "build");
    let receipt = json!({
        "id": "thread-0:call-1", "agent": LEAD, "threadId": "thread-0", "turnId": "turn-0",
        "epoch": 3, "accountKey": "default", "connectionId": "connection", "callId": "call-1",
        "tool": TOOL, "signature": "signature", "readOnly": false, "stage": "running",
        "outcome": "pending", "cancelRequested": false, "created": 10.0, "updated": 10.0,
        "hostExecRequest": {"requestId": id, "params": {"agentId": LEAD, "action": "execute",
            "operationId": id}, "timeout": 3960.0},
    });
    store
        .transaction_with(move |tx| -> Result<(), studio_runtime_store::StoreError> {
            tx.execute(
                "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",
                rusqlite::params!["thread-0:call-1", receipt.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    crate::provider_callbacks::recover_tool_requests_on_startup(&store, 30.0)
        .await
        .unwrap();
    let result = store
        .read_async("test", |tx| {
            studio_runtime_store::system_reads::get(tx, "runtime_tool_results", "thread-0:call-1")
        })
        .await
        .unwrap()
        .unwrap();
    let message = text(&result);
    assert!(message.contains("\"outcome\":\"unknown\""), "{message}");
    assert!(message.contains(&operation_note(&id)), "{message}");
}
