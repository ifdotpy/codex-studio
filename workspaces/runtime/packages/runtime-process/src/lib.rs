//! Asynchronous provider adapters, process ports and the supervisor v2 client.

pub use studio_runtime_store::blocking_pool;
pub mod child_priority;
mod client;
#[cfg(unix)]
pub mod clock;
pub mod operational_log;
#[cfg(unix)]
mod owned_process_group;
mod ports;
mod protocol;
#[cfg(unix)]
pub mod provider_direct;
mod provider_launch;
pub mod provider_seam;
mod provider_shared;
#[cfg(unix)]
pub mod provider_stdio;
#[cfg(unix)]
pub mod provider_task;
#[cfg(unix)]
mod pty_host;
#[cfg(unix)]
pub mod stream_child;
#[cfg(unix)]
mod supervisor_child;
mod supervisor_contract;
#[cfg(unix)]
pub mod supervisor_daemon;
#[cfg(unix)]
pub(crate) mod supervisor_identity;
mod transport;

pub use client::SupervisorClient;
pub use ports::PtyHost;
pub use protocol::*;
pub use provider_launch::{ProviderHandle, ProviderLaunchSpec};
#[cfg(unix)]
pub use pty_host::{
    LocalPtyHost, PtyHostError, PtyIdentity, PtySession, cleanup_orphan, terminal_environment,
    terminal_environment_from,
};
pub use supervisor_contract::MAX_SUPERVISOR_FRAME_BYTES;
pub use transport::SupervisorTransport;

#[cfg(test)]
mod tests;
