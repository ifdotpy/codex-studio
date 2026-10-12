pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod catalog;
mod desktop;
mod panels;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod transcript;
mod work_items;
mod workspace_files;
mod workspace_views;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        session::routes(),
        rules::routes(),
        sync::routes(),
        sync_documents::routes(),
        panels::routes(),
        transcript::routes(),
        work_items::routes(),
        workspace_files::routes(),
        workspace_views::routes(),
        desktop::routes(),
        catalog::routes(),
    ]
}
