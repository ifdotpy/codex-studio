//! Discovery consumes injected status/probe observations. It never invokes
//! Tailscale, probes a host, or restores a revoked credential.
use std::collections::BTreeSet;

use rusqlite::params;
use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

use crate::multi_server_auth::{
    AccessError, MAX_OBSERVATION_BYTES, origin_host, refused, schema, stored, text,
};
use crate::multi_server_pairing::read_record;

pub const MAX_CANDIDATES: usize = 64;
const MAX_DISCOVERED: i64 = 256;

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
            .filter(|v| v.is_object())
            .ok_or_else(invalid)?;
        let peers = status
            .get("Peer")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        let user = node
            .get("UserID")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
            .ok_or_else(invalid)?;
        if text(status, "BackendState") != "Running" || tagged(node) {
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
        for (is_local, peer) in
            std::iter::once((true, node)).chain(peers.values().map(|v| (false, v)))
        {
            if (!is_local && peer.get("Online") != Some(&Value::Bool(true)))
                || tagged(peer)
                || peer.get("UserID").and_then(Value::as_u64) != Some(user)
            {
                continue;
            }
            let hostname = text(peer, "DNSName").trim_end_matches('.').to_lowercase();
            for suffix in ["", ":8443"] {
                let origin = format!("https://{hostname}{suffix}");
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
    pub fn owner(&self) -> &str {
        &self.owner
    }
}
fn tagged(node: &Value) -> bool {
    node.get("Tags")
        .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
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
    schema("DiscoveryIdentity", peer)?;
    if !snapshot.candidates.contains(origin) || text(peer, "origin") != origin {
        return Err(refused(
            403,
            "identity_mismatch",
            "The peer identity is invalid",
        ));
    }
    let id = text(peer, "serverId");
    let previous = previous
        .map(|r| stored("PersistedAccessClient", "runtime_access_discovered", id, r))
        .transpose()?;
    let client = client
        .map(|r| stored("PersistedAccessClient", "runtime_access_clients", id, r))
        .transpose()?;
    if id == snapshot.local_id
        || text(peer, "tailscaleUser").trim().to_lowercase() != snapshot.owner
        || client.as_ref().is_some_and(|v| {
            text(v, "publicKey") != text(peer, "publicKey") || text(v, "origin") != origin
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
        .unwrap_or_else(|| peer["label"].clone());
    let created = previous
        .as_ref()
        .and_then(|v| v.get("created"))
        .cloned()
        .unwrap_or(json!(now));
    let record = json!({"id":id,"clientId":id,"serverId":id,"kind":"server","label":label,"origin":origin,"publicKey":peer["publicKey"],"tailscaleUser":snapshot.owner,"status":"discovered","created":created,"lastAccess":null,"revoked":null,"lastSeen":now,"autoPair":peer["autoPair"]});
    Ok(DiscoveryDecision {
        record: Some(record),
        auto_pair: client.is_none()
            && auto_pair
            && peer.get("autoPair") == Some(&Value::Bool(true)),
    })
}

/// Apply one bounded observation. Auto pairing is returned as a proposal for
/// the host driver, never performed by an unsigned discovery observation.
pub fn record_probe(
    store: &RuntimeStore,
    snapshot: &DiscoverySnapshot,
    origin: &str,
    peer: &Value,
    now: i64,
) -> Result<DiscoveryDecision, AccessError> {
    store.transaction_with(|tx| {
        let id=text(peer,"serverId");
        let previous=read_record(tx,"runtime_access_discovered",id)?;
        let client=read_record(tx,"runtime_access_clients",id)?;
        let enabled:bool=tx.query_row("SELECT auto_pair FROM runtime_access_settings WHERE id=1",[],|r|r.get(0))?;
        let decision=decide_probe(snapshot,origin,peer,previous.as_deref(),client.as_deref(),enabled,now)?;
        if let Some(record)=&decision.record {
            tx.execute("INSERT INTO runtime_access_discovered(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,record.to_string()])?;
            tx.execute("DELETE FROM runtime_access_discovered WHERE id IN (SELECT id FROM runtime_access_discovered ORDER BY json_extract(record,'$.lastSeen') DESC,id LIMIT -1 OFFSET ?1)",[MAX_DISCOVERED])?;
        }
        Ok(decision)
    })
}
