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
                Method::Initialize
                | Method::Initialized
                | Method::Ack
                | Method::AccountLoginCancel
                | Method::AccountLoginStart
                | Method::AccountRateLimitResetCreditConsume
                | Method::ThreadLoadedList
                | Method::ConfigRead
                | Method::SkillsExtraRootsSet
                | Method::ModelList
                | Method::AccountRead
                | Method::AccountRateLimitsRead
                | Method::ThreadStart
                | Method::ThreadRealtimeStart
                | Method::ThreadRealtimeStop
                | Method::ThreadRealtimeAppendSpeech
                | Method::ThreadRealtimeItemStarted
                | Method::ThreadRealtimeTranscriptDelta
                | Method::ThreadRealtimeTranscriptDone
                | Method::ThreadRealtimeSdp
                | Method::ThreadRealtimeError
                | Method::ThreadRealtimeClosed
                | Method::ThreadResume
                | Method::ThreadRead
                | Method::ThreadTurnsList
                | Method::TurnStart
                | Method::TurnSteer
                | Method::TurnInterrupt
                | Method::ThreadList
                | Method::ThreadArchive
                | Method::ThreadFork
                | Method::ThreadNameSet
                | Method::ThreadMetadataUpdate
                | Method::ThreadSettingsUpdate
                | Method::ThreadCompactStart
                | Method::ThreadItemsList
                | Method::ThreadGoalSet
                | Method::ThreadGoalUpdated
                | Method::ThreadUnarchive
                | Method::ThreadUnsubscribe
                | Method::ProcessSpawn
                | Method::ProcessWriteStdin
                | Method::ProcessKill
                | Method::ProcessResizePty
                | Method::CommandExec
                | Method::CommandExecWrite
                | Method::CommandExecResize
                | Method::CommandExecTerminate
                | Method::ThreadBackgroundTerminalsList
                | Method::ThreadBackgroundTerminalsTerminate
                | Method::ThreadBackgroundTerminalsClean
                | Method::ThreadCommandExecutionBackground
                | Method::ClaudeRepairReport
                | Method::ClaudeFileBackups
                | Method::ClaudeActivity
                | Method::ClaudeStopTask
                | Method::ClaudeDiagnostics
                | Method::ClaudeMoveVersions
                | Method::McpServerReconnect
                | Method::McpServerStatusList
                | Method::SkillsList
                | Method::OperationStatus
                | Method::ReviewStart
                | Method::BrowserAnswer
                | Method::BrowserCallback
                | Method::BrowserClose
                | Method::BrowserWait
                | Method::ClaudeCommands
                | Method::ClaudeMoveExport
                | Method::ClaudeMoveIdentity
                | Method::ClaudeMovePreflight
                | Method::ClaudeMoveProof
                | Method::Detach
                | Method::EventRead
                | Method::Exec
                | Method::Generate
                | Method::Health
                | Method::Info
                | Method::LayrHealth
                | Method::Next
                | Method::Open
                | Method::ProjectEnsure
                | Method::ProjectExec
                | Method::ProjectImport
                | Method::ProviderList
                | Method::Replay
                | Method::ResponseStatus
                | Method::RetireCommand
                | Method::SecretsList
                | Method::SecretsRemove
                | Method::SecretsSet
                | Method::ShareConfigure
                | Method::ShareName
                | Method::ShareStatus
                | Method::Sign
                | Method::Status
                | Method::SyncMacApply
                | Method::SyncMacHandoff
                | Method::SyncMacHashes
                | Method::SyncMacHead
                | Method::SyncMacRead
                | Method::SyncMacWorktree
                | Method::VaultConfigure
                | Method::VaultPush
                | Method::VaultStatus
                | Method::UploadBegin
                | Method::UploadChunk
                | Method::UploadCommit
                | Method::Verify
                | Method::Write
                | Method::CurrentTimeRead
                | Method::ItemToolCall
                | Method::ItemCommandExecutionRequestApproval
                | Method::ItemFileChangeRequestApproval
                | Method::ItemPermissionsRequestApproval
                | Method::McpServerElicitationRequest
                | Method::ItemToolRequestUserInput
                | Method::ItemStarted
                | Method::ItemCompleted
                | Method::ItemAgentMessageDelta
                | Method::ItemReasoningTextDelta
                | Method::ItemCommandExecutionOutputDelta
                | Method::CommandExecOutputDelta
                | Method::TurnStarted
                | Method::TurnCompleted
                | Method::TurnPlanUpdated
                | Method::TurnDiffUpdated
                | Method::ThreadClosed
                | Method::ThreadStatusChanged
                | Method::ThreadTokenUsageUpdated
                | Method::ThreadIdleCompactionUpdated
                | Method::AccountLoginCompleted
                | Method::AccountUpdated
                | Method::AccountRateLimitsUpdated
                | Method::ModelSafetyBufferingUpdated
                | Method::ProviderGenerationStarted
                | Method::ProtocolError => panic!("unexpected control"),
            };
            Ok(ClaudeControlReceipt {
                identity: call.identity,
                result,
            })
        })
    }
}
#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn restart_at_each_control_transport_boundary_never_resends_an_uncertain_mutation() {
    let root = tempfile::tempdir().expect("scratch");
    let template = root.path().join("template");
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&template).expect("template");
    std::fs::create_dir(&cwd).expect("cwd");
    let captured =
        std::str::from_utf8(include_bytes!("wave7_p08_controls.json")).expect("fixture UTF8");
    let captured = captured.replace("/fixture", &cwd.to_string_lossy());
    crate::turn_fixtures::write_state(captured.as_bytes(), &template);
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
                let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
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
                    let body = if action == "rollback" {
                        json!({"id":"agent","action":action,"request_id":"control-one","turn_id":"two"})
                    } else {
                        json!({"id":"agent","action":action,"request_id":"control-one","settings":{"permissionMode":"plan"}})
                    };
                    serde_json::from_value::<ControlRequest>(body).expect("request")
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
                let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("restart"));
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
                        if call.identity.method == mutation {
                            Some("control-one")
                        } else {
                            // Idle reads use fresh identities; only the mutation
                            // retains the caller's durable receipt identity.
                            None
                        }
                    );
                }
                let agent = AgentRepository::get(
                    &store.test_reader().expect("read"),
                    &AgentId("agent".into()),
                )
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
