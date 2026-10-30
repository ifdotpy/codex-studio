//! Background work for text blobs ([`studio_runtime_store::text_blobs`]): move
//! final item text from `runtime_item_fulltext` to blobs, then collect blobs
//! that no item points at. File I/O runs on the blocking pool, never inside a
//! SQLite transaction; a transaction only reads or writes rows.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use studio_runtime_store::text_blobs::{self, MovedText};
use studio_runtime_store::{RuntimeStore, StoreError, StoreResult};

use crate::payload_blobs::{self, BlobWrite};

/// Wait after startup before the first pass, so recovery runs first. Unit: seconds.
pub const PASS_START_DELAY: Duration = Duration::from_secs(120);
/// Delay between passes. Unit: seconds.
pub const PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Texts moved in one round. Bounds the memory of a round. Unit: items.
pub const MOVE_BATCH_ITEMS: usize = 8;

const LABEL: &str = "runtime-services/text_blob_sweep";

fn io_error(message: impl ToString) -> StoreError {
    StoreError::StateDirectory(std::io::Error::other(message.to_string()))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> StoreResult<T> + Send + 'static,
) -> StoreResult<T> {
    studio_runtime_process::blocking_pool::spawn_blocking(LABEL, work)
        .await
        .map_err(|_| io_error("blocking task failed"))?
}

/// Move final item text to blobs until none is left. Returns the number of
/// items pointed at a blob.
pub async fn move_pass(store: &RuntimeStore) -> StoreResult<usize> {
    let mut total = 0;
    loop {
        let batch = store
            .read_async(LABEL, |connection| {
                text_blobs::pending(connection, MOVE_BATCH_ITEMS)
            })
            .await?;
        if batch.is_empty() {
            return Ok(total);
        }
        let root = store.root().to_path_buf();
        let moved = blocking(move || {
            batch
                .into_iter()
                .map(|(id, text)| {
                    let (blob, bytes) = text_blobs::encode_blob(&text);
                    payload_blobs::write_blobs(
                        &root,
                        &[BlobWrite {
                            sha256: blob.hash.clone(),
                            bytes,
                            preview: Value::Null,
                        }],
                    )
                    .map_err(io_error)?;
                    Ok(MovedText {
                        id,
                        digest: text_blobs::digest(&text),
                        blob,
                    })
                })
                .collect::<StoreResult<Vec<_>>>()
        })
        .await?;
        let set = store
            .transaction(move |transaction| text_blobs::commit(transaction, &moved))
            .await?;
        if set == 0 {
            // Every text changed or lost its file; a later pass retries.
            return Ok(total);
        }
        total += set;
    }
}

/// Collect old unreferenced text blobs. Returns the number of deleted files.
pub async fn sweep_pass(store: &RuntimeStore) -> StoreResult<usize> {
    let pointed = store
        .read_async(LABEL, |connection| text_blobs::pointed_hashes(connection))
        .await?;
    let root = store.root().join("blobs");
    let quarantine_root = root.clone();
    let names = blocking(move || {
        if !quarantine_root.is_dir() {
            return Ok(Vec::new());
        }
        text_blobs::quarantine(&quarantine_root, &pointed, SystemTime::now())
    })
    .await?;
    if names.is_empty() {
        return Ok(0);
    }
    // A writer transaction serializes this read with `commit`.
    let for_check = names.clone();
    let referenced = store
        .transaction(move |transaction| text_blobs::referenced_among(transaction, &for_check))
        .await?;
    blocking(move || text_blobs::release(&root, &referenced, &names)).await
}

/// Run passes until shutdown.
pub async fn run(store: Arc<RuntimeStore>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::select! {
        () = tokio::time::sleep(PASS_START_DELAY) => {}
        _ = shutdown.changed() => return,
    }
    while !*shutdown.borrow() {
        tokio::select! {
            result = async {
                move_pass(&store).await?;
                sweep_pass(&store).await
            } => {
                if let Err(error) = result {
                    eprintln!("{LABEL} failed: {error}");
                }
            }
            _ = shutdown.changed() => return,
        }
        tokio::select! {
            () = tokio::time::sleep(PASS_INTERVAL) => {}
            _ = shutdown.changed() => return,
        }
    }
}
