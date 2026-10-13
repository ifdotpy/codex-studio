//! Bounded process-local resource invalidations. Call publishers only after commit.
//!
//! Notify identity is scoped to this workspace and process, as in Python's relay.
//! Exact retries refresh the 256-entry LRU; conflicting retries never publish.
//! Restart or eviction permits another invalidation (never another store write).

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

struct Publications {
    receipts: VecDeque<(String, [u8; 32])>,
    versions: VecDeque<(String, i64)>,
    pruned_revision: i64,
    entity_reset_revision: i64,
}

/// A consistent resource view, with only the requested resources that changed.
pub struct ResourceChanges {
    pub cursor: ChangeSnapshot,
    pub resources: Vec<Value>,
    pub versions: Vec<i64>,
    pub entity_reset: bool,
    pub overflow: bool,
}

/// One instance per HTTP state's workspace; it shares the existing commit cursor.
pub struct ResourceNotifications {
    changes: Arc<ChangeNotificationHub>,
    publications: Mutex<Publications>,
}

impl ResourceNotifications {
    pub fn new(changes: Arc<ChangeNotificationHub>) -> Self {
        Self {
            changes,
            publications: Mutex::new(Publications {
                receipts: VecDeque::new(),
                versions: VecDeque::new(),
                pruned_revision: 0,
                entity_reset_revision: 0,
            }),
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
    pub fn changes_since(
        &self,
        resources: &[Value],
        after: Option<i64>,
    ) -> Result<ResourceChanges, ResourceNotifyError> {
        let state = self
            .publications
            .lock()
            .map_err(|_| ResourceNotifyError::Unavailable)?;
        let cursor = self.changes.snapshot();
        let overflow = after.is_some_and(|revision| revision < state.pruned_revision);
        let mut selected = Vec::new();
        let mut versions = Vec::new();
        for resource in resources {
            let key = resource.to_string();
            let explicit = if resource["kind"] == "state" {
                state.entity_reset_revision
            } else {
                state
                    .versions
                    .iter()
                    .find(|(existing, _)| *existing == key)
                    .map_or(0, |(_, revision)| *revision)
            };
            let version = explicit.max(cursor.resource_revision);
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
                    json!({"kind":"panel","agentId":index.to_string()}).to_string(),
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
            .changes_since(&[json!({"kind":"panel","agentId":"0"})], Some(0))
            .expect("lagged reader");
        assert!(view.overflow);
        assert_eq!(view.resources.len(), 1);
        assert_eq!(view.versions, vec![2]);
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
