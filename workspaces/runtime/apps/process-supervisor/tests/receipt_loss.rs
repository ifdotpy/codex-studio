#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "../../../packages/runtime-process/tests/support/mod.rs"]
mod process_support;

#[path = "../../../packages/runtime-process/tests/supervisor_receipt_loss.rs"]
mod receipt_loss;
