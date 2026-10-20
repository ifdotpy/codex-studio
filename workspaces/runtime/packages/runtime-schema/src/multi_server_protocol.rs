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
