pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod desktop;
mod queue_write;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod work_items;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        session::routes(),
        rules::routes(),
        sync::routes(),
        sync_documents::routes(),
        work_items::routes(),
        desktop::routes(),
    ]
}
