pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod catalog;
mod desktop;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod terminals;
mod work_items;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        session::routes(),
        rules::routes(),
        sync::routes(),
        sync_documents::routes(),
        terminals::routes(),
        work_items::routes(),
        desktop::routes(),
        catalog::routes(),
    ]
}
