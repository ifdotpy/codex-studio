//! Asynchronous provider adapters, process ports and the supervisor v1 client.

mod client;
mod ports;
mod protocol;
#[cfg(unix)]
mod pty_host;
mod provider_launch;
pub mod provider_seam;
mod provider_shared;
#[cfg(unix)]
pub mod provider_stdio;
#[cfg(unix)]
pub mod provider_task;
#[cfg(unix)]
pub mod stream_child;
#[cfg(unix)]
mod supervisor_child;
#[cfg(unix)]
pub mod supervisor_daemon;
#[cfg(unix)]
mod supervisor_identity;
mod transport;

pub use client::SupervisorClient;
pub use ports::{ProgressWatch, PtyHost};
pub use protocol::*;
#[cfg(unix)]
pub use pty_host::{LocalPtyHost, PtyHostError, PtyIdentity, PtySession, cleanup_orphan};
pub use provider_launch::{ProviderHandle, ProviderLaunchSpec};
pub use pty_host::{
    LocalPtyHost, PtyHostError, PtyIdentity, PtySession, cleanup_orphan, terminal_environment,
    terminal_environment_from,
};
pub use transport::SupervisorTransport;

#[cfg(test)]
mod tests;
