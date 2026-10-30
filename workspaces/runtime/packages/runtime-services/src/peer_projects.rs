//! Projects of paired servers, published to this server's own clients.
//!
//! A client that cannot sign requests to paired servers (the browser) still shows their
//! projects: this server reads each paired server's `projects` list through the signed
//! orchestration exchange and publishes every project as a `project` sync entity. The entity
//! id is `peer:<server>:<project>`, its `homeServerId` is the paired server and its
//! `serverLabel` is that server's name. The entities carry no chats and no write path:
//! opening a remote project's chat stays with a client that has the signed transport.
//!
//! Refresh is event driven. It runs at startup, when a server is paired, when a paired server
//! that failed a read reaches this server again, and when a paired server sends
//! `projects.changed` after its own project list changed. No timer reads the peers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::Transaction;
use serde_json::json;
use studio_runtime_store::{RuntimeStore, StoreError, SyncDocumentsRepository};
use tokio::sync::{Notify, watch};

use crate::multi_server_orchestration::{ExchangeOutcome, OrchestrationDriver};

/// The orchestration action that tells a paired server that this server's projects changed.
/// The notice carries no data: the receiver reads the list with the `projects` action.
pub const CHANGED_ACTION: &str =
    studio_runtime_schema::persistence::ServerOperationKind::ProjectsChanged.as_str();
const ID_PREFIX: &str = "peer:";
const LABEL_FALLBACK: &str = "Paired server";

fn entity_id(server: &str, project: &str) -> String {
    format!("{ID_PREFIX}{server}:{project}")
}

/// Pending causes of a refresh. Callers set a cause and wake the single refresh task.
#[derive(Default)]
pub struct PeerProjectsWake {
    notify: Notify,
    causes: Mutex<Causes>,
}

#[derive(Default)]
struct Causes {
    notified: BTreeSet<String>,
    peer_reached: bool,
    local_changed: bool,
}

impl PeerProjectsWake {
    /// A paired server reported a change of its projects.
    pub fn peer_changed(&self, server: &str) {
        self.set(|causes| {
            causes.notified.insert(server.to_owned());
        });
    }
    /// A paired server made a signed request to this server, or the pairing list changed.
    pub fn peer_reached(&self) {
        self.set(|causes| causes.peer_reached = true);
    }
    /// A project of this server changed.
    pub fn local_changed(&self) {
        self.set(|causes| causes.local_changed = true);
    }
    fn set(&self, change: impl FnOnce(&mut Causes)) {
        change(&mut self.causes.lock().unwrap_or_else(PoisonError::into_inner));
        self.notify.notify_one();
    }
    fn take(&self) -> Causes {
        std::mem::take(&mut *self.causes.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerProject {
    id: Option<String>,
    path: Option<String>,
    name: Option<String>,
    created: Option<f64>,
    updated: Option<f64>,
    #[serde(default)]
    project_aliases: Vec<ProjectAlias>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectAlias {
    server_id: String,
}
#[derive(serde::Deserialize)]
struct ProjectList {
    items: Vec<PeerProject>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectMirror {
    created: Option<f64>,
    home_server_id: String,
    id: String,
    name: Option<String>,
    path: Option<String>,
    server_label: String,
    updated: Option<f64>,
}

fn local_aliases(tx: &Transaction<'_>) -> Result<BTreeSet<(String, String)>, StoreError> {
    let mut query = tx.prepare("SELECT json_extract(a.value,'$.serverId'),json_extract(a.value,'$.projectId') FROM runtime_projects p, json_each(CASE WHEN json_type(p.record,'$.projectAliases')='array' THEN json_extract(p.record,'$.projectAliases') ELSE '[]' END) a WHERE json_extract(p.record,'$.removal') IS NULL AND COALESCE(json_type(p.record,'$.sidebarHidden'),'')!='true' AND a.type='object' AND json_type(a.value,'$.serverId')='text' AND json_type(a.value,'$.projectId')='text'")?;
    Ok(query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

/// Entities that mirror `items` of one paired server, skipping the projects that this server
/// already lists under another identity (a project that has a location on both servers).
fn mirrored(
    server: &str,
    label: &str,
    items: &[PeerProject],
    local: &BTreeSet<(String, String)>,
    local_server: &str,
) -> BTreeMap<String, ProjectMirror> {
    let mut result = BTreeMap::new();
    for item in items {
        let Some(id) = item.id.as_deref().filter(|id| !id.is_empty()) else {
            continue;
        };
        let aliases_here = item
            .project_aliases
            .iter()
            .any(|alias| alias.server_id == local_server);
        if aliases_here || local.contains(&(server.to_owned(), id.to_owned())) {
            continue;
        }
        let bound = |s: &str| s.chars().take(crate::sync_views::SYNC_TEXT_CHARS).collect();
        let record = ProjectMirror {
            id: bound(&entity_id(server, id)),
            path: item.path.as_deref().map(bound),
            name: item.name.as_deref().map(bound),
            created: item.created,
            updated: item.updated,
            home_server_id: bound(server),
            server_label: bound(label),
        };
        result.insert(entity_id(server, id), record);
    }
    result
}

/// Make the published entities of `server` equal to `entities`; no entities removes them all.
fn publish(
    tx: &Transaction<'_>,
    server: &str,
    entities: &BTreeMap<String, ProjectMirror>,
) -> Result<(), StoreError> {
    let prefix = format!("{ID_PREFIX}{server}:");
    let mut statement = tx.prepare(
        "SELECT id FROM sync_entities WHERE collection='project' AND deleted=0 \
         AND substr(id,1,length(?1))=?1",
    )?;
    let stale = statement
        .query_map([&prefix], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in stale.iter().filter(|id| !entities.contains_key(*id)) {
        SyncDocumentsRepository::put_entity(tx, "project", id, (), true)?;
    }
    for (id, value) in entities {
        SyncDocumentsRepository::put_entity(tx, "project", id, value, false)?;
    }
    Ok(())
}

/// Servers that have published entities.
fn published_servers(tx: &Transaction<'_>) -> Result<BTreeSet<String>, StoreError> {
    let mut statement = tx.prepare(
        "SELECT DISTINCT json_extract(payload,'$.value.homeServerId') FROM sync_entities \
         WHERE collection='project' AND deleted=0 AND substr(id,1,?1)=?2",
    )?;
    let servers = statement
        .query_map(
            rusqlite::params![ID_PREFIX.len() as i64, ID_PREFIX],
            |row| row.get::<_, Option<String>>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(servers.into_iter().flatten().collect())
}

fn fail(what: &str, error: &dyn std::fmt::Display) {
    crate::diagnostics::log_stderr(format_args!("peer projects: could not {what}: {error}"));
}

/// Reads paired servers and publishes their projects. One task owns it.
pub struct PeerProjects {
    store: Arc<RuntimeStore>,
    driver: Arc<OrchestrationDriver>,
    /// Paired servers whose last read failed; a later contact from them retries the read.
    stale: BTreeSet<String>,
    /// Paired servers at the last pass; `None` before the first pass.
    known: Option<BTreeSet<String>>,
}

impl PeerProjects {
    pub fn new(store: Arc<RuntimeStore>, driver: Arc<OrchestrationDriver>) -> Self {
        Self {
            store,
            driver,
            stale: BTreeSet::new(),
            known: None,
        }
    }

    /// Run until `shutdown` changes. Startup counts as a contact with every paired server.
    pub async fn run(mut self, wake: Arc<PeerProjectsWake>, mut shutdown: watch::Receiver<bool>) {
        wake.peer_reached();
        loop {
            tokio::select! {
                () = wake.notify.notified() => {}
                _ = shutdown.changed() => break,
            }
            if *shutdown.borrow() {
                break;
            }
            let causes = wake.take();
            self.handle(causes).await;
        }
    }

    async fn handle(&mut self, causes: Causes) {
        let Ok(paired) = self.paired().await else {
            // The next cause reads the list again.
            return;
        };
        if causes.local_changed {
            for server in paired.keys() {
                self.notify(server).await;
            }
        }
        let mut refresh = causes.notified;
        let known = self.known.clone().unwrap_or_default();
        if causes.peer_reached {
            refresh.extend(paired.keys().filter(|id| !known.contains(*id)).cloned());
            refresh.extend(self.stale.iter().cloned());
        }
        let ids = paired.keys().cloned().collect::<BTreeSet<_>>();
        if self.known.as_ref() != Some(&ids) {
            let kept = ids.clone();
            if let Err(error) = self
                .store
                .transaction_with(move |tx| forget_unpaired(tx, &kept))
                .await
            {
                fail("remove unpaired projects", &error);
            }
        }
        self.known = Some(ids);
        self.stale.retain(|id| paired.contains_key(id));
        for server in refresh {
            if let Some(label) = paired.get(&server) {
                self.refresh(&server, label).await;
            }
        }
    }

    /// Paired servers by id with their display names.
    async fn paired(&self) -> Result<BTreeMap<String, String>, StoreError> {
        self.store
            .read_async("runtime-services/peer_projects", |tx| {
                let mut paired = BTreeMap::new();
                for server in crate::multi_server::paired_servers(tx)? {
                    if let Some(id) = server["id"].as_str() {
                        let label = server["label"].as_str().filter(|label| !label.is_empty());
                        paired.insert(id.to_owned(), label.unwrap_or(LABEL_FALLBACK).to_owned());
                    }
                }
                Ok::<_, StoreError>(paired)
            })
            .await
    }

    async fn notify(&self, server: &str) {
        let Ok(now) = crate::event_write::unix_time() else {
            return;
        };
        let key = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        // The receiver reads the list; a lost notice is repaired by its next contact.
        if let Err(error) = self
            .driver
            .exchange(server, CHANGED_ACTION, &json!({}), &key, now as i64)
            .await
        {
            fail("send projects.changed", &error);
        }
    }

    async fn refresh(&mut self, server: &str, label: &str) {
        let items = self.read(server).await;
        let Some(items) = items else {
            self.stale.insert(server.to_owned());
            return;
        };
        self.stale.remove(server);
        let store = Arc::clone(&self.store);
        let local_server = {
            let store = Arc::clone(&store);
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/peer_projects",
                move || store.local_server_id(),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
            .unwrap_or_default()
        };
        let (server, label) = (server.to_owned(), label.to_owned());
        if let Err(error) = store
            .transaction_with(move |tx| {
                let local = local_aliases(tx)?;
                let entities = mirrored(&server, &label, &items, &local, &local_server);
                publish(tx, &server, &entities)
            })
            .await
        {
            fail("publish projects", &error);
        }
    }

    /// The `projects` list of a paired server, or `None` when it did not answer.
    async fn read(&self, server: &str) -> Option<Vec<PeerProject>> {
        let key = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let now = crate::event_write::unix_time().ok()? as i64;
        match self
            .driver
            .exchange(server, "projects", &json!({}), &key, now)
            .await
        {
            Ok(ExchangeOutcome::Applied(list)) => {
                serde_json::from_str::<ProjectList>(list.value.get())
                    .ok()
                    .map(|list| list.items)
            }
            _ => None,
        }
    }
}

/// Remove the published entities of servers that are no longer paired.
pub fn forget_unpaired(tx: &Transaction<'_>, paired: &BTreeSet<String>) -> Result<(), StoreError> {
    for server in published_servers(tx)?.difference(paired) {
        publish(tx, server, &BTreeMap::new())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::multi_server_auth::AccessError;
    use crate::multi_server_orchestration::{
        OrchestrationEffects, PairedTransport, RuntimeOrchestrationEffects,
    };
    use futures_util::future::BoxFuture;
    use serde_json::Value;
    use studio_runtime_store::ProjectsRepository;

    const PEER: &str = "peer-server";

    #[test]
    fn typed_aliases_filter_mirrors_and_keep_projection() {
        let items: ProjectList = serde_json::from_str(
            r#"{"items":[{"id":"one","name":"First"},{"id":"two","name":"Second"}]}"#,
        )
        .unwrap();
        let aliases = BTreeSet::from([(PEER.to_owned(), "one".to_owned())]);
        let result = mirrored(PEER, "Peer", &items.items, &aliases, "local");
        assert_eq!(result.len(), 1);
        let view = json!({"id":"peer:peer-server:two","path":null,"name":"Second","created":null,"updated":null,"homeServerId":PEER,"serverLabel":"Peer"});
        let old = crate::sync_views::project_entity_view(&view, None, None, None).unwrap();
        assert_eq!(
            serde_json::to_string(&result[&entity_id(PEER, "two")]).unwrap(),
            serde_json::to_string(&old).unwrap()
        );
    }

    #[test]
    fn local_aliases_ignore_hidden_removed_and_non_object_entries() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
            .unwrap();
        for (id, record) in [
            (
                "shown",
                json!({"projectAliases":["invalid", 7, null, {"serverId":"s","projectId":"p"}]}),
            ),
            (
                "hidden",
                json!({"sidebarHidden":true,"projectAliases":[{"serverId":"hidden","projectId":"p"}]}),
            ),
            (
                "removed",
                json!({"removal":{},"projectAliases":[{"serverId":"removed","projectId":"p"}]}),
            ),
        ] {
            db.execute(
                "INSERT INTO runtime_projects VALUES(?1,?2)",
                rusqlite::params![id, record.to_string()],
            )
            .unwrap();
        }
        assert_eq!(
            local_aliases(&db.transaction().unwrap()).unwrap(),
            BTreeSet::from([("s".into(), "p".into())])
        );
    }
    /// Test-only identity of the local server (a throwaway key pair).
    const IDENTITY: &str = "{\"label\":\"Studio server\",\"privateKey\":\"-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIEABs3CR9YO/2snXAA7CZxSJcf+AXY62nmdKhdyv14QS\\n-----END PRIVATE KEY-----\\n\",\"publicKey\":\"-----BEGIN PUBLIC KEY-----\\nMCowBQYDK2VwAyEAElmQYRu24EVfx1oxPVvQb30C+R3RQaB0QboX7Xsi6OQ=\\n-----END PUBLIC KEY-----\\n\",\"serverId\":\"628c18136e1542fa9f45b899f3b75503\"}";

    /// A paired server that answers `projects` from a list and can be offline.
    #[derive(Default)]
    struct FakePeer {
        items: Mutex<Vec<Value>>,
        offline: Mutex<bool>,
        notices: Mutex<usize>,
    }

    impl PairedTransport for FakePeer {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            envelope: &'a Value,
            _timeout: std::time::Duration,
        ) -> BoxFuture<
            'a,
            Result<studio_runtime_store::multi_server_delivery::RemoteReply, AccessError>,
        > {
            Box::pin(async move {
                let key = envelope["requestId"].clone();
                if *self.offline.lock().unwrap() {
                    return Err(AccessError::Refused {
                        status: 503,
                        code: "exchange_unknown",
                        message: "The paired delivery outcome is unknown",
                    });
                }
                #[derive(serde::Serialize)]
                struct Items<'a, T> {
                    items: &'a [T],
                }
                #[derive(serde::Deserialize)]
                struct Request {
                    action: String,
                }
                let request: Request = serde_json::from_str(&envelope.to_string()).unwrap();
                let value = match request.action.as_str() {
                    "projects" => serde_json::value::to_raw_value(&Items {
                        items: &self.items.lock().unwrap(),
                    })
                    .unwrap(),
                    CHANGED_ACTION => {
                        *self.notices.lock().unwrap() += 1;
                        serde_json::value::RawValue::from_string("{}".to_owned()).unwrap()
                    }
                    _ => panic!("unexpected action"),
                };
                Ok(studio_runtime_store::multi_server_delivery::RemoteReply {
                    request_id: key.as_str().unwrap().to_owned(),
                    outcome: studio_runtime_schema::persistence::RemoteOutcome::Applied,
                    value: Some(value),
                    error: None,
                    waiting_for: None,
                })
            })
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        store: Arc<RuntimeStore>,
        peer: Arc<FakePeer>,
        projects: PeerProjects,
    }

    async fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let identity = directory.path().join("multi-server");
        std::fs::create_dir(&identity).unwrap();
        std::fs::write(identity.join("identity.json"), IDENTITY).unwrap();
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
        let record = json!({"id": PEER, "clientId": PEER, "serverId": PEER, "label": "Laptop",
            "kind": "server", "origin": "https://peer.fixture.ts.net", "publicKey": "fixture-key",
            "tailscaleUser": "owner@example.test", "status": "paired", "created": 10,
            "lastAccess": null, "revoked": null});
        store
            .transaction_with(move |tx| -> Result<(), StoreError> {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    rusqlite::params![PEER, record.to_string()],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let peer = Arc::new(FakePeer::default());
        let driver = Arc::new(OrchestrationDriver::new(
            Arc::clone(&store),
            Arc::clone(&peer) as Arc<dyn PairedTransport>,
            Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store))),
        ));
        let projects = PeerProjects::new(Arc::clone(&store), driver);
        Fixture {
            _directory: directory,
            store,
            peer,
            projects,
        }
    }

    fn project(id: &str, name: &str) -> Value {
        json!({"id": id, "path": id, "name": name, "created": 1.0, "homeServerId": PEER})
    }

    /// Live `project` entities of paired servers, by id.
    async fn published(store: &RuntimeStore) -> BTreeMap<String, Value> {
        store
            .read_async("test", |tx| {
                let mut statement = tx.prepare(
                    "SELECT id,payload FROM sync_entities WHERE collection='project' AND deleted=0 AND id LIKE 'peer:%' ORDER BY id",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, StoreError>(
                    rows.into_iter()
                        .map(|(id, payload)| {
                            (id, serde_json::from_str::<Value>(&payload).unwrap()["value"].clone())
                        })
                        .collect(),
                )
            })
            .await
            .unwrap()
    }

    fn reached() -> Causes {
        Causes {
            peer_reached: true,
            ..Causes::default()
        }
    }

    fn notified() -> Causes {
        Causes {
            notified: BTreeSet::from([PEER.to_owned()]),
            ..Causes::default()
        }
    }

    #[tokio::test]
    async fn a_paired_servers_project_is_published_and_follows_its_changes() {
        let mut f = fixture().await;
        *f.peer.items.lock().unwrap() = vec![project("/work/attar", "attar")];
        f.projects.handle(reached()).await;
        let rows = published(&f.store).await;
        let row = &rows[&format!("peer:{PEER}:/work/attar")];
        assert_eq!(row["name"], "attar");
        assert_eq!(row["homeServerId"], PEER);
        assert_eq!(row["serverLabel"], "Laptop");

        *f.peer.items.lock().unwrap() = vec![project("/work/attar", "attar 2"), project("/b", "b")];
        f.projects.handle(notified()).await;
        let rows = published(&f.store).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[&format!("peer:{PEER}:/work/attar")]["name"], "attar 2");

        *f.peer.items.lock().unwrap() = vec![project("/b", "b")];
        f.projects.handle(notified()).await;
        assert_eq!(
            published(&f.store).await.keys().collect::<Vec<_>>(),
            [&format!("peer:{PEER}:/b")]
        );
    }

    #[tokio::test]
    async fn an_unreachable_server_keeps_its_projects_and_is_read_again_on_contact() {
        let mut f = fixture().await;
        *f.peer.items.lock().unwrap() = vec![project("/a", "a")];
        f.projects.handle(reached()).await;
        *f.peer.offline.lock().unwrap() = true;
        *f.peer.items.lock().unwrap() = vec![project("/a", "a"), project("/c", "c")];
        f.projects.handle(notified()).await;
        assert_eq!(
            published(&f.store).await.len(),
            1,
            "a failed read changes nothing"
        );
        *f.peer.offline.lock().unwrap() = false;
        f.projects.handle(reached()).await;
        assert_eq!(
            published(&f.store).await.len(),
            2,
            "contact retries the failed read"
        );
    }

    #[tokio::test]
    async fn an_unpaired_server_loses_its_projects_and_a_project_known_here_is_not_repeated() {
        let mut f = fixture().await;
        let local = f.store.local_server_id().unwrap().unwrap();
        let known = json!({"id": "/here", "path": "/here", "name": "here", "created": 1.0,
            "homeServerId": local, "locations": [], "locationsRevision": 0,
            "projectAliases": [{"serverId": PEER, "projectId": "/there"}]});
        f.store
            .transaction_with(move |tx| ProjectsRepository::put_project(tx, &known))
            .await
            .unwrap();
        *f.peer.items.lock().unwrap() =
            vec![project("/there", "there"), project("/other", "other")];
        f.projects.handle(reached()).await;
        assert_eq!(
            published(&f.store).await.keys().collect::<Vec<_>>(),
            [&format!("peer:{PEER}:/other")],
            "the aliased project appears once, under its local identity"
        );
        f.store
            .transaction_with(|tx| -> Result<(), StoreError> {
                tx.execute("DELETE FROM runtime_access_clients", [])?;
                Ok(())
            })
            .await
            .unwrap();
        f.projects.handle(reached()).await;
        assert!(published(&f.store).await.is_empty());
    }

    #[tokio::test]
    async fn a_changed_notice_wakes_the_refresh_and_a_local_change_notifies_the_peer() {
        let f = fixture().await;
        let wake = Arc::new(PeerProjectsWake::default());
        let effects = RuntimeOrchestrationEffects::new(Arc::clone(&f.store))
            .with_projects_changed(Arc::clone(&wake));
        let envelope = json!({"requestId": "r", "action": CHANGED_ACTION, "payload": {}});
        assert!(effects.supports(CHANGED_ACTION));
        effects.execute(PEER, &envelope).await.unwrap();
        assert_eq!(wake.take().notified, BTreeSet::from([PEER.to_owned()]));

        let mut f = f;
        f.projects
            .handle(Causes {
                local_changed: true,
                ..Causes::default()
            })
            .await;
        assert_eq!(*f.peer.notices.lock().unwrap(), 1);
    }
}
