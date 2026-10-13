//! Synchronous process ports and the supervisor v1 client.

mod client;
mod ports;
mod protocol;
pub mod provider_process;
pub mod provider_seam;
mod provider_shared;
#[cfg(unix)]
pub mod provider_stdio;
#[cfg(unix)]
pub mod provider_task;
mod transport;

pub use client::SupervisorClient;
pub use ports::{ProgressWatch, PtyHost};
pub use protocol::*;
pub use provider_process::{
    ProviderFactoryError, ProviderHandle, ProviderLaunchResolver, ProviderLaunchSpec,
    ProviderProcessFactory, SupervisorProviderProcess, SupervisorProviderProcessFactory,
};
pub use transport::SupervisorTransport;

#[cfg(test)]
mod tests;
