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

/// A complete, schema-checked snapshot from the token-rate producer.
#[derive(Clone)]
pub struct TokenRates(Arc<Value>);

impl TokenRates {
    pub fn checked(value: Value) -> Result<Self, ResourceNotifyError> {
        validate(false, &value)?;
        Ok(Self(Arc::new(value)))
    }

    pub fn value(&self) -> &Value {
        &self.0
    }
}

struct Receipt {
    request_id: String,
    signature: [u8; 32],
}

struct Publications {
    receipts: VecDeque<Receipt>,
    rates: TokenRates,
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
                rates: TokenRates(Arc::new(json!({"rates": {}, "teams": {}}))),
            }),
        }
    }

    /// Publish an authenticated, schema-valid external writer's post-commit hint.
    /// No I/O or durable mutation occurs here; a lost ACK can safely be replayed.
    pub fn publish_once(&self, request: &Value) -> Result<bool, ResourceNotifyError> {
        validate(true, request)?;
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

    /// Adapter port for B6 token_rate's workspace snapshot, after usage commit.
    /// Equal snapshots do not advance the cursor; slow readers retain one value.
    pub fn publish_token_rates(&self, rates: TokenRates) -> Result<bool, ResourceNotifyError> {
        let mut state = self
            .publications
            .lock()
            .map_err(|_| ResourceNotifyError::Unavailable)?;
        if self.changes.snapshot().closed {
            return Err(ResourceNotifyError::Unavailable);
        }
        if state.rates.value() == rates.value() {
            return Ok(false);
        }
        state.rates = rates;
        self.changes.notify_one();
        Ok(true)
    }

    /// External state notifications require a reset even if their entity sequence is unchanged.
    pub fn entity_reset_after(&self, revision: i64) -> Result<bool, ResourceNotifyError> {
        self.publications
            .lock()
            .map(|state| state.entity_reset_revision > revision)
            .map_err(|_| ResourceNotifyError::Unavailable)
    }

    pub fn token_rates(&self) -> Result<TokenRates, ResourceNotifyError> {
        self.publications
            .lock()
            .map(|state| state.rates.clone())
            .map_err(|_| ResourceNotifyError::Unavailable)
    }
}

fn validate(notify: bool, value: &Value) -> Result<(), ResourceNotifyError> {
    static VALIDATORS: OnceLock<Result<(jsonschema::Validator, jsonschema::Validator), String>> =
        OnceLock::new();
    let validators = VALIDATORS
        .get_or_init(|| {
            let document = crate::sync_views::openapi_document();
            let registry = studio_runtime_schema::registry_for_document(document)?;
            let compile = |name: &str| {
                jsonschema::options().with_registry(&registry).build(&json!({
                "$ref": format!("{}#/components/schemas/{name}", studio_runtime_schema::OPENAPI_URI)
            })).map_err(|error| error.to_string())
            };
            let mut rate_schema =
                document["components"]["schemas"]["ResourceTokenRatesEvent"].clone();
            let properties = rate_schema["properties"]
                .as_object_mut()
                .ok_or("missing token-rate properties")?;
            properties.retain(|name, _| matches!(name.as_str(), "rates" | "teams"));
            rate_schema["required"] = json!(["rates", "teams"]);
            rate_schema["components"] = document["components"].clone();
            let rates = jsonschema::options()
                .build(&rate_schema)
                .map_err(|error| error.to_string())?;
            Ok((compile("ResourceNotifyRequest")?, rates))
        })
        .as_ref()
        .map_err(|_| ResourceNotifyError::Unavailable)?;
    let validator = if notify { &validators.0 } else { &validators.1 };
    if value.to_string().len() > MAX_PUBLICATION_BYTES || !validator.is_valid(value) {
        return Err(ResourceNotifyError::Invalid);
    }
    Ok(())
}
