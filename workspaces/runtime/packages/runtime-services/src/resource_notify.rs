//! Bounded process-local resource invalidations. Call publishers only after commit.
//!
//! Notify identity is scoped to this workspace and process, as in Python's relay.
//! Exact retries refresh the 256-entry LRU; conflicting retries never publish.
//! Restart or eviction permits another invalidation (never another store write).

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(any(test, feature = "test-support"))]
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_store::ResourceChange;
use thiserror::Error;

use crate::change_notifications::{ChangeNotificationHub, ChangeSnapshot};

pub const MAX_NOTIFY_RECEIPTS: usize = 256;
pub const MAX_PUBLICATION_BYTES: usize = 262_144;
const MAX_RESOURCE_VERSIONS: usize = 4096;

#[derive(Debug, Error, PartialEq)]
pub enum ResourceNotifyError {
    #[error("Invalid resource publication")]
    Invalid,
    #[error("Request identity already has different resources")]
    Conflict,
    #[error("Resource invalidation could not be published")]
    Unavailable,
}

// Fixture adapter for tests of notification selection.
#[cfg(any(test, feature = "test-support"))]
#[derive(Deserialize)]
struct ResourceScope {
    kind: ResourceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "test-support"), derive(Deserialize))]
#[cfg_attr(any(test, feature = "test-support"), serde(rename_all = "kebab-case"))]
pub enum ResourceKind {
    State,
    #[cfg_attr(any(test, feature = "test-support"), serde(rename = "agentDetail"))]
    AgentDetail,
    Costs,
    Desktop,
    Monitors,
    SessionCost,
    Terminal,
    Terminals,
    Transcript,
    Transcripts,
    Room,
    Processes,
    Complaint,
    Work,
    TerminalOutput,
    Task,
    Tasks,
    Workspace,
    MultiServer,
    #[cfg_attr(any(test, feature = "test-support"), serde(other))]
    Other,
}

/// The notification service selects and returns the caller's typed resources.
pub trait ResourceIdentity: Clone {
    fn notification_key(&self) -> Result<String, ResourceNotifyError>;
    fn notification_kind(&self) -> Result<ResourceKind, ResourceNotifyError>;
}

#[cfg(any(test, feature = "test-support"))]
impl ResourceIdentity for Value {
    fn notification_key(&self) -> Result<String, ResourceNotifyError> {
        Ok(self.to_string())
    }
    fn notification_kind(&self) -> Result<ResourceKind, ResourceNotifyError> {
        ResourceScope::deserialize(self)
            .map(|scope| scope.kind)
            .map_err(|_| ResourceNotifyError::Invalid)
    }
}

impl ResourceKind {
    fn is_entity(&self) -> bool {
        matches!(
            self,
            Self::State
                | Self::AgentDetail
                | Self::Costs
                | Self::Desktop
                | Self::Monitors
                | Self::SessionCost
                | Self::Task
                | Self::Tasks
                | Self::Terminal
                | Self::Terminals
                | Self::Work
        )
    }
}

struct ContentVersion {
    key: String,
    digest: Option<[u8; 32]>,
    checked_revision: i64,
    revision: i64,
}

struct Publications {
    receipts: VecDeque<(String, [u8; 32])>,
    versions: VecDeque<(String, i64)>,
    pruned_revision: i64,
    entity_reset_revision: i64,
    entity_revision: i64,
}

impl Publications {
    fn resource_revision(&self, key: &str) -> i64 {
        self.versions
            .iter()
            .find(|(existing, _)| existing == key)
            .map_or(0, |(_, revision)| *revision)
    }
}

/// A consistent resource view, with only the requested resources that changed.
pub struct ResourceChanges<T = Value> {
    pub cursor: ChangeSnapshot,
    pub resources: Vec<T>,
    pub versions: Vec<i64>,
    pub entity_reset: bool,
    pub overflow: bool,
}

/// One instance per HTTP state's workspace; it shares the existing commit cursor.
pub struct ResourceNotifications {
    changes: Arc<ChangeNotificationHub>,
    publications: Mutex<Publications>,
    // Share completed projections without holding the guard during reads.
    content: tokio::sync::Mutex<VecDeque<ContentVersion>>,
}

impl ResourceNotifications {
    pub fn new(changes: Arc<ChangeNotificationHub>) -> Self {
        Self {
            changes,
            content: tokio::sync::Mutex::new(VecDeque::new()),
            publications: Mutex::new(Publications {
                receipts: VecDeque::new(),
                versions: VecDeque::new(),
                pruned_revision: 0,
                entity_reset_revision: 0,
                entity_revision: 0,
            }),
        }
    }

    /// One post-commit wake; exact entity-only writes do not move the broad cursor.
    pub fn notify_store_commit(&self, change: ResourceChange) {
        match change {
            ResourceChange::All => self.changes.notify_one(),
            ResourceChange::Entities => {
                let Ok(mut state) = self.publications.lock() else {
                    self.changes.notify_one();
                    return;
                };
                self.changes.try_publish(1, false, |cursor| {
                    state.entity_revision = cursor.revision;
                });
            }
        }
    }

    /// Publish an authenticated, schema-valid external writer's post-commit hint.
    /// No I/O or durable mutation occurs here; a lost ACK can safely be replayed.
    pub fn publish_once(&self, request: &Value) -> Result<bool, ResourceNotifyError> {
        validate(request)?;
        let request_id = request["requestId"]
            .as_str()
            .ok_or(ResourceNotifyError::Invalid)?;
        let resources = request["resources"]
            .as_array()
            .ok_or(ResourceNotifyError::Invalid)?;
        // serde_json maps are sorted; invalidation is a set operation.
        let resource_keys = resources
            .iter()
            .map(Value::to_string)
            .collect::<BTreeSet<_>>();
        let signature: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&resource_keys).map_err(|_| ResourceNotifyError::Invalid)?,
        )
        .into();
        let mut state = self
            .publications
            .lock()
            .map_err(|_| ResourceNotifyError::Unavailable)?;
        let replay = state.receipts.iter().position(|(id, _)| id == request_id);
        if replay.is_some_and(|index| state.receipts[index].1 != signature) {
            return Err(ResourceNotifyError::Conflict);
        }
        let has_state = resources.iter().any(|value| value["kind"] == "state");
        let has_others = resources.iter().any(|value| value["kind"] != "state");
        let steps = if replay.is_some() {
            0
        } else {
            u32::from(has_state) + u32::from(has_others)
        };
        // Close cannot interleave between the decision, publication and receipt.
        let published = self.changes.try_publish(steps, false, |cursor| {
            if let Some(index) = replay {
                state.receipts.remove(index);
            } else {
                for resource in resources {
                    if resource["kind"] == "state" {
                        state.entity_reset_revision = cursor.revision;
                        continue;
                    }
                    let key = resource.to_string();
                    state.versions.retain(|(existing, _)| *existing != key);
                    if state.versions.len() == MAX_RESOURCE_VERSIONS
                        && let Some((_, revision)) = state.versions.pop_front()
                    {
                        state.pruned_revision = state.pruned_revision.max(revision);
                    }
                    state
                        .versions
                        .push_back((key, cursor.revision - i64::from(has_state)));
                }
            }
            if state.receipts.len() == MAX_NOTIFY_RECEIPTS {
                state.receipts.pop_front();
            }
            state.receipts.push_back((request_id.to_owned(), signature));
        });
        if !published {
            return Err(ResourceNotifyError::Unavailable);
        }
        Ok(replay.is_none())
    }

    /// Read versions and their cursor under the publication guard. Retain only bounded metadata.
    pub fn changes_since<T: ResourceIdentity>(
        &self,
        resources: &[T],
        after: Option<i64>,
    ) -> Result<ResourceChanges<T>, ResourceNotifyError> {
        let state = self
            .publications
            .lock()
            .map_err(|_| ResourceNotifyError::Unavailable)?;
        let cursor = self.changes.snapshot();
        let overflow = after.is_some_and(|revision| revision < state.pruned_revision);
        let mut selected = Vec::new();
        let mut versions = Vec::new();
        for resource in resources {
            let key = resource.notification_key()?;
            let kind = resource.notification_kind()?;
            let explicit = if matches!(kind, ResourceKind::State) {
                state.entity_reset_revision
            } else {
                state.resource_revision(&key)
            };
            let version = match kind {
                // Discovery reads refresh volatile access timestamps; only its
                // explicit publications may advance this semantic cursor.
                ResourceKind::MultiServer => explicit,
                ResourceKind::AgentDetail
                | ResourceKind::Costs
                | ResourceKind::Desktop
                | ResourceKind::Monitors
                | ResourceKind::SessionCost
                | ResourceKind::Terminal
                | ResourceKind::Terminals
                | ResourceKind::State
                | ResourceKind::Transcript
                | ResourceKind::Transcripts
                | ResourceKind::Room
                | ResourceKind::Processes
                | ResourceKind::Complaint
                | ResourceKind::Work
                | ResourceKind::TerminalOutput
                | ResourceKind::Task
                | ResourceKind::Tasks
                | ResourceKind::Workspace => explicit
                    .max(cursor.resource_revision)
                    .max(state.entity_revision),
                ResourceKind::Other => explicit.max(cursor.resource_revision),
            };
            if after.is_none_or(|revision| version > revision) || overflow {
                selected.push(resource.clone());
                versions.push(if version == 0 || overflow {
                    cursor.revision
                } else {
                    version
                });
            }
        }
        Ok(ResourceChanges {
            cursor,
            resources: selected,
            versions,
            entity_reset: after.is_some_and(|revision| state.entity_reset_revision > revision),
            overflow,
        })
    }

    /// Entity candidates go straight to scoped cursor delivery. Compare only
    /// transient resources against their served content.
    /// Uncomputable reads retain the original safe refresh. The bounded
    /// cache shares computations across streams; no publication does projection work.
    pub async fn changes_since_with_content<T, F, Fut>(
        &self,
        resources: &[T],
        after: Option<i64>,
        mut read: F,
    ) -> Result<ResourceChanges<T>, ResourceNotifyError>
    where
        T: ResourceIdentity,
        F: FnMut(T) -> Fut,
        Fut: std::future::Future<Output = Option<Vec<u8>>>,
    {
        let mut view = self.changes_since(resources, after)?;
        if view.cursor.closed {
            return Ok(view);
        }
        let mut selected = Vec::new();
        let mut versions = Vec::new();
        for (resource, version) in view.resources.iter().zip(&view.versions) {
            let key = resource.notification_key()?;
            if resource.notification_kind()?.is_entity() {
                selected.push(resource.clone());
                versions.push(*version);
                continue;
            }
            let cached = {
                let content = self.content.lock().await;
                let entry = content.iter().find(|entry| entry.key == key);
                entry
                    .filter(|entry| entry.checked_revision == *version)
                    .map(|entry| entry.revision)
            };
            let revision = if let Some(revision) = cached {
                revision
            } else {
                let digest = read(resource.clone())
                    .await
                    .map(|bytes| Sha256::digest(&bytes).into());
                if self.changes.snapshot().closed {
                    view.cursor.closed = true;
                    return Ok(view);
                }
                let mut content = self.content.lock().await;
                let existing = content.iter().position(|entry| entry.key == key);
                // A later sampled publication can finish first. Never overwrite it
                // with an older read; keep this view's sampled revision conservative.
                if existing.is_some_and(|index| content[index].checked_revision > *version) {
                    if after.is_none_or(|after| *version > after) {
                        selected.push(resource.clone());
                        versions.push(*version);
                    }
                    continue;
                }
                let revision = existing
                    .filter(|index| digest.is_some() && content[*index].digest == digest)
                    .map_or(*version, |index| content[index].revision);
                if let Some(index) = existing {
                    content.remove(index);
                }
                if content.len() == MAX_RESOURCE_VERSIONS {
                    content.pop_front();
                }
                content.push_back(ContentVersion {
                    key,
                    digest,
                    // Cover only the sampled candidate, so a publication during
                    // the read is checked again even when this digest is equal.
                    checked_revision: *version,
                    revision,
                });
                revision
            };
            if after.is_none_or(|after| revision > after) {
                selected.push(resource.clone());
                versions.push(revision);
            }
        }
        view.resources = selected;
        view.versions = versions;
        Ok(view)
    }

    /// Latest scoped external publication; exact receipt retries do not advance it.
    pub fn published_revision(&self, resource: &Value) -> Result<i64, ResourceNotifyError> {
        self.publications
            .lock()
            .map(|state| state.resource_revision(&resource.to_string()))
            .map_err(|_| ResourceNotifyError::Unavailable)
    }

    /// External state notifications require a reset even if their entity sequence is unchanged.
    pub fn entity_reset_after(&self, revision: i64) -> Result<bool, ResourceNotifyError> {
        self.publications
            .lock()
            .map(|state| state.entity_reset_revision > revision)
            .map_err(|_| ResourceNotifyError::Unavailable)
    }
}

fn validate(value: &Value) -> Result<(), ResourceNotifyError> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR.get_or_init(|| {
        let registry = studio_runtime_schema::registry_for_document(crate::sync_views::openapi_document())?;
        jsonschema::options().with_registry(&registry).build(&json!({
            "$ref": format!("{}#/components/schemas/ResourceNotifyRequest", studio_runtime_schema::OPENAPI_URI)
        })).map_err(|error| error.to_string())
    }).as_ref().map_err(|_| ResourceNotifyError::Unavailable)?;
    if value.to_string().len() > MAX_PUBLICATION_BYTES || !validator.is_valid(value) {
        return Err(ResourceNotifyError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[tokio::test]
    async fn unrelated_content_reads_do_not_wait_for_a_projection() {
        let notifications = Arc::new(ResourceNotifications::new(Arc::new(
            ChangeNotificationHub::new(),
        )));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let first = {
            let notifications = notifications.clone();
            tokio::spawn(async move {
                let mut started_tx = Some(started_tx);
                let mut release_rx = Some(release_rx);
                notifications
                    .changes_since_with_content(&[json!({"kind":"accounts"})], None, |_| {
                        let started_tx = started_tx.take().expect("one read");
                        let release_rx = release_rx.take().expect("one read");
                        async move {
                            started_tx.send(()).expect("test receiver");
                            release_rx.await.expect("test release");
                            Some(vec![1])
                        }
                    })
                    .await
                    .expect("first projection")
            })
        };
        started_rx.await.expect("first projection started");
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            notifications.changes_since_with_content(
                &[json!({"kind":"drafts"})],
                None,
                |_| async { Some(vec![2]) },
            ),
        )
        .await;
        release_tx.send(()).expect("release first projection");
        first.await.expect("first task");
        assert!(
            second.is_ok(),
            "unrelated projection was blocked by the cache guard"
        );
    }

    #[tokio::test]
    async fn older_projection_cannot_overwrite_a_newer_revision() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = Arc::new(ResourceNotifications::new(hub.clone()));
        let resources = [json!({"kind":"accounts"})];
        notifications
            .changes_since_with_content(&resources, None, |_| async { Some(vec![0]) })
            .await
            .expect("initial");
        hub.notify_one();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let old = {
            let notifications = notifications.clone();
            let resources = resources.clone();
            tokio::spawn(async move {
                let mut started = Some(started_tx);
                let mut release = Some(release_rx);
                notifications
                    .changes_since_with_content(&resources, Some(0), |_| {
                        let started = started.take().expect("one read");
                        let release = release.take().expect("one read");
                        async move {
                            started.send(()).expect("start");
                            release.await.expect("release");
                            Some(vec![1])
                        }
                    })
                    .await
                    .expect("older read")
            })
        };
        started_rx.await.expect("older read started");
        hub.notify_one();
        let newer = notifications
            .changes_since_with_content(&resources, Some(0), |_| async { Some(vec![2]) })
            .await
            .expect("newer read");
        assert_eq!(newer.versions, vec![2]);
        release_tx.send(()).expect("release older read");
        old.await.expect("older task");
        let shared = notifications
            .changes_since_with_content(&resources, Some(0), |_| async {
                panic!("newer entry must survive")
            })
            .await
            .expect("shared read");
        assert_eq!(shared.versions, vec![2]);
    }

    #[tokio::test]
    async fn entity_candidates_bypass_content_reads_for_seq_cursor_delivery() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = ResourceNotifications::new(hub.clone());
        let resources = [
            json!({"kind":"state"}),
            json!({"kind":"tasks","agentId":"lead"}),
        ];
        let baseline = notifications
            .changes_since_with_content(&resources, None, |_| async {
                panic!("entity candidates must go straight to cursor delivery")
            })
            .await
            .expect("baseline candidates");
        assert_eq!(baseline.resources, resources);
        hub.notify_one();
        let changes = notifications
            .changes_since_with_content(&resources, Some(baseline.cursor.revision), |_| async {
                panic!("notification must not reread selected content")
            })
            .await
            .expect("changed candidates");
        assert_eq!(changes.resources, resources);
        let document = crate::sync_views::openapi_document();
        let mapping = document["components"]["schemas"]["ResourceRef"]["discriminator"]["mapping"]
            .as_object()
            .expect("resource owner");
        for kind in mapping.keys() {
            let role = ResourceKind::deserialize(serde::de::value::StrDeserializer::<
                serde::de::value::Error,
            >::new(kind))
            .expect("notification role");
            assert_eq!(
                role.is_entity(),
                studio_runtime_schema::sync_protocol::subscription_entity_scopes()
                    .contains_key(kind),
                "OpenAPI owns entity scopes: {kind}"
            );
        }
    }

    #[tokio::test]
    async fn subscribed_content_is_computed_once_per_coalesced_revision() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = ResourceNotifications::new(hub.clone());
        let resources = vec![
            json!({"kind":"accounts"}),
            json!({"kind":"drafts"}),
            json!({"kind":"limits","accountKey":"default"}),
            json!({"kind":"voice","agentId":"chat"}),
        ];
        let calls = AtomicUsize::new(0);
        let read = |resource: Value| {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Some(serde_json::to_vec(&resource).expect("content")))
        };
        let initial = notifications
            .changes_since_with_content(&resources, None, read)
            .await
            .expect("initial");
        assert_eq!(initial.resources, resources);
        assert_eq!(calls.load(Ordering::Relaxed), resources.len());
        for _ in 0..42 {
            hub.notify_one();
        }
        let changed = notifications
            .changes_since_with_content(&resources, Some(0), read)
            .await
            .expect("coalesced wake");
        assert!(
            changed.resources.is_empty(),
            "unchanged content causes no client re-reads"
        );
        assert_eq!(calls.load(Ordering::Relaxed), resources.len() * 2);
        let second = notifications
            .changes_since_with_content(&resources, Some(0), read)
            .await
            .expect("second stream");
        assert!(second.resources.is_empty());
        assert_eq!(
            calls.load(Ordering::Relaxed),
            resources.len() * 2,
            "subscribers share the projection"
        );
        notifications
            .publish_once(&json!({"requestId":"changed-account","resources":[{"kind":"accounts"}]}))
            .expect("publish");
        let changed = notifications
            .changes_since_with_content(&resources, Some(42), |resource| {
                std::future::ready(Some(if resource == resources[0] {
                    b"changed".to_vec()
                } else {
                    serde_json::to_vec(&resource).expect("content")
                }))
            })
            .await
            .expect("changed content");
        assert_eq!(changed.resources, vec![resources[0].clone()]);
        assert_eq!(changed.versions, vec![43]);
    }

    #[tokio::test]
    async fn token_publications_do_not_repeat_or_invalidate_content_projections() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = ResourceNotifications::new(hub.clone());
        let resources = [json!({"kind":"accounts"})];
        notifications
            .changes_since_with_content(&resources, None, |_| async { Some(vec![0]) })
            .await
            .expect("initial");
        hub.notify_one();
        let view = notifications
            .changes_since_with_content(&resources, Some(0), |_| {
                hub.try_publish(1, false, |_| {});
                std::future::ready(Some(vec![0]))
            })
            .await
            .expect("token raced with unchanged projection");
        assert!(
            view.resources.is_empty(),
            "tokens do not change resource content"
        );
        let second = notifications
            .changes_since_with_content(&resources, Some(0), |_| async {
                panic!("token-only revision changes must reuse the computed resource version")
            })
            .await
            .expect("second subscriber");
        assert!(second.resources.is_empty());
    }

    #[tokio::test]
    async fn equal_racing_content_keeps_version_and_checks_the_pending_change() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = ResourceNotifications::new(hub.clone());
        let resources = [json!({"kind":"accounts"})];
        let initial = notifications
            .changes_since_with_content(&resources, None, |_| async { Some(vec![0]) })
            .await
            .expect("initial");
        hub.notify_one();
        let raced = notifications
            .changes_since_with_content(&resources, Some(initial.cursor.revision), |_| {
                // The projection completed before the raced commit changes its content.
                hub.notify_one();
                std::future::ready(Some(vec![0]))
            })
            .await
            .expect("equal raced projection");
        assert!(
            raced.resources.is_empty(),
            "equal content must keep its version"
        );
        assert_eq!(raced.cursor.revision, 1, "the raced commit remains pending");
        let changed = notifications
            .changes_since_with_content(&resources, Some(raced.cursor.revision), |_| async {
                Some(vec![1])
            })
            .await
            .expect("raced commit now has changed content");
        assert_eq!(changed.resources, resources);
        assert_eq!(changed.versions, vec![2]);
        let shared = notifications
            .changes_since_with_content(&resources, Some(initial.cursor.revision), |_| async {
                panic!("the second subscriber must share the checked digest")
            })
            .await
            .expect("second subscriber");
        assert_eq!(shared.versions, changed.versions);
    }

    #[tokio::test]
    async fn unavailable_and_racing_content_keep_the_safe_refresh() {
        let hub = Arc::new(ChangeNotificationHub::new());
        let notifications = ResourceNotifications::new(hub.clone());
        let resources = [json!({"kind":"accounts"})];
        notifications
            .changes_since_with_content(&resources, None, |_| async { Some(vec![0]) })
            .await
            .expect("initial");
        hub.notify_one();
        let unavailable = notifications
            .changes_since_with_content(&resources, Some(0), |_| async { None })
            .await
            .expect("unavailable");
        assert_eq!(unavailable.resources, resources);
        let shared = notifications
            .changes_since_with_content(&resources, Some(0), |_| async {
                panic!("do not retry a failed projection in the same revision")
            })
            .await
            .expect("shared unavailable");
        assert_eq!(shared.resources, resources);
        hub.notify_one();
        let racing = notifications
            .changes_since_with_content(&resources, Some(1), |_| {
                hub.notify_one();
                std::future::ready(Some(vec![0]))
            })
            .await
            .expect("racing");
        assert_eq!(racing.resources, resources);
        let stable = notifications
            .changes_since_with_content(&resources, Some(1), |_| async { Some(vec![0]) })
            .await
            .expect("stable");
        assert_eq!(
            stable.resources, resources,
            "unavailable read retired the old digest"
        );
    }

    #[test]
    fn resource_version_metadata_is_bounded_and_eviction_requires_refresh() {
        let changes = Arc::new(ChangeNotificationHub::new());
        changes.notify_one();
        let notifications = ResourceNotifications::new(changes);
        // Fill the bounded metadata directly: this checks eviction without a publication load run.
        notifications
            .publications
            .lock()
            .expect("metadata")
            .versions
            .extend((0..MAX_RESOURCE_VERSIONS).map(|index| {
                (
                    json!({"kind":"queue","agentId":index.to_string()}).to_string(),
                    1,
                )
            }));
        notifications
            .publish_once(&json!({"requestId":"boundary","resources":[{"kind":"accounts"}]}))
            .expect("publish");
        assert_eq!(
            notifications
                .publications
                .lock()
                .expect("metadata")
                .versions
                .len(),
            MAX_RESOURCE_VERSIONS
        );
        let view = notifications
            .changes_since(&[json!({"kind":"queue","agentId":"0"})], Some(0))
            .expect("lagged reader");
        assert!(view.overflow);
        assert_eq!(view.resources.len(), 1);
        assert_eq!(view.versions, vec![2]);
    }

    #[test]
    fn multi_server_publication_is_typed_targeted_and_replay_safe() {
        let changes = Arc::new(ChangeNotificationHub::new());
        let notifications = ResourceNotifications::new(changes.clone());
        let multi_server = json!({"kind": "multi-server"});

        // Ordinary store commits advance the broad cursor but must not invalidate
        // this resource, or a discovery read could trigger another discovery read.
        changes.notify_one();
        let broad_commit = notifications
            .changes_since(std::slice::from_ref(&multi_server), Some(0))
            .expect("multi-server revision after unrelated commit");
        assert!(broad_commit.resources.is_empty());

        let request = json!({
            "requestId": "multi-server-change",
            "resources": [multi_server.clone()],
        });
        assert!(notifications.publish_once(&request).expect("publish"));

        let state = json!({"kind": "state"});
        let targeted = notifications
            .changes_since(std::slice::from_ref(&multi_server), Some(0))
            .expect("multi-server revision");
        assert_eq!(targeted.resources, vec![multi_server.clone()]);
        assert_eq!(targeted.versions.len(), 1);

        let unrelated = notifications
            .changes_since(&[state], Some(broad_commit.cursor.revision))
            .expect("unrelated state revision");
        assert!(unrelated.resources.is_empty());

        assert!(!notifications.publish_once(&request).expect("exact replay"));
        let replay = notifications
            .changes_since(std::slice::from_ref(&multi_server), Some(0))
            .expect("replay revision");
        assert_eq!(replay.versions, targeted.versions);
    }

    #[test]
    fn close_before_publication_gate_rejects_without_receipt() {
        let changes = Arc::new(ChangeNotificationHub::new());
        let notifications = Arc::new(ResourceNotifications::new(changes.clone()));
        // Hold the publication gate while the request is admitted, then let close win.
        let guard = notifications.publications.lock().expect("gate");
        let (entered, admission) = mpsc::channel();
        let publisher = {
            let notifications = notifications.clone();
            std::thread::spawn(move || {
                entered.send(()).expect("admitted");
                notifications
                    .publish_once(&json!({"requestId":"race","resources":[{"kind":"accounts"}]}))
            })
        };
        admission.recv().expect("request admitted");
        changes.close();
        drop(guard);
        assert_eq!(
            publisher.join().expect("publisher"),
            Err(ResourceNotifyError::Unavailable)
        );
        let state = notifications.publications.lock().expect("state");
        assert!(state.receipts.is_empty());
        assert!(state.versions.is_empty());
        assert_eq!(changes.snapshot().revision, 0);
    }
}

/// Source-owned process notifications for projections derived outside Canvas.
/// Values are never carried here: login URLs and credentials stay in their owners.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundedSource {
    Desktop,
    Costs,
}

fn bounded_source_sender() -> &'static tokio::sync::broadcast::Sender<BoundedSource> {
    static SENDER: OnceLock<tokio::sync::broadcast::Sender<BoundedSource>> = OnceLock::new();
    SENDER.get_or_init(|| tokio::sync::broadcast::channel(MAX_NOTIFY_RECEIPTS).0)
}

pub fn bounded_source_changes() -> tokio::sync::broadcast::Receiver<BoundedSource> {
    bounded_source_sender().subscribe()
}

pub fn publish_bounded_source(source: BoundedSource) {
    // No HTTP owner is required during startup or isolated service tests.
    if bounded_source_sender().receiver_count() > 0 {
        let _receivers = bounded_source_sender().send(source);
    }
}
