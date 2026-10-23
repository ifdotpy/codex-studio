//! Sync protocol identity. This file is the owner; `openapi.json`
//! (`SyncProtocolResponse`) is checked against it and the web copy at the end
//! of `api.ts` is rendered by `studio-contracts`.

/// The single supported sync protocol version.
pub const SYNC_PROTOCOL_VERSION: u32 = 4;
/// `SYNC_PROTOCOL_VERSION` as text, for headers and query values.
pub const SYNC_PROTOCOL_VERSION_TEXT: &str = "4";
/// Scope name of the sync entity stream.
pub const SYNC_ENTITY_SCOPE: &str = "state:entities:v1";

#[cfg(test)]
mod tests {
    #[test]
    fn version_text_matches_version() {
        assert_eq!(
            super::SYNC_PROTOCOL_VERSION.to_string(),
            super::SYNC_PROTOCOL_VERSION_TEXT
        );
    }
    #[test]
    fn entity_v4_schema_requires_typed_values_and_omits_delete_values() {
        use serde_json::json;
        let valid = json!({"op":"upsert","collection":"workspace","id":"current","seq":1,"value":{"stateDir":"temporary"}});
        assert!(crate::first_error("SyncEntity", &valid).unwrap().is_none());
        let delete = json!({"op":"delete","collection":"agent","id":"a","seq":2});
        assert!(crate::first_error("SyncEntity", &delete).unwrap().is_none());
        for invalid in [
            json!({"op":"upsert","collection":"workspace","id":"current","seq":1,"value":"{}"}),
            json!({"op":"upsert","collection":"workspace","id":"current","seq":1}),
            json!({"op":"upsert","collection":"workspace","id":"current","seq":1,"value":{"stateDir":42}}),
            json!({"op":"delete","collection":"agent","id":"a","seq":2,"value":{}}),
            json!({"op":"delete","collection":"unknown","id":"a","seq":2}),
            json!({"op":"delete","collection":"agent","id":"a","seq":2,"payload":"{}"}),
        ] {
            assert!(
                crate::first_error("SyncEntity", &invalid)
                    .unwrap()
                    .is_some(),
                "{invalid}"
            );
        }
    }
}
