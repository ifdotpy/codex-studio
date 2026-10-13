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

use crate::change_notifications::ChangeNotificationHub;

pub const MAX_NOTIFY_RECEIPTS: usize = 256;
pub const MAX_PUBLICATION_BYTES: usize = 262_144;

#[derive(Debug, Error, PartialEq)]
pub enum ResourceNotifyError {
    #[error("Invalid resource publication")]
    Invalid,
    #[error("Request identity already has different resources")]
    Conflict,
    #[error("Resource invalidation could not be published")]
    Unavailable,
}

struct Receipt {
    request_id: String,
    signature: [u8; 32],
}

struct Publications {
    receipts: VecDeque<Receipt>,
    entity_reset_revision: i64,
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
        let resources = resources
            .iter()
            .map(Value::to_string)
            .collect::<BTreeSet<_>>();
        let signature: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&resources).map_err(|_| ResourceNotifyError::Invalid)?,
        )
        .into();
        let mut state = self
            .publications
            .lock()
            .map_err(|_| ResourceNotifyError::Unavailable)?;
        if self.changes.snapshot().closed {
            return Err(ResourceNotifyError::Unavailable);
        }
        if let Some(index) = state
            .receipts
            .iter()
            .position(|receipt| receipt.request_id == request_id)
        {
            if state.receipts[index].signature != signature {
                return Err(ResourceNotifyError::Conflict);
            }
            if let Some(receipt) = state.receipts.remove(index) {
                state.receipts.push_back(receipt);
            }
            return Ok(false);
        }
        // Match the relay's separate entity reset and other-resource publication.
        let is_state = |resource: &Value| resource["kind"] == "state";
        let values = request["resources"]
            .as_array()
            .ok_or(ResourceNotifyError::Invalid)?;
        if values.iter().any(|value| !is_state(value)) {
            self.changes.notify_one();
        }
        if values.iter().any(is_state) {
            self.changes.notify_one();
            state.entity_reset_revision = self.changes.snapshot().revision;
        }
        if state.receipts.len() == MAX_NOTIFY_RECEIPTS {
            state.receipts.pop_front();
        }
        state.receipts.push_back(Receipt {
            request_id: request_id.to_owned(),
            signature,
        });
        Ok(true)
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
