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
    #[test]
    fn signing_domain_names_the_protocol_version() {
        assert_eq!(
            super::MULTI_SERVER_SIGNING_DOMAIN,
            format!("studio-multi-server-v{}", super::ACCESS_PROTOCOL_VERSION)
        );
    }
}
