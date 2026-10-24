mod browser_launcher;
mod host_exec_service;
mod install_layout;
mod linux_vm_start;
mod operator_cli;
mod state_check;
mod wave7_p01_registry;
// Register every integration test module here; the tooling witness checks coverage.

#[cfg(unix)]
pub(crate) use wave7_p01_registry::{account_registry, provider_ports};
// Only the runtime_shutdown witnesses import crate::host_runtime.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use wave7_p01_registry::host_runtime;

mod fixture_support;
