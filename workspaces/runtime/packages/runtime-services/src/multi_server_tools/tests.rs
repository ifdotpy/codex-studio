#![allow(clippy::duplicate_mod, clippy::unwrap_used)]

#[path = "../../tests/support/prepared_python.rs"]
mod prepared_python;

use super::*;
use crate::multi_server_orchestration::{PairedTransport, RuntimeOrchestrationEffects};
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, ManagedToolPortsConfig, ManagedToolRuntimePorts,
    dispatch_with_managed_tools,
};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};

/// Far-future command output expiry shared with the Python fixture.
const FAR: f64 = 4102444800.0;
const PEER: &str = "peer-server";

struct Oracle {
    _scratch: tempfile::TempDir,
    value: Value,
}

/// The real Python tool path over the recorded fixture transport
/// (tests/multi_server_tools/oracle.py).
fn oracle() -> &'static Oracle {
    static ORACLE: OnceLock<Oracle> = OnceLock::new();
    ORACLE.get_or_init(|| {
        let scratch = tempfile::tempdir().unwrap();
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("../../apps/server/src");
        let output = Command::new(prepared_python::prepared_python(&source))
            .arg(manifest.join("tests/multi_server_tools/oracle.py"))
            .arg(scratch.path())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Python oracle: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Oracle {
            _scratch: scratch,
            value: serde_json::from_slice(&output.stdout).unwrap(),
        }
    })
}

/// The paired fixture server: the reply rules of the Python oracle's `reply`.
fn reply(envelope: &Value) -> Value {
    let key = envelope["requestId"].clone();
    let payload = &envelope["payload"];
    match envelope["action"].as_str().unwrap() {
        "projects" => json!({"requestId": key, "outcome": "applied", "value": {"items": [
            {"id": "/peer/project", "path": "/peer/project", "name": "Peer project", "created": 1.0}]}}),
        "git" => {
            json!({"requestId": key, "outcome": "applied", "value": {"exitCode": 0, "output": " M a.txt\n", "truncated": false}})
        }
        "folders" => {
            json!({"requestId": key, "outcome": "not_applied", "error": "This directory is unavailable"})
        }
        "exec" => json!({"requestId": key, "outcome": "applied", "value": {
            "handle": key, "serverId": PEER, "status": "running", "startedAt": 10.0, "finishedAt": null,
            "exitCode": null, "signal": null, "duration": null, "timedOut": false,
            "outputLimit": 262144, "outputExpiresAt": FAR}}),
        "exec_read" => json!({"requestId": key, "outcome": "applied", "value": {
            "handle": payload["handle"], "status": "completed", "exitCode": 0, "stdout": "ok\n",
            "stdoutOffset": 0, "stdoutNextOffset": 3, "outputExpiresAt": FAR}}),
        "add_location" => json!({"requestId": key, "outcome": "applied", "value": {
            "serverId": PEER, "path": payload["path"], "canonicalPath": payload["path"],
            "projectId": payload["path"], "gitOrigin": null, "homeProjectId": payload["project"],
            "requestId": key}}),
        "remove_location" => {
            json!({"requestId": key, "outcome": "applied", "value": {"removed": true, "serverId": PEER}})
        }
        "location_matches" => json!({"requestId": key, "outcome": "applied", "value": {"matches": [
            {"projectId": "/peer/project", "path": "/peer/project", "name": "project"}]}}),
        action => panic!("unexpected fixture action {action}"),
    }
}

#[derive(Default)]
struct RecordingTransport {
    sent: Mutex<Vec<Value>>,
}

impl PairedTransport for RecordingTransport {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        envelope: &'a Value,
        _timeout: std::time::Duration,
    ) -> BoxFuture<'a, Result<Value, AccessError>> {
        Box::pin(async move {
            self.sent
                .lock()
                .unwrap()
                .push(json!({"server": server, "envelope": envelope}));
            Ok(reply(envelope))
        })
    }
}

async fn python_written_store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let template = std::path::PathBuf::from(oracle().value["template"].as_str().unwrap());
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(name), directory.path().join(name)).unwrap();
    }
    let identity = directory.path().join("multi-server");
    std::fs::create_dir(&identity).unwrap();
    std::fs::copy(
        template.join("multi-server/identity.json"),
        identity.join("identity.json"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(
            identity.join("identity.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    (directory, store)
}

fn context(actor: &Value) -> CallbackContext {
    CallbackContext {
        agent: actor["id"].as_str().unwrap().to_owned(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: actor["epoch"].as_u64().unwrap(),
        thread: actor["threadId"].as_str().unwrap().to_owned(),
        turn: actor["turnId"].as_str().unwrap().to_owned(),
    }
}

fn frame(context: &CallbackContext, name: &str, call_id: &str, args: &Value) -> Frame {
    Frame::Request(RequestFrame {
        id: RequestId::String(format!("rpc-{name}")),
        method: Method::ItemToolCall,
        params: Some(json!({
            "threadId": context.thread,
            "turnId": context.turn,
            "callId": call_id,
            "tool": "orchestration_servers",
            "arguments": args,
        })),
        fields: Default::default(),
    })
}

fn runtime_ports(tools: Option<Arc<dyn MultiServerToolPort>>) -> ManagedToolRuntimePorts {
    let late = crate::provider_callbacks::tools::LatePorts::default();
    if let Some(tools) = tools {
        assert!(late.multi_server.set(tools).is_ok());
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

async fn call_tool(
    store: &Arc<RuntimeStore>,
    actor: &Value,
    name: &str,
    call_id: &str,
    args: &Value,
    tools: Option<Arc<dyn MultiServerToolPort>>,
) -> Value {
    let context = context(actor);
    let frame = frame(&context, name, call_id, args);
    let ports = crate::provider_callbacks::managed_tool_ports_with_runtime(
        store,
        &context,
        &frame,
        ManagedToolPortsConfig {
            wake: Arc::new(tokio::sync::Notify::new()),
            monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
            runtime_ports: runtime_ports(tools),
        },
    )
    .unwrap();
    let decision =
        dispatch_with_managed_tools(Arc::clone(store), &context, &frame, 20.0, Arc::new(ports))
            .await
            .unwrap();
    let CallbackDecision::Answer(reply) = decision else {
        panic!("{name}: no tool answer: {decision:?}");
    };
    let ResponseBody::Result(result) = reply.body else {
        panic!("{name}: not a result");
    };
    result
}

fn parsed(result: &Value) -> Value {
    let text = result["contentItems"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

#[tokio::test]
async fn servers_tool_matches_python_envelopes_receipts_and_project_locations() {
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    assert_eq!(
        store.local_server_id().unwrap().as_deref(),
        oracle["localServer"].as_str()
    );
    let transport = Arc::new(RecordingTransport::default());
    let driver = Arc::new(OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::clone(&transport) as Arc<dyn PairedTransport>,
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    ));
    let tools: Arc<dyn MultiServerToolPort> =
        Arc::new(ServerTools::new(Arc::clone(&store), driver, false));
    for case in oracle["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let actor = if case["actor"] == oracle["lead"]["id"] {
            &oracle["lead"]
        } else {
            &oracle["worker"]
        };
        let before = transport.sent.lock().unwrap().len();
        let result = call_tool(
            &store,
            actor,
            name,
            case["callId"].as_str().unwrap(),
            &case["arguments"],
            Some(Arc::clone(&tools)),
        )
        .await;
        let sent = transport.sent.lock().unwrap()[before..].to_vec();
        assert_eq!(json!(sent), case["sent"], "{name}: sent envelopes");
        assert_eq!(result["success"], case["success"], "{name}: {result}");
        assert_eq!(parsed(&result), case["value"], "{name}: tool value");
        assert_eq!(
            result
                .get("serverOutputExpiresAt")
                .cloned()
                .unwrap_or(Value::Null),
            case["serverOutputExpiresAt"],
            "{name}: command output expiry"
        );
        if name == "add-location" || name == "remove-location" {
            let project = oracle["projectId"].as_str().unwrap().to_owned();
            let record = store
                .read_transaction(|tx| Ok(ProjectsRepository::get(tx, &project)?.unwrap().1))
                .unwrap();
            let expected = if name == "add-location" {
                &oracle["projectAfterAdd"]
            } else {
                &oracle["projectAfterRemove"]
            };
            assert_eq!(
                record["locations"], expected["locations"],
                "{name}: locations"
            );
            assert_eq!(
                record["locationsRevision"], expected["locationsRevision"],
                "{name}: revision"
            );
        }
    }
}

#[tokio::test]
async fn servers_tool_names_its_dependency_until_the_driver_is_attached() {
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    let result = call_tool(
        &store,
        &oracle["lead"],
        "unattached",
        "call-unattached",
        &json!({"action": "list"}),
        None,
    )
    .await;
    assert_eq!(result["success"], false);
    let text = result["contentItems"][0]["text"].as_str().unwrap();
    assert!(text.contains("multi-server/tool-coordinator"), "{text}");
}

#[tokio::test]
async fn a_lost_reply_is_unknown_and_an_exact_retry_does_not_send_again_in_flight() {
    struct Lost(Mutex<usize>);
    impl PairedTransport for Lost {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            _envelope: &'a Value,
            _timeout: std::time::Duration,
        ) -> BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move {
                *self.0.lock().unwrap() += 1;
                Err(crate::multi_server_auth::refused(
                    503,
                    "exchange_unknown",
                    "The paired delivery outcome is unknown",
                ))
            })
        }
    }
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    let transport = Arc::new(Lost(Mutex::new(0)));
    let driver = Arc::new(OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::clone(&transport) as Arc<dyn PairedTransport>,
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    ));
    let tools: Arc<dyn MultiServerToolPort> =
        Arc::new(ServerTools::new(Arc::clone(&store), driver, false));
    let args = json!({"action": "exec", "server": PEER, "cwd": "/peer/repo", "command": ["true"]});
    let first = call_tool(
        &store,
        &oracle["lead"],
        "lost",
        "call-lost",
        &args,
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(first["success"], true, "{first}");
    assert_eq!(parsed(&first)["outcome"], "unknown");
    assert_eq!(*transport.0.lock().unwrap(), 1);
    // The command row is now in receipt form: a new call with the same
    // request identity can only probe for the receipt.
    let outbox = parsed(&first)["requestId"].as_str().unwrap().to_owned();
    let body: String = store
        .read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT body FROM runtime_server_outbox WHERE id=?1",
                [&outbox],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body["payload"].get("command").is_none(), "{body}");
    assert!(body["payload"]["receiptDigest"].is_string(), "{body}");
}

/// A paired peer that answers with a real Rust receiver over the same store
/// contract that a Python peer uses.
struct LoopbackPeer {
    principal: String,
    driver: OrchestrationDriver,
    sent: Mutex<Vec<String>>,
}

impl PairedTransport for LoopbackPeer {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        envelope: &'a Value,
        _timeout: std::time::Duration,
    ) -> BoxFuture<'a, Result<Value, AccessError>> {
        Box::pin(async move {
            assert_eq!(server, PEER);
            self.sent
                .lock()
                .unwrap()
                .push(envelope["action"].as_str().unwrap().to_owned());
            let key = envelope["requestId"].as_str().unwrap().to_owned();
            let now = crate::event_write::unix_time().unwrap() as i64;
            let outcome = self.driver.receive(&self.principal, envelope, now).await?;
            Ok(outcome.to_value(&key))
        })
    }
}

fn run_git(directory: &std::path::Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn peer_store(home: &str) -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let (directory, store) = python_written_store().await;
    drop(store);
    std::fs::write(
        directory.path().join("multi-server/identity.json"),
        json!({"serverId": PEER}).to_string(),
    )
    .unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let record = json!({"id": home, "clientId": home, "serverId": home, "label": "Home",
        "kind": "server", "origin": "https://home.fixture.ts.net", "publicKey": "fixture-public-key",
        "tailscaleUser": "owner@example.test", "status": "paired", "created": 10,
        "lastAccess": null, "revoked": null});
    let home = home.to_owned();
    store
        .transaction_with(move |tx| -> Result<(), StoreError> {
            tx.execute(
                "INSERT OR REPLACE INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                [&home, &record.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (directory, store)
}

#[tokio::test]
async fn fetch_copies_a_remote_worker_branch_through_a_verified_bundle_once() {
    let oracle = &oracle().value;
    let (_home_directory, home) = python_written_store().await;
    let home_id = home.local_server_id().unwrap().unwrap();
    let (peer_directory, peer) = peer_store(&home_id).await;
    let repositories = tempfile::tempdir().unwrap();
    let remote = repositories.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    run_git(&remote, &["init", "-q", "-b", "main"]);
    std::fs::write(remote.join("a.txt"), "a\n").unwrap();
    run_git(&remote, &["add", "a.txt"]);
    run_git(&remote, &["commit", "-q", "-m", "base"]);
    let destination = repositories.path().join("local");
    let output = Command::new("git")
        .args(["clone", "-q"])
        .arg(&remote)
        .arg(&destination)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    run_git(&remote, &["checkout", "-q", "-b", "worker"]);
    std::fs::write(remote.join("b.txt"), "b\n").unwrap();
    run_git(&remote, &["add", "b.txt"]);
    run_git(&remote, &["commit", "-q", "-m", "worker change"]);
    let tip = run_git(&remote, &["rev-parse", "HEAD"]);
    // The lead's remote worker proxy names the peer and its folder.
    let lead = oracle["lead"].clone();
    let mut worker = oracle["worker"].clone();
    worker["id"] = json!("0b5c2f2e-1f3b-4f7a-9a51-2f3c4d5e6f70");
    worker["remoteWorker"] = json!({"server": PEER});
    worker["cwd"] = json!(remote);
    let worker_id = worker["id"].as_str().unwrap().to_owned();
    let record = studio_runtime_store::AgentRecord::from_value(worker).unwrap();
    home.transaction_with(move |tx| -> Result<(), StoreError> {
        AgentRepository::put(tx, &record)?;
        Ok(())
    })
    .await
    .unwrap();
    let transport = Arc::new(LoopbackPeer {
        principal: home_id.clone(),
        driver: OrchestrationDriver::new(
            Arc::clone(&peer),
            Arc::new(RecordingTransport::default()),
            Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&peer))),
        ),
        sent: Mutex::new(Vec::new()),
    });
    let driver = Arc::new(OrchestrationDriver::new(
        Arc::clone(&home),
        Arc::clone(&transport) as Arc<dyn PairedTransport>,
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&home))),
    ));
    let tools: Arc<dyn MultiServerToolPort> =
        Arc::new(ServerTools::new(Arc::clone(&home), driver, false));
    let args = json!({"action": "fetch", "server": PEER, "agent_id": worker_id,
        "branch": "worker", "destination": destination});
    let result = call_tool(
        &home,
        &lead,
        "fetch",
        "call-fetch",
        &args,
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(result["success"], true, "{result}");
    let value = parsed(&result);
    assert_eq!(value["commit"], tip, "{value}");
    assert_eq!(value["branch"], "worker");
    assert!(value["bytes"].as_u64().unwrap() > 0);
    assert_eq!(run_git(&destination, &["rev-parse", "FETCH_HEAD"]), tip);
    assert_eq!(
        *transport.sent.lock().unwrap(),
        ["export", "chunk", "release"],
        "one export, one chunk, one release"
    );
    // The peer keeps no chunk data in its receipt and released the bundle.
    let chunk_result: String = peer
        .read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT result FROM runtime_server_inbox WHERE json_extract(result,'$.value.offset')=0 AND json_extract(result,'$.value.sha256') IS NOT NULL",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert!(!chunk_result.contains("\"data\""), "{chunk_result}");
    let exports = std::fs::read_dir(peer_directory.path().join(server_git::EXPORT_FOLDER))
        .unwrap()
        .count();
    assert_eq!(exports, 0, "the release removed the bundle");
    // An exact replay answers from the saved tool result without a transport call.
    let replay = call_tool(
        &home,
        &lead,
        "fetch",
        "call-fetch",
        &args,
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(parsed(&replay), value);
    assert_eq!(transport.sent.lock().unwrap().len(), 3);
    // A new request for a branch the destination already has sends no bundle.
    let again = call_tool(
        &home,
        &lead,
        "fetch-again",
        "call-fetch-again",
        &args,
        Some(tools),
    )
    .await;
    assert_eq!(again["success"], true, "{again}");
    assert_eq!(parsed(&again)["commit"], tip);
    assert_eq!(parsed(&again)["bytes"], 0);
    assert_eq!(transport.sent.lock().unwrap().len(), 4, "one export only");
}
