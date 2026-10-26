// Register every integration test module here; the tooling witness checks coverage.
mod child_priority;
mod provider_stdio;
mod provider_task;
mod supervisor_receipt_loss;
mod supervisor_v1;
mod wave7_p01_factory;

#[cfg(unix)]
#[path = "support/mod.rs"]
pub(crate) mod process_support;
