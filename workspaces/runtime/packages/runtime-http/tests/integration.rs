mod claude_login;
mod multi_server_incoming;
mod orchestration_settings;
mod process_controls;
mod project_preparation;
mod recovery_http;
mod remote_access_origin;
mod voice_provider;
mod wave7_p04_callback_http;
mod wave7_p10_lifecycle_http;
mod wave7_p15_http;
mod wave7_p18_account_session;
// Register every integration test module here; the tooling witness checks coverage.

// The shared callback fixture prepares files before any HTTP handler runs; the
// HTTP-only blocking filesystem ban does not apply to that test setup.
#[allow(clippy::disallowed_methods)]
#[path = "../../runtime-services/tests/wave7_p04_callback/fixture.rs"]
pub(crate) mod callback_fixtures;

#[path = "../../runtime-services/tests/claude_runtime/login_fixture.rs"]
pub(crate) mod login_fixtures;

#[path = "../../runtime-services/tests/monitors/fixtures.rs"]
pub(crate) mod monitor_fixtures;

#[path = "../../runtime-services/tests/wave7_p15_support/fixtures.rs"]
pub(crate) mod monitor_runtime_fixtures;

#[path = "../../runtime-services/tests/wave7_p15_support/mod.rs"]
pub(crate) mod monitor_runtime_support;

#[path = "../../runtime-services/tests/multi_server/common.rs"]
pub(crate) mod multi_server_fixtures;
