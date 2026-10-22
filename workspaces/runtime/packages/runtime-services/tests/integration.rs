// Register every integration test module here; the tooling witness checks coverage.
mod account_login_registry_cleanup;
mod account_login_session_start;
mod account_transfer_corpus;
mod account_transfer_driver;
mod agent_management_corpus;
mod agent_management_store;
mod analytics_capture_corpus;
mod analytics_history_checkpoint;
mod browser_host_flow;
mod budget_corpus;
mod catalog_reads_corpus;
mod change_notifications;
mod claude_accounts;
mod claude_bridge;
mod claude_controls_actor;
mod claude_login;
mod claude_login_actor;
mod claude_usage_limits;
mod codex_login_completion;
mod context_repair_corpus;
mod continuation_corpus;
mod continuation_recovery;
mod image_workspace_corpus;
mod media_auth_fixtures;
mod monitor_flood_and_missed_rules;
mod monitor_recovery;
mod monitor_start_preparation;
mod monitor_wake_guarantees;
mod multi_server_corpus;
mod native_runtime_state_corpus;
mod notification_dispatch_corpus;
mod payload_safety_oracle;
mod portable_history_python_parity;
mod process_controls;
mod project_preparation;
mod provider_binaries_differential;
mod provider_callbacks;
mod provider_session_task;
mod radio_policy_corpus;
mod recovery_corpus;
mod recovery_history;
mod recovery_store;
mod relocation_corpus;
mod rules_engine_corpus;
mod rules_tick_atomic;
mod scheduler_python_parity;
mod session_cost_golden;
mod startup_recovery;
mod startup_recovery_corpus;
mod submit_gates_corpus;
mod supervisor_journal;
mod task_recovery;
mod terminal_corpus;
mod thread_prepare_corpus;
mod title_generation_corpus;
mod title_generation_effects;
mod turn_fixtures;
mod turn_lifecycle_corpus;
mod turn_prepare_corpus;
mod vm_macsync_corpus;
mod voice_provider_corpus;
mod wave7_p03_message_ingress;
mod wave7_p04_callback_runtime;
mod wave7_p04_review;
mod wave7_p07_runtime;
mod wave7_p08_control_replay;
mod wave7_p08_frozen_input;
mod wave7_p08_retained_controls;
mod wave7_p08_submission;
mod wave7_p10_lifecycle;
mod wave7_p11_native;
mod wave7_p12_completion;
mod wave7_p13_transcript;
mod wave7_p14_resources;
mod wave7_p15_runtime;
mod wave7_p16_assignment;
mod wave7_p18_account_session;
mod wave7_p18_registry;
mod wave7_p18_transfer_executor;
mod wave7_p19_radio;
mod wave9_account_session_not_sent;
mod wave9_login_parity;
mod wave9_safety_buffering_executor;
mod workspace_operations_corpus;

#[path = "analytics_capture/common.rs"]
pub(crate) mod analytics_fixtures;

#[path = "wave7_p04_callback/fixture.rs"]
pub(crate) mod callback_fixtures;

#[path = "claude_runtime/login_fixture.rs"]
pub(crate) mod login_fixtures;

#[path = "monitors/fixtures.rs"]
pub(crate) mod monitor_fixtures;

#[path = "wave7_p15_support/fixtures.rs"]
pub(crate) mod monitor_runtime_fixtures;

#[path = "wave7_p15_support/mod.rs"]
pub(crate) mod monitor_runtime_support;

#[path = "multi_server/common.rs"]
pub(crate) mod multi_server_fixtures;

#[path = "support/state_template.rs"]
pub(crate) mod state_template;

#[cfg(unix)]
#[path = "../../runtime-process/tests/support/mod.rs"]
pub(crate) mod process_support;

#[path = "support/projections.rs"]
pub(crate) mod projection_fixtures;

pub(crate) use callback_fixtures::frozen as recovery_fixtures;
