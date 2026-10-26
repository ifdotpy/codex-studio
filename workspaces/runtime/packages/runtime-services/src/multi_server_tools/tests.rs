#![allow(clippy::unwrap_used)]

use super::*;
use crate::multi_server_orchestration::{PairedTransport, RuntimeOrchestrationEffects};
use crate::provider_callbacks::{
    CallbackContext, CallbackDecision, ManagedToolPortsConfig, ManagedToolRuntimePorts,
    dispatch_with_managed_tools,
};
use std::process::Command;
use std::sync::Mutex;
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};

/// Far-future command output expiry shared with the fixture server.
const FAR: f64 = 4102444800.0;
const PEER: &str = "peer-server";

/// Test-only identity of the local server (a throwaway key pair).
const IDENTITY: &str = "{\"label\":\"Studio server\",\"privateKey\":\"-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIEABs3CR9YO/2snXAA7CZxSJcf+AXY62nmdKhdyv14QS\\n-----END PRIVATE KEY-----\\n\",\"publicKey\":\"-----BEGIN PUBLIC KEY-----\\nMCowBQYDK2VwAyEAElmQYRu24EVfx1oxPVvQb30C+R3RQaB0QboX7Xsi6OQ=\\n-----END PUBLIC KEY-----\\n\",\"serverId\":\"628c18136e1542fa9f45b899f3b75503\"}";
const LEAD: &str = "738cc392-d597-4954-80be-9f64ec57723e";
const WORKER: &str = "c4426480-f59c-4b32-832c-3ba4f43c1683";
const REMOTE: &str = "97118dce-786a-4bf7-b652-d545a80ffd0c";
const MOVED: &str = "a313b5da-674e-43d0-86b8-c4204bc46ed8";
const LINK: &str = "fixture-remote-link";

/// Bind the immutable actor shape to this test's live project directory.
fn seed(directory: &std::path::Path) -> Value {
    let project = directory.join("project");
    std::fs::create_dir(&project).unwrap();
    let cwd = project.to_string_lossy().into_owned();
    let base: Value =
            serde_json::from_str(&"{\"accountKey\": \"default\", \"executionSettingsAccountKey\": \"default\", \"provider\": \"codex\", \"yoloMode\": true, \"prompt\": \"fixture\", \"cwd\": \"@cwd\", \"role\": \"orchestrator\", \"needsTitle\": false, \"subagentConcurrencyVersion\": 2, \"maxAgentsExplicit\": false, \"model\": \"gpt-6-astra\", \"effort\": \"medium\", \"fastMode\": false, \"daybreakEnabled\": false, \"cyberAccessProgram\": \"standard\", \"concurrency\": 32, \"maxAgents\": 64, \"tokenBudget\": null, \"status\": \"running\", \"autoWake\": true, \"epoch\": 3, \"inFlight\": false, \"turnEpoch\": 3, \"usageResumeEnabled\": true, \"tokensUsed\": 0, \"compactions\": 0, \"compactionsObservedOnly\": false, \"contextUsage\": null, \"events\": 0, \"error\": null, \"profileId\": null, \"profileInstructions\": \"\", \"tail\": \"\", \"worktree\": false, \"workspaceMode\": \"worktree\", \"executionMode\": \"native\", \"hostProjectPath\": \"@cwd\", \"layrReady\": false, \"workspaceBackend\": null, \"workspaceModeExplicit\": false, \"workspaceInGit\": false, \"worktreeReady\": false, \"environment\": \"host\", \"worktreeWarning\": null, \"workerBaseRef\": null, \"workerBaseCommit\": null, \"workerBaseBehindMain\": null, \"workerBaseMainRef\": null, \"workerDefaults\": {\"model\": \"gpt-6-luna\", \"effort\": \"high\", \"fastMode\": false, \"daybreakEnabled\": false, \"delegationDepth\": 1}, \"reviewDefaults\": {\"model\": null, \"effort\": null}, \"agentMode\": \"multi\", \"agentModeRevision\": 0, \"agentModeSupported\": true}".replace("@cwd", &cwd)).unwrap();
    let actor = |index: usize, id: &str, name: &str, parent: Option<&str>, lead: bool| {
        let mut record = base.clone();
        record["id"] = json!(id);
        record["name"] = json!(name);
        record["isLead"] = json!(lead);
        record["parentId"] = json!(parent);
        record["rootId"] = json!(parent.unwrap_or(id));
        record["threadId"] = json!(format!("thread-{index}"));
        record["turnId"] = json!(format!("turn-{index}"));
        record["created"] = json!(1_792_510_147.0 + index as f64);
        record["projectId"] = json!(cwd);
        record
    };
    let mut remote = actor(2, REMOTE, "Remote worker", None, false);
    remote["remoteOrigin"] = json!({"home": PEER, "link": LINK});
    let mut moved = actor(3, MOVED, "Moved worker", None, false);
    moved["remoteOrigin"] = json!({"home": PEER, "link": LINK});
    moved["movedFrom"] = json!(PEER);
    json!({
        "lead": actor(0, LEAD, "Servers lead", None, true),
        "worker": actor(1, WORKER, "Servers worker", Some(LEAD), false),
        "remote": remote,
        "moved": moved,
        "projectId": project.to_string_lossy(),
    })
}

/// The paired fixture server's reply rules.
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

async fn seeded_store() -> (tempfile::TempDir, Arc<RuntimeStore>, Value) {
    seeded_store_with_identity(IDENTITY).await
}

async fn seeded_store_with_identity(
    identity_record: &str,
) -> (tempfile::TempDir, Arc<RuntimeStore>, Value) {
    let directory = tempfile::tempdir().unwrap();
    let identity = directory.path().join("multi-server");
    std::fs::create_dir(&identity).unwrap();
    std::fs::write(identity.join("identity.json"), identity_record).unwrap();
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
    let local = store.local_server_id().unwrap().unwrap();
    let seed = seed(directory.path());
    let project = seed["projectId"].as_str().unwrap().to_owned();
    let agents: Vec<_> = ["lead", "worker", "remote", "moved"]
        .into_iter()
        .map(|role| studio_runtime_store::AgentRecord::from_value(seed[role].clone()).unwrap())
        .collect();
    let link = json!({"id": LINK, "side": "remote", "server": PEER, "parent": "home-parent",
        "parentEpoch": 4, "workers": [REMOTE, MOVED]});
    let project_record = json!({"id": project, "path": project, "name": "Fixture project",
        "created": 1_792_510_147.0, "accountKey": "default", "accountRevision": 1,
        "homeServerId": local, "locations": [{"serverId": local, "path": project, "projectId": project, "gitOrigin": "https://example.invalid/fixture.git"}],
        "locationsRevision": 0});
    let peer = json!({"id": PEER, "clientId": PEER, "serverId": PEER, "label": "Peer",
        "kind": "server", "origin": "https://peer.fixture.ts.net", "publicKey": "fixture-public-key",
        "tailscaleUser": "owner@example.test", "status": "paired", "created": 10,
        "lastAccess": null, "revoked": null});
    store
        .transaction_with(move |tx| -> Result<(), StoreError> {
            for record in &agents {
                AgentRepository::put(tx, record)?;
            }
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES(?1,?2)",
                rusqlite::params![LINK, link.to_string()],
            )?;
            ProjectsRepository::put_project(tx, &project_record)?;
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                rusqlite::params![PEER, peer.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (directory, store, seed)
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
    call_with_ports(
        store,
        actor,
        tool,
        name,
        call_id,
        args,
        runtime_ports(tools, voice),
    )
    .await
}

async fn call_with_ports(
    store: &Arc<RuntimeStore>,
    actor: &Value,
    tool: &str,
    name: &str,
    call_id: &str,
    args: &Value,
    runtime_ports: ManagedToolRuntimePorts,
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
            runtime_ports,
            preparation: None,
        },
    )
    .await
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

struct Case {
    name: &'static str,
    actor: &'static str,
    tool: &'static str,
    args: Value,
    success: bool,
    sent: &'static [&'static str],
    expect: Value,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "list",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "list"}),
            success: true,
            sent: &[],
            expect: json!({"servers": [{"id": "peer-server", "status": "paired"}]}),
        },
        Case {
            name: "worker-refused",
            actor: "worker",
            tool: "orchestration_servers",
            args: json!({"action": "list"}),
            success: false,
            sent: &[],
            expect: json!("Only the lead can use remote server tools"),
        },
        Case {
            name: "unknown-action",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "nope", "server": "peer-server"}),
            success: false,
            sent: &[],
            expect: json!("Select list, projects, folders, git, fetch, or receipt"),
        },
        Case {
            name: "unpaired-server",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "git", "server": "other-server", "cwd": "/r", "argv": ["status", "--porcelain=v1"]}),
            success: false,
            sent: &[],
            expect: json!("Select a paired server"),
        },
        Case {
            name: "projects",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "projects", "server": "peer-server"}),
            success: true,
            sent: &["projects"],
            expect: json!({"outcome": "applied", "value": {"items": [{"id": "/peer/project", "path": "/peer/project", "name": "Peer project", "created": 1.0}]}}),
        },
        Case {
            name: "git",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "git", "server": "peer-server", "cwd": "/peer/repo", "argv": ["status", "--porcelain=v1"], "request_id": "git-read-1"}),
            success: true,
            sent: &["git"],
            expect: json!({"outcome": "applied", "value": {"exitCode": 0, "output": " M a.txt\n", "truncated": false}}),
        },
        Case {
            name: "folders-refused",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "folders", "server": "peer-server", "cwd": "/missing"}),
            success: true,
            sent: &["folders"],
            expect: json!({"outcome": "not_applied", "error": "This directory is unavailable"}),
        },
        Case {
            name: "receipt",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "receipt", "server": "peer-server", "request_id": "@git"}),
            success: true,
            sent: &[],
            expect: json!({"state": "complete", "error": null, "result": {"outcome": "applied", "value": {"exitCode": 0, "output": " M a.txt\n", "truncated": false}}}),
        },
        Case {
            name: "receipt-unknown",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "receipt", "server": "peer-server", "request_id": "no-such-request"}),
            success: false,
            sent: &[],
            expect: json!("Unknown remote request receipt"),
        },
        Case {
            name: "exec",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "exec", "server": "peer-server", "cwd": "/peer/repo", "command": ["true"], "timeout": 30}),
            success: true,
            sent: &["exec"],
            expect: json!({"outcome": "applied", "value": {"serverId": "peer-server", "status": "running", "startedAt": 10.0, "finishedAt": null, "exitCode": null, "signal": null, "duration": null, "timedOut": false, "outputLimit": 262144, "outputExpiresAt": 4102444800.0}}),
        },
        Case {
            name: "exec-read",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "exec_read", "server": "peer-server", "handle": "@exec"}),
            success: true,
            sent: &["exec_read"],
            expect: json!({"outcome": "applied", "value": {"status": "completed", "exitCode": 0, "stdout": "ok\n", "stdoutOffset": 0, "stdoutNextOffset": 3, "outputExpiresAt": 4102444800.0}}),
        },
        Case {
            name: "exec-foreign-handle",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "exec_read", "server": "peer-server", "handle": "not-a-handle"}),
            success: false,
            sent: &[],
            expect: json!("The command handle belongs to another actor or server"),
        },
        Case {
            name: "exec-relative-cwd",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "exec", "server": "peer-server", "cwd": "relative", "command": ["true"]}),
            success: false,
            sent: &[],
            expect: json!("Supply an absolute cwd on the selected server"),
        },
        Case {
            name: "exec-bad-timeout",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "exec", "server": "peer-server", "cwd": "/peer/repo", "command": ["true"], "timeout": 0}),
            success: false,
            sent: &[],
            expect: json!("Supply timeout seconds from 1 to 1800"),
        },
        Case {
            name: "location-matches",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "location_matches", "project": "@project", "server": "peer-server"}),
            success: true,
            sent: &["location_matches"],
            expect: json!({"outcome": "applied", "value": {"matches": [{"projectId": "/peer/project", "path": "/peer/project", "name": "project"}]}}),
        },
        Case {
            name: "add-location",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "add_location", "project": "@project", "server": "peer-server", "path": "/peer/project"}),
            success: true,
            sent: &["add_location"],
            expect: json!({"outcome": "applied", "value": {"serverId": "peer-server", "path": "/peer/project", "canonicalPath": "/peer/project", "projectId": "/peer/project", "gitOrigin": null}}),
        },
        Case {
            name: "add-location-again",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "add_location", "project": "@project", "server": "peer-server", "path": "/peer/other"}),
            success: false,
            sent: &[],
            expect: json!("This project already has a location on that server"),
        },
        Case {
            name: "remove-location",
            actor: "lead",
            tool: "orchestration_servers",
            args: json!({"action": "remove_location", "project": "@project", "server": "peer-server"}),
            success: true,
            sent: &["remove_location"],
            expect: json!({"outcome": "applied", "value": {"removed": true, "serverId": "peer-server"}}),
        },
        Case {
            name: "remote-task",
            actor: "remote",
            tool: "orchestration_task",
            args: json!({"action": "list"}),
            success: true,
            sent: &["task"],
            expect: json!({"apiVersion": 2, "items": [{"id": "home-task", "title": "Home task"}]}),
        },
        Case {
            name: "remote-message",
            actor: "remote",
            tool: "orchestration_message",
            args: json!({"target": "lead", "text": "  progress  ", "importance": "progress", "progress_key": "build", "progress_version": 2, "delivery": studio_runtime_schema::DeliveryMoment::CurrentTurn}),
            success: true,
            sent: &["message"],
            expect: json!({"stored": true, "target": "lead"}),
        },
        Case {
            name: "remote-message-bad-importance",
            actor: "remote",
            tool: "orchestration_message",
            args: json!({"target": "lead", "text": "x", "importance": "loud"}),
            success: false,
            sent: &[],
            expect: json!("Unknown message importance"),
        },
        Case {
            name: "remote-send-parent",
            actor: "remote",
            tool: "orchestration_send",
            args: json!({"agent_id": "parent", "text": "done", "delivery": studio_runtime_schema::DeliveryMoment::NextTurn}),
            success: true,
            sent: &["message"],
            expect: json!({"stored": true, "target": "parent"}),
        },
        Case {
            name: "remote-status",
            actor: "remote",
            tool: "orchestration_status",
            args: json!({}),
            success: true,
            sent: &["directory"],
            expect: json!({"tool": "orchestration_status", "agents": []}),
        },
        Case {
            name: "remote-peers",
            actor: "remote",
            tool: "orchestration_peers",
            args: json!({"scope": "team"}),
            success: true,
            sent: &["directory"],
            expect: json!({"tool": "orchestration_peers", "agents": []}),
        },
        Case {
            name: "remote-chat-read",
            actor: "remote",
            tool: "orchestration_chat_read",
            args: json!({"room_id": "home-room"}),
            success: true,
            sent: &["chat_read"],
            expect: json!({"messages": [], "room": "home-room"}),
        },
        Case {
            name: "remote-context-plan",
            actor: "remote",
            tool: "orchestration_context",
            args: json!({"topic": "plan"}),
            success: true,
            sent: &["context"],
            expect: json!({"topic": "plan", "version": "v1", "content": "Home plan"}),
        },
        Case {
            name: "remote-context-unknown",
            actor: "remote",
            tool: "orchestration_context",
            args: json!({"topic": "complaints"}),
            success: true,
            sent: &["context"],
            expect: json!({"outcome": "unknown", "detail": "The request is saved. Do not submit another identity."}),
        },
        Case {
            name: "remote-complaint-refused",
            actor: "remote",
            tool: "orchestration_complaint",
            args: json!({"action": "submit", "text": "help"}),
            success: false,
            sent: &["complaint"],
            expect: json!("The home server refused the complaint"),
        },
        Case {
            name: "moved-send",
            actor: "moved",
            tool: "orchestration_send",
            args: json!({"agent_id": "parent", "text": "moved hello"}),
            success: true,
            sent: &["move_send"],
            expect: json!({"forwarded": "move_send"}),
        },
        Case {
            name: "moved-spawn",
            actor: "moved",
            tool: "orchestration_spawn",
            args: json!({"agents": [{"name": "Child", "prompt": "work"}]}),
            success: true,
            sent: &["move_spawn"],
            expect: json!({"forwarded": "move_spawn"}),
        },
        Case {
            name: "moved-interrupt",
            actor: "moved",
            tool: "orchestration_interrupt",
            args: json!({"agent_id": "some-child"}),
            success: true,
            sent: &["move_interrupt"],
            expect: json!({"forwarded": "move_interrupt"}),
        },
        Case {
            name: "moved-manage",
            actor: "moved",
            tool: "orchestration_agent_manage",
            args: json!({"action": "rename", "agent_id": "some-child", "name": "New"}),
            success: true,
            sent: &["move_manage"],
            expect: json!({"forwarded": "move_manage"}),
        },
        Case {
            name: "speak-saved",
            actor: "lead",
            tool: "orchestration_speak",
            args: json!({"text": "Build finished"}),
            success: true,
            sent: &[],
            expect: json!({"status": "saved", "playback": "not confirmed", "record": {"kind": "orchestrator", "text": "Build finished"}}),
        },
        Case {
            name: "speak-worker-refused",
            actor: "worker",
            tool: "orchestration_speak",
            args: json!({"text": "Hello"}),
            success: false,
            sent: &[],
            expect: json!("Voice is available in an orchestrator chat only"),
        },
        Case {
            name: "speak-blank-refused",
            actor: "lead",
            tool: "orchestration_speak",
            args: json!({"text": "   "}),
            success: false,
            sent: &[],
            expect: json!("Speech text must contain 1 to 4096 characters"),
        },
    ]
}

/// True when `actual` holds every field of `expected` (objects) or equals it.
fn covers(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|found| covers(found, value))),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len() && actual.iter().zip(expected).all(|(a, e)| covers(a, e))
        }
        _ => actual == expected,
    }
}

fn resolve(
    value: &Value,
    results: &std::collections::HashMap<&str, Value>,
    project: &str,
) -> Value {
    match value {
        Value::String(text) => match text.as_str() {
            "@project" => json!(project),
            "@git" => results["git"]["requestId"].clone(),
            "@exec" => results["exec"]["requestId"].clone(),
            _ => value.clone(),
        },
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| resolve(v, results, project)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve(v, results, project)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[tokio::test]
async fn servers_tool_sends_envelopes_keeps_receipts_and_project_locations() {
    let (_directory, store, seed) = seeded_store().await;
    for role in ["lead", "worker", "remote", "moved"] {
        for field in ["cwd", "hostProjectPath", "projectId"] {
            assert!(std::path::Path::new(seed[role][field].as_str().unwrap()).is_dir());
        }
    }
    let transport = Arc::new(RecordingTransport::default());
    let driver = Arc::new(OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::clone(&transport) as Arc<dyn PairedTransport>,
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    ));
    let tools: Arc<dyn MultiServerToolPort> =
        Arc::new(ServerTools::new(Arc::clone(&store), driver, false));
    let project = seed["projectId"].as_str().unwrap().to_owned();
    let mut results = std::collections::HashMap::new();
    for case in cases() {
        let name = case.name;
        let actor = &seed[case.actor];
        let before = transport.sent.lock().unwrap().len();
        let args = resolve(&case.args, &results, &project);
        let result = call_named_tool(
            &store,
            actor,
            case.tool,
            name,
            &format!("call-{name}"),
            &args,
            Some(Arc::clone(&tools)),
            None,
        )
        .await;
        let sent = transport.sent.lock().unwrap()[before..].to_vec();
        let actions: Vec<&str> = sent
            .iter()
            .map(|entry| entry["envelope"]["action"].as_str().unwrap())
            .collect();
        assert_eq!(actions, case.sent, "{name}: sent envelopes");
        if matches!(name, "remote-message" | "remote-send-parent") {
            assert_eq!(
                sent[0]["envelope"]["payload"]["args"]["delivery"], args["delivery"],
                "{name}: explicit timing survives tool forwarding"
            );
        }
        assert!(
            sent.iter().all(|entry| entry["server"] == PEER),
            "{name}: sent to the paired server"
        );
        assert_eq!(result["success"], case.success, "{name}: {result}");
        let value = parsed(&result);
        assert!(
            covers(&value, &case.expect),
            "{name}: tool value {value} lacks {}",
            case.expect
        );
        let expiry = result
            .get("serverOutputExpiresAt")
            .cloned()
            .unwrap_or(Value::Null);
        let expected_expiry = if name.starts_with("exec") && case.success {
            json!(FAR)
        } else {
            Value::Null
        };
        assert_eq!(expiry, expected_expiry, "{name}: command output expiry");
        if name == "add-location" || name == "remove-location" {
            let record = store
                .test_read_transaction(|tx| Ok(ProjectsRepository::get(tx, &project)?.unwrap().1))
                .unwrap();
            let (locations, revision) = if name == "add-location" {
                (2, 1)
            } else {
                (1, 2)
            };
            assert_eq!(
                record["locations"].as_array().unwrap().len(),
                locations,
                "{name}: locations"
            );
            assert_eq!(record["locationsRevision"], revision, "{name}: revision");
        }
        results.insert(name, value);
    }
}

#[tokio::test]
async fn servers_tool_names_its_dependency_until_the_driver_is_attached() {
    let (_directory, store, seed) = seeded_store().await;
    let result = call_tool(
        &store,
        &seed["lead"],
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
    let (_directory, store, seed) = seeded_store().await;
    let transport = Arc::new(Lost(Mutex::new(0)));
    let driver = Arc::new(OrchestrationDriver::new(
        Arc::clone(&store),
        Arc::clone(&transport) as Arc<dyn PairedTransport>,
        Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
    ));
    let tools: Arc<dyn MultiServerToolPort> =
        Arc::new(ServerTools::new(Arc::clone(&store), driver, false));
    let lead = &seed["lead"];
    let args = json!({"action": "exec", "server": PEER, "cwd": "/peer/repo", "command": ["true"], "request_id":"lost-operation"});
    let first = call_tool(
        &store,
        lead,
        "lost",
        "call-lost",
        &args,
        Some(tools.clone()),
    )
    .await;
    assert_eq!(first["success"], true, "{first}");
    assert_eq!(parsed(&first)["outcome"], "unknown");
    let replay = call_tool(
        &store,
        lead,
        "lost",
        "call-lost",
        &args,
        Some(tools.clone()),
    )
    .await;
    assert_eq!(replay, first);
    assert_eq!(*transport.0.lock().unwrap(), 1);
    // The command row is now in receipt form: a new call with the same
    // request identity can only probe for the receipt.
    let outbox = parsed(&first)["requestId"].as_str().unwrap().to_owned();
    let body: String = store
        .test_read_transaction(|tx| {
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
    let retry = call_tool(&store, lead, "retry", "new-call", &args, Some(tools)).await;
    assert_eq!(retry, first);
    assert_eq!(*transport.0.lock().unwrap(), 1);
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

#[allow(clippy::disallowed_methods)]
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
    let (directory, store, _) =
        seeded_store_with_identity(&json!({"serverId": PEER}).to_string()).await;
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
#[allow(clippy::disallowed_methods)]
async fn fetch_copies_a_remote_worker_branch_through_a_verified_bundle_once() {
    let (_home_directory, home, seed) = seeded_store().await;
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
    let lead = seed["lead"].clone();
    let mut worker = seed["worker"].clone();
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
        .test_read_transaction(|tx| {
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

/// `orchestration_sign_in` (docs/vm-browser-bridge.md, section 7) through the managed-tool
/// dispatcher: the host bridge queues the link and the card is written at once; a second call
/// with the same link returns the same card; without the attached bridges the tool refuses.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sign_in_shows_one_card_through_the_chat_bridge() {
    let (directory, store, seed) = seeded_store().await;
    let lead = &seed["lead"];
    let service = crate::browser_host::HostBridgeService::start(
        directory.path(),
        Arc::new(crate::browser_host::StoreChats {
            store: Arc::clone(&store),
        }),
        Default::default(),
        None,
    )
    .unwrap();
    let guest: Arc<dyn crate::linux_vm_guest::GuestRpc> = Arc::new(
        crate::linux_vm_guest::UnixSocketGuestRpc::new(service.control_socket.clone()),
    );
    let sources = crate::sign_in::SignInSources::new(vec![crate::browser_cards::CardSource {
        source: "host",
        guest,
        forwarder: None,
        http_providers: Default::default(),
    }]);
    let link = json!({"url": "https://device.sso.us-east-1.amazonaws.com/?user_code=WDJB-MJHT"});
    let attached = || {
        let ports = runtime_ports(None, None);
        assert!(ports.late.sign_in.set(sources.clone()).is_ok());
        ports
    };
    let first = call_with_ports(
        &store,
        lead,
        crate::sign_in::TOOL,
        "sign-in",
        "call-sign-in",
        &link,
        attached(),
    )
    .await;
    assert_eq!(first["success"], true, "{first}");
    let value = parsed(&first);
    assert_eq!(value["status"], "pending", "{value}");
    let card = value["requestId"].as_str().unwrap().to_owned();
    let saved = store
        .read_async("test", move |tx| {
            studio_runtime_store::QuestionsWriteRepository::request(tx, &card)
        })
        .await
        .unwrap()
        .expect("the card is written before the tool answers");
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        (saved["method"].as_str(), saved["agent"].as_str()),
        (Some("browser/open"), lead["id"].as_str())
    );
    let again = call_with_ports(
        &store,
        lead,
        crate::sign_in::TOOL,
        "sign-in",
        "call-sign-in-again",
        &link,
        attached(),
    )
    .await;
    assert_eq!(parsed(&again)["requestId"], value["requestId"]);
    let detached = call_with_ports(
        &store,
        lead,
        crate::sign_in::TOOL,
        "sign-in",
        "call-sign-in-detached",
        &link,
        runtime_ports(None, None),
    )
    .await;
    assert_eq!(detached["success"], false, "{detached}");
    service.stop().await;
}

#[tokio::test]
async fn speak_plays_once_in_the_ready_voice_session() {
    let (_directory, store, seed) = seeded_store().await;
    let lead = &seed["lead"];
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
    let (_directory, store, seed) = seeded_store().await;
    let lead = &seed["lead"];
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
        .test_read_transaction(|tx| {
            Ok(tx.query_row("SELECT count(*) FROM voice_records", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(records, 0);
}

#[tokio::test]
async fn a_remote_worker_keeps_its_local_tools_and_names_the_driver_for_forwarded_ones() {
    let (_directory, store, seed) = seeded_store().await;
    let remote = &seed["remote"];
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
        .test_read_transaction(|tx| {
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
    let committed = Arc::new(tokio::sync::Notify::new());
    store.add_commit_observer({
        let committed = Arc::clone(&committed);
        move || committed.notify_one()
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let status: String = store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT json_extract(record,'$.status') FROM runtime_server_exec WHERE id=?1",
                    [handle],
                    |row| row.get(0),
                )
                .unwrap();
            if !matches!(status.as_str(), "starting" | "running") {
                break;
            }
            committed.notified().await;
        }
    })
    .await
    .expect("command commits its final state");
    let read = call_tool(
        store,
        lead,
        name,
        &format!("call-{name}"),
        &json!({"action": "exec_read", "handle": handle}),
        Some(Arc::clone(tools)),
    )
    .await;
    parsed(&read)["value"].clone()
}

fn exit_events(store: &RuntimeStore) -> Vec<Value> {
    store
        .test_read_transaction(|tx| {
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
    let (_directory, store, seed) = seeded_store().await;
    let local = store.local_server_id().unwrap().unwrap();
    let lead = &seed["lead"];
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
        .test_read_transaction(|tx| {
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
        .test_read_transaction(|tx| {
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
    let (_directory, store, _) = seeded_store().await;
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
        .test_read_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT body FROM runtime_server_outbox WHERE server=?1 AND json_extract(body,'$.action')='exec_event'",
                [PEER],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let notice: Value = serde_json::from_str(&notice).unwrap();
    assert_eq!(notice["requestId"], "b75e85fe-064e-5373-9c79-ff2454f3c4e1");
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
    let (_directory, store, seed) = seeded_store().await;
    let lead = seed["lead"]["id"].as_str().unwrap().to_owned();
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
        .test_read_transaction(|tx| {
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

#[test]
fn forwarded_message_rejects_invalid_delivery_before_transport() {
    for delivery in [json!("later"), json!(7), json!({})] {
        assert!(worker_message("lead", &json!({"text":"hello", "delivery":delivery})).is_err());
    }
}
