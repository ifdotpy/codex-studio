mod account_login;
mod account_transfer;
mod agent_management;
pub use agent_management::NativeContextRepairPort;
pub use agents_create::managed_creation_environment;
pub use work_write::managed_work_port;
mod agents_create;
mod branches;
pub(crate) mod browser;
mod catalog;
mod catalog_agent;
mod checkpoints;
mod claude_controls;
mod claude_login;
pub(crate) mod compose;
mod context_repair;
mod desktop;
pub use desktop::{backend_build_identity, initialize_backend_identity};
pub(crate) mod extractors;
mod federation;
mod host_secrets;
pub use host_secrets::{VaultJobClient, VaultJobOwner};
mod imports;
pub(crate) mod incoming_multi_server;
mod insights;
mod linux_vm_settings;
mod mac_sync;
pub(crate) mod manifest;
mod messages;
mod models;
mod monitor_runtime;
mod monitors;
mod multi_server;
mod orchestration_settings;
mod panels;
mod process_controls;
mod projects;
mod questions_reads;
mod questions_write;
mod queue_read;
mod queue_write;
mod recovery;
mod rules;
mod session;
mod sync;
mod sync_delta;
mod sync_documents;
mod sync_stream;
pub(crate) use sync_stream::SyncSessions;
mod system_reads;
mod terminals;
mod tool_requests;
mod transcript;
mod usage;
pub(crate) mod vm_native_provider;
pub(crate) mod voice;
mod work_items;
mod work_write;
mod workspace_files;
mod workspace_views;

use manifest::RouteGroup;
use serde::de::DeserializeOwned;
use serde_json::Value;

type RegisteredRouteGroup = (&'static str, fn() -> RouteGroup);

pub(super) fn accounts_response(
    snapshot: studio_runtime_services::account_policy::AccountsSnapshot,
) -> Result<axum::Json<crate::generated::AccountsResponse>, crate::error::ApiError> {
    use crate::generated::{Account, AccountsResponse, CodexLoginReceipt};

    let accounts = snapshot
        .accounts
        .into_iter()
        .map(serde_json::from_value::<Account>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| crate::error::ApiError::internal())?;
    let archived_accounts = snapshot
        .archived_accounts
        .into_iter()
        .map(serde_json::from_value::<Account>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| crate::error::ApiError::internal())?;
    let logins = snapshot
        .logins
        .into_iter()
        .map(serde_json::from_value::<CodexLoginReceipt>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| crate::error::ApiError::internal())?;

    Ok(axum::Json(AccountsResponse {
        sync_entities: None,
        sync_entities_after: None,
        accounts,
        archived_accounts,
        default_account_key: serde_json::from_value(snapshot.default_account_key)
            .map_err(|_| crate::error::ApiError::internal())?,
        default_account_groups: snapshot.default_account_groups,
        logins,
        supports_delete: true,
        supports_disconnect: true,
    }))
}

fn generated_json<T: DeserializeOwned>(value: Value) -> Result<T, crate::error::ApiError> {
    serde_json::from_value::<T>(value).map_err(|_| crate::error::ApiError::internal())
}

fn registered_route_groups() -> Vec<RegisteredRouteGroup> {
    let mut registered = inventory::iter::<manifest::RouteModuleRegistration>
        .into_iter()
        .filter_map(|registration| {
            registration
                .routes
                .map(|routes| (registration.name, routes))
        })
        .collect::<Vec<_>>();
    registered.sort_unstable_by_key(|(name, _)| *name);
    registered
}

pub(crate) fn groups() -> Vec<RouteGroup> {
    registered_route_groups()
        .into_iter()
        .map(|(_, routes)| routes())
        .collect()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod registration_tests {
    use super::manifest::RouteModuleRegistration;
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

    fn route_directory() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes")
    }

    #[test]
    fn route_modules_match_the_source_directory_in_alphabetical_order() {
        let directory = route_directory();
        let mut source_modules = fs::read_dir(&directory)
            .expect("routes directory is readable")
            .map(|entry| entry.expect("routes directory entry is readable").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .filter_map(|path| {
                let stem = path.file_stem()?.to_str()?;
                (stem != "mod").then(|| stem.to_owned())
            })
            .collect::<Vec<_>>();
        source_modules.sort_unstable();

        assert_eq!(
            declared_modules(include_str!("mod.rs")),
            source_modules,
            "each routes source file must have exactly one explicit module declaration"
        );

        let mut registered_modules = inventory::iter::<RouteModuleRegistration>
            .into_iter()
            .map(|registration| registration.name.to_owned())
            .collect::<Vec<_>>();
        registered_modules.sort_unstable();
        let route_modules = source_modules
            .iter()
            .filter(|module| module.as_str() != "manifest")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(registered_modules, route_modules);

        let mut route_files = source_modules
            .iter()
            .filter(|module| {
                fs::read_to_string(directory.join(format!("{module}.rs")))
                    .expect("route source is readable")
                    .contains("pub(crate) fn routes() -> RouteGroup")
            })
            .cloned()
            .collect::<Vec<_>>();
        route_files.sort_unstable();
        let mut route_groups = inventory::iter::<RouteModuleRegistration>
            .into_iter()
            .filter(|registration| registration.routes.is_some())
            .map(|registration| registration.name.to_owned())
            .collect::<Vec<_>>();
        route_groups.sort_unstable();
        assert_eq!(route_groups, route_files);
        assert_eq!(route_groups.len(), route_files.len());
    }

    #[test]
    fn dependency_manifest_is_registered_in_stable_order() {
        let dependencies = super::manifest::registered_dependencies();
        assert!(!dependencies.is_empty());
        assert!(dependencies.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            inventory::iter::<RouteModuleRegistration>
                .into_iter()
                .flat_map(|registration| registration.dependencies.iter())
                .all(|dependency| dependencies.contains(dependency))
        );
    }

    #[test]
    fn route_groups_are_returned_in_stable_order() {
        let groups = super::registered_route_groups();
        assert_eq!(groups.len(), super::registered_route_groups().len());
        assert!(groups.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(super::groups().len(), groups.len());
    }
}

#[cfg(test)]
mod stored_shape_tests {
    use serde_json::json;

    #[test]
    fn a_provider_argument_vector_is_an_analytics_command() {
        let call = json!({
            "agentId": "agent-1", "rootId": "agent-1", "accountKey": "default",
            "accountHistory": [], "id": "call-1", "at": 1.5, "recordedAt": 2,
            "source": "history", "timestampSource": "record", "type": "exec",
            "name": "shell", "isTool": true, "status": "completed",
            "payloadBoundary": "protocol", "command": ["/bin/zsh", "-lc", "ls"]
        });
        super::generated_json::<crate::generated::AnalyticsResponse>(json!({"calls": [call]}))
            .expect("an argument vector command is part of the analytics view");
    }

    #[test]
    fn a_provider_turn_error_object_is_part_of_the_transcript() {
        let item = json!({
            "id": "item-1", "role": "assistant", "text": "",
            "turnError": {
                "message": "The model refused", "codexErrorInfo": "misalignmentPolicyViolation",
                "additionalDetails": null, "misalignment": null
            }
        });
        super::generated_json::<crate::generated::TranscriptPageResponse>(
            json!({"items": [item], "truncated": false}),
        )
        .expect("a turn error object is part of the transcript page");
    }
}
