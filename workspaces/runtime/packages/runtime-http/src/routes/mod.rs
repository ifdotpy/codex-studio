pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod agents_create;
mod desktop;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod work_items;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        agents_create::routes(),
        session::routes(),
        rules::routes(),
        sync::routes(),
        sync_documents::routes(),
        work_items::routes(),
        desktop::routes(),
    ]
}
