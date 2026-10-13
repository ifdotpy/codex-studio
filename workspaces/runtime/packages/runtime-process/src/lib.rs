//! Synchronous process ports and the supervisor v1 client.

pub mod child_priority;
mod client;
mod ports;
mod protocol;
#[cfg(unix)]
pub mod stream_child;
mod transport;

pub use client::SupervisorClient;
pub use ports::{ProgressWatch, PtyHost};
pub use protocol::*;
pub use transport::SupervisorTransport;

#[cfg(test)]
mod tests;
