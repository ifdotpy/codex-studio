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
        "task" => {
            json!({"requestId": key, "outcome": "applied", "value": {"apiVersion": 2, "items": [
            {"id": "home-task", "title": "Home task"}]}})
        }
        "message" => {
            json!({"requestId": key, "outcome": "applied", "value": {"stored": true, "target": payload["args"]["target"]}})
        }
        "directory" => {
            json!({"requestId": key, "outcome": "applied", "value": {"tool": payload["args"]["tool"], "agents": []}})
        }
        "chat_read" => {
            json!({"requestId": key, "outcome": "applied", "value": {"messages": [], "room": payload["args"]["room_id"]}})
        }
        "context" if payload["args"]["topic"] == "complaints" => {
            json!({"requestId": key, "outcome": "unknown"})
        }
        "context" => {
            json!({"requestId": key, "outcome": "applied", "value": {"topic": "plan", "version": "v1", "content": "Home plan"}})
        }
        "complaint" => {
            json!({"requestId": key, "outcome": "not_applied", "error": "The home server refused the complaint"})
        }
        action @ ("move_send" | "move_spawn" | "move_interrupt" | "move_manage") => {
            json!({"requestId": key, "outcome": "applied", "value": {"forwarded": action}})
        }
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

fn frame(context: &CallbackContext, tool: &str, name: &str, call_id: &str, args: &Value) -> Frame {
    Frame::Request(RequestFrame {
        id: RequestId::String(format!("rpc-{name}")),
        method: Method::ItemToolCall,
        params: Some(json!({
            "threadId": context.thread,
            "turnId": context.turn,
            "callId": call_id,
            "tool": tool,
            "arguments": args,
        })),
        fields: Default::default(),
    })
}

fn runtime_ports(
    tools: Option<Arc<dyn MultiServerToolPort>>,
    voice: Option<Arc<dyn crate::voice_runtime::VoiceRequestPort>>,
) -> ManagedToolRuntimePorts {
    let late = crate::provider_callbacks::tools::LatePorts::default();
    if let Some(tools) = tools {
        assert!(late.multi_server.set(tools).is_ok());
    }
    if let Some(voice) = voice {
        assert!(late.voice.set(voice).is_ok());
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
    call_named_tool(
        store,
        actor,
        "orchestration_servers",
        name,
        call_id,
        args,
        tools,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn call_named_tool(
    store: &Arc<RuntimeStore>,
    actor: &Value,
    tool: &str,
    name: &str,
    call_id: &str,
    args: &Value,
    tools: Option<Arc<dyn MultiServerToolPort>>,
    voice: Option<Arc<dyn crate::voice_runtime::VoiceRequestPort>>,
) -> Value {
    let context = context(actor);
    let frame = frame(&context, tool, name, call_id, args);
    let ports = crate::provider_callbacks::managed_tool_ports_with_runtime(
        store,
        &context,
        &frame,
        ManagedToolPortsConfig {
            wake: Arc::new(tokio::sync::Notify::new()),
            monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
            runtime_ports: runtime_ports(tools, voice),
            preparation: None,
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
        let actor = ["lead", "worker", "remote", "moved"]
            .into_iter()
            .map(|role| &oracle[role])
            .find(|actor| actor["id"] == case["actor"])
            .unwrap();
        let before = transport.sent.lock().unwrap().len();
        let result = call_named_tool(
            &store,
            actor,
            case["tool"].as_str().unwrap(),
            name,
            case["callId"].as_str().unwrap(),
            &case["arguments"],
            Some(Arc::clone(&tools)),
            None,
        )
        .await;
        let sent = transport.sent.lock().unwrap()[before..].to_vec();
        assert_eq!(json!(sent), case["sent"], "{name}: sent envelopes");
        assert_eq!(result["success"], case["success"], "{name}: {result}");
        let mut value = parsed(&result);
        let mut expected = case["value"].clone();
        if name.starts_with("speak") {
            // The record clock and sequence belong to each store.
            for record in [&mut value, &mut expected] {
                if let Some(row) = record.get_mut("record").and_then(Value::as_object_mut) {
                    row.remove("created");
                    row.remove("seq");
                }
            }
        }
        assert_eq!(value, expected, "{name}: tool value");
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

/// A provider session that accepts every speech request.
#[derive(Default)]
struct RecordingVoice {
    requests: Mutex<Vec<(String, Value)>>,
}

impl crate::voice_runtime::VoiceRequestPort for RecordingVoice {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        _session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> crate::voice_runtime::VoiceFuture<
        'a,
        Result<
            crate::voice_runtime::VoiceConnectionScope,
            crate::voice_runtime::VoiceWriteCertainty,
        >,
    > {
        Box::pin(async move {
            Ok(crate::voice_runtime::VoiceConnectionScope {
                agent_id: agent_id.to_owned(),
                epoch,
                account_key: account_key.to_owned(),
                thread_id: thread_id.to_owned(),
                connection_id: "connection".into(),
                generation: 7,
            })
        })
    }

    fn request<'a>(
        &'a self,
        _scope: crate::voice_runtime::VoiceConnectionScope,
        operation_id: String,
        _method: crate::voice_runtime::Method,
        params: Value,
        _deadline: std::time::Instant,
    ) -> crate::voice_runtime::VoiceFuture<'a, crate::voice_runtime::VoiceRequestReceipt> {
        Box::pin(async move {
            self.requests.lock().unwrap().push((operation_id, params));
            crate::voice_runtime::VoiceRequestReceipt {
                certainty: crate::voice_runtime::VoiceWriteCertainty::Written,
            }
        })
    }

    fn reply<'a>(
        &'a self,
        _scope: crate::voice_runtime::VoiceConnectionScope,
        _operation_id: String,
        _reply: crate::voice_runtime::VoiceReply,
        _deadline: std::time::Instant,
    ) -> crate::voice_runtime::VoiceFuture<'a, crate::voice_runtime::VoiceRequestReceipt> {
        Box::pin(async move {
            crate::voice_runtime::VoiceRequestReceipt {
                certainty: crate::voice_runtime::VoiceWriteCertainty::NotSent,
            }
        })
    }
}

async fn ready_voice_session(store: &RuntimeStore, lead: &Value) {
    let lead_id = lead["id"].as_str().unwrap().to_owned();
    let thread = lead["threadId"].as_str().unwrap().to_owned();
    store
        .transaction_with(move |tx| -> Result<(), StoreError> {
            tx.execute(
                "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('voice-session',?1,1,?2,'default','connection','ready')",
                [&lead_id, &thread],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn speak_plays_once_in_the_ready_voice_session() {
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    let lead = &oracle["lead"];
    ready_voice_session(&store, lead).await;
    let voice = Arc::new(RecordingVoice::default());
    let port: Arc<dyn crate::voice_runtime::VoiceRequestPort> = voice.clone();
    let args = json!({"text": "Build finished"});
    let first = call_named_tool(
        &store,
        lead,
        "orchestration_speak",
        "speak",
        "call-speak",
        &args,
        None,
        Some(Arc::clone(&port)),
    )
    .await;
    assert_eq!(first["success"], true, "{first}");
    let value = parsed(&first);
    assert_eq!(value["record"]["id"], "speak:thread-0:call-speak");
    assert_eq!(value["record"]["text"], "Build finished");
    assert_eq!(value["session"], "voice-session");
    assert_eq!(value["state"], "submitted");
    let requests = voice.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["text"], "Build finished");
    // The saved tool result answers an exact replay without a second request.
    let replay = call_named_tool(
        &store,
        lead,
        "orchestration_speak",
        "speak",
        "call-speak",
        &args,
        None,
        Some(port),
    )
    .await;
    assert_eq!(parsed(&replay), value);
    assert_eq!(voice.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn speak_refuses_before_saving_when_a_ready_session_has_no_provider() {
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    let lead = &oracle["lead"];
    ready_voice_session(&store, lead).await;
    let result = call_named_tool(
        &store,
        lead,
        "orchestration_speak",
        "speak-no-port",
        "call-speak-no-port",
        &json!({"text": "Hello"}),
        None,
        None,
    )
    .await;
    assert_eq!(result["success"], false, "{result}");
    assert!(
        parsed(&result)
            .as_str()
            .unwrap()
            .contains("voice/provider-session"),
        "{result}"
    );
    let records: i64 = store
        .read_transaction(|tx| {
            Ok(tx.query_row("SELECT count(*) FROM voice_records", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(records, 0);
}

#[tokio::test]
async fn a_remote_worker_keeps_its_local_tools_and_names_the_driver_for_forwarded_ones() {
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    let remote = &oracle["remote"];
    // Python runs a remote worker's request reads on its own server.
    let local = call_named_tool(
        &store,
        remote,
        "orchestration_request",
        "remote-request",
        "call-remote-request",
        &json!({"action": "list"}),
        None,
        None,
    )
    .await;
    assert_eq!(local["success"], true, "{local}");
    // The task board belongs to the home server; without a driver the call
    // names its dependency and writes nothing.
    let forwarded = call_named_tool(
        &store,
        remote,
        "orchestration_task",
        "remote-task-unattached",
        "call-remote-task-unattached",
        &json!({"action": "list"}),
        None,
        None,
    )
    .await;
    assert_eq!(forwarded["success"], false, "{forwarded}");
    assert!(
        parsed(&forwarded)
            .as_str()
            .unwrap()
            .contains("multi-server/worker-call"),
        "{forwarded}"
    );
    let outbox: i64 = store
        .read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM runtime_server_outbox WHERE json_extract(body,'$.action')='task'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(outbox, 0);
}

fn local_tools(store: &Arc<RuntimeStore>) -> Arc<dyn MultiServerToolPort> {
    let driver = Arc::new(OrchestrationDriver::new(
        Arc::clone(store),
        Arc::new(RecordingTransport::default()),
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(store))),
    ));
    Arc::new(ServerTools::new(Arc::clone(store), driver, true))
}

async fn wait_for_status(
    store: &Arc<RuntimeStore>,
    lead: &Value,
    tools: &Arc<dyn MultiServerToolPort>,
    handle: &str,
    name: &str,
) -> Value {
    for attempt in 0..250 {
        let read = call_tool(
            store,
            lead,
            &format!("{name}-{attempt}"),
            &format!("call-{name}-{attempt}"),
            &json!({"action": "exec_read", "handle": handle}),
            Some(Arc::clone(tools)),
        )
        .await;
        let value = parsed(&read)["value"].clone();
        if !matches!(value["status"].as_str(), Some("starting" | "running")) {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    panic!("{name}: the command did not finish");
}

fn exit_events(store: &RuntimeStore) -> Vec<Value> {
    store
        .read_transaction(|tx| {
            let mut statement = tx.prepare(
                "SELECT text FROM runtime_events WHERE kind='monitor_exit' ORDER BY created",
            )?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .map(|text| serde_json::from_str(&text).unwrap())
                .collect())
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_exec_runs_once_with_output_input_cancel_and_exit_notices() {
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    let local = store.local_server_id().unwrap().unwrap();
    let lead = &oracle["lead"];
    let tools = local_tools(&store);
    let folder = tempfile::tempdir().unwrap();
    let cwd = folder.path().to_str().unwrap();
    // A short command answers with its final page.
    let short = call_tool(
        &store,
        lead,
        "exec-short",
        "call-exec-short",
        &json!({"action": "exec", "server": "local", "cwd": cwd, "timeout": 5,
            "command": ["sh", "-c", "printf hi; printf err >&2; exit 3"]}),
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(short["success"], true, "{short}");
    let receipt = parsed(&short);
    assert_eq!(receipt["outcome"], "applied", "{receipt}");
    let value = &receipt["value"];
    assert_eq!(value["status"], "completed", "{value}");
    assert_eq!(value["exitCode"], 3);
    assert_eq!(value["serverId"], local);
    assert_eq!(value["stdout"], "hi");
    assert_eq!(value["stderr"], "err");
    assert_eq!(value["stdoutBytes"], 2);
    let handle = receipt["requestId"].as_str().unwrap().to_owned();
    assert_eq!(value["handle"], handle);
    assert!(short["serverOutputExpiresAt"].is_number(), "{short}");
    // The command record and its audit row exist once.
    let (records, audits): (i64, i64) = store
        .read_transaction(|tx| {
            Ok((
                tx.query_row("SELECT count(*) FROM runtime_server_exec", [], |row| {
                    row.get(0)
                })?,
                tx.query_row(
                    "SELECT count(*) FROM runtime_server_exec_audit WHERE end IS NOT NULL",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!((records, audits), (1, 1));
    let read = call_tool(
        &store,
        lead,
        "exec-short-read",
        "call-exec-short-read",
        &json!({"action": "exec_read", "handle": handle, "stdout_offset": 1}),
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(parsed(&read)["value"]["stdout"], "i", "{read}");
    // Input reaches a running command.
    let started = call_tool(
        &store,
        lead,
        "exec-input",
        "call-exec-input",
        &json!({"action": "exec", "cwd": cwd, "timeout": 30,
            "command": ["sh", "-c", "read line; printf got:%s \"$line\""]}),
        Some(Arc::clone(&tools)),
    )
    .await;
    let started = parsed(&started);
    assert_eq!(started["value"]["status"], "starting", "{started}");
    let input_handle = started["requestId"].as_str().unwrap().to_owned();
    let accepted = call_tool(
        &store,
        lead,
        "exec-input-send",
        "call-exec-input-send",
        &json!({"action": "exec_input", "handle": input_handle, "input": "abc\n", "close_stdin": true}),
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(parsed(&accepted)["value"]["accepted"], true, "{accepted}");
    let finished = wait_for_status(&store, lead, &tools, &input_handle, "exec-input-read").await;
    assert_eq!(finished["status"], "completed", "{finished}");
    assert_eq!(finished["stdout"], "got:abc");
    // Cancel stops the process group.
    let sleeping = call_tool(
        &store,
        lead,
        "exec-cancel",
        "call-exec-cancel",
        &json!({"action": "exec", "cwd": cwd, "timeout": 60, "command": ["sleep", "30"]}),
        Some(Arc::clone(&tools)),
    )
    .await;
    let cancel_handle = parsed(&sleeping)["requestId"].as_str().unwrap().to_owned();
    let cancel = call_tool(
        &store,
        lead,
        "exec-cancel-send",
        "call-exec-cancel-send",
        &json!({"action": "exec_cancel", "handle": cancel_handle}),
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(parsed(&cancel)["value"]["accepted"], true, "{cancel}");
    let cancelled = wait_for_status(&store, lead, &tools, &cancel_handle, "exec-cancel-read").await;
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    assert_eq!(cancelled["signal"], 9, "{cancelled}");
    // A background process that outlives the command stops with it.
    let background = call_tool(
        &store,
        lead,
        "exec-background",
        "call-exec-background",
        &json!({"action": "exec", "cwd": cwd, "timeout": 5,
            "command": ["sh", "-c", "sleep 30 & printf started"]}),
        Some(Arc::clone(&tools)),
    )
    .await;
    let background = parsed(&background)["value"].clone();
    assert_eq!(background["status"], "completed", "{background}");
    assert_eq!(background["exitCode"], 0);
    assert_eq!(background["stdout"], "started");
    assert_eq!(background["stoppedDescendants"], 1, "{background}");
    // Each final record told the lead once.
    let events = exit_events(&store);
    let handles: Vec<&str> = events
        .iter()
        .filter_map(|event| event["id"].as_str())
        .collect();
    for expected in [&handle, &input_handle, &cancel_handle] {
        assert_eq!(
            handles
                .iter()
                .filter(|id| **id == expected.as_str())
                .count(),
            1,
            "{events:?}"
        );
    }
    assert_eq!(
        events[0]["detail"],
        "Read output with orchestration_servers action=exec_read."
    );
    // The exact call replays its saved receipt; the command does not run again.
    let replay = call_tool(
        &store,
        lead,
        "exec-short",
        "call-exec-short",
        &json!({"action": "exec", "server": "local", "cwd": cwd, "timeout": 5,
            "command": ["sh", "-c", "printf hi; printf err >&2; exit 3"]}),
        Some(Arc::clone(&tools)),
    )
    .await;
    assert_eq!(parsed(&replay), receipt);
    let audits: i64 = store
        .read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM runtime_server_exec_audit",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(audits, 4, "the replay started no command");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_python_lead_runs_a_command_here_and_gets_one_exit_notice() {
    let (_directory, store) = python_written_store().await;
    let driver = OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::new(RecordingTransport::default()),
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    );
    let folder = tempfile::tempdir().unwrap();
    // Python `_exec_tool` on the paired server sends this envelope.
    let envelope = json!({"requestId": "peer-exec-1", "action": "exec", "payload": {
        "actor": "peer-lead", "cwd": folder.path(), "command": ["sh", "-c", "printf remote"],
        "env": {}, "timeout": 5, "output_limit": 262144}});
    let now = crate::event_write::unix_time().unwrap() as i64;
    let outcome = driver.receive(PEER, &envelope, now).await.unwrap();
    let ExchangeOutcome::Applied(value) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(value["status"], "completed", "{value}");
    assert_eq!(value["stdout"], "remote");
    // A replay of the same envelope answers from the receipt.
    let replay = driver.receive(PEER, &envelope, now).await.unwrap();
    assert_eq!(replay, ExchangeOutcome::Applied(value.clone()));
    // Another server's handle is refused without an effect.
    let foreign = driver
        .receive(
            PEER,
            &json!({"requestId": "peer-read-1", "action": "exec_read",
                "payload": {"actor": "another-lead", "handle": "peer-exec-1"}}),
            now,
        )
        .await
        .unwrap();
    assert_eq!(
        foreign,
        ExchangeOutcome::NotApplied("The command handle belongs to another actor or server".into())
    );
    let notice: String = store
        .read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT body FROM runtime_server_outbox WHERE server=?1 AND json_extract(body,'$.action')='exec_event'",
                [PEER],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let notice: Value = serde_json::from_str(&notice).unwrap();
    assert_eq!(
        notice["requestId"],
        crate::projects::cross_server_identity(&["command-exit", "peer-exec-1", "completed"])
    );
    assert_eq!(notice["payload"]["actor"], "peer-lead");
    assert_eq!(notice["payload"]["handle"], "peer-exec-1");
    assert_eq!(notice["payload"]["record"]["exitCode"], 0);
    // Python reports a command that cannot start with `os.strerror`.
    let missing = driver
        .receive(
            PEER,
            &json!({"requestId": "peer-exec-2", "action": "exec", "payload": {
                "actor": "peer-lead", "cwd": folder.path(), "command": ["/nonexistent/command"],
                "timeout": 5}}),
            now,
        )
        .await
        .unwrap();
    let ExchangeOutcome::Applied(missing) = missing else {
        panic!("{missing:?}");
    };
    assert_eq!(missing["status"], "failed", "{missing}");
    assert_eq!(missing["error"], "No such file or directory");
}

#[tokio::test]
async fn a_command_without_a_final_record_after_a_restart_stays_unknown() {
    let oracle = &oracle().value;
    let (_directory, store) = python_written_store().await;
    let lead = oracle["lead"]["id"].as_str().unwrap().to_owned();
    let record = json!({"handle": "lost-command", "serverId": "x", "status": "running",
        "startedAt": crate::event_write::unix_time().unwrap(), "finishedAt": null, "exitCode": null,
        "signal": null, "duration": null, "timedOut": false, "outputLimit": 262144});
    store
        .transaction_with(move |tx| -> Result<(), StoreError> {
            tx.execute(
                "INSERT INTO runtime_server_exec VALUES('lost-command',?1,?2,?3)",
                rusqlite::params![PEER, lead, record.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    struct Silent;
    impl crate::server_exec::ExecEvents for Silent {
        fn completed<'a>(&'a self, _: &'a str, _: &'a str, _: &'a Value) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }
    crate::server_exec::ServerExec::for_store(&store, Arc::new(Silent))
        .await
        .unwrap();
    let saved: String = store
        .read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT record FROM runtime_server_exec WHERE id='lost-command'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["status"], "unknown");
    assert_eq!(
        saved["error"],
        "Command outcome unknown; inspect the target server. The command was not run again."
    );
    assert!(saved["finishedAt"].is_number());
}
