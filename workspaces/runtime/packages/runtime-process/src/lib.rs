//! Synchronous process ports and the supervisor v1 client.

mod client;
mod ports;
mod protocol;
pub mod provider_process;
mod provider_shared;
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
