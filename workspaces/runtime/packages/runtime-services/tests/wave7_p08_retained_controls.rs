use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_services::{
    claude_controls::{ControlError, service::*},
    claude_runtime::*,
};

struct AccountPort {
    binding: Mutex<ControlBinding>,
    calls: Mutex<Vec<ClaudeControlCall>>,
    fault: &'static str,
}
fn binding() -> ControlBinding {
    ControlBinding::new("account".into(), "retained-connection".into(), 7).expect("binding")
}
impl ClaudeSessionPort for AccountPort {
    fn binding(&self) -> ControlBinding {
        self.binding.lock().expect("binding").clone()
    }
    fn request(&self, call: ClaudeControlCall) -> ControlFuture<'_, ClaudeControlReceipt> {
        Box::pin(async move {
            self.calls.lock().expect("calls").push(call.clone());
            let mut identity = call.identity;
            match self.fault {
                "lost" => return Err(ControlError::Unknown),
                "deadline" => std::future::pending::<()>().await,
                "agent" => identity.scope.agent = "foreign".into(),
                "epoch" => identity.scope.epoch += 1,
                "request" => identity.scope.request_id = Some("foreign".into()),
                "payload" => identity.signature = "foreign".into(),
                "method" => identity.method = studio_runtime_providers::Method::ClaudeCommands,
                "account" => {
                    identity.binding =
                        ControlBinding::new("foreign".into(), "retained-connection".into(), 7)
                            .expect("binding")
                }
                "connection" => {
                    identity.binding =
                        ControlBinding::new("account".into(), "foreign".into(), 7).expect("binding")
                }
                "generation" => {
                    identity.binding =
                        ControlBinding::new("account".into(), "retained-connection".into(), 8)
                            .expect("binding")
                }
                "restart" => {
                    *self.binding.lock().expect("binding") =
                        ControlBinding::new("account".into(), "replacement".into(), 8)
                            .expect("binding")
                }
                _ => {}
            }
            let result = match identity.method {
                studio_runtime_providers::Method::ClaudeState => json!({"tasks":[],"turns":[]}),
                studio_runtime_providers::Method::Initialize
                | studio_runtime_providers::Method::Initialized
                | studio_runtime_providers::Method::Ack
                | studio_runtime_providers::Method::AccountLoginCancel
                | studio_runtime_providers::Method::AccountLoginStart
                | studio_runtime_providers::Method::AccountRateLimitResetCreditConsume
                | studio_runtime_providers::Method::ThreadLoadedList
                | studio_runtime_providers::Method::ConfigRead
                | studio_runtime_providers::Method::SkillsExtraRootsSet
                | studio_runtime_providers::Method::ModelList
                | studio_runtime_providers::Method::AccountRead
                | studio_runtime_providers::Method::AccountRateLimitsRead
                | studio_runtime_providers::Method::ThreadStart
                | studio_runtime_providers::Method::ThreadRealtimeStart
                | studio_runtime_providers::Method::ThreadRealtimeStop
                | studio_runtime_providers::Method::ThreadRealtimeAppendSpeech
                | studio_runtime_providers::Method::ThreadRealtimeItemStarted
                | studio_runtime_providers::Method::ThreadRealtimeTranscriptDelta
                | studio_runtime_providers::Method::ThreadRealtimeTranscriptDone
                | studio_runtime_providers::Method::ThreadRealtimeSdp
                | studio_runtime_providers::Method::ThreadRealtimeError
                | studio_runtime_providers::Method::ThreadRealtimeClosed
                | studio_runtime_providers::Method::ThreadResume
                | studio_runtime_providers::Method::ThreadRead
                | studio_runtime_providers::Method::ThreadTurnsList
                | studio_runtime_providers::Method::TurnStart
                | studio_runtime_providers::Method::TurnSteer
                | studio_runtime_providers::Method::TurnInterrupt
                | studio_runtime_providers::Method::ThreadList
                | studio_runtime_providers::Method::ThreadArchive
                | studio_runtime_providers::Method::ThreadFork
                | studio_runtime_providers::Method::ThreadNameSet
                | studio_runtime_providers::Method::ThreadMetadataUpdate
                | studio_runtime_providers::Method::ThreadSettingsUpdate
                | studio_runtime_providers::Method::ThreadCompactStart
                | studio_runtime_providers::Method::ThreadRollback
                | studio_runtime_providers::Method::ThreadItemsList
                | studio_runtime_providers::Method::ThreadGoalSet
                | studio_runtime_providers::Method::ThreadGoalUpdated
                | studio_runtime_providers::Method::ThreadUnarchive
                | studio_runtime_providers::Method::ThreadUnsubscribe
                | studio_runtime_providers::Method::ProcessSpawn
                | studio_runtime_providers::Method::ProcessWriteStdin
                | studio_runtime_providers::Method::ProcessKill
                | studio_runtime_providers::Method::ProcessResizePty
                | studio_runtime_providers::Method::CommandExec
                | studio_runtime_providers::Method::CommandExecWrite
                | studio_runtime_providers::Method::CommandExecResize
                | studio_runtime_providers::Method::CommandExecTerminate
                | studio_runtime_providers::Method::ThreadBackgroundTerminalsList
                | studio_runtime_providers::Method::ThreadBackgroundTerminalsTerminate
                | studio_runtime_providers::Method::ThreadBackgroundTerminalsClean
                | studio_runtime_providers::Method::ThreadCommandExecutionBackground
                | studio_runtime_providers::Method::ThreadQueueList
                | studio_runtime_providers::Method::ClaudeRepairReport
                | studio_runtime_providers::Method::ClaudeFileBackups
                | studio_runtime_providers::Method::ClaudeSettings
                | studio_runtime_providers::Method::ClaudeActivity
                | studio_runtime_providers::Method::ClaudeStopTask
                | studio_runtime_providers::Method::ClaudeShellPolicy
                | studio_runtime_providers::Method::ClaudeDiagnostics
                | studio_runtime_providers::Method::ClaudeMoveVersions
                | studio_runtime_providers::Method::McpServerReconnect
                | studio_runtime_providers::Method::McpServerStatusList
                | studio_runtime_providers::Method::SkillsList
                | studio_runtime_providers::Method::OperationStatus
                | studio_runtime_providers::Method::ReviewStart
                | studio_runtime_providers::Method::BrowserAnswer
                | studio_runtime_providers::Method::BrowserCallback
                | studio_runtime_providers::Method::BrowserClose
                | studio_runtime_providers::Method::BrowserWait
                | studio_runtime_providers::Method::ClaudeCommands
                | studio_runtime_providers::Method::ClaudeMoveExport
                | studio_runtime_providers::Method::ClaudeMoveIdentity
                | studio_runtime_providers::Method::ClaudeMovePreflight
                | studio_runtime_providers::Method::ClaudeMoveProof
                | studio_runtime_providers::Method::Detach
                | studio_runtime_providers::Method::EventRead
                | studio_runtime_providers::Method::Exec
                | studio_runtime_providers::Method::Generate
                | studio_runtime_providers::Method::Health
                | studio_runtime_providers::Method::Info
                | studio_runtime_providers::Method::LayrHealth
                | studio_runtime_providers::Method::Next
                | studio_runtime_providers::Method::Open
                | studio_runtime_providers::Method::ProjectEnsure
                | studio_runtime_providers::Method::ProjectExec
                | studio_runtime_providers::Method::ProjectImport
                | studio_runtime_providers::Method::ProviderList
                | studio_runtime_providers::Method::Replay
                | studio_runtime_providers::Method::ResponseStatus
                | studio_runtime_providers::Method::RetireCommand
                | studio_runtime_providers::Method::SecretsList
                | studio_runtime_providers::Method::SecretsRemove
                | studio_runtime_providers::Method::SecretsSet
                | studio_runtime_providers::Method::ShareConfigure
                | studio_runtime_providers::Method::ShareName
                | studio_runtime_providers::Method::ShareStatus
                | studio_runtime_providers::Method::Sign
                | studio_runtime_providers::Method::Status
                | studio_runtime_providers::Method::SyncMacApply
                | studio_runtime_providers::Method::SyncMacHandoff
                | studio_runtime_providers::Method::SyncMacHashes
                | studio_runtime_providers::Method::SyncMacHead
                | studio_runtime_providers::Method::SyncMacRead
                | studio_runtime_providers::Method::SyncMacWorktree
                | studio_runtime_providers::Method::VaultConfigure
                | studio_runtime_providers::Method::VaultPush
                | studio_runtime_providers::Method::VaultStatus
                | studio_runtime_providers::Method::UploadBegin
                | studio_runtime_providers::Method::UploadChunk
                | studio_runtime_providers::Method::UploadCommit
                | studio_runtime_providers::Method::Verify
                | studio_runtime_providers::Method::Write
                | studio_runtime_providers::Method::CurrentTimeRead
                | studio_runtime_providers::Method::ItemToolCall
                | studio_runtime_providers::Method::ItemCommandExecutionRequestApproval
                | studio_runtime_providers::Method::ItemFileChangeRequestApproval
                | studio_runtime_providers::Method::ItemPermissionsRequestApproval
                | studio_runtime_providers::Method::McpServerElicitationRequest
                | studio_runtime_providers::Method::ItemToolRequestUserInput
                | studio_runtime_providers::Method::ItemStarted
                | studio_runtime_providers::Method::ItemCompleted
                | studio_runtime_providers::Method::ItemAgentMessageDelta
                | studio_runtime_providers::Method::ItemReasoningTextDelta
                | studio_runtime_providers::Method::ItemCommandExecutionOutputDelta
                | studio_runtime_providers::Method::CommandExecOutputDelta
                | studio_runtime_providers::Method::TurnStarted
                | studio_runtime_providers::Method::TurnCompleted
                | studio_runtime_providers::Method::TurnPlanUpdated
                | studio_runtime_providers::Method::TurnDiffUpdated
                | studio_runtime_providers::Method::ThreadClosed
                | studio_runtime_providers::Method::ThreadStatusChanged
                | studio_runtime_providers::Method::ThreadTokenUsageUpdated
                | studio_runtime_providers::Method::ThreadShellPolicyUpdated
                | studio_runtime_providers::Method::ThreadIdleCompactionUpdated
                | studio_runtime_providers::Method::AccountLoginCompleted
                | studio_runtime_providers::Method::AccountUpdated
                | studio_runtime_providers::Method::AccountRateLimitsUpdated
                | studio_runtime_providers::Method::ModelSafetyBufferingUpdated
                | studio_runtime_providers::Method::ProviderGenerationStarted
                | studio_runtime_providers::Method::ProtocolError => json!({"ok":true}),
            };
            Ok(ClaudeControlReceipt { identity, result })
        })
    }
}
fn port(fault: &'static str) -> Arc<AccountPort> {
    Arc::new(AccountPort {
        binding: Mutex::new(binding()),
        calls: Mutex::new(vec![]),
        fault,
    })
}
fn scope() -> ControlScope {
    ControlScope {
        agent: "agent".into(),
        epoch: 2,
        request_id: Some("logical-request".into()),
    }
}
#[tokio::test(start_paused = true)]
async fn retained_controls_bind_every_receipt_field_and_never_retry_a_lost_reply() {
    for fault in [
        "none",
        "lost",
        "agent",
        "epoch",
        "request",
        "payload",
        "method",
        "account",
        "connection",
        "generation",
        "restart",
    ] {
        let port = port(fault);
        let session = RetainedClaudeSession::new(port.clone());
        let result = session
            .call_scoped(
                scope(),
                "claude/settings",
                json!({"threadId":"thread","settings":{}}),
            )
            .await;
        if fault == "none" {
            assert_eq!(result.expect("receipt"), json!({"ok":true}));
        } else {
            assert!(matches!(result, Err(ControlError::Unknown)), "{fault}");
        }
        assert_eq!(port.calls.lock().expect("calls").len(), 1, "{fault}");
    }
}
#[tokio::test(start_paused = true)]
async fn replaced_generation_and_unscoped_or_unrecognized_calls_never_reach_the_owner() {
    let port = port("none");
    let session = RetainedClaudeSession::new(port.clone());
    assert!(session.call("claude/state", json!({})).await.is_err());
    assert!(
        session
            .call_scoped(scope(), "turn/start", json!({}))
            .await
            .is_err()
    );
    assert!(
        session
            .call_scoped(scope(), "claude/settings", Value::Null)
            .await
            .is_err()
    );
    *port.binding.lock().expect("binding") =
        ControlBinding::new("account".into(), "replacement".into(), 8).expect("binding");
    assert!(matches!(
        session
            .call_scoped(scope(), "claude/state", json!({}))
            .await,
        Err(ControlError::Changed)
    ));
    assert!(port.calls.lock().expect("calls").is_empty());
}
struct NoWrites;
impl ControlStorage for NoWrites {
    fn reserve(
        &self,
        _: studio_runtime_services::claude_controls::ControlRequest,
        _: ControlBinding,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        panic!("read must not write")
    }
    fn submitted(
        &self,
        _: studio_runtime_services::claude_controls::ControlSnapshot,
        _: studio_runtime_services::claude_controls::ControlRequest,
        _: Value,
        _: Value,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        panic!("read must not write")
    }
    fn provider(
        &self,
        _: studio_runtime_services::claude_controls::ControlSnapshot,
        _: Value,
    ) -> ControlFuture<'_, studio_runtime_services::claude_controls::ControlSnapshot> {
        panic!("read must not write")
    }
    fn complete(
        &self,
        _: studio_runtime_services::claude_controls::ControlSnapshot,
        _: studio_runtime_services::claude_controls::ControlRequest,
    ) -> ControlFuture<'_, Value> {
        panic!("read must not write")
    }
    fn failed(
        &self,
        _: studio_runtime_services::claude_controls::ControlSnapshot,
        _: FailureEvidence,
    ) -> ControlFuture<'_, ()> {
        panic!("read must not write")
    }
}
#[tokio::test(start_paused = true)]
async fn controls_caller_supplies_scope_and_bounds_the_retained_request_deadline() {
    let agent = json!({"id":"agent","rootId":"agent","provider":"claude","epoch":2,"accountKey":"account","cwd":"/fixture","status":"paused","threadId":"thread"});
    for fault in ["none", "deadline"] {
        let port = port(fault);
        let controls = Controls::new(
            Arc::new(NoWrites),
            Arc::new(RetainedClaudeSession::new(port.clone())),
        );
        let result = controls
            .read_or_stop(
                &agent,
                &json!({"action":"stop_task","task_id":"task"}),
                None,
            )
            .await;
        assert_eq!(result.is_ok(), fault == "none");
        let calls = port.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].identity.scope.agent, "agent");
        assert_eq!(calls[0].identity.scope.epoch, 2);
        assert_eq!(
            calls[0].identity.scope.request_id.as_deref(),
            // SHA-256 of the Python JSON text `["thread", "task"]`.
            Some("stop-task:e26e670c92bf185ae6ad6f81b8eb96770944cd906f864ebfd7a13e65f1377e00")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn opaque_stop_ids_have_distinct_stable_request_identities() {
    let port = port("none");
    let controls = Controls::new(
        Arc::new(NoWrites),
        Arc::new(RetainedClaudeSession::new(port.clone())),
    );
    for (thread, task, request) in [
        ("a:b", "c", None),
        ("a", "b:c", None),
        ("a:b", "c", None),
        ("a:b", "c", Some("caller-request")),
    ] {
        let agent = json!({"id":"agent","rootId":"agent","provider":"claude","epoch":2,"accountKey":"account","cwd":"/fixture","status":"paused","threadId":thread});
        let mut body = json!({"action":"stop_task","task_id":task});
        if let Some(request) = request {
            body["request_id"] = json!(request);
        }
        controls
            .read_or_stop(&agent, &body, None)
            .await
            .expect("stop");
    }
    let calls = port.calls.lock().expect("calls");
    assert_eq!(calls.len(), 4);
    assert_ne!(
        calls[0].identity.scope.request_id, calls[1].identity.scope.request_id,
        "colon concatenation collision witness"
    );
    assert_eq!(
        calls[0].identity.scope.request_id, calls[2].identity.scope.request_id,
        "same tuple replays the same identity"
    );
    assert_eq!(
        calls[3].identity.scope.request_id.as_deref(),
        Some("caller-request")
    );
    assert_eq!(calls[0].params, json!({"threadId":"a:b","taskId":"c"}));
    assert_eq!(calls[1].params, json!({"threadId":"a","taskId":"b:c"}));
}
