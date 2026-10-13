mod agents_create;
mod catalog;
pub(crate) mod compose;
mod desktop;
pub(crate) mod extractors;
mod federation;
pub(crate) mod manifest;
mod messages;
mod models;
mod monitors;
mod multi_server;
mod panels;
mod projects;
mod questions_reads;
mod questions_write;
mod queue_read;
mod queue_write;
mod rules;
mod session;
mod sync;
mod sync_delta;
mod sync_documents;
mod terminals;
mod transcript;
mod usage;
mod work_items;
mod work_write;
mod workspace_files;
mod workspace_views;

use manifest::RouteGroup;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

type RegisteredRouteGroup = (&'static str, fn() -> RouteGroup);

fn generated_json<T: DeserializeOwned + Serialize>(
    value: Value,
) -> Result<Value, crate::error::ApiError> {
    serde_json::from_value::<T>(value)
        .and_then(serde_json::to_value)
        .map_err(|_| crate::error::ApiError::internal())
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
        assert_eq!(registered_modules, source_modules);

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
        assert_eq!(route_groups.len(), 23);
    }

    #[test]
    fn dependency_manifest_is_registered_in_stable_order() {
        let dependencies = super::manifest::registered_dependencies();
        assert_eq!(dependencies.len(), 63);
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
        assert_eq!(groups.len(), 23);
        assert!(groups.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(super::groups().len(), groups.len());
    }
}
