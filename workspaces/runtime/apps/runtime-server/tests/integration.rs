mod browser_launcher;
mod install_layout;
mod operator_cli;
mod state_check;
mod wave7_p01_registry;
// Register every integration test module here; the tooling witness checks coverage.

#[cfg(unix)]
pub(crate) use wave7_p01_registry::{account_registry, provider_ports};
// Only the Linux-only runtime_shutdown.rs module reads crate::host_runtime.
#[cfg(target_os = "linux")]
pub(crate) use wave7_p01_registry::host_runtime;

mod fixture_support;
