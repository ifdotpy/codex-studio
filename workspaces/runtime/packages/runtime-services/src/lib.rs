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
pub mod change_notifications;
pub mod agents_create;
pub mod coordination;
pub mod event_write;
pub mod federation;
pub mod model_catalog;
pub mod multi_server;
pub mod panels;
pub mod payload_store;
pub mod progress_panel;
pub mod progress_watch;
pub mod projects;
pub mod provider_session;
pub mod questions_reads;
pub mod questions_write;
pub mod queue_write;
pub mod rules;
pub mod scheduler;
pub mod sync_documents;
pub(crate) mod sync_entity_context;
pub mod sync_policy;
pub mod sync_views;
pub mod terminal_driver;
pub mod transcript;
pub mod usage;
pub mod work_policy;
pub mod work_write;
pub mod workspace_files;
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
