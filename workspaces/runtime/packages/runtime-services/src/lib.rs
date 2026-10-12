//! Synchronous application services between the HTTP edge and the store,
//! provider and process crates.
//!
//! Each service is one leaf module owned by one slice. A service takes
//! explicit inputs (snapshots, clocks, ports) and returns decisions or
//! results, so its rules are testable in seconds without a database, a
//! provider or the server. Route modules in `studio-runtime-http` call
//! services; services never depend on HTTP types.

pub mod account_driver;
pub mod account_policy;
pub mod rules;
pub mod sync_policy;
