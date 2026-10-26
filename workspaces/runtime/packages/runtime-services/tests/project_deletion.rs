//! Controlled creation/deletion interleavings through the real admission caller.
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_services::{
    account_policy::AccountPolicyState,
    agent_lifecycle_runtime::UnavailableLifecyclePorts,
    agents_create::{self, CreateError},
    host_worker_create::{AccountIdentity, CreationCatalog, ExecutionSettings},
    project_deletion,
};
use studio_runtime_store::{ProjectsRepository, RuntimeStore, SyncDocumentsRepository};
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
struct Catalog {
    observed: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl CreationCatalog for Catalog {
    fn resolve(
        &self,
        _: &str,
        _: &AccountIdentity,
        model: &str,
        effort: Option<&str>,
        fast: bool,
        daybreak: bool,
    ) -> Result<ExecutionSettings, CreateError> {
        self.observed
            .lock()
            .expect("signal")
            .take()
            .expect("one catalog call")
            .send(())
            .expect("creation observed");
        self.release
            .lock()
            .expect("release")
            .recv_timeout(DEADLINE)
            .expect("controlled write completed");
        Ok(ExecutionSettings {
            model: model.into(),
            effort: effort.map(str::to_owned),
            native_effort: "high".into(),
            fast_mode: fast,
            daybreak_enabled: daybreak,
            cyber_access_program: "standard".into(),
        })
    }
}
fn accounts() -> AccountPolicyState {
    AccountPolicyState::from_startup(Some(br#"{"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/fixture-provider-home","label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{},"deleteReceipts":{}}"#),[]).expect("accounts")
}
fn driver(root: &std::path::Path) -> Arc<studio_runtime_services::account_driver::AccountDriver> {
    let registry = root.join("accounts.json");
    std::fs::write(&registry,json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":root,"label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{},"deleteReceipts":{}}).to_string()).expect("registry");
    Arc::new(
        studio_runtime_services::account_driver::AccountDriver::open(&registry).expect("driver"),
    )
}
async fn setup() -> (tempfile::TempDir, Arc<RuntimeStore>, Value) {
    let root = tempfile::tempdir().expect("state");
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
    store
        .set_local_server_id(Some("owner".into()))
        .expect("identity");
    let path = studio_runtime_store::path_identity::resolve(root.path())
        .expect("canonical test path")
        .to_string_lossy()
        .into_owned();
    let project = json!({"id":path,"path":path,"name":"Project","created":1,"homeServerId":"owner","locations":[{"serverId":"owner","path":path,"projectId":path}]});
    let p = project.clone();
    store
        .transaction(move |tx| ProjectsRepository::put_project(tx, &p))
        .await
        .expect("project");
    (root, store, project)
}
async fn delete(store: Arc<RuntimeStore>, id: String) -> Value {
    delete_with_ports(store, id, Arc::new(UnavailableLifecyclePorts)).await
}
struct StagedLifecycle;
impl studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts for StagedLifecycle {
    fn stage(
        &self,
        _: &studio_runtime_store::Transaction<'_>,
        _: &studio_runtime_services::agent_lifecycle_runtime::LifecycleEffect,
    ) -> Result<(), studio_runtime_services::agent_management::ManagementError> {
        Ok(())
    }
    fn committed(
        &self,
    ) -> studio_runtime_services::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}
async fn delete_with_ports(
    store: Arc<RuntimeStore>,
    id: String,
    ports: Arc<dyn studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts>,
) -> Value {
    let reader = store.test_reader().expect("reader");
    let seq = reader
        .query_row(
            "SELECT seq FROM sync_entities WHERE collection='project' AND id=?1",
            [&id],
            |r| r.get(0),
        )
        .expect("seq");
    let workspace = SyncDocumentsRepository::workspace_id(&reader).expect("workspace");
    drop(reader);
    project_deletion::execute(
        store,
        project_deletion::Request {
            id,
            request_id: "delete".into(),
            expected_project_seq: seq,
            expected_workspace_id: workspace,
        },
        2.,
        ports,
    )
    .await
    .expect("delete")
}
fn creation(path: &str, bound: bool) -> agents_create::Creation {
    agents_create::Creation {
        id: Some("new-lead".into()),
        cwd: Some(path.into()),
        account: Some("default".into()),
        workspace: Some("worktree".into()),
        catalog: true,
        project_binding: bound,
        intent: if bound {
            json!({"model":"gpt-fixture","project_id":path,"project_server_id":"owner"})
        } else {
            json!({"model":"gpt-fixture"})
        },
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_deletion_fences_prepared_bound_and_legacy_creates_even_after_reregistration() {
    for bound in [false, true] {
        for reregister in [false, true] {
            let (_root, store, project) = setup().await;
            let path = project["id"].as_str().expect("id").to_owned();
            let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let catalog = Arc::new(Catalog {
                observed: Mutex::new(Some(observed_tx)),
                release: Mutex::new(release_rx),
            });
            let driver = driver(store.root());
            let operation_store = store.clone();
            let request = creation(&path, bound);
            let pending = tokio::spawn(async move {
                let accounts = accounts();
                let context = agents_create::Context {
                    store: operation_store,
                    accounts: Some(&accounts),
                    driver: Some(driver),
                    catalog: Some(catalog),
                    managed_creation_environment: None,
                    validate: Arc::new(|_, _| true),
                };
                agents_create::lead(&context, request).await
            });
            tokio::time::timeout(DEADLINE, observed_rx)
                .await
                .expect("catalog deadline")
                .expect("catalog paused");
            assert_eq!(
                delete(store.clone(), path.clone()).await["deletedAgentIds"],
                json!([])
            );
            if reregister {
                store
                    .transaction(move |tx| ProjectsRepository::put_project(tx, &project))
                    .await
                    .expect("fresh registration");
            }
            release_tx.send(()).expect("release preparation");
            let result = tokio::time::timeout(DEADLINE, pending)
                .await
                .expect("creation deadline")
                .expect("creation task");
            assert!(
                result.is_err(),
                "stale prepared admission must fail: {result:?}"
            );
            assert!(
                ProjectsRepository::agent(&store.test_reader().expect("reader"), "new-lead")
                    .expect("read")
                    .is_none()
            );
        }
    }
}

#[tokio::test]
async fn project_deletion_includes_a_create_that_wins_the_writer() {
    let (_root, store, project) = setup().await;
    let path = project["id"].as_str().expect("id");
    let accounts = accounts();
    let context = agents_create::Context {
        store: store.clone(),
        accounts: Some(&accounts),
        driver: Some(driver(store.root())),
        catalog: None,
        managed_creation_environment: None,
        validate: Arc::new(|_, _| true),
    };
    let mut request = creation(path, false);
    request.catalog = false;
    request.intent = json!({});
    agents_create::lead(&context, request)
        .await
        .expect("creation before deletion");
    assert_eq!(
        delete(store.clone(), path.to_owned()).await["deletedAgentIds"],
        json!(["new-lead"])
    );
    assert!(
        ProjectsRepository::agent(&store.test_reader().expect("reader"), "new-lead")
            .expect("read")
            .expect("retained agent")
            .1["deletedAt"]
            .is_number()
    );
}

#[tokio::test]
async fn project_deletion_receipt_survives_store_restart() {
    let (root, store, project) = setup().await;
    let id = project["id"].as_str().expect("id").to_owned();
    let reader = store.test_reader().expect("reader");
    let seq = reader
        .query_row(
            "SELECT seq FROM sync_entities WHERE collection='project' AND id=?1",
            [&id],
            |r| r.get(0),
        )
        .expect("seq");
    let workspace = SyncDocumentsRepository::workspace_id(&reader).expect("workspace");
    drop(reader);
    let first = delete(store.clone(), id.clone()).await;
    drop(store);
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart"),
    );
    store
        .set_local_server_id(Some("owner".into()))
        .expect("identity");
    let result = project_deletion::execute(
        store,
        project_deletion::Request {
            id,
            request_id: "delete".into(),
            expected_project_seq: seq,
            expected_workspace_id: workspace,
        },
        99.,
        Arc::new(UnavailableLifecyclePorts),
    )
    .await
    .expect("replay after restart");
    assert_eq!(result, first);
}

#[tokio::test]
async fn project_deletion_registry_only_remove_allows_fresh_creation() {
    let (_root, store, project) = setup().await;
    let path = project["id"].as_str().expect("id");
    let response = studio_runtime_services::projects::write(
        store.clone(),
        json!({"action":"remove","path":path}),
        "default".into(),
        vec![],
    )
    .await
    .expect("sidebar removal");
    assert_eq!(response["removed"], true);
    let accounts = accounts();
    let context = agents_create::Context {
        store: store.clone(),
        accounts: Some(&accounts),
        driver: Some(driver(store.root())),
        catalog: None,
        managed_creation_environment: None,
        validate: Arc::new(|_, _| true),
    };
    let mut request = creation(path, false);
    request.catalog = false;
    request.intent = json!({});
    agents_create::lead(&context, request)
        .await
        .expect("fresh create retains registry-only remove compatibility");
}

#[tokio::test]
async fn project_deletion_location_reservation_rechecks_the_checked_lifetime() {
    use studio_runtime_services::multi_server_transport::{
        AccessError, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
    };
    struct Peer(std::sync::atomic::AtomicUsize);
    impl PairedTransport for Peer {
        fn exchange<'a>(
            &'a self,
            _: &'a str,
            envelope: &'a Value,
            _: std::time::Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(
                async move { Ok(json!({"requestId":envelope["requestId"],"outcome":"unknown"})) },
            )
        }
    }
    let (_root, store, project) = setup().await;
    let id = project["id"].as_str().expect("project").to_owned();
    store.transaction(|tx|{tx.execute("INSERT INTO runtime_access_clients(id,record) VALUES('peer',?1)",[json!({"id":"peer","clientId":"client","kind":"server","label":"Peer","created":1,"publicKey":"fixture-key","status":"paired"}).to_string()])?;Ok(())}).await.expect("paired server");
    // This is the earlier checked lifetime; no outbound reservation exists yet.
    let reader = store.test_reader().expect("reader");
    let admission =
        studio_runtime_services::project_locations::location_admission(&reader, &id, None)
            .expect("checked location lifetime");
    drop(reader);
    let mut payload = json!({"project":id,"name":"Project","path":"/remote","request":{"action":"add_location","project":id,"server":"peer","path":"/remote"},"signature":"signature"});
    payload[studio_runtime_services::project_locations::LOCATION_ADMISSION_FIELD] = admission;
    delete(store.clone(), id).await;
    let peer = Arc::new(Peer(std::sync::atomic::AtomicUsize::new(0)));
    let driver = OrchestrationDriver::new(
        store.clone(),
        peer.clone(),
        Arc::new(RuntimeOrchestrationEffects::new(store.clone())),
    );
    assert!(
        driver
            .exchange("peer", "add_location", &payload, "location", 3)
            .await
            .is_err(),
        "deletion between preparation and reservation must refuse"
    );
    let enqueue_payload = payload.clone();
    let enqueue = store
        .transaction_with(move |tx| {
            studio_runtime_services::multi_server_transport::enqueue_in_transaction(
                tx,
                "peer",
                "add_location",
                &enqueue_payload,
                "marked-enqueue",
                3,
            )
        })
        .await;
    assert!(
        enqueue.is_err(),
        "a marked stale enqueue also rechecks the project lifetime"
    );

    assert_eq!(
        peer.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no remote effect"
    );
    assert_eq!(
        store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT count(*) FROM runtime_server_outbox WHERE id='location'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("reservation"),
        0
    );
}

async fn seed_parent(store: &Arc<RuntimeStore>, path: &str) {
    let accounts = accounts();
    let context = agents_create::Context {
        store: store.clone(),
        accounts: Some(&accounts),
        driver: Some(driver(store.root())),
        catalog: None,
        managed_creation_environment: None,
        validate: Arc::new(|_, _| true),
    };
    let mut request = creation(path, false);
    request.catalog = false;
    request.intent = json!({});
    agents_create::lead(&context, request)
        .await
        .expect("parent");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_deletion_fences_prepared_spawn_and_includes_prior_spawn() {
    for deletion_first in [false, true] {
        let (_root, store, project) = setup().await;
        let path = project["id"].as_str().expect("path");
        seed_parent(&store, path).await;
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let catalog = Arc::new(Catalog {
            observed: Mutex::new(Some(observed_tx)),
            release: Mutex::new(release_rx),
        });
        let worker_store = store.clone();
        let account_driver = driver(store.root());
        let pending = tokio::spawn(async move {
            let accounts = accounts();
            let context = agents_create::Context {
                store: worker_store,
                accounts: Some(&accounts),
                driver: Some(account_driver),
                catalog: Some(catalog),
                managed_creation_environment: None,
                validate: Arc::new(|_, _| true),
            };
            agents_create::agent(
                &context,
                agents_create::Creation {
                    id: Some("spawned".into()),
                    parent: Some("new-lead".into()),
                    workspace: Some("shared".into()),
                    account: Some("default".into()),
                    prompt: "Task".into(),
                    intent: json!({"model":"gpt-fixture"}),
                    ..Default::default()
                },
            )
            .await
        });
        tokio::time::timeout(DEADLINE, observed_rx)
            .await
            .expect("spawn deadline")
            .expect("spawn prepared");
        if deletion_first {
            delete_with_ports(store.clone(), path.into(), Arc::new(StagedLifecycle)).await;
        }
        release_tx.send(()).expect("release spawn");
        let result = tokio::time::timeout(DEADLINE, pending)
            .await
            .expect("spawn deadline")
            .expect("spawn task");
        if deletion_first {
            assert!(result.is_err(), "stale spawn must refuse");
        } else {
            result.expect("spawn admitted before delete");
            assert_eq!(
                delete_with_ports(store.clone(), path.into(), Arc::new(StagedLifecycle)).await["deletedAgentIds"],
                json!(["new-lead", "spawned"])
            );
        }
    }
}

#[tokio::test]
async fn project_deletion_orders_archive_restore_with_its_writer() {
    use studio_runtime_services::{agent_management::Action, agent_management_store};
    use studio_runtime_store::{AgentRecord, AgentRepository};
    for deletion_first in [false, true] {
        let (_root, store, project) = setup().await;
        let path = project["id"].as_str().expect("id");
        seed_parent(&store, path).await;
        let parent = ProjectsRepository::agent(&store.test_reader().expect("reader"), "new-lead")
            .expect("read")
            .expect("parent")
            .1;
        let epoch = parent["epoch"].as_i64().expect("epoch");
        let mut worker = parent.clone();
        for(k,v)in json!({"id":"archived","rootId":"new-lead","parentId":"new-lead","isLead":false,"role":"implementer","epoch":1,"status":"paused","autoWake":false,"deletedAt":1,"agentArchive":{"at":1,"epoch":1,"cleanupPending":false},"worktree":false,"worktreeReady":false,"workspaceMode":"shared"}).as_object().expect("patch"){worker[k]=v.clone();}
        store
            .transaction(move |tx| AgentRepository::put(tx, &AgentRecord::from_value(worker)?))
            .await
            .expect("archived worker");
        if deletion_first {
            delete(store.clone(), path.into()).await;
        }
        let result = agent_management_store::execute(
            store.clone(),
            &Action::Restore {
                actor: "new-lead".into(),
                epoch,
                target: "archived".into(),
            },
            3.,
        )
        .await;
        if deletion_first {
            assert!(result.is_err(), "stale restore must refuse");
        } else {
            assert_eq!(result.expect("restore before delete")["status"], "restored");
            assert_eq!(
                delete(store.clone(), path.into()).await["deletedAgentIds"],
                json!(["archived", "new-lead"])
            );
        }
    }
}

#[tokio::test]
async fn project_deletion_refuses_fresh_markerless_location_reservations_but_replays_legacy() {
    use studio_runtime_services::multi_server_transport::{
        AccessError, OrchestrationDriver, PairedTransport, RuntimeOrchestrationEffects,
        enqueue_in_transaction,
    };
    struct Peer(std::sync::atomic::AtomicUsize);
    impl PairedTransport for Peer {
        fn exchange<'a>(
            &'a self,
            _: &'a str,
            envelope: &'a Value,
            _: std::time::Duration,
        ) -> futures_util::future::BoxFuture<'a, Result<Value, AccessError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(
                async move { Ok(json!({"requestId":envelope["requestId"],"outcome":"unknown"})) },
            )
        }
    }
    let (_root, store, project) = setup().await;
    let id = project["id"].as_str().expect("id").to_owned();
    store.transaction(|tx|{tx.execute("INSERT INTO runtime_access_clients(id,record) VALUES('peer',?1)",[json!({"id":"peer","clientId":"client","kind":"server","label":"Peer","created":1,"publicKey":"fixture-key","status":"paired"}).to_string()])?;Ok(())}).await.expect("paired peer");
    let payload = json!({"project":id,"name":"Project","path":"/remote","request":{"action":"add_location","project":id,"server":"peer","path":"/remote"},"signature":"signature"});
    delete(store.clone(), id).await;
    let peer = Arc::new(Peer(std::sync::atomic::AtomicUsize::new(0)));
    let driver = OrchestrationDriver::new(
        store.clone(),
        peer.clone(),
        Arc::new(RuntimeOrchestrationEffects::new(store.clone())),
    );
    assert!(
        driver
            .exchange("peer", "add_location", &payload, "fresh", 3)
            .await
            .is_err(),
        "fresh exchange requires admission"
    );
    assert_eq!(peer.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let fresh = payload.clone();
    let enqueue = store
        .transaction_with(move |tx| {
            enqueue_in_transaction(tx, "peer", "add_location", &fresh, "fresh-enqueue", 3)
        })
        .await;
    assert!(
        enqueue.is_err(),
        "the transaction enqueue writer also requires admission"
    );
    // A saved exact marker-less reservation represents a previous server version.
    let envelope = json!({"requestId":"legacy","action":"add_location","payload":payload});
    let encoded = studio_runtime_services::remote_transport::canonical_json(&envelope)
        .expect("canonical envelope");
    store.transaction(move|tx|{tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,created_at,next_at,attempts) VALUES('legacy','peer',?1,'queued',1,0,0)",[encoded])?;Ok(())}).await.expect("legacy reservation");
    let replay = payload.clone();
    store
        .transaction_with(move |tx| {
            enqueue_in_transaction(tx, "peer", "add_location", &replay, "legacy", 3)
        })
        .await
        .expect("exact legacy enqueue replay");
    let unknown = driver
        .exchange("peer", "add_location", &payload, "legacy", 3)
        .await
        .expect("exact legacy reservation remains recoverable");
    assert!(matches!(
        unknown,
        studio_runtime_services::multi_server_transport::ExchangeOutcome::Unknown
    ));
    assert_eq!(
        peer.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "legacy uncertain writes retain their existing no-resend policy"
    );
    store
        .transaction(|tx| {
            tx.execute(
                "UPDATE runtime_server_outbox SET state='complete',result=?1 WHERE id='legacy'",
                [
                    json!({"requestId":"legacy","outcome":"applied","value":{"retained":true}})
                        .to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .expect("retained legacy receipt");
    let replayed = driver
        .exchange("peer", "add_location", &payload, "legacy", 4)
        .await
        .expect("legacy completed receipt replays without admission");
    assert!(
        matches!(replayed,studio_runtime_services::multi_server_transport::ExchangeOutcome::Applied(ref value) if value["retained"]==true)
    );
    let mut changed = payload;
    changed["path"] = json!("/changed");
    assert!(
        driver
            .exchange("peer", "add_location", &changed, "legacy", 30)
            .await
            .is_err(),
        "legacy replay still binds exact content"
    );
    assert_eq!(peer.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}
