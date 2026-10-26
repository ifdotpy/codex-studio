mod analytics_state_check;
mod cross_runtime;
mod fresh_state;
mod local_server_id;
mod schema_compat;
mod serde_float;
mod sql_callback_guard;
// Register every integration test module here; the tooling witness checks coverage.

#[path = "../../runtime-services/tests/support/prepared_python.rs"]
pub(crate) mod prepared_python_support;
