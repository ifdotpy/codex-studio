//! Project deletion is exercised through its authenticated HTTP caller.
use crate::monitor_runtime_fixtures as fixtures;
use axum::{
    Extension,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::{
    agent_lifecycle_runtime::{LifecycleCommitFuture, LifecycleEffect, LifecyclePorts},
    agent_management::ManagementError,
};
use studio_runtime_store::{
    AgentRecord, AgentRepository, ProjectsRepository, RuntimeStore, SyncDocumentsRepository,
    Transaction,
};
use tower::ServiceExt;

struct Ports {
    store: Arc<RuntimeStore>,
    wakes: AtomicUsize,
    fail: bool,
}
impl LifecyclePorts for Ports {
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        assert_eq!(
            self.wakes.load(Ordering::SeqCst),
            0,
            "no precommit dispatch"
        );
        if self.fail {
            return Err(ManagementError::Unavailable(effect.kind().dependency()));
        }
        tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'effect','{}') ON CONFLICT DO NOTHING", [format!("effect:{:?}:{}",effect.kind(),effect.before()["id"])])
            .map_err(studio_runtime_store::StoreError::from)?;
        Ok(())
    }
    fn committed(&self) -> LifecycleCommitFuture<'_> {
        Box::pin(async {
            let reader = self.store.test_reader()?;
            assert!(
                ProjectsRepository::get(&reader, "/p")?.is_none(),
                "effects follow project commit"
            );
            self.wakes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    store: Arc<RuntimeStore>,
    app: axum::Router,
    body: Value,
    ports: Arc<Ports>,
}
impl Fixture {
    async fn new(agents: Vec<Value>, rooms: Vec<Value>, fail: bool) -> Self {
        let root = fixtures::tempdir();
        let _ = fixtures::corpus(root.path());
        let store = Arc::new(fixtures::store(root.path(), "project-deletion").await);
        store
            .set_local_server_id(Some("owner".into()))
            .expect("identity");
        studio_runtime_services::entity_publication::install(&store)
            .await
            .expect("publisher");
        store.transaction(move |tx| {
            for path in ["/p","/prefix"] {
                ProjectsRepository::put_project(tx,&json!({"id":path,"path":path,"name":path,"created":1,"homeServerId":"owner","locations":[{"serverId":"owner","path":path,"projectId":path}]}))?;
            }
            for agent in agents { AgentRepository::put(tx,&AgentRecord::from_value(agent)?)?; }
            for room in rooms {
                tx.execute("INSERT INTO runtime_rooms(id,record) VALUES(?1,?2)",rusqlite::params![room["id"].as_str(),room.to_string()])?;
                SyncDocumentsRepository::put_entity(tx,"room",room["id"].as_str().expect("id"),room.clone(),false)?;
            }
            Ok(())
        }).await.expect("seed");
        let reader = store.test_reader().expect("reader");
        let seq: i64 = reader
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='project' AND id='/p'",
                [],
                |r| r.get(0),
            )
            .expect("seq");
        let workspace = SyncDocumentsRepository::workspace_id(&reader).expect("workspace");
        let body = json!({"id":"/p","request_id":"delete-1","expected_workspace_id":workspace,"expected_project_seq":seq});
        let ports = Arc::new(Ports {
            store: store.clone(),
            wakes: AtomicUsize::new(0),
            fail,
        });
        let state = HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            store.root().to_string_lossy().into(),
            49171,
            None,
            Some(store.clone()),
            None,
        );
        let lifecycle: Arc<dyn LifecyclePorts> = ports.clone();
        let app = router(state).expect("router").layer(Extension(lifecycle));
        Self {
            _root: root,
            store,
            app,
            body,
            ports,
        }
    }
    async fn call(&self, body: Value) -> (u16, Value) {
        self.call_auth(body, true).await
    }
    async fn call_auth(&self, body: Value, auth: bool) -> (u16, Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/projects/delete")
            .header("host", "127.0.0.1:49171")
            .header("content-type", "application/json")
            .header(
                studio_runtime_http::API_SCHEMA_HASH_HEADER,
                studio_runtime_http::api_schema_hash(),
            )
            .extension(ConnectInfo(
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("address"),
            ));
        if auth {
            request = request.header("x-canvas-token", "fixture-token");
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).expect("request"))
            .await
            .expect("HTTP");
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response");
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
        )
    }
    fn agent(&self, id: &str) -> Value {
        ProjectsRepository::agent(&self.store.test_reader().expect("reader"), id)
            .expect("read")
            .expect("agent")
            .1
    }
}
fn agent(id: &str, parent: Option<&str>, path: &str) -> Value {
    json!({"id":id,"rootId":if parent.is_some(){"lead"}else{id},"parentId":parent,"epoch":1,"status":"idle","isLead":parent.is_none(),"autoWake":true,"cwd":path})
}

#[tokio::test]
async fn project_deletion_membership_and_retention() {
    let mut lead = agent("lead", None, "/p");
    lead["archivedAt"] = json!(2);
    let mut archived = agent("archived", Some("lead"), "/worktrees/a");
    archived["deletedAt"] = json!(2);
    archived["agentArchive"] = json!({"at":2,"epoch":1});
    let mut orphan = agent("orphan", Some("missing"), "/else");
    orphan["projectId"] = json!("/p");
    orphan["projectServerId"] = json!("owner");
    let f = Fixture::new(
        vec![
            lead,
            archived,
            agent("nested", Some("archived"), "/else"),
            orphan,
            agent("other", None, "/prefix"),
        ],
        vec![
            json!({"id":"shared","kind":"shared","projectPath":"/p","members":["lead"]}),
            json!({"id":"broadcast","kind":"broadcast","rootId":"lead"}),
            json!({"id":"global","kind":"broadcast","rootId":"all"}),
        ],
        false,
    )
    .await;
    let (status, result) = f.call(f.body.clone()).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(
        result["deletedAgentIds"],
        json!(["archived", "lead", "nested", "orphan"])
    );
    assert_eq!(result["deletedRoomIds"], json!(["broadcast", "shared"]));
    for id in ["lead", "archived", "nested", "orphan"] {
        let a = f.agent(id);
        assert_eq!(a["autoWake"], false);
        assert!(a["deletedAt"].is_number());
        assert!(
            a["agentArchive"].is_null(),
            "permanent deletion must refuse restore"
        );
    }
    assert!(f.agent("other")["deletedAt"].is_null());
    let reader = f.store.test_reader().expect("reader");
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM runtime_rooms", [], |r| r
                .get::<_, i64>(0))
            .expect("rooms"),
        3,
        "room history retained"
    );
    for id in ["shared", "broadcast"] {
        assert!(
            reader
                .query_row(
                    "SELECT deleted FROM sync_entities WHERE collection='room' AND id=?1",
                    [id],
                    |r| r.get::<_, bool>(0)
                )
                .expect("tombstone")
        );
    }
}

#[tokio::test]
async fn project_deletion_receipt_replay_changed_body_and_new_lifetime() {
    let f = Fixture::new(vec![agent("lead", None, "/p")], vec![], false).await;
    let first = f.call(f.body.clone()).await;
    assert_eq!(first.0, 200, "{}", first.1);
    let epoch = f.agent("lead")["epoch"].clone();
    f.store.transaction(|tx|ProjectsRepository::put_project(tx,&json!({"id":"/p","path":"/p","name":"Recreated","created":3,"homeServerId":"owner","locations":[{"serverId":"owner","path":"/p","projectId":"/p"}]}))).await.expect("re-register");
    assert_eq!(f.call(f.body.clone()).await, first);
    assert_eq!(f.agent("lead")["epoch"], epoch);
    assert_eq!(f.ports.wakes.load(Ordering::SeqCst), 1);
    let mut changed = f.body.clone();
    changed["id"] = json!("/prefix");
    let (status, r) = f.call(changed).await;
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["code"], "project_request_conflict");
    let mut stale = f.body.clone();
    stale["request_id"] = json!("delete-2");
    let (status, r) = f.call(stale).await;
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["code"], "project_sequence_conflict");
}

#[tokio::test]
async fn project_deletion_rolls_back_every_write_boundary() {
    for trigger in [
        "CREATE TRIGGER fail BEFORE UPDATE ON runtime_agents WHEN NEW.id='z' BEGIN SELECT RAISE(ABORT,'injected second agent'); END",
        "CREATE TRIGGER fail BEFORE UPDATE ON runtime_rooms BEGIN SELECT RAISE(ABORT,'injected room'); END",
        "CREATE TRIGGER fail BEFORE INSERT ON runtime_operation_receipts WHEN NEW.id LIKE 'project-delete:%' BEGIN SELECT RAISE(ABORT,'injected receipt'); END",
        "CREATE TRIGGER fail BEFORE UPDATE ON sync_entities WHEN NEW.collection='project' AND NEW.deleted=1 BEGIN SELECT RAISE(ABORT,'injected publication'); END",
    ] {
        let f = Fixture::new(
            vec![agent("a", None, "/p"), agent("z", None, "/p")],
            vec![json!({"id":"shared","kind":"shared","projectPath":"/p","members":["a"]})],
            false,
        )
        .await;
        f.store
            .transaction(move |tx| {
                tx.execute_batch(trigger)?;
                Ok(())
            })
            .await
            .expect("inject");
        let (status, result) = f.call(f.body.clone()).await;
        assert_eq!(status, 500, "{trigger}: {status} {result}");
        assert_eq!(f.agent("a")["epoch"], 1);
        assert_eq!(f.agent("z")["epoch"], 1);
        assert_eq!(f.ports.wakes.load(Ordering::SeqCst), 0);
        assert!(
            ProjectsRepository::get(&f.store.test_reader().expect("reader"), "/p")
                .expect("project")
                .is_some()
        );
        f.store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER fail")?;
                Ok(())
            })
            .await
            .expect("remove failure injection");
        let (status, retry) = f.call(f.body.clone()).await;
        assert_eq!(
            status, 200,
            "same intention after definite rollback: {retry}"
        );
        assert_eq!(f.agent("a")["epoch"], 2);
    }
}

#[tokio::test]
async fn project_deletion_remote_and_ambiguous_membership_fail_closed() {
    for patch in [
        json!({"locations":[{"serverId":"remote","path":"/p","projectId":"/p"}]}),
        json!({"projectAliases":[{"serverId":"remote","projectId":"/p"}]}),
        json!({"homeServerId":"remote"}),
    ] {
        let f = Fixture::new(vec![agent("lead", None, "/p")], vec![], false).await;
        f.store
            .transaction(move |tx| {
                let mut p = ProjectsRepository::get(tx, "/p")?.expect("project").1;
                for (k, v) in patch.as_object().expect("patch") {
                    p[k] = v.clone();
                }
                ProjectsRepository::put_project(tx, &p)
            })
            .await
            .expect("remote");
        let reader = f.store.test_reader().expect("reader");
        let seq: i64 = reader
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='project' AND id='/p'",
                [],
                |r| r.get(0),
            )
            .expect("seq");
        let mut body = f.body.clone();
        body["expected_project_seq"] = json!(seq);
        let (status, r) = f.call(body).await;
        assert_eq!(status, 409, "{r}");
        assert_eq!(r["code"], "project_remote_scope");
        assert_eq!(f.agent("lead")["epoch"], 1);
    }
    for patch in [
        json!({"projectId":"/prefix","projectServerId":"owner"}),
        json!({"parentId":"child"}),
        json!({"parentId":"child","projectId":"/p","projectServerId":"owner"}),
        json!({"remoteWorker":{"serverId":"remote"}}),
    ] {
        let mut child = agent("child", Some("lead"), "/worktree");
        for (k, v) in patch.as_object().expect("patch") {
            child[k] = v.clone();
        }
        let f = Fixture::new(vec![agent("lead", None, "/p"), child], vec![], false).await;
        let (status, r) = f.call(f.body.clone()).await;
        assert_eq!(status, 409, "{r}");
        assert_eq!(f.agent("lead")["epoch"], 1);
    }
}

#[tokio::test]
async fn project_deletion_empty_missing_auth_and_guards() {
    let f = Fixture::new(vec![], vec![], false).await;
    assert_eq!(f.call_auth(f.body.clone(), false).await.0, 403);
    let mut invalid = f.body.clone();
    invalid["agents"] = json!([]);
    assert_eq!(f.call(invalid).await.0, 400);
    let mut wrong = f.body.clone();
    wrong["expected_workspace_id"] = json!("foreign");
    let (status, r) = f.call(wrong).await;
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["code"], "project_workspace_conflict");
    let mut missing = f.body.clone();
    missing["id"] = json!("missing");
    assert_eq!(f.call(missing).await.0, 404);
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["deletedAgentIds"], json!([]));
    assert_eq!(r["deletedRoomIds"], json!([]));
}

#[tokio::test]
async fn project_deletion_stages_active_effects_or_rolls_back() {
    for fail in [true, false] {
        let mut running = agent("lead", None, "/p");
        running["status"] = json!("running");
        running["inFlight"] = json!(true);
        running["turnId"] = json!("turn");
        running["threadId"] = json!("thread");
        let f = Fixture::new(vec![running], vec![], fail).await;
        let (status, r) = f.call(f.body.clone()).await;
        if fail {
            assert_eq!(status, 503, "{r}");
            assert_eq!(f.agent("lead")["epoch"], 1);
            assert_eq!(f.ports.wakes.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(status, 200, "{r}");
            assert_eq!(f.ports.wakes.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn project_deletion_keeps_archive_cleanup_identity_and_drains_it() {
    use studio_runtime_services::workspace_cleanup_runtime::{
        self as cleanup, CleanupAction, CleanupEffect, CleanupPorts, EffectFailure,
    };
    struct NoIo;
    impl CleanupPorts for NoIo {
        fn perform(&self, _: &Value, _: &CleanupEffect, _: &str) -> Result<Value, EffectFailure> {
            panic!("absent worktree needs no IO")
        }
    }
    let mut archived = agent("archived", Some("lead"), "/absent-worktree");
    archived["deletedAt"] = json!(2);
    archived["agentArchive"] = json!({"at":2,"epoch":1,"cleanupPending":true});
    let intent = cleanup::queued_cleanup_intent("archived", 1, CleanupAction::Archive);
    archived["workspaceCleanupIntent"] = intent.clone();
    let f = Fixture::new(vec![agent("lead", None, "/p"), archived], vec![], false).await;
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 200, "{r}");
    let archived = f.agent("archived");
    assert!(archived["agentArchive"].is_null());
    assert_eq!(archived["workspaceCleanupIntent"], intent);
    cleanup::cleanup_agent_workspace_with_ports(
        f.store.clone(),
        "archived".into(),
        CleanupAction::Archive,
        Arc::new(NoIo),
    )
    .await
    .expect("prior archive cleanup can finish");
    let archived = f.agent("archived");
    assert!(archived["agentArchive"].is_null());
    assert!(archived["workspaceCleanupIntent"].is_null());
}

#[tokio::test]
async fn project_deletion_one_commit_and_room_rebuild_cannot_revive() {
    let f = Fixture::new(
        vec![agent("a", None, "/p"), agent("b", None, "/p")],
        vec![
            json!({"id":"shared","kind":"shared","projectPath":"/p","members":["a","b"]}),
            json!({"id":"broadcast","kind":"broadcast","rootId":"a"}),
        ],
        false,
    )
    .await;
    let generation = f.store.committed_change_generation();
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(
        f.store.committed_change_generation(),
        generation + 1,
        "one authoritative transaction"
    );
    // Rebuild through the same projector used for repair/startup, retaining the
    // room records and history. Both retained broadcast and shared rows stay dead.
    let (sources, projects) = f
        .store
        .read_async("project-deletion/rebuild", |tx| {
            Ok::<_, studio_runtime_store::StoreError>((
                studio_runtime_services::sync_entity_context::RoomSources::read_for_sync(tx)?,
                studio_runtime_services::sync_policy::ProjectVaultSnapshot::stored_projects(tx)?,
            ))
        })
        .await
        .expect("sources");
    let directory = studio_runtime_services::sync_entity_context::RoomDirectory::prepare(sources)
        .expect("directory");
    let vault = studio_runtime_services::sync_policy::ProjectVaultSnapshot::capture(
        f.store.root(),
        &projects,
    );
    let root = f.store.root().to_string_lossy().into_owned();
    f.store
        .transaction(move |tx| {
            studio_runtime_services::sync_policy::maintain_entity_projections(
                tx,
                &root,
                &vault,
                &directory,
                |_, _, _, _| true,
            )
        })
        .await
        .expect("restart rebuild");
    let reader = f.store.test_reader().expect("reader");
    for id in ["shared", "broadcast"] {
        let raw: String = reader
            .query_row("SELECT record FROM runtime_rooms WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .expect("retained room");
        let room: Value = serde_json::from_str(&raw).expect("room");
        assert!(
            studio_runtime_services::sync_views::room_entity_view(&room, &Default::default())
                .is_none()
        );
        assert!(
            reader
                .query_row(
                    "SELECT deleted FROM sync_entities WHERE collection='room' AND id=?1",
                    [id],
                    |r| r.get::<_, bool>(0)
                )
                .expect("tombstone")
        );
    }
}

#[tokio::test]
async fn project_deletion_prunes_sidebar_and_preserves_history() {
    let f = Fixture::new(
        vec![agent("lead", None, "/p"), agent("other", None, "/prefix")],
        vec![json!({"id":"shared","kind":"shared","projectPath":"/p","members":["lead"]})],
        false,
    )
    .await;
    let native = f._root.path().join("retained-native-history");
    tokio::fs::write(&native, "native history")
        .await
        .expect("native fixture");
    f.store.transaction(|tx|{
        ProjectsRepository::put_sidebar_order(tx,&json!({"revision":1,"groups":{"/p":["lead"],"/prefix":["other"],"pinned":["lead","other"]}}))?;
        tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES('retained-message','shared','lead','retained history',1,'{}')",[])?;
        Ok(())
    }).await.expect("history and sidebar");
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 200, "{r}");
    let reader = f.store.test_reader().expect("reader");
    let order = ProjectsRepository::sidebar_order(&reader).expect("sidebar");
    assert!(order["groups"].get("/p").is_none());
    assert_eq!(order["groups"]["/prefix"], json!(["other"]));
    assert_eq!(order["groups"]["pinned"], json!(["other"]));
    assert_eq!(
        reader
            .query_row(
                "SELECT text FROM runtime_chat_messages WHERE id='retained-message'",
                [],
                |r| r.get::<_, String>(0)
            )
            .expect("retained message"),
        "retained history"
    );
    assert_eq!(
        tokio::fs::read_to_string(native)
            .await
            .expect("retained native history"),
        "native history"
    );
}

#[tokio::test]
async fn project_deletion_unknown_interrupt_is_not_repeated() {
    use std::sync::Mutex;
    use studio_runtime_services::{
        account_session_runtime::{
            AccountSessionFuture, AccountSessionPort, ConnectionScope, ProviderRequest,
            RequestCertainty, SessionReceipt,
        },
        lifecycle_effects::ProductionLifecyclePorts,
    };
    struct UnknownBroker {
        store: Arc<RuntimeStore>,
        calls: Mutex<Vec<String>>,
    }
    impl AccountSessionPort for UnknownBroker {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            Some(ConnectionScope {
                account: account.into(),
                connection: "connection".into(),
                generation: 7,
            })
        }
        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'_, SessionReceipt> {
            Box::pin(async move {
                assert!(
                    ProjectsRepository::get(
                        &self.store.test_reader().expect("committed reader"),
                        "/p"
                    )
                    .expect("project read")
                    .is_none()
                );
                self.calls
                    .lock()
                    .expect("calls")
                    .push(request.operation.clone());
                Ok(SessionReceipt {
                    scope: ConnectionScope {
                        account: "account".into(),
                        connection: "connection".into(),
                        generation: 7,
                    },
                    operation: request.operation,
                    ticket: None,
                    certainty: RequestCertainty::Unknown,
                    result: None,
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }
    let mut running = agent("lead", None, "/p");
    for(k,v)in json!({"status":"running","inFlight":true,"accountKey":"account","connectionId":"connection","connectionGeneration":7,"threadId":"thread","turnId":"turn","startAttempt":{"id":"attempt","epoch":1,"action":"turn","submitted":true,"accountKey":"account","connectionId":"connection","connectionGeneration":7,"threadId":"thread","turnId":"turn","events":[]}}).as_object().expect("patch"){running[k]=v.clone();}
    let mut f = Fixture::new(vec![running], vec![], false).await;
    let broker = Arc::new(UnknownBroker {
        store: f.store.clone(),
        calls: Mutex::new(vec![]),
    });
    let ports: Arc<dyn LifecyclePorts> = Arc::new(ProductionLifecyclePorts::new(
        f.store.clone(),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        broker.clone(),
        None,
    ));
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        f.store.root().to_string_lossy().into(),
        49171,
        None,
        Some(f.store.clone()),
        None,
    );
    f.app = router(state).expect("router").layer(Extension(ports));
    let first = f.call(f.body.clone()).await;
    assert_eq!(first.0, 200, "{}", first.1);
    let attempts = broker.calls.lock().expect("calls").clone();
    assert_eq!(
        attempts.len(),
        2,
        "one interrupt and one background-command cleanup"
    );
    assert_eq!(
        f.agent("lead")["startAttempt"]["interrupt"]["outcome"],
        "unknown"
    );
    assert_eq!(f.call(f.body.clone()).await, first);
    assert_eq!(*broker.calls.lock().expect("calls"), attempts);
}

#[tokio::test]
async fn project_deletion_local_aliases_belong_to_one_scope() {
    let mut bound = agent("logical", None, "/else");
    bound["projectId"] = json!("logical-id");
    bound["projectServerId"] = json!("owner");
    let f = Fixture::new(vec![agent("physical", None, "/p"), bound], vec![], false).await;
    f.store.transaction(|tx|{
        let mut physical=ProjectsRepository::get(tx,"/p")?.expect("physical").1;
        physical["projectAliases"]=json!([{"serverId":"owner","projectId":"logical-id"}]);ProjectsRepository::put_project(tx,&physical)?;
        ProjectsRepository::put_project(tx,&json!({"id":"logical-id","path":"/p","name":"Logical","created":1,"homeServerId":"owner","locations":[{"serverId":"owner","path":"/p","projectId":"/p"}]}))
    }).await.expect("logical alias");
    let mut body = f.body.clone();
    body["expected_project_seq"] = json!(
        f.store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='project' AND id='/p'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("seq")
    );
    let (status, r) = f.call(body).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["deletedAgentIds"], json!(["logical", "physical"]));
    assert!(
        ProjectsRepository::get(&f.store.test_reader().expect("reader"), "logical-id")
            .expect("read")
            .is_none()
    );
}

#[tokio::test]
async fn project_deletion_streams_tombstones_to_two_clients_and_reconnect() {
    use futures_util::StreamExt;
    async fn connect(app: &axum::Router) -> axum::body::BodyDataStream {
        let response=app.clone().oneshot(Request::builder().uri("/api/sync/stream?protocol=4&resources=%5B%7B%22kind%22%3A%22state%22%7D%5D&tokenRates=false").header("host","127.0.0.1:49171").header("x-canvas-token","fixture-token").extension(ConnectInfo("127.0.0.1:12345".parse::<std::net::SocketAddr>().expect("address"))).body(Body::empty()).expect("request")).await.expect("stream");
        assert_eq!(response.status(), 200);
        response.into_body().into_data_stream()
    }
    async fn resource(stream: &mut axum::body::BodyDataStream) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let bytes = stream.next().await.expect("frame").expect("bytes");
                let frame = String::from_utf8_lossy(&bytes);
                if frame.starts_with("event: resources\n") {
                    return serde_json::from_str(
                        frame
                            .lines()
                            .find_map(|line| line.strip_prefix("data: "))
                            .expect("JSON frame"),
                    )
                    .expect("resource event");
                }
            }
        })
        .await
        .expect("stream event deadline")
    }
    fn changes(event: &Value) -> Vec<Value> {
        event["resourceVersions"]
            .as_array()
            .expect("versions")
            .iter()
            .flat_map(|v| {
                v["entityChanges"]["documents"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }
    let f = Fixture::new(
        vec![agent("lead", None, "/p"), agent("other", None, "/prefix")],
        vec![],
        false,
    )
    .await;
    let mut first = connect(&f.app).await;
    let mut second = connect(&f.app).await;
    resource(&mut first).await;
    resource(&mut second).await;
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 200, "{r}");
    for stream in [&mut first, &mut second] {
        let mut seen = vec![];
        while !seen.iter().any(|row: &Value| {
            row["collection"] == "project" && row["id"] == "/p" && row["op"] == "delete"
        }) {
            seen.extend(changes(&resource(stream).await));
        }
        assert!(
            !seen
                .iter()
                .any(|row| row["id"] == "other" && row["op"] == "delete")
        );
    }
    let mut reconnected = connect(&f.app).await;
    let baseline = resource(&mut reconnected).await;
    assert!(
        !changes(&baseline)
            .iter()
            .any(|row| row["collection"] == "project"
                && row["id"] == "/p"
                && row["op"] != "delete")
    );
}

#[tokio::test]
async fn project_deletion_refuses_unresolved_move_before_any_agent_redirect() {
    let f = Fixture::new(vec![agent("lead", None, "/p")], vec![], false).await;
    f.store.transaction(|tx|{
        tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_agent_moves(id TEXT PRIMARY KEY,record TEXT NOT NULL)")?;
        tx.execute("INSERT INTO runtime_agent_moves(id,record) VALUES('move',?1)",[json!({"id":"move","side":"source","phase":"unknown","agent":"lead","server":"remote","created":1}).to_string()])?;
        Ok(())
    }).await.expect("unresolved relocation");
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["code"], "project_remote_scope");
    assert_eq!(f.agent("lead")["epoch"], 1);
}

#[tokio::test]
async fn project_deletion_refuses_an_unbound_orphan_associated_by_root_id() {
    let f = Fixture::new(
        vec![
            agent("lead", None, "/p"),
            agent("unbound-orphan", Some("missing"), "/other"),
        ],
        vec![],
        false,
    )
    .await;
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["code"], "project_membership_conflict");
    assert_eq!(f.agent("lead")["epoch"], 1);
    assert_eq!(f.agent("unbound-orphan")["epoch"], 1);
}

#[tokio::test]
async fn project_deletion_work_release_never_changes_another_project() {
    use studio_runtime_services::{
        account_session_runtime::AccountSessionPort, lifecycle_effects::ProductionLifecyclePorts,
    };
    struct NoBroker;
    impl AccountSessionPort for NoBroker {}
    let mut other = agent("unrelated-worker", Some("other"), "/prefix");
    other["rootId"] = json!("other");
    other["status"] = json!("failed");
    let mut f = Fixture::new(
        vec![
            agent("lead", None, "/p"),
            agent("worker", Some("lead"), "/p"),
            agent("other", None, "/prefix"),
            other,
        ],
        vec![],
        false,
    )
    .await;
    f.store.transaction(|tx|{
        for(id,root,owner)in [("selected-task","lead","worker"),("unrelated-task","other","unrelated-worker")] {
            let task=json!({"id":id,"rootId":root,"owner":owner,"status":"running","title":"Task","description":"","created":1,"updated":1,"version":1,"dependencies":[],"results":[],"decisions":[],"createdBy":root});
            tx.execute("INSERT INTO runtime_work(id,record) VALUES(?1,?2)",rusqlite::params![id,task.to_string()])?;
        }Ok(())
    }).await.expect("tasks");
    let before: String = f
        .store
        .test_reader()
        .expect("reader")
        .query_row(
            "SELECT record FROM runtime_work WHERE id='unrelated-task'",
            [],
            |r| r.get(0),
        )
        .expect("before");
    let ports: Arc<dyn LifecyclePorts> = Arc::new(ProductionLifecyclePorts::new(
        f.store.clone(),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(NoBroker),
        None,
    ));
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        f.store.root().to_string_lossy().into(),
        49171,
        None,
        Some(f.store.clone()),
        None,
    );
    f.app = router(state).expect("router").layer(Extension(ports));
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 200, "{r}");
    let reader = f.store.test_reader().expect("reader");
    assert_eq!(
        reader
            .query_row(
                "SELECT record FROM runtime_work WHERE id='unrelated-task'",
                [],
                |r| r.get::<_, String>(0)
            )
            .expect("after"),
        before
    );
    let selected: String = reader
        .query_row(
            "SELECT record FROM runtime_work WHERE id='selected-task'",
            [],
            |r| r.get(0),
        )
        .expect("selected");
    assert!(serde_json::from_str::<Value>(&selected).expect("task")["owner"].is_null());
}

#[tokio::test]
async fn project_deletion_unknown_commit_without_receipt_retries_the_same_intention() {
    struct AbortFirstCommit {
        armed: std::sync::atomic::AtomicBool,
        wakes: AtomicUsize,
    }
    impl LifecyclePorts for AbortFirstCommit {
        fn stage(&self, tx: &Transaction<'_>, _: &LifecycleEffect) -> Result<(), ManagementError> {
            if self.armed.swap(false, Ordering::SeqCst) {
                tx.commit_hook(Some(|| true))
                    .map_err(studio_runtime_store::StoreError::from)?;
            }
            Ok(())
        }
        fn committed(&self) -> LifecycleCommitFuture<'_> {
            Box::pin(async {
                self.wakes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }
    let mut active = agent("lead", None, "/p");
    active["status"] = json!("running");
    active["inFlight"] = json!(true);
    active["turnId"] = json!("turn");
    active["threadId"] = json!("thread");
    let mut f = Fixture::new(vec![active], vec![], false).await;
    let abort = Arc::new(AbortFirstCommit {
        armed: std::sync::atomic::AtomicBool::new(true),
        wakes: AtomicUsize::new(0),
    });
    let ports: Arc<dyn LifecyclePorts> = abort.clone();
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        f.store.root().to_string_lossy().into(),
        49171,
        None,
        Some(f.store.clone()),
        None,
    );
    f.app = router(state).expect("router").layer(Extension(ports));
    let (status, unknown) = f.call(f.body.clone()).await;
    assert_eq!(status, 500, "{unknown}");
    assert!(
        unknown.get("outcome").is_none(),
        "a COMMIT error must not assert not_applied"
    );
    assert_eq!(f.agent("lead")["epoch"], 1);
    assert_eq!(abort.wakes.load(Ordering::SeqCst), 0);
    assert!(
        ProjectsRepository::receipt(
            &f.store.test_reader().expect("reader"),
            "project-delete:delete-1"
        )
        .expect("receipt lookup")
        .is_none()
    );
    f.store
        .transaction(|tx| {
            tx.commit_hook(None::<fn() -> bool>)?;
            Ok(())
        })
        .await
        .expect("remove controlled commit rejection");
    let (status, reply) = f.call(f.body.clone()).await;
    assert_eq!(
        status, 200,
        "same intention may apply after absent receipt: {reply}"
    );
    assert_eq!(f.agent("lead")["epoch"], 2);
    assert_eq!(abort.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(f.call(f.body.clone()).await, (200, reply));
    assert_eq!(f.agent("lead")["epoch"], 2);
}

#[tokio::test]
async fn project_deletion_refuses_an_unbound_orphan_associated_by_project_folder() {
    let mut orphan = agent("orphan", Some("missing"), "/p");
    orphan["rootId"] = json!("unknown-root");
    let f = Fixture::new(vec![agent("lead", None, "/p"), orphan], vec![], false).await;
    let (status, r) = f.call(f.body.clone()).await;
    assert_eq!(status, 409, "{r}");
    assert_eq!(r["code"], "project_membership_conflict");
    assert_eq!(f.agent("lead")["epoch"], 1);
    assert_eq!(f.agent("orphan")["epoch"], 1);
}
