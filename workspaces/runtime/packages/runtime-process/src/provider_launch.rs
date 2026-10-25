//! Provider identity and pre-resolved launch data shared by asynchronous adapters.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

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
    /// Earlier commands of this launch. A running supervised child started
    /// with one of them is reattached with its own command, so a changed
    /// launch command reaches new children without refusing running ones.
    pub reattach_commands: Vec<Vec<String>>,
    /// Launch values that a running child of this provider may hold in
    /// another form. Such a child reattaches with its own command and
    /// environment.
    pub retained: RetainedLaunchValues,
}

/// Launch values that differ between launches of the same code because they
/// come from state that changes while a supervised child runs. A running
/// child launched with another such value is the same provider.
#[derive(Clone, Default)]
pub struct RetainedLaunchValues {
    /// Earlier values of launch variables whose value the owner learns over
    /// time, by variable.
    pub earlier: BTreeMap<String, Vec<String>>,
    /// Variables that hold the path of an executable which the system can
    /// replace while the child runs, such as a CLI that updates itself. The
    /// child may hold another absolute path.
    pub executable_variables: BTreeSet<String>,
    /// Whether the program, `command[0]`, is such an executable, such as a
    /// runtime that a package manager upgrades. The child may run another
    /// absolute path with the same arguments.
    pub executable_program: bool,
}

impl ProviderLaunchSpec {
    /// Launch with the supervisor's inherited working directory.
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
            reattach_commands: Vec::new(),
            retained: RetainedLaunchValues::default(),
        }
    }
}
