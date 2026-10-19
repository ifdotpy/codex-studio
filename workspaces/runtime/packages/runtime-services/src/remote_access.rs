//! The Tailscale Serve origin that this server accepts, with the rules of
//! `codex_remote.py:RemoteAccess`.
//!
//! An operator override in `PUBLIC_ORIGIN_ENVIRONMENT` always wins. Without
//! it, `REMOTE_ACCESS_FILE` in the state directory enables Serve access and
//! names its exact origin. The file is read again for each call, so an owner
//! can disable access without a restart.

use crate::multi_server_protocol_generated::{PUBLIC_ORIGIN_ENVIRONMENT, REMOTE_ACCESS_FILE};
use crate::remote_transport::validate_federation_origin;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("The public origin override must be an HTTPS origin ending in .ts.net, without a path")]
pub struct InvalidPublicOrigin;

#[derive(Debug, Clone)]
pub struct RemoteAccess {
    path: PathBuf,
    override_origin: Option<String>,
}

impl RemoteAccess {
    /// `configured` is the operator override. An empty value is no override.
    pub fn new(state_root: &Path, configured: Option<&str>) -> Result<Self, InvalidPublicOrigin> {
        let override_origin = configured
            .filter(|value| !value.is_empty())
            .map(|value| validate_federation_origin(value).map_err(|_| InvalidPublicOrigin))
            .transpose()?;
        Ok(Self {
            path: state_root.join(REMOTE_ACCESS_FILE),
            override_origin,
        })
    }

    pub fn from_environment(state_root: &Path) -> Result<Self, InvalidPublicOrigin> {
        Self::new(
            state_root,
            std::env::var(PUBLIC_ORIGIN_ENVIRONMENT).ok().as_deref(),
        )
    }

    /// The accepted origin now, or `None` when Serve access is off or the
    /// file is missing or invalid.
    pub fn origin(&self) -> Option<String> {
        if let Some(origin) = &self.override_origin {
            return Some(origin.clone());
        }
        let value: Value = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        if value.get("enabled") != Some(&Value::Bool(true)) {
            return None;
        }
        validate_federation_origin(value.get("origin")?.as_str()?).ok()
    }

    /// The startup origin while the current configuration still names it.
    /// A changed or disabled configuration refuses Serve access until the
    /// server restarts with the new origin.
    pub fn still_allows(&self, startup_origin: Option<&str>) -> Option<String> {
        let startup_origin = startup_origin?;
        self.origin().filter(|current| current == startup_origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://studio.example.ts.net";

    fn write(root: &Path, value: &str) {
        std::fs::write(root.join(REMOTE_ACCESS_FILE), value).expect("write remote access fixture");
    }

    #[test]
    fn the_state_file_enables_its_exact_origin() {
        let root = tempfile::tempdir().expect("state root");
        let access = RemoteAccess::new(root.path(), None).expect("no override");
        assert_eq!(access.origin(), None, "a missing file disables access");
        write(
            root.path(),
            r#"{"enabled":true,"origin":"https://studio.example.ts.net/"}"#,
        );
        assert_eq!(access.origin().as_deref(), Some(ORIGIN));
        write(
            root.path(),
            r#"{"enabled":true,"origin":"https://studio.example.ts.net:443"}"#,
        );
        assert_eq!(access.origin().as_deref(), Some(ORIGIN));
        write(
            root.path(),
            r#"{"enabled":true,"origin":"https://studio.example.ts.net:8443"}"#,
        );
        assert_eq!(
            access.origin().as_deref(),
            Some("https://studio.example.ts.net:8443")
        );
    }

    #[test]
    fn a_disabled_invalid_or_malformed_file_refuses_access() {
        let root = tempfile::tempdir().expect("state root");
        let access = RemoteAccess::new(root.path(), Some("")).expect("empty override is none");
        for value in [
            r#"{"enabled":false,"origin":"https://studio.example.ts.net"}"#,
            r#"{"enabled":"true","origin":"https://studio.example.ts.net"}"#,
            r#"{"origin":"https://studio.example.ts.net"}"#,
            r#"{"enabled":true}"#,
            r#"{"enabled":true,"origin":"https://evil.test"}"#,
            r#"{"enabled":true,"origin":"http://studio.example.ts.net"}"#,
            r#"{"enabled":true,"origin":"https://studio.example.ts.net/path"}"#,
            r#"{"enabled":true,"origin":"https://user@studio.example.ts.net"}"#,
            r#"{"enabled":true,"origin":"https://studio.example.ts.net?x=1"}"#,
            r#"["enabled"]"#,
            "not json",
        ] {
            write(root.path(), value);
            assert_eq!(access.origin(), None, "{value}");
        }
    }

    #[test]
    fn the_operator_override_wins_and_must_be_valid() {
        let root = tempfile::tempdir().expect("state root");
        write(root.path(), r#"{"enabled":false}"#);
        let access =
            RemoteAccess::new(root.path(), Some("https://other.example.ts.net/")).expect("valid");
        assert_eq!(
            access.origin().as_deref(),
            Some("https://other.example.ts.net")
        );
        assert_eq!(
            RemoteAccess::new(root.path(), Some("https://evil.test")).unwrap_err(),
            InvalidPublicOrigin
        );
    }

    #[test]
    fn a_disabled_or_changed_file_revokes_the_startup_origin_without_a_restart() {
        let root = tempfile::tempdir().expect("state root");
        write(
            root.path(),
            r#"{"enabled":true,"origin":"https://studio.example.ts.net"}"#,
        );
        let access = RemoteAccess::new(root.path(), None).expect("no override");
        let startup = access.origin();
        assert_eq!(
            access.still_allows(startup.as_deref()).as_deref(),
            Some(ORIGIN)
        );
        write(root.path(), r#"{"enabled":false}"#);
        assert_eq!(access.still_allows(startup.as_deref()), None);
        write(
            root.path(),
            r#"{"enabled":true,"origin":"https://other.example.ts.net"}"#,
        );
        assert_eq!(access.still_allows(startup.as_deref()), None);
        write(
            root.path(),
            r#"{"enabled":true,"origin":"https://studio.example.ts.net"}"#,
        );
        assert_eq!(
            access.still_allows(startup.as_deref()).as_deref(),
            Some(ORIGIN)
        );
        assert_eq!(
            access.still_allows(None),
            None,
            "access enabled after startup needs a restart"
        );
    }
}
