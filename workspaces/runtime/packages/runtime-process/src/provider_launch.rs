//! Provider identity and pre-resolved launch data shared by asynchronous adapters.
use std::{collections::BTreeMap, path::PathBuf};

/// Stable identity for a supervisor-owned child.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderHandle(String);

impl ProviderHandle {
    #[must_use]
    pub fn for_account(account_key: &str) -> Self {
        Self(format!("account:{account_key}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Child launch values resolved by the owning server from account/runtime state.
/// This intentionally has no `Debug` implementation because it contains secrets.
#[derive(Clone)]
pub struct ProviderLaunchSpec {
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    /// Recovered native-to-session request IDs installed before replay starts.
    pub retained_response_ids: BTreeMap<u64, u64>,
    /// Cursor already handled by runtime state, used to make an uncertain ACK idempotent.
    pub handled_cursor: u64,
}

impl ProviderLaunchSpec {
    /// Preserve Python supervisor launches, whose working directory is inherited.
    #[must_use]
    pub fn with_inherited_cwd(mut self) -> Self {
        self.cwd = None;
        self
    }

    #[must_use]
    pub fn new(command: Vec<String>, environment: BTreeMap<String, String>, cwd: PathBuf) -> Self {
        Self {
            command,
            environment,
            cwd: Some(cwd),
            retained_response_ids: BTreeMap::new(),
            handled_cursor: 0,
        }
    }
}
