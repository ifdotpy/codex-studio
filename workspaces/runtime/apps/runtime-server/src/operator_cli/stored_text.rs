//! `stored-text inflate --state DIRECTORY`: rewrite every compressed
//! full-text body and analytics item record as plain TEXT, with the backend
//! stopped, before a release that reads only plain TEXT is installed. Opening
//! the store takes the runtime lock, so the command refuses a state directory
//! that a backend owns.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use studio_runtime_store::RuntimeStore;
use studio_runtime_store::stored_text_compaction::FULLTEXT_BODY;

use super::Result;

const USAGE: &str = "usage: stored-text inflate --state DIRECTORY";

pub(super) fn run(args: &[String]) -> Result<ExitCode> {
    let state = match args {
        [command, flag, state] if command == "inflate" && flag == "--state" => state,
        _ => return Err(USAGE.into()),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let store = Arc::new(RuntimeStore::open(Some(Path::new(state))).await?);
        let bodies =
            studio_runtime_services::stored_text_compaction::inflate(&store, FULLTEXT_BODY).await?;
        let records =
            studio_runtime_services::stored_text_compaction::inflate_analytics(&store).await?;
        println!(
            "stored-text inflate: rewrote {bodies} full-text bodies and {records} analytics item records as plain text"
        );
        Result::Ok(ExitCode::SUCCESS)
    })
}
