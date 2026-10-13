//! Asynchronous provider adapters, process ports and the supervisor v1 client.

pub mod child_priority;
mod client;
mod ports;
mod protocol;
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
pub use provider_launch::{ProviderHandle, ProviderLaunchSpec};
pub use transport::SupervisorTransport;

#[cfg(test)]
mod tests;
