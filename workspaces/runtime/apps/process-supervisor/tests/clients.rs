//! Run the existing Rust client contracts against this crate's built binary in
//! the default fast tier. Each test creates and cleans its own disposable state.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "../../../packages/runtime-process/tests/supervisor_v1.rs"]
mod protocol_v1;
