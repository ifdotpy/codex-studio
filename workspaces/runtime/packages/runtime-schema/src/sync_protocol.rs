//! Sync protocol identity. This file is the owner; `openapi.json`
//! (`SyncProtocolResponse`) is checked against it and the web copy at the end
//! of `api.ts` is rendered by `studio-contracts`.

/// The single supported sync protocol version.
pub const SYNC_PROTOCOL_VERSION: u32 = 3;
/// `SYNC_PROTOCOL_VERSION` as text, for headers and query values.
pub const SYNC_PROTOCOL_VERSION_TEXT: &str = "3";
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
}
