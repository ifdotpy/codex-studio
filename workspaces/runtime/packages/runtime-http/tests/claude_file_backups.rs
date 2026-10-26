//! HTTP, retained account adapter and durable restore settlement in one caller test.
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use studio_runtime_http::{HttpState, router};
use studio_runtime_process::{
    EventCursor, Generation, OperationId, ProviderHandle, ProviderLaunchSpec, provider_seam::*,
};
use studio_runtime_services::{
    account_policy::AccountPolicyState,
    account_session_runtime::*,
    account_worker::{
        AccountHandle, StoreBridge, preparation::PreparationResources, start_account_worker,
    },
    claude_controls::service::ControlSession,
    provider_session_task::SessionTaskConfig,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};
use tokio::sync::{Notify, mpsc, watch};
use tower::ServiceExt;

#[derive(Default)]
pub(crate) struct Native {
    full: AtomicBool,
    pub(crate) calls: Mutex<Vec<Value>>,
    pub(crate) repair_report: Mutex<Option<Value>>,
    ready: Notify,
}
impl Native {
    fn response(&self, request: &Value) -> Value {
        self.calls.lock().unwrap().push(request.clone());
        let p = &request["params"];
        let result = match request["method"].as_str().unwrap() {
            "claude/repairReport" => {
                self.repair_report.lock().unwrap().clone().unwrap_or_else(|| json!({"status":"needs_repair","skippedRows":3,"invalidSequence":true,"scopeConfirmed":true,"nativeHistoryConfirmed":false,"secret":"must-not-escape","transcript":"private transcript content","path":"/private/account"}))
            }
            "claude/state" => {
                json!({"tasks":[],"turns":[{"id":"one","status":"completed","canRestoreFiles":true},{"id":"two","status":"completed","canRestoreFiles":true}]})
            }
            "thread/queue/list" => json!({"data":[]}),
            "thread/rollback" => {
                let id = p["requestId"].as_str().unwrap();
                if self.full.load(Ordering::SeqCst) || ["partial", "wrong-id"].contains(&id) {
                    let mut error = json!({"code":-32000,"message":"The before-restore backup limit is reached."});
                    if id != "partial" {
                        error["data"] = json!({"fileRestoreOutcome":"not_applied","requestId":if id == "wrong-id" {"foreign"} else {id},"turnId":p["turnId"]});
                    } else {
                        error["message"] = json!("Claude restored only one file.");
                    }
                    return json!({"id":request["id"],"error":error});
                }
                json!({"threadId":"thread","nativeId":"rewound","filesRestored":true})
            }
            "claude/fileBackups" => {
                let release = p["settledRequestIds"]
                    .as_array()
                    .is_some_and(|ids| !ids.is_empty());
                if release {
                    self.full.store(false, Ordering::SeqCst);
                }
                json!({"count":if self.full.load(Ordering::SeqCst) {100} else {1},"bytes":500,"maxCount":100,"maxBytes":1073741824,"released":if release {1} else {0}})
            }
            method => panic!("unexpected provider method {method}"),
        };
        json!({"id":request["id"],"result":result})
    }
}
struct Factory(Arc<Native>);
impl ProviderTaskFactory for Factory {
    fn open(&self, handle: ProviderHandle, options: OpenOptions, stop: Shutdown) -> OpenFuture<'_> {
        let native = self.0.clone();
        Box::pin(async move {
            let generation = options.expected_generation.unwrap();
            let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(16);
            let (entries, journal) = mpsc::channel(16);
            let (status, observed) = watch::channel(AttachmentStatus {
                handle: handle.clone(),
                open_attempted: true,
                info: Some(AttachmentInfo {
                    handle,
                    generation,
                    pid: 1,
                    acknowledged: EventCursor(0),
                    resumed: false,
                }),
                initialization: InitializationStatus::AwaitingReply,
            });
            let mut stopped = stop.subscribe();
            let task = tokio::spawn(async move {
                let _status = status;
                let mut cursor = 0;
                loop {
                    let command = tokio::select! {biased; _=Shutdown::cancelled(&mut stopped)=>break, command=inbox.recv()=>match command {Some(c)=>c,None=>break}};
                    match command {
                        AdapterCommand::Write {
                            write,
                            reply,
                            observed,
                        } => {
                            let method = write.message()["method"].as_str().unwrap_or("");
                            if method == "initialize" {
                                drop(reply.send(CommandReceipt::CachedInitialize(json!({}))));
                            } else {
                                drop(reply.send(CommandReceipt::Written {
                                    remote_id: None,
                                    duplicate: false,
                                }));
                                if method == "initialized" {
                                    native.ready.notify_one();
                                } else {
                                    let response = native.response(write.message());
                                    cursor += 1;
                                    entries
                                        .send(JournalEntry {
                                            received_at_ms: options.clock.now_ms(),
                                            cursor: EventCursor(cursor),
                                            generation,
                                            payload: JournalPayload::Frame(
                                                match studio_runtime_providers::decode_journal_entry_borrowed(1, 1, format!("{response}\n").as_bytes()).payload { studio_runtime_providers::DecodedJournalPayload::Frame(frame) => frame, other => panic!("fixture must decode: {other:?}"), },
                                            ),
                                        })
                                        .await
                                        .unwrap();
                                }
                            }
                            let _observed = observed.await;
                        }
                        AdapterCommand::Ack { cursor, reply } => {
                            drop(reply.send(CommandReceipt::Acknowledged(cursor)));
                        }
                    }
                }
                TaskExit {
                    result: Ok(()),
                    cleanup_error: None,
                    recovery_needed: None,
                    provider_retained: false,
                }
            });
            Ok(ProviderAttachment::from_task(
                commands, journal, observed, stop, task,
            ))
        })
    }
}
struct Broker(Arc<dyn ControlSession>);
impl AccountSessionPort for Broker {
    fn claude_control_session(
        &self,
        _: &str,
    ) -> Result<Arc<dyn ControlSession>, AccountSessionError> {
        Ok(self.0.clone())
    }
}
async fn post(app: &axum::Router, path: &str, body: Value) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("host", "127.0.0.1:49171")
                .header("content-type", "application/json")
                .header("x-canvas-token", "fixture-token")
                .header(
                    studio_runtime_http::API_SCHEMA_HASH_HEADER,
                    studio_runtime_http::schema::api_schema_hash(),
                )
                .extension(ConnectInfo(
                    "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap(),
    )
}
pub(crate) async fn fixture() -> (
    tempfile::TempDir,
    Arc<RuntimeStore>,
    axum::Router,
    AccountHandle,
    Arc<Native>,
) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    let store = Arc::new(RuntimeStore::open(Some(&state_dir)).await.unwrap());
    let agent = json!({"id":"agent","rootId":"agent","epoch":2,"cwd":cwd,"provider":"claude","accountKey":"account","threadId":"thread","status":"paused","autoWake":false});
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('agent',?1)",
                [agent.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .transaction(|tx| {
            studio_runtime_store::OrchestrationSettingsRepository::set(
                tx,
                studio_runtime_store::IDLE_SESSION_UNLOAD_SETTING,
                0,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let native = Arc::new(Native::default());
    let clock = SessionClock::default();
    let worker = start_account_worker(
        StoreBridge::new(store.clone()).await.unwrap(),
        "account".into(),
        Arc::new(Factory(native.clone())),
        OpenOptions {
            clock,
            launch: ProviderLaunchSpec::new(vec!["fixture".into()], BTreeMap::new(), cwd),
            expected_generation: Some(Generation(1)),
        },
        SessionTaskConfig {
            initialize_params: json!({}),
            generation: Generation(1),
            wire: studio_runtime_providers::ProviderKind::Claude.wire(),
            timeout_ms: 20000,
            initialize_operation: OperationId("init".into()),
            request_id_prefix: None,
            recovered: vec![],
        },
        PreparationResources::bundled(
            String::new(),
            studio_runtime_services::model_catalog::ModelCatalogService::new(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    native.ready.notified().await;
    let policy=AccountPolicyState::from_startup(Some(br#"{"version":1,"defaultAccountKey":"account","accounts":{"account":{"id":"account","provider":"claude","home":"/fixture","label":"Fixture","source":"Claude Code","status":"ready"}},"logins":{}}"#),[]).unwrap();
    let mut state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        root.path().display().to_string(),
        49171,
        None,
        Some(store.clone()),
        Some(Arc::new(policy)),
    );
    state.account_session = Some(Arc::new(Broker(Arc::new(worker.claude_session().unwrap()))));
    (root, store, router(state).unwrap(), worker, native)
}
fn rewind(id: &str) -> Value {
    json!({"id":"agent","action":"rollback","turn_id":"two","request_id":id,"restore_files":true})
}
fn agent(store: &RuntimeStore) -> Value {
    AgentRepository::get(&store.test_reader().unwrap(), &AgentId("agent".into()))
        .unwrap()
        .unwrap()
        .value()
}

#[tokio::test]
async fn quota_reason_crosses_retained_adapter_and_http_release_allows_same_request_retry() {
    let (_root, store, app, mut worker, native) = fixture().await;
    let completed = post(&app, "/api/claude/session", rewind("completed")).await;
    assert_eq!(
        completed.0,
        200,
        "{completed:?}; agent={:?}; calls={:?}",
        agent(&store),
        native.calls.lock().unwrap()
    );
    // An unresolved restore in another workspace must never appear in this chat's release proof.
    let other_cwd = _root.path().join("other");
    std::fs::create_dir(&other_cwd).unwrap();
    store.transaction(move |tx| {
        tx.execute("INSERT INTO runtime_agents(id,record) SELECT 'other',json_set(record,'$.id','other','$.rootId','other','$.cwd',?1,'$.threadId','other-thread') FROM runtime_agents WHERE id='agent'", [other_cwd.to_string_lossy().as_ref()])?;
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) SELECT 'claude-control:unresolved',json_set(record,'$.id','claude-control:unresolved','$.agent','other','$.requestId','unresolved','$.phase','recovery_required','$.source.threadId','other-thread','$.source.cwd',?1) FROM runtime_workspace_operations WHERE id='claude-control:completed'",[other_cwd.to_string_lossy().as_ref()])?;
        Ok(())
    }).await.unwrap();
    native.full.store(true, Ordering::SeqCst);
    let rejected = post(&app, "/api/claude/session", rewind("quota")).await;
    assert_eq!(rejected.0, 400, "{rejected:?}");
    assert!(
        rejected.1.to_string().contains("backup limit"),
        "{rejected:?}; agent={:?}; calls={:?}",
        agent(&store),
        native.calls.lock().unwrap()
    );
    assert!(agent(&store)["workspaceReservationId"].is_null());
    assert!(agent(&store)["workspaceOperation"].is_null());
    let usage = post(
        &app,
        "/api/claude/file-backups",
        json!({"id":"agent","release":false,"request_id":"usage"}),
    )
    .await;
    assert_eq!(usage.0, 200, "{usage:?}");
    assert_eq!(usage.1["count"], 100);
    let release = post(
        &app,
        "/api/claude/file-backups",
        json!({"id":"agent","release":true,"request_id":"release"}),
    )
    .await;
    assert_eq!(release.0, 200, "{release:?}");
    assert_eq!(release.1["released"], 1);
    let calls = native.calls.lock().unwrap().clone();
    let proof = calls
        .iter()
        .find(|r| {
            r["method"] == "claude/fileBackups" && r["params"]["settledRequestIds"].is_array()
        })
        .unwrap();
    assert_eq!(proof["params"]["settledRequestIds"], json!(["completed"]));
    let retry = post(&app, "/api/claude/session", rewind("quota")).await;
    assert_eq!(retry.0, 200, "{retry:?}");
    assert_eq!(retry.1["filesRestored"], true);
    worker.shutdown().await.unwrap();
}
#[tokio::test]
async fn generic_and_wrong_identity_rejections_keep_recovery_reservations_through_http() {
    for id in ["partial", "wrong-id"] {
        let (_root, store, app, mut worker, _native) = fixture().await;
        let result = post(&app, "/api/claude/session", rewind(id)).await;
        assert_eq!(result.0, 409, "{result:?}");
        assert!(
            result.1.to_string().contains(if id == "partial" {
                "only one file"
            } else {
                "backup limit"
            }),
            "{result:?}; agent={:?}; calls={:?}",
            agent(&store),
            _native.calls.lock().unwrap()
        );
        assert_eq!(agent(&store)["workspaceOperation"], "branch_recovery");
        assert!(!agent(&store)["workspaceReservationId"].is_null());
        worker.shutdown().await.unwrap();
    }
}
