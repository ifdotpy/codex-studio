pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod catalog;
mod desktop;
mod monitors;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod work_items;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        session::routes(),
        monitors::routes(),
        rules::routes(),
        sync::routes(),
        sync_documents::routes(),
        work_items::routes(),
        desktop::routes(),
        catalog::routes(),
    ]
}
