//! Periodic collection of unreferenced text blobs
//! ([`studio_runtime_store::text_blobs`]). Each pass is one writer
//! transaction, so it never runs between a blob write and its row insert.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use studio_runtime_store::{RuntimeStore, StoreResult, text_blobs};

/// Wait after startup before the first pass, so recovery runs first. Unit: seconds.
pub const SWEEP_START_DELAY: Duration = Duration::from_secs(120);
/// Delay between passes. Unit: seconds.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

const LABEL: &str = "runtime-services/text_blob_sweep";

/// One pass. Returns the number of deleted blobs.
pub async fn run_pass(store: &RuntimeStore) -> StoreResult<usize> {
    store
        .transaction(move |transaction| text_blobs::sweep(transaction, SystemTime::now()))
        .await
}

/// Run passes until shutdown.
pub async fn run(store: Arc<RuntimeStore>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::select! {
        () = tokio::time::sleep(SWEEP_START_DELAY) => {}
        _ = shutdown.changed() => return,
    }
    while !*shutdown.borrow() {
        if let Err(error) = run_pass(&store).await {
            eprintln!("{LABEL} failed: {error}");
        }
        tokio::select! {
            () = tokio::time::sleep(SWEEP_INTERVAL) => {}
            _ = shutdown.changed() => return,
        }
    }
}
