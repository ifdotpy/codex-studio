//! Application services between the HTTP edge and the store,
//! provider and process crates.
//!
//! Each service is one leaf module owned by one slice. A service takes
//! explicit inputs (snapshots, clocks, ports) and returns decisions or
//! results, so its rules are testable in seconds without a database, a
//! provider or the server. Route modules in `studio-runtime-http` call
//! services; services never depend on HTTP types.

pub mod account_auth_gate;
pub mod account_driver;
pub mod account_policy;
pub mod account_preference;
pub mod account_selection;
pub mod account_session_runtime;
pub mod account_start_gate;
pub mod account_transfer;
pub mod account_transfer_driver;
pub mod account_transfer_executor;
pub mod account_worker;
pub mod agent_lifecycle_runtime;
pub mod agent_management;
pub mod agent_management_store;
pub mod agent_move;
pub(crate) mod agent_progress;
pub mod agents_create;
pub mod analytics;
pub mod analytics_capture;
pub mod analytics_history;
pub mod analytics_history_repair;
pub mod analytics_history_schedule;
pub mod analytics_measurements;
pub mod analytics_projection;
pub mod analytics_rollout;
pub mod atomic_file;
pub mod bounded_child;
pub mod browser;
pub mod browser_answer;
pub mod browser_cards;
pub mod browser_environment;
#[cfg(unix)]
pub mod browser_host;
pub mod browser_policy;
pub mod budget;
pub mod budget_runtime;
pub mod budget_service;
pub mod budget_wait;
pub mod callback_runtime;
pub mod capacity_retry;
pub mod catalog_reads;
pub mod change_notifications;
pub mod checkpoints;
mod child_permissions;
pub mod claude_bridge;
pub mod claude_controls;
pub mod claude_input_recovery;
pub mod claude_login;
/// Receipt-scoped retained controls; worker submission uses claude_input_recovery::runtime.
pub mod claude_runtime;
pub mod context_repair;
pub mod context_repair_files;
pub mod continuation;
pub mod continuation_driver;
pub mod continuation_runtime;
pub mod coordination;
pub mod coordination_reads;
pub mod costs;
pub mod diagnostics;
pub mod disk_space;
pub mod event_write;
pub mod federation;
pub mod federation_credentials;
pub mod federation_delivery;
pub mod federation_effects;
pub mod federation_limits;
pub mod federation_messaging;
pub mod file_derived;
pub mod host_exec;
#[cfg(unix)]
pub mod host_exec_service;
pub mod host_project_runtime;
pub mod host_worker_create;
pub mod image_workspace;
pub mod image_workspace_agents;
pub(crate) mod layr_read;
pub mod lifecycle_effects;
pub mod limit_reset;
pub mod linux_vm_guest;
pub mod linux_vm_provider;
#[cfg(unix)]
pub mod linux_vm_runtime;
pub mod linux_vm_settings;
#[cfg(unix)]
pub mod linux_vm_share;
pub mod mac_sync;
pub mod maintenance;
#[allow(dead_code)] // The managed-tools callback caller is integrated by its owning package.
pub(crate) mod managed_child_create;
pub(crate) mod managed_host_tools;
pub mod managed_status_watch;
pub mod message_admission;
pub mod model_catalog;
pub mod monitor_driver;
pub mod monitor_event_queue;
pub mod monitor_lifecycle;
pub mod monitor_matches;
mod monitor_observation;
pub mod monitor_output;
pub mod monitor_policy;
pub mod monitor_receipts;
pub mod monitor_runtime;
pub mod monitor_runtime_wiring;
pub mod multi_server;
pub mod multi_server_auth;
pub mod multi_server_discovery;
pub mod multi_server_orchestration;
pub mod multi_server_pairing;
pub(crate) mod multi_server_tools;
/// Shared signed server-to-server transport API. Production callers use the
/// durable HTTPS adapter; tests and transport alternatives implement
/// `PairedTransport` with a local fake peer.
pub mod multi_server_transport;
pub mod multi_server_transport_limits;
pub mod native_action_ports;
pub mod native_actions;
pub mod native_review;
pub mod native_review_turn;
pub mod native_runtime;
pub mod notification_dispatch;
pub mod notification_runtime_effects;
pub mod organization_effects;
pub mod panels;
pub mod payload_blobs;
pub mod payload_store;
pub mod peer_conversion;
pub mod peer_updates;
pub mod portable_history;
pub mod pricing;
pub mod process_controls;
pub mod project_locations;
pub mod project_secrets;
pub mod projects;
pub mod provider_adapter;
pub mod provider_binaries;
pub mod provider_callbacks;
pub mod provider_session_task;
pub mod provider_versions;
pub mod questions_reads;
pub mod questions_write;
pub mod queue_write;
pub mod radio;
pub mod radio_runtime;
pub use radio_runtime::{
    Interrupt, InterruptOutcome, InterruptReceipt, InterruptTransport, NativeAcceptanceReceipt,
    RadioCreation, handoff_interrupt,
};
pub mod recovery;
pub mod recovery_absence;
pub mod recovery_history;
pub mod recovery_policy;
pub mod recovery_runtime;
pub mod recovery_store;
pub mod relocation;
pub mod remote_access;
pub mod remote_transport;
pub mod resource_notify;
pub mod rules;
pub mod rules_engine;
pub mod rules_tick;
pub mod safety_buffering;
pub mod scheduler;
pub mod server_exec;
pub mod server_git;
pub mod shell_command;
pub mod sign_in;
pub mod storage_retention;
pub mod submit_gates;
pub mod sync_documents;
pub mod sync_entity_context;
pub mod sync_policy;
pub mod sync_views;
pub mod system_reads;
pub mod tar_gzip;
pub mod task_gate;
pub mod task_output;
pub mod task_recovery;
pub mod task_recovery_store;
pub mod task_settlement;
pub mod terminal_driver;
#[cfg(unix)]
pub mod terminal_pty;
pub mod thread_fork_runtime;
pub mod thread_prepare;
pub mod title_generation;
pub mod token_rate;
pub mod tool_requests;
pub mod transcript;
pub mod transcript_runtime;
pub mod turn_prepare;
pub mod usage;
pub mod usage_resume;
pub mod vault;
pub mod vm_preparation;
pub mod voice;
pub mod voice_runtime;
pub mod wakeups;
pub mod web_push;
pub mod work_assignment_runtime;
pub mod work_policy;
pub mod work_write;
mod worker_capabilities;
pub mod workspace_cleanup_runtime;
pub mod workspace_files;
pub mod workspace_operations;
pub mod workspace_operations_effects;
pub mod workspace_ready;
pub mod workspace_ready_git;
pub mod workspace_views;

#[cfg(test)]
mod registration_tests {
    use std::fs;
    use std::path::Path;

    fn declared_modules(source: &str) -> Vec<String> {
        source
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                ["pub(crate) mod ", "pub mod ", "mod "]
                    .into_iter()
                    .find_map(|prefix| line.strip_prefix(prefix))
                    .and_then(|declaration| declaration.strip_suffix(';'))
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn service_modules_match_the_source_directory_in_alphabetical_order() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let path_included_children = fs::read_dir(&directory)
            .expect("services source directory is readable")
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                path.extension()
                    .is_some_and(|extension| extension == "rs")
                    .then_some(path)
            })
            .flat_map(|path| {
                let source = fs::read_to_string(path).expect("services source file is readable");
                source
                    .lines()
                    .filter_map(|line| {
                        let target = line
                            .trim()
                            .strip_prefix("#[path = \"")?
                            .strip_suffix("\"]")?;
                        let target = Path::new(target);
                        target
                            .parent()
                            .is_none_or(|parent| parent.as_os_str().is_empty())
                            .then(|| target.file_stem()?.to_str().map(str::to_owned))?
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let nested_path_modules = fs::read_dir(&directory)
            .expect("services source directory is readable")
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if !path.extension().is_some_and(|extension| extension == "rs")
                    || path.file_name().is_some_and(|name| name == "lib.rs")
                {
                    return None;
                }
                fs::read_to_string(path).ok()
            })
            .flat_map(|source| {
                source
                    .lines()
                    .filter_map(|line| {
                        line.trim()
                            .strip_prefix("#[path = \"")
                            .and_then(|path| path.strip_suffix("\"]"))
                            .map(str::to_owned)
                    })
                    .collect::<std::collections::HashSet<_>>()
            })
            .collect::<std::collections::HashSet<_>>();
        let mut source_modules = fs::read_dir(&directory)
            .expect("services source directory is readable")
            .map(|entry| {
                entry
                    .expect("services source directory entry is readable")
                    .path()
            })
            .filter_map(|path| {
                if path.is_dir() && path.join("mod.rs").is_file() {
                    return path.file_name()?.to_str().map(str::to_owned);
                }
                if !path.extension().is_some_and(|extension| extension == "rs") {
                    return None;
                }
                if nested_path_modules.contains(path.file_name()?.to_str()?) {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?;
                (stem != "lib" && !path_included_children.iter().any(|child| child == stem))
                    .then(|| stem.to_owned())
            })
            .collect::<Vec<_>>();
        source_modules.sort_unstable();

        assert_eq!(declared_modules(include_str!("lib.rs")), source_modules);
    }
}
