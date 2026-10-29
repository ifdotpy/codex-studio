//! Discovery consumes injected status/probe observations. It never invokes
//! Tailscale, probes a host, or restores a revoked credential.
use futures_util::future::BoxFuture;
use std::collections::BTreeSet;
use std::sync::Arc;

use rusqlite::params;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};
use studio_runtime_schema::queue_command::Field as PresentField;
use studio_runtime_store::RuntimeStore;

use crate::multi_server_auth::{
    AccessError, MAX_OBSERVATION_BYTES, discovery_identity, origin_host, refused, stored, text,
};
use crate::multi_server_pairing::read_record;

pub const MAX_CANDIDATES: usize = 64;
pub const DISCOVERY_PORTS: [Option<u16>; 2] = [None, Some(8443)];
const MAX_DISCOVERED: i64 = 256;

/// Partial stored peer evidence used by readers that historically tolerate
/// missing and malformed fields. Other properties survive status updates.
#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredPeer {
    #[serde(default, skip_serializing_if = "PresentField::is_missing")]
    origin: PresentField<Value>,
    #[serde(default, skip_serializing_if = "PresentField::is_missing")]
    public_key: PresentField<Value>,
    #[serde(default, skip_serializing_if = "PresentField::is_missing")]
    status: PresentField<Value>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl StoredPeer {
    /// Scalars have no fields, as in the historical JSON readers.
    pub fn from_value(value: Value) -> Option<Self> {
        studio_runtime_schema::json_object::JsonObject::<Self>::deserialize(value)
            .ok()
            .map(|object| object.0)
    }

    pub fn origin(&self) -> &str {
        self.origin
            .value()
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    pub fn public_key(&self) -> &str {
        self.public_key
            .value()
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    pub fn is_paired(&self) -> bool {
        self.status.value().and_then(Value::as_str) == Some("paired")
    }

    pub fn is_revoked(&self) -> bool {
        self.status.value().and_then(Value::as_str) == Some("revoked")
    }

    /// Retains explicit null and arbitrary historical creation evidence.
    pub fn created(&self) -> Option<&Value> {
        self.extra.get("created")
    }

    pub fn mark_unreachable(&mut self) {
        self.status = PresentField::Value(Value::String("unreachable".into()));
    }

    pub fn set_label(&mut self, label: String) {
        self.extra.insert("label".into(), Value::String(label));
    }
}

// Tailscale observations can contain malformed individual peer fields. Those
// fields have always been treated as absent, while malformed tags fail closed.
#[derive(Deserialize)]
#[serde(untagged)]
enum Field<T> {
    Valid(T),
    Other(IgnoredAny),
}
impl<T> Field<T> {
    fn value(&self) -> Option<&T> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Other(_) => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Tags {
    Null(()),
    List(Vec<IgnoredAny>),
    Other(IgnoredAny),
}
impl Tags {
    fn tagged(&self) -> bool {
        match self {
            Self::Null(()) => false,
            Self::List(tags) => !tags.is_empty(),
            Self::Other(_) => true,
        }
    }
}

#[derive(Deserialize)]
struct DiscoveryNode {
    #[serde(rename = "UserID")]
    user: Option<Field<u64>>,
    #[serde(rename = "DNSName")]
    dns: Option<Field<String>>,
    #[serde(rename = "Online")]
    online: Option<Field<bool>>,
    #[serde(rename = "Tags")]
    tags: Option<Tags>,
}
impl DiscoveryNode {
    fn from_object(value: &Value) -> Option<Self> {
        value.as_object()?;
        Self::deserialize(value).ok()
    }
    fn tagged(&self) -> bool {
        self.tags.as_ref().is_some_and(Tags::tagged)
    }
}

/// Host-owned discovery adapter. The HTTP and service layers pass the
/// persisted server identity and public Serve origin; tests inject a fake
/// adapter and never consult the machine's Tailscale installation.
pub trait ServerDiscoveryPort: Send + Sync {
    fn discover<'a>(
        &'a self,
        store: Arc<RuntimeStore>,
        identity: &'a Value,
        public_origin: &'a str,
    ) -> BoxFuture<'a, Result<Value, AccessError>>;
}

#[derive(Clone)]
pub struct DiscoverySnapshot {
    owner: String,
    local_id: String,
    candidates: BTreeSet<String>,
}
impl DiscoverySnapshot {
    /// Checked constructor from a Tailscale status observation. The owner login
    /// and candidates come from the same observation, never caller headers.
    pub fn from_status(
        status: &Value,
        local_id: &str,
        local_origin: &str,
    ) -> Result<Self, AccessError> {
        origin_host(local_origin)?;
        let invalid = || {
            refused(
                503,
                "identity_unavailable",
                "The Tailscale peers are unavailable",
            )
        };
        if status.to_string().len() > MAX_OBSERVATION_BYTES {
            return Err(invalid());
        }
        let node = status
            .get("Self")
            .and_then(DiscoveryNode::from_object)
            .ok_or_else(invalid)?;
        let peers = status
            .get("Peer")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        let user = node
            .user
            .as_ref()
            .and_then(Field::value)
            .copied()
            .filter(|v| *v > 0)
            .ok_or_else(invalid)?;
        if text(status, "BackendState") != "Running" || node.tagged() {
            return Err(invalid());
        }
        let owner = status
            .get("User")
            .and_then(|v| v.get(user.to_string()))
            .map(|v| text(v, "LoginName"))
            .filter(|v| !v.trim().is_empty() && v.len() <= 254)
            .ok_or_else(invalid)?
            .trim()
            .to_lowercase();
        let mut candidates = BTreeSet::new();
        for (is_local, peer) in std::iter::once((true, Some(node))).chain(
            peers
                .values()
                .map(|v| (false, DiscoveryNode::from_object(v))),
        ) {
            let Some(peer) = peer else { continue };
            if (!is_local && peer.online.as_ref().and_then(Field::value) != Some(&true))
                || peer.tagged()
                || peer.user.as_ref().and_then(Field::value) != Some(&user)
            {
                continue;
            }
            let hostname = peer
                .dns
                .as_ref()
                .and_then(Field::value)
                .map_or("", String::as_str)
                .trim_end_matches('.')
                .to_lowercase();
            for port in DISCOVERY_PORTS {
                let authority =
                    port.map_or_else(|| hostname.clone(), |port| format!("{hostname}:{port}"));
                let origin = format!("https://{authority}");
                if origin != local_origin && origin_host(&origin).is_ok() {
                    candidates.insert(origin);
                }
            }
        }
        Ok(Self {
            owner,
            local_id: local_id.to_owned(),
            candidates,
        })
    }
    pub fn candidates(&self) -> Vec<&str> {
        self.candidates
            .iter()
            .take(MAX_CANDIDATES)
            .map(String::as_str)
            .collect()
    }
    pub fn has_candidate(&self, origin: &str) -> bool {
        self.candidates.contains(origin)
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
}
pub struct DiscoveryDecision {
    record: Option<Value>,
    auto_pair: bool,
}
impl DiscoveryDecision {
    pub fn record(&self) -> Option<&Value> {
        self.record.as_ref()
    }
    pub fn auto_pair(&self) -> bool {
        self.auto_pair
    }
}

/// A signed status refresh must already have been applied to `peer` by the
/// observer for existing paired servers; unsigned probes cannot rename them.
pub fn decide_probe(
    snapshot: &DiscoverySnapshot,
    origin: &str,
    peer: &Value,
    previous: Option<&str>,
    client: Option<&str>,
    auto_pair: bool,
    now: i64,
) -> Result<DiscoveryDecision, AccessError> {
    let identity = discovery_identity(peer)?;
    if !snapshot.candidates.contains(origin)
        || identity.origin.as_deref().unwrap_or_default() != origin
    {
        return Err(refused(
            403,
            "identity_mismatch",
            "The peer identity is invalid",
        ));
    }
    let id = identity.server_id.as_str();
    let previous = previous
        .map(|r| stored("PersistedAccessClient", "runtime_access_discovered", id, r))
        .transpose()?;
    let client = client
        .map(|r| stored("PersistedAccessClient", "runtime_access_clients", id, r))
        .transpose()?;
    if id == snapshot.local_id
        || identity
            .tailscale_user
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            != snapshot.owner
        || client.as_ref().is_some_and(|v| {
            text(v, "publicKey") != identity.public_key || text(v, "origin") != origin
        })
    {
        return Ok(DiscoveryDecision {
            record: None,
            auto_pair: false,
        });
    }
    let label = client
        .as_ref()
        .filter(|v| text(v, "status") == "paired")
        .and_then(|v| v.get("label"))
        .cloned()
        .unwrap_or_else(|| json!(identity.label));
    let created = previous
        .as_ref()
        .and_then(|v| v.get("created"))
        .cloned()
        .unwrap_or(json!(now));
    let record = json!({"id":id,"clientId":id,"serverId":id,"kind":"server","label":label,"origin":origin,"publicKey":identity.public_key,"tailscaleUser":snapshot.owner,"status":"discovered","created":created,"lastAccess":null,"revoked":null,"lastSeen":now,"autoPair":identity.auto_pair});
    Ok(DiscoveryDecision {
        record: Some(record),
        auto_pair: client.is_none() && auto_pair && identity.auto_pair,
    })
}

/// Apply one bounded observation. Auto pairing is returned as a proposal for
/// the host driver, never performed by an unsigned discovery observation.
pub async fn record_probe(
    store: &RuntimeStore,
    snapshot: &DiscoverySnapshot,
    origin: &str,
    peer: &Value,
    now: i64,
) -> Result<DiscoveryDecision, AccessError> {
    let snapshot = snapshot.clone();
    let origin = origin.to_owned();
    let peer = peer.clone();
    store.transaction_with(move |tx| {
        let id=text(&peer,"serverId");
        let previous=read_record(tx,"runtime_access_discovered",id)?;
        let client=read_record(tx,"runtime_access_clients",id)?;
        let enabled:bool=tx.query_row("SELECT auto_pair FROM runtime_access_settings WHERE id=1",[],|r|r.get(0))?;
        let decision=decide_probe(&snapshot,&origin,&peer,previous.as_deref(),client.as_deref(),enabled,now)?;
        if let Some(record)=&decision.record {
            tx.execute("INSERT INTO runtime_access_discovered(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,record.to_string()])?;
            tx.execute("DELETE FROM runtime_access_discovered WHERE id IN (SELECT id FROM runtime_access_discovered ORDER BY json_extract(record,'$.lastSeen') DESC,id LIMIT -1 OFFSET ?1)",[MAX_DISCOVERED])?;
        }
        Ok(decision)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_peer_projection_preserves_partial_and_opaque_fields() {
        for raw in ["null", "[]", "7", r#""text""#] {
            assert!(StoredPeer::from_value(serde_json::from_str(raw).unwrap()).is_none());
        }
        let malformed =
            StoredPeer::from_value(serde_json::from_str(r#"{"status":{"paired":null}}"#).unwrap())
                .unwrap();
        assert!(!malformed.is_paired());
        let raw = r#"{"origin":false,"publicKey":["opaque"],"status":{"paired":null},"unknown":{"v":3},"status":"paired"}"#;
        let mut peer = StoredPeer::from_value(serde_json::from_str(raw).unwrap()).unwrap();
        assert_eq!(peer.origin(), "");
        assert_eq!(peer.public_key(), "");
        assert!(peer.is_paired());
        peer.mark_unreachable();
        let saved = serde_json::to_string(&peer).unwrap();
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["status"], "unreachable");
        assert_eq!(saved["publicKey"], json!(["opaque"]));
        assert_eq!(saved["unknown"]["v"], 3);
    }

    #[test]
    fn discovery_tolerates_bad_peer_fields_and_rejects_tags() {
        let status = json!({
            "BackendState": "Running",
            "Self": {"UserID": 1, "DNSName": "local.test.ts.net."},
            "User": {"1": {"LoginName": "owner@example.test"}},
            "Peer": {
                "valid": {"UserID": 1, "DNSName": "peer.test.ts.net.", "Online": true, "Tags": []},
                "bad-online": {"UserID": 1, "DNSName": "bad.test.ts.net", "Online": "true"},
                "bad-tags": {"UserID": 1, "DNSName": "tagged.test.ts.net", "Online": true, "Tags": {}},
                "array": [1, "array.test.ts.net", true]
            }
        });
        let snapshot =
            DiscoverySnapshot::from_status(&status, "local", "https://local.test.ts.net").unwrap();
        assert_eq!(
            snapshot.candidates(),
            vec![
                "https://local.test.ts.net:8443",
                "https://peer.test.ts.net",
                "https://peer.test.ts.net:8443"
            ]
        );
        let mut invalid = status;
        invalid["Self"] = json!([1, "local.test.ts.net"]);
        assert!(
            DiscoverySnapshot::from_status(&invalid, "local", "https://local.test.ts.net").is_err()
        );
    }
}
