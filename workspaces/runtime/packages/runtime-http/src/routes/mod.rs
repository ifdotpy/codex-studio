pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod desktop;
mod panels;
mod session;
mod sync;
mod sync_documents;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        session::routes(),
        sync::routes(),
        sync_documents::routes(),
        panels::routes(),
        desktop::routes(),
    ]
}
