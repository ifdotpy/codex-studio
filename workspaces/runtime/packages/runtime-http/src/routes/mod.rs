pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod desktop;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod workspace_views;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        session::routes(),
        rules::routes(),
        sync::routes(),
        sync_documents::routes(),
        workspace_views::routes(),
        desktop::routes(),
    ]
}
