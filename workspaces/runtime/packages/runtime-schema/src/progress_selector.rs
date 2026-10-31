//! Identifies transient command progress on the live stream.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgressSelector {
    Auth {
        request_id: String,
    },
    Vault {
        request_id: String,
        job: VaultJob,
    },
    Rename {
        request_id: String,
        agent_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VaultJob {
    Setup,
    Login,
    Pull,
}
