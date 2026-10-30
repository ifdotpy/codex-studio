//! Multi-server access protocol identity. This file is the owner; the web copy
//! `multiServerProtocol.ts` is rendered by `studio-contracts`.

/// Version of the signed multi-server access protocol.
pub const ACCESS_PROTOCOL_VERSION: u64 = 1;
/// Domain separator prefixed to every signed multi-server message.
pub const MULTI_SERVER_SIGNING_DOMAIN: &str = "studio-multi-server-v1";
/// File name of the remote access record in the state directory.
pub const REMOTE_ACCESS_FILE: &str = "remote-access.json";
/// Environment variable that overrides the public origin of this server.
pub const PUBLIC_ORIGIN_ENVIRONMENT: &str = "CODEX_CANVAS_PUBLIC_ORIGIN";

/// Fields of a schema-validated discovery response. Validate the complete
/// DiscoveryIdentity schema before deserializing this view.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryIdentity {
    pub server_id: String,
    pub label: String,
    pub origin: Option<String>,
    pub public_key: String,
    pub tailscale_user: Option<String>,
    pub auto_pair: bool,
}

/// A validated stored invitation. Extra fields are preserved when it is used.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAccessInvite {
    invite_id: String,
    status: InviteStatus,
    expires: serde_json::Number,
    token_hash: String,
    tailscale_user: String,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum InviteStatus {
    Open,
    Used,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InviteWire {
    invite_id: String,
    status: InviteStatus,
    expires: serde_json::Number,
    token_hash: String,
    tailscale_user: String,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl PersistedAccessInvite {
    /// Construct a domain invitation only after checking the authoritative schema.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, String> {
        match crate::first_error("PersistedAccessInvite", value) {
            Ok(None) => {}
            Ok(Some((location, _))) => return Err(location),
            Err(_) => return Err("schema".to_owned()),
        }
        let wire = <InviteWire as serde::Deserialize>::deserialize(value)
            .map_err(|_| "schema".to_owned())?;
        Ok(Self {
            invite_id: wire.invite_id,
            status: wire.status,
            expires: wire.expires,
            token_hash: wire.token_hash,
            tailscale_user: wire.tailscale_user,
            extra: wire.extra,
        })
    }
    pub fn invite_id(&self) -> &str {
        &self.invite_id
    }
    pub fn is_open_at(&self, now: f64) -> bool {
        matches!(self.status, InviteStatus::Open) && self.expires.as_f64().is_some_and(|v| v >= now)
    }
    pub fn token_hash(&self) -> &str {
        &self.token_hash
    }
    pub fn tailscale_user(&self) -> &str {
        &self.tailscale_user
    }
    pub fn mark_used(&mut self) {
        self.status = InviteStatus::Used;
    }
}

/// The signed transport mutation root is distinct from the deterministic message ID.
pub(crate) fn refresh_openapi(openapi: &mut serde_json::Value) {
    for name in ["FederationAck", "FederationAckResult"] {
        let ack = &mut openapi["components"]["schemas"][name];
        ack["properties"]["requestId"] = serde_json::json!({"type":"string","format":"uuid"});
        if let Some(required) = ack["required"].as_array_mut()
            && !required.iter().any(|field| field == "requestId")
        {
            required.push(serde_json::json!("requestId"));
        }
    }
    let schema = &mut openapi["components"]["schemas"]["SignedEnvelope"];
    schema["properties"]["requestId"] = serde_json::json!({"type":"string","format":"uuid","description":"UUIDv7 mutation root, issued once and retained across delivery retries."});
    if let Some(required) = schema["required"].as_array_mut()
        && !required.iter().any(|field| field == "requestId")
    {
        required.push(serde_json::json!("requestId"));
    }
    let operation = &mut openapi["components"]["schemas"]["ServerOperationRequest"];
    operation["properties"]["operation"] = serde_json::json!({"type":"array","items":{"type":"string"},"description":"Scoped suboperation tuple; empty for a single-step request."});
    if let Some(required) = operation["required"].as_array_mut()
        && !required.iter().any(|field| field == "operation")
    {
        required.push(serde_json::json!("operation"));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    #[test]
    fn signing_domain_names_the_protocol_version() {
        assert_eq!(
            super::MULTI_SERVER_SIGNING_DOMAIN,
            format!("studio-multi-server-v{}", super::ACCESS_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn discovery_view_tracks_schema_fields_and_valid_formats() {
        let document: Value = serde_json::from_slice(crate::OPENAPI).unwrap();
        let schema = &document["components"]["schemas"]["DiscoveryIdentity"];
        let expected = [
            "autoPair",
            "label",
            "origin",
            "protocol",
            "publicKey",
            "serverId",
            "tailscaleUser",
        ];
        let fields = schema["properties"].as_object().unwrap();
        assert_eq!(
            fields.keys().map(String::as_str).collect::<Vec<_>>(),
            expected
        );
        let mut constraints = Value::Object(fields.clone());
        for field in constraints.as_object_mut().unwrap().values_mut() {
            field.as_object_mut().unwrap().remove("title");
        }
        assert_eq!(
            constraints,
            json!({
                "serverId":{"type":"string"}, "label":{"type":"string"},
                "origin":{"anyOf":[{"type":"null"},{"type":"string"}]},
                "publicKey":{"type":"string"},
                "tailscaleUser":{"anyOf":[{"type":"null"},{"type":"string"}]},
                "protocol":{"type":"integer","const":1,"minimum":1,"maximum":1},
                "autoPair":{"type":"boolean"}
            })
        );
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), expected.len());
        assert!(expected.iter().all(|name| required.contains(&json!(name))));
        assert_eq!(schema["additionalProperties"], false);
        let valid = json!({"serverId":"s", "label":"l", "origin":null, "publicKey":"k", "tailscaleUser":null, "protocol":1, "autoPair":true});
        for (field, replacement) in [
            ("serverId", json!(null)),
            ("label", json!(0)),
            ("origin", json!(false)),
            ("publicKey", json!([])),
            ("tailscaleUser", json!({})),
            ("protocol", json!(2)),
            ("autoPair", json!(1)),
        ] {
            let mut bad = valid.clone();
            bad[field] = replacement;
            assert!(
                !crate::validate("DiscoveryIdentity", &bad).unwrap(),
                "{field}"
            );
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                !crate::validate("DiscoveryIdentity", &missing).unwrap(),
                "missing {field}"
            );
        }
        for protocol in [json!(1), json!(1.0)] {
            let mut input = valid.clone();
            input["protocol"] = protocol;
            assert!(crate::validate("DiscoveryIdentity", &input).unwrap());
            assert!(serde_json::from_value::<super::DiscoveryIdentity>(input).is_ok());
        }
        assert!(!crate::validate("DiscoveryIdentity", &json!([])).unwrap());
        let mut extra = valid;
        extra["unexpected"] = json!(true);
        assert!(!crate::validate("DiscoveryIdentity", &extra).unwrap());
    }

    #[test]
    fn invite_view_tracks_schema_and_preserves_extensions() {
        let schema: Value =
            serde_json::from_str(include_str!("schemas/persisted-access-invite.schema.json"))
                .unwrap();
        let mut properties = schema["properties"].clone();
        for field in properties.as_object_mut().unwrap().values_mut() {
            field.as_object_mut().unwrap().remove("title");
        }
        assert_eq!(
            properties,
            json!({
                "inviteId":{"type":"string","minLength":1,"maxLength":128},
                "status":{"type":"string","enum":["open","used"]},
                "expires":{"type":"number","minimum":0},
                "tokenHash":{"type":"string","pattern":"^[0-9a-f]{64}$"},
                "tailscaleUser":{"type":"string","minLength":1,"maxLength":254}
            })
        );
        assert_eq!(
            schema["required"],
            json!([
                "inviteId",
                "status",
                "expires",
                "tokenHash",
                "tailscaleUser"
            ])
        );
        assert_eq!(
            schema["additionalProperties"],
            json!({"$ref":"#/$defs/JsonValue"})
        );
        let valid = json!({"inviteId":"i", "status":"open", "expires":9007199254740993_u64, "tokenHash":"a".repeat(64), "tailscaleUser":"owner", "extension":{"opaque":[null,1]}});
        assert!(crate::validate("PersistedAccessInvite", &valid).unwrap());
        let mut invite = super::PersistedAccessInvite::from_value(&valid).unwrap();
        assert_eq!(serde_json::to_value(&invite).unwrap(), valid);
        invite.mark_used();
        let mut expected = valid;
        expected["status"] = json!("used");
        assert_eq!(serde_json::to_value(invite).unwrap(), expected);
        for (field, invalid) in [
            ("status", json!("unknown")),
            ("expires", json!(-1)),
            ("inviteId", json!("")),
            ("tokenHash", json!("bad")),
            ("tailscaleUser", json!("")),
        ] {
            let mut input = expected.clone();
            input[field] = invalid;
            assert!(
                super::PersistedAccessInvite::from_value(&input).is_err(),
                "{field}"
            );
        }
    }
}
