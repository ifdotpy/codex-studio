mod browser_launcher;
mod install_layout;
mod operator_cli;
mod state_check;
mod wave7_p01_registry;
// Register every integration test module here; the tooling witness checks coverage.

#[cfg(unix)]
pub(crate) use wave7_p01_registry::{account_registry, provider_ports};

mod fixture_support;
