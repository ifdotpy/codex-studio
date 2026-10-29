//! Checks the approved server snapshot and invitation before native pairing.
use serde::Deserialize;
use serde_json::{Value, json};

pub struct Approval {
    pub local_server_id: String,
    pub server_id: String,
    pub invite_request_id: String,
    generation: Value,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalWire {
    #[serde(default, deserialize_with = "optional_text")]
    local_server_id: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    server_id: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    invite_request_id: Option<String>,
    #[serde(default)]
    generation: Value,
}
impl Approval {
    pub fn parse(params: &Value) -> Result<Self, String> {
        let wire: ApprovalWire = object(&params["approval"]);
        fn checked(value: Option<String>, name: &str) -> Result<String, String> {
            let value = value.map_or(Value::Null, Value::String);
            let id = crate::server_keys::field_value(&value, name)?;
            if id.len() > 128
                || !id
                    .bytes()
                    .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-'))
            {
                return Err("Invalid approved server identity.".into());
            }
            Ok(id.to_owned())
        }
        Ok(Self {
            local_server_id: checked(wire.local_server_id, "localServerId")?,
            server_id: checked(wire.server_id, "serverId")?,
            invite_request_id: checked(wire.invite_request_id, "inviteRequestId")?,
            generation: wire.generation,
        })
    }
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerIdentity {
    #[serde(default, deserialize_with = "optional_text")]
    server_id: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Peer {
    #[serde(default, deserialize_with = "optional_text")]
    server_id: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    status: Option<String>,
    #[serde(default)]
    origin: Value,
    #[serde(default)]
    created: Value,
    #[serde(default)]
    public_key: Value,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Invitation {
    #[serde(default)]
    protocol: Value,
    #[serde(default)]
    invite_id: Value,
    #[serde(default)]
    token: Value,
    #[serde(default)]
    server_id: Value,
    #[serde(default)]
    origin: Value,
    #[serde(default)]
    public_key: Value,
    #[serde(default)]
    tailscale_user: Value,
    #[serde(default)]
    expires: Value,
}
#[derive(Default, Deserialize)]
struct Snapshot {
    #[serde(default)]
    protocol: Value,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotIdentity {
    #[serde(default, deserialize_with = "optional_text")]
    server_id: Option<String>,
}
fn optional_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(d)?.as_str().map(str::to_owned))
}
fn object<T: for<'de> Deserialize<'de> + Default>(value: &Value) -> T {
    if value.is_object() {
        T::deserialize(value).unwrap_or_default()
    } else {
        T::default()
    }
}
pub struct ApprovedPeer(Peer);
pub fn approved_peer(
    snapshot: &Value,
    params: &Value,
    approval: &Approval,
) -> Result<ApprovedPeer, String> {
    let origin = crate::server_keys::origin(crate::server_keys::field(params, "origin")?)?;
    let snapshot_fields: Snapshot = object(snapshot);
    let identity: SnapshotIdentity = object(&snapshot["identity"]);
    let peer = snapshot["servers"]
        .as_array()
        .and_then(|rows| {
            rows.iter().find(|v| {
                object::<PeerIdentity>(v).server_id.as_deref() == Some(approval.server_id.as_str())
            })
        })
        .ok_or("The approved server is no longer available.")?;
    let peer: Peer = object(peer);
    let generation = json!([peer.created, peer.public_key]).to_string();
    if snapshot_fields.protocol != 1
        || identity.server_id.as_deref() != Some(approval.local_server_id.as_str())
        || peer.kind.as_deref() != Some("server")
        || peer.status.as_deref() != Some("paired")
        || crate::server_keys::origin(crate::server_keys::field_value(&peer.origin, "origin")?)?
            != origin
        || approval.generation != generation
    {
        return Err("The server approval or its identity changed.".into());
    }
    Ok(ApprovedPeer(peer))
}
pub fn check_invitation(
    response: &Value,
    requested: &Value,
    peer: &ApprovedPeer,
) -> Result<(), String> {
    let a: Invitation = object(&response["invitation"]);
    let b: Invitation = object(&requested["invitation"]);
    if a.server_id.as_str() != peer.0.server_id.as_deref()
        || a.public_key != peer.0.public_key
        || a.protocol != b.protocol
        || a.invite_id != b.invite_id
        || a.token != b.token
        || a.server_id != b.server_id
        || a.origin != b.origin
        || a.public_key != b.public_key
        || a.tailscale_user != b.tailscale_user
        || a.expires != b.expires
    {
        return Err("The invitation does not match the approved server.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_snapshot_baseline_preserves_first_match_and_generation() {
        let params = json!({"origin":"https://node.ts.net", "approval":{"localServerId":"local","serverId":"remote","inviteRequestId":"invite", "generation":"[null,\"key\"]"},"invitation":{"serverId":"remote","publicKey":"key"}});
        let peer = json!({"serverId":"remote","kind":"server","status":"paired","origin":"https://node.ts.net","publicKey":"key"});
        let snapshot = json!({"protocol":1,"identity":{"serverId":"local"},"servers":[peer, {"serverId":"remote","kind":"wrong"}]});
        let approval = Approval::parse(&params).unwrap();
        let first = approved_peer(&snapshot, &params, &approval).unwrap();
        assert!(
            check_invitation(&json!({"invitation":params["invitation"]}), &params, &first).is_ok()
        );
        let mut changed = snapshot.clone();
        changed["servers"][0]["created"] = json!(1);
        assert!(approved_peer(&changed, &params, &approval).is_err());
        changed["servers"][0]["serverId"] = json!("other");
        assert!(approved_peer(&changed, &params, &approval).is_err());
    }

    #[test]
    fn approvals_keep_validation_order_and_compare_opaque_invitation_fields() {
        let mut params = json!({"origin":"https://node.ts.net", "approval":{"localServerId":"local","serverId":"remote","inviteRequestId":"request-1", "generation":"[null,\"key\"]"},"invitation":{"serverId":"remote","publicKey":"key","tailscaleUser":{"opaque":[1]},"expires":null}});
        let snapshot = json!({"protocol":1,"identity":{"serverId":"local"},"servers":[{"serverId":"remote","kind":"server","status":"paired","origin":"https://node.ts.net","publicKey":"key"}]});
        let approval = Approval::parse(&params).unwrap();
        let peer = approved_peer(&snapshot, &params, &approval).unwrap();
        let response = json!({"invitation":params["invitation"]});
        assert!(check_invitation(&response, &params, &peer).is_ok());
        params["invitation"]["tailscaleUser"] = json!({"opaque":[2]});
        assert!(check_invitation(&response, &params, &peer).is_err());
        params["approval"]["localServerId"] = json!(null);
        params["approval"]["serverId"] = json!(null);
        assert_eq!(
            Approval::parse(&params).err().unwrap(),
            "Missing or invalid localServerId."
        );
        params["approval"]["localServerId"] = json!("valid");
        assert_eq!(
            Approval::parse(&params).err().unwrap(),
            "Missing or invalid serverId."
        );
        params["approval"]["serverId"] = json!("bad/id");
        params["approval"]["inviteRequestId"] = json!(null);
        assert_eq!(
            Approval::parse(&params).err().unwrap(),
            "Invalid approved server identity."
        );
        params["approval"]["serverId"] = json!("valid");
        assert_eq!(
            Approval::parse(&params).err().unwrap(),
            "Missing or invalid inviteRequestId."
        );
    }

    #[test]
    fn fake_pair_sequence_rechecks_approval_after_invitation() {
        let params = json!({"origin":"https://node.ts.net", "approval":{"localServerId":"local","serverId":"remote","inviteRequestId":"same-request", "generation":"[1,\"key\"]"},"invitation":{"serverId":"remote","publicKey":"key"}});
        let mut snapshot = json!({"protocol":1,"identity":{"serverId":"local"},"servers":[{"serverId":"remote","kind":"server","status":"paired","origin":"https://node.ts.net","created":1,"publicKey":"key"}]});
        let approval = Approval::parse(&params).unwrap();
        let mut calls = vec!["GET"];
        let peer = approved_peer(&snapshot, &params, &approval).unwrap();
        calls.push("POST");
        assert_eq!(approval.invite_request_id, "same-request");
        check_invitation(&json!({"invitation":params["invitation"]}), &params, &peer).unwrap();
        snapshot["servers"][0]["created"] = json!(2);
        calls.push("GET");
        assert!(approved_peer(&snapshot, &params, &approval).is_err());
        assert_eq!(calls, ["GET", "POST", "GET"]);
    }
}
