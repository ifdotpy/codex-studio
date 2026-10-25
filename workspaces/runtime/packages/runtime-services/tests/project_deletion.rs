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
        Arc::new(UnavailableLifecyclePorts),
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
