//! Background job that ends the transcript copies in `sync_entities`: it numbers the old
//! rows of `runtime_items` online, then runs the copy check. Only when both finish does
//! the writer stop writing the copies (`durable_seq::retire_transcript_copies`).

use std::sync::Arc;

use studio_runtime_store::RuntimeStore;
use studio_runtime_store::durable_seq::{retire_transcript_copies, transcript_copies_retired};
use studio_runtime_store::item_seq_backfill::ItemSeqBackfill;
use studio_runtime_store::online_migration::{self, Step};

/// Run until the copies are retired, the check finds a missing item, or shutdown.
/// Stopping is safe: the next start resumes from the saved mark.
pub async fn run(store: Arc<RuntimeStore>, shutdown: tokio::sync::watch::Receiver<bool>) {
    let stop = shutdown.clone();
    match online_migration::run(&store, Arc::new(ItemSeqBackfill), move || *stop.borrow()).await {
        Ok(Step::Complete) => {}
        Ok(Step::Progress) => return,
        Err(error) => {
            eprintln!("transcript sequence backfill failed: {error}");
            return;
        }
    }
    if *shutdown.borrow() {
        return;
    }
    let outcome = store
        .transaction(|transaction| {
            if transcript_copies_retired(transaction)? {
                return Ok(Vec::new());
            }
            retire_transcript_copies(transaction)
        })
        .await;
    match outcome {
        Ok(missing) if missing.is_empty() => {}
        Ok(missing) => eprintln!(
            "transcript copies kept: {} items are missing from runtime_items",
            missing.len()
        ),
        Err(error) => eprintln!("transcript copy check failed: {error}"),
    }
}
