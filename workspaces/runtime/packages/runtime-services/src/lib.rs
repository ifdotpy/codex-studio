//! Application services between the HTTP edge and the store,
//! provider and process crates.
//!
//! Each service is one leaf module owned by one slice. A service takes
//! explicit inputs (snapshots, clocks, ports) and returns decisions or
//! results, so its rules are testable in seconds without a database, a
//! provider or the server. Route modules in `studio-runtime-http` call
//! services; services never depend on HTTP types.

pub mod account_driver;
pub mod account_policy;
pub mod account_selection;
pub mod account_transfer;
pub mod account_transfer_driver;
pub mod account_worker;
pub mod agent_management;
pub mod agent_management_store;
pub mod agent_move;
pub mod agents_create;
pub mod analytics;
pub mod bounded_child;
pub mod budget;
pub mod budget_service;
pub mod budget_wait;
pub mod catalog_reads;
pub mod change_notifications;
pub mod checkpoints;
pub mod claude_bridge;
pub mod claude_controls;
pub mod claude_input_recovery;
pub mod claude_login;
pub mod context_repair;
pub mod coordination;
pub mod coordination_reads;
pub mod cost_usage;
pub mod costs;
pub mod event_write;
pub mod federation;
pub mod federation_credentials;
pub mod federation_delivery;
pub mod federation_effects;
pub mod federation_messaging;
pub mod historical_input_receipts;
pub mod model_catalog;
pub mod monitor_driver;
pub mod monitor_output;
pub mod monitor_policy;
pub mod monitor_receipts;
pub mod monitor_runtime;
pub mod multi_server;
pub mod notification_dispatch;
pub mod notification_runtime_effects;
pub mod panels;
pub mod payload_store;
pub mod pricing;
pub mod progress_panel;
pub mod progress_watch;
pub mod project_locations;
pub mod projects;
pub mod provider_callbacks;
pub(crate) mod provider_exchange;
pub mod provider_session_task;
pub mod questions_reads;
pub mod questions_write;
pub mod queue_write;
pub mod radio;
pub mod recovery;
pub mod recovery_absence;
pub mod recovery_history;
pub mod recovery_policy;
pub mod recovery_store;
pub mod relocation;
pub mod remote_transport;
pub mod rules;
pub mod rules_engine;
pub mod rules_tick;
pub mod scheduler;
pub mod submit_gates;
pub mod sync_documents;
pub(crate) mod sync_entity_context;
pub mod sync_policy;
pub mod sync_views;
pub mod system_reads;
pub mod task_recovery;
pub mod task_recovery_store;
pub mod terminal_driver;
pub mod thread_prepare;
pub mod token_rate;
pub mod tool_requests;
pub mod transcript;
pub mod turn_prepare;
pub mod usage;
pub mod voice;
pub mod work_policy;
pub mod work_write;
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
        let mut source_modules = fs::read_dir(&directory)
            .expect("services source directory is readable")
            .map(|entry| {
                entry
                    .expect("services source directory entry is readable")
                    .path()
            })
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .filter_map(|path| {
                let stem = path.file_stem()?.to_str()?;
                (stem != "lib").then(|| stem.to_owned())
            })
            .collect::<Vec<_>>();
        source_modules.sort_unstable();

        assert_eq!(declared_modules(include_str!("lib.rs")), source_modules);
    }
}
