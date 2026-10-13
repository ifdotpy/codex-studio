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
pub mod agents_create;
pub mod coordination;
pub mod event_write;
pub mod model_catalog;
pub mod panels;
pub mod progress_panel;
pub mod provider_session;
pub mod questions_reads;
pub mod queue_write;
pub mod rules;
pub mod scheduler;
pub mod sync_policy;
pub mod terminal_driver;
pub mod transcript;
pub mod work_policy;
pub mod work_write;
pub mod workspace_files;
pub mod workspace_views;
