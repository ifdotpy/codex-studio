//! Typed replies and retained table requirements for durable transport.
use crate::request_results::{
    Admission, AdmittedRequest, RequestResultError, RequestResults, ResultEnvelope,
};
use crate::schema::{SchemaRequirements, TableRequirement};
use serde::{Deserialize, Serialize};
use studio_runtime_schema::persistence::RequestIdentity;

/// The exact transport reply. Its value is opaque domain data, decoded by its owner.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteReply {
    pub request_id: String,
    pub outcome: studio_runtime_schema::persistence::RemoteOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Box<serde_json::value::RawValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedRemoteReply {
    pub principal: Option<String>,
    pub server: Option<String>,
    pub actor: Option<String>,
    pub body: Option<String>,
    pub response: RemoteReply,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpAccessReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub completed: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessPending {
    pub client: String,
    pub request_id: String,
    pub session: String,
    pub record: Box<serde_json::value::RawValue>,
    pub admission: AdmittedRequest,
}

/// Immutable federation content fingerprint, including the original domain ID.
/// The signed UUID root is transport provenance and does not change old content hashes.
pub fn federation_envelope_signature(envelope: &serde_json::Value) -> crate::StoreResult<String> {
    let mut immutable = envelope
        .as_object()
        .cloned()
        .ok_or(crate::StoreError::RecordContract("federation envelope"))?;
    for key in ["timestamp", "nonce", "signature", "requestId"] {
        immutable.remove(key);
    }
    Ok(crate::sha256_hex(
        crate::python_json_compact(&serde_json::Value::Object(immutable)).as_bytes(),
    ))
}

/// Composite lookup is also used for migrated composite primary keys.
pub fn remote_result_key(namespace: &str, authority: &[&str], root: &str) -> String {
    remote_operation_key(namespace, authority, root, &[])
}

pub fn remote_operation_key(
    namespace: &str,
    authority: &[&str],
    root: &str,
    operation: &[&str],
) -> String {
    if let Ok(identity) = RequestIdentity::new(namespace, authority, root, operation) {
        return identity.id().to_owned();
    }
    serde_json::to_string(&(namespace, authority, root, operation))
        .expect("string tuple serializes")
}

pub fn admit_remote(
    tx: &rusqlite::Transaction<'_>,
    namespace: &str,
    authority: &[&str],
    root: &str,
    signature: &str,
    now_ms: i64,
) -> Result<Admission, RequestResultError> {
    let identity = RequestIdentity::new(namespace, authority, root, &[]).map_err(|_| {
        studio_runtime_schema::persistence::RequestIdentityError::Expired {
            issued_ms: 0,
            expired_before_ms: now_ms,
        }
    })?;
    RequestResults::admit(tx, &identity, signature, now_ms)
}

pub fn replay_remote<R: serde::de::DeserializeOwned>(
    db: &rusqlite::Connection,
    id: &str,
    signature: &str,
    now_ms: i64,
) -> Result<Option<ResultEnvelope<R>>, RequestResultError> {
    RequestResults::replay(db, id, signature, now_ms)?
        .map(|stored| serde_json::from_str(&stored.result).map_err(Into::into))
        .transpose()
}

pub fn settle_remote<R: Serialize>(
    tx: &rusqlite::Transaction<'_>,
    admitted: &AdmittedRequest,
    actor: Option<String>,
    reply: &R,
) -> Result<(), RequestResultError> {
    RequestResults::settle(
        tx,
        admitted,
        &ResultEnvelope {
            actor,
            reply,
            references: Vec::new(),
        },
    )?;
    Ok(())
}

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_access_discovered",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_access_invites",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_access_settings",
        columns: &["id", "auto_pair"],
    },
    TableRequirement {
        name: "runtime_server_inbox",
        columns: &[
            "id",
            "signature",
            "state",
            "result",
            "created_at",
            "completed_at",
            "principal",
            "actor",
            "admission",
        ],
    },
    TableRequirement {
        name: "runtime_server_outbox",
        columns: &[
            "id",
            "server",
            "body",
            "state",
            "result",
            "created_at",
            "completed_at",
            "fingerprint",
            "attempts",
            "admission",
        ],
    },
];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "multi_server_delivery",
        requirements: &REQUIREMENTS,
    }
}
