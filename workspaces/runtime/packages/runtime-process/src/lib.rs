//! Synchronous process ports and the supervisor v1 client.

mod client;
mod ports;
mod protocol;
mod transport;

pub use client::SupervisorClient;
pub use ports::{ProgressWatch, PtyHost};
pub use protocol::*;
pub use transport::SupervisorTransport;

#[cfg(test)]
mod tests;
