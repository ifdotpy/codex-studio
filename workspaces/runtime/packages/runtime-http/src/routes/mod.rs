pub(crate) mod compose;
pub(crate) mod extractors;
pub(crate) mod manifest;

mod agents_create;
mod catalog;
mod desktop;
mod monitors;
mod panels;
mod questions_reads;
mod queue_read;
mod queue_write;
mod rules;
mod session;
mod sync;
mod sync_documents;
mod transcript;
mod work_items;
mod work_write;
mod workspace_files;
mod workspace_views;

use manifest::RouteGroup;

pub(crate) fn groups() -> Vec<RouteGroup> {
    vec![
        agents_create::routes(),
        session::routes(),
        rules::routes(),
        questions_reads::routes(),
        sync::routes(),
        sync_documents::routes(),
        monitors::routes(),
        panels::routes(),
        transcript::routes(),
        work_items::routes(),
        work_write::routes(),
        workspace_files::routes(),
        workspace_views::routes(),
        desktop::routes(),
        catalog::routes(),
    ]
}
