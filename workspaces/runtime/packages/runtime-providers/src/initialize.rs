//! Single owner of the provider `initialize` request parameters.

use serde_json::{Value, json};

/// Client names Studio reports to a provider peer in `initialize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientName {
    /// The retained per-account (or per-VM-agent) provider session.
    AccountSession,
    /// An ephemeral model-catalog read.
    Catalog,
    /// An ephemeral catalog-reads or command-cancel session.
    CatalogReads,
    /// The native runtime update probe.
    UpdateProbe,
}

impl ClientName {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AccountSession => "codex_agents_canvas",
            Self::Catalog => "codex_studio_catalog",
            Self::CatalogReads => "codex_studio_catalog_reads",
            Self::UpdateProbe => "codex_studio_update",
        }
    }
}

/// Version string Studio reports in `clientInfo`.
const CLIENT_VERSION: &str = "1.0.0";

/// `initialize` params for `client`; the Claude bridge accepts and ignores them.
#[must_use]
pub fn initialize_params(client: ClientName) -> Value {
    json!({
        "clientInfo": {"name": client.name(), "version": CLIENT_VERSION},
        "capabilities": {"experimentalApi": true}
    })
}
