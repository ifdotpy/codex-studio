//! Background conversion of plain stored-text values to stored format 1:
//! `runtime_item_fulltext.body` in Canvas, then `analytics_items.record` in
//! analytics with a dictionary trained from that table's own rows.
//!
//! Each Canvas step is a bounded reader window and one short writer
//! transaction ([`studio_runtime_store::stored_text_compaction`]), with a pause
//! between steps so interactive writes keep the writer actor. An analytics
//! step scans and writes one small batch in one analytics transaction. The pass stops while the
//! state volume has less free space than the critical desktop warning, and
//! resumes on the next interval. It never deletes data and never shrinks the
//! database file: returning freed pages to the disk is an operator step
//! (`docs/canvas-maintenance.md`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use studio_runtime_schema::shared_values::DISK_SPACE_CRITICAL_FREE_BYTES;
use studio_runtime_store::stored_text_compaction::{
    self as compaction, ANALYTICS_ITEMS_RECORD, CompactedColumn, Direction, FULLTEXT_BODY,
};
use studio_runtime_store::stored_text_dictionary as dictionaries;
use studio_runtime_store::{AnalyticsRepository, RuntimeStore, StoreError, StoreResult};

/// Wait after startup before the first pass, so recovery runs first.
pub const COMPACTION_START_DELAY: Duration = Duration::from_secs(60);
/// Pause between batches so interactive writers keep the actor.
pub const COMPACTION_BATCH_PAUSE: Duration = Duration::from_millis(50);
/// Delay between passes. A pass over a complete column only reads its
/// progress row.
pub const COMPACTION_PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The pass writes nothing while the state volume has less free space than
/// this. Owner: the critical desktop warning in `shared_values`. Unit: bytes.
pub const COMPACTION_MIN_FREE_BYTES: u64 = DISK_SPACE_CRITICAL_FREE_BYTES;

const LABEL: &str = "runtime-services/stored_text_compaction";

/// How one pass ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PassOutcome {
    /// The column has no plain values left from before the pass.
    Complete,
    /// Free space fell below [`COMPACTION_MIN_FREE_BYTES`], or it is unknown.
    WaitingForSpace,
    /// The table has too few rows to train its dictionary yet.
    WaitingForRows,
}

/// Free bytes of the state volume, or `None` when unknown.
pub type FreeSpace = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// Run passes until shutdown. Stopping between awaits is safe: a submitted
/// batch commits or rolls back on its own, and the next start resumes from the
/// saved cursor.
pub async fn run(store: Arc<RuntimeStore>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let root: PathBuf = store.root().to_path_buf();
    let free: FreeSpace = Arc::new(move || crate::disk_space::free_bytes(&root));
    tokio::select! {
        () = tokio::time::sleep(COMPACTION_START_DELAY) => {}
        _ = shutdown.changed() => return,
    }
    while !*shutdown.borrow() {
        tokio::select! {
            result = run_pass(&store, FULLTEXT_BODY, &free) => {
                if let Err(error) = result {
                    eprintln!("stored text compaction failed: {error}");
                    record_failure(&store, FULLTEXT_BODY, &error).await;
                }
            }
            _ = shutdown.changed() => return,
        }
        tokio::select! {
            result = run_analytics_pass(&store, &free) => {
                if let Err(error) = result {
                    eprintln!("analytics stored text compaction failed: {error}");
                    record_analytics_failure(&store, &error).await;
                }
            }
            _ = shutdown.changed() => return,
        }
        tokio::select! {
            () = tokio::time::sleep(COMPACTION_PASS_INTERVAL) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// One pass over `column` from its saved cursor to the end of the table.
pub async fn run_pass(
    store: &Arc<RuntimeStore>,
    column: CompactedColumn,
    free: &FreeSpace,
) -> StoreResult<PassOutcome> {
    let progress = store
        .read_async(LABEL, move |connection| {
            compaction::progress(connection, column)
        })
        .await?;
    if progress.complete {
        return Ok(PassOutcome::Complete);
    }
    let mut cursor = progress.cursor;
    loop {
        if free().is_none_or(|bytes| bytes < COMPACTION_MIN_FREE_BYTES) {
            let now = now_seconds();
            store
                .transaction(move |transaction| {
                    compaction::save_progress(
                        transaction,
                        column,
                        cursor,
                        false,
                        "waitingForSpace",
                        now,
                    )
                })
                .await?;
            return Ok(PassOutcome::WaitingForSpace);
        }
        let window = store
            .read_async(LABEL, move |connection| {
                compaction::scan_window(connection, column, Direction::Compress, cursor)
            })
            .await?;
        let next = window.next_cursor;
        let done = window.done;
        if !window.candidates.is_empty() {
            let now = now_seconds();
            store
                .transaction(move |transaction| {
                    compaction::apply_window(transaction, column, &window, now)
                })
                .await?;
        } else if done {
            let now = now_seconds();
            store
                .transaction(move |transaction| {
                    compaction::save_progress(transaction, column, next, true, "complete", now)
                })
                .await?;
        }
        if done {
            return Ok(PassOutcome::Complete);
        }
        cursor = next;
        tokio::time::sleep(COMPACTION_BATCH_PAUSE).await;
    }
}

/// One pass over `analytics_items.record`: train its dictionary when the
/// table has enough rows and none exists, then convert plain values from the
/// saved cursor to the end of the table.
pub async fn run_analytics_pass(
    store: &Arc<RuntimeStore>,
    free: &FreeSpace,
) -> StoreResult<PassOutcome> {
    let column = ANALYTICS_ITEMS_RECORD;
    let progress = store
        .analytics_projection_transaction(move |transaction| {
            compaction::progress(transaction, column)
        })
        .await?;
    if progress.complete {
        return Ok(PassOutcome::Complete);
    }
    if !ensure_dictionary(store).await? {
        return Ok(PassOutcome::WaitingForRows);
    }
    let mut cursor = progress.cursor;
    loop {
        let now = now_seconds();
        if free().is_none_or(|bytes| bytes < COMPACTION_MIN_FREE_BYTES) {
            store
                .analytics_projection_transaction(move |transaction| {
                    compaction::save_progress(
                        transaction,
                        column,
                        cursor,
                        false,
                        "waitingForSpace",
                        now,
                    )
                })
                .await?;
            return Ok(PassOutcome::WaitingForSpace);
        }
        let (next, done) = store
            .analytics_projection_transaction(move |transaction| {
                let window =
                    compaction::scan_window(transaction, column, Direction::Compress, cursor)?;
                if !window.candidates.is_empty() {
                    compaction::apply_window(transaction, column, &window, now)?;
                } else if window.done {
                    compaction::save_progress(
                        transaction,
                        column,
                        window.next_cursor,
                        true,
                        "complete",
                        now,
                    )?;
                }
                Ok::<_, StoreError>((window.next_cursor, window.done))
            })
            .await?;
        if done {
            return Ok(PassOutcome::Complete);
        }
        cursor = next;
        tokio::time::sleep(COMPACTION_BATCH_PAUSE).await;
    }
}

/// Make sure `analytics_items.record` has a dictionary. Samples are read and
/// the dictionary is trained on a read-only connection; only the insert takes
/// the analytics writer. Returns false while the table has too few rows.
async fn ensure_dictionary(store: &Arc<RuntimeStore>) -> StoreResult<bool> {
    let root = store.root().to_path_buf();
    let trained = studio_runtime_store::blocking_pool::spawn_blocking(
        "stored-text/train",
        move || -> StoreResult<Option<Option<Vec<u8>>>> {
            let connection = AnalyticsRepository::new(&root)
                .open_read_only()
                .map_err(|error| StoreError::InvalidStoredObject {
                    table: "analytics_stored_text_dictionaries",
                    id: error.to_string(),
                })?;
            if dictionaries::current(&connection, dictionaries::ANALYTICS_ITEMS_RECORD)?.is_some() {
                return Ok(None);
            }
            Ok(Some(
                dictionary_samples(&connection)?.and_then(|samples| dictionaries::train(&samples)),
            ))
        },
    )
    .await
    .map_err(|_| StoreError::ReadTaskFailed)??;
    let bytes = match trained {
        None => return Ok(true),
        Some(None) => return Ok(false),
        Some(Some(bytes)) => bytes,
    };
    let now = now_seconds();
    store
        .analytics_projection_transaction(move |transaction| {
            if dictionaries::current(transaction, dictionaries::ANALYTICS_ITEMS_RECORD)?.is_none() {
                dictionaries::insert(
                    transaction,
                    dictionaries::ANALYTICS_ITEMS_RECORD,
                    bytes,
                    now,
                )?;
            }
            Ok::<_, StoreError>(())
        })
        .await?;
    Ok(true)
}

/// Plain `analytics_items.record` values spread over the table, or `None`
/// when the table has fewer than [`dictionaries::DICTIONARY_MIN_ROWS`] rows.
fn dictionary_samples(connection: &rusqlite::Connection) -> StoreResult<Option<Vec<Vec<u8>>>> {
    const SPANS: i64 = 32;
    let last: Option<i64> =
        connection.query_row("SELECT max(rowid) FROM main.analytics_items", [], |row| {
            row.get(0)
        })?;
    let Some(last) = last.filter(|last| *last >= dictionaries::DICTIONARY_MIN_ROWS) else {
        return Ok(None);
    };
    let per_span = (dictionaries::DICTIONARY_SAMPLE_ROWS as i64 / SPANS).max(1);
    let mut statement = connection.prepare(
        "SELECT CAST(record AS BLOB) FROM main.analytics_items \
         WHERE rowid>?1 AND typeof(record)='text' ORDER BY rowid LIMIT ?2",
    )?;
    let mut samples = Vec::new();
    for span in 0..SPANS {
        let start = last / SPANS * span;
        let rows = statement
            .query_map(rusqlite::params![start, per_span], |row| {
                row.get::<_, Vec<u8>>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        samples.extend(rows);
    }
    Ok((samples.len() as i64 >= dictionaries::DICTIONARY_MIN_ROWS / 2).then_some(samples))
}

async fn record_analytics_failure(store: &Arc<RuntimeStore>, error: &StoreError) {
    let message = error.to_string();
    let now = now_seconds();
    let result = store
        .analytics_projection_transaction(move |transaction| {
            compaction::record_error(transaction, ANALYTICS_ITEMS_RECORD, &message, now)
        })
        .await;
    if let Err(error) = result {
        eprintln!("analytics stored text status write failed: {error}");
    }
}

/// Rewrite every compressed value of `column` as plain TEXT, then forget the
/// compaction progress. An operator runs this, with the backend stopped,
/// before installing a release that reads only plain TEXT. Returns the
/// number of values rewritten.
pub async fn inflate(store: &Arc<RuntimeStore>, column: CompactedColumn) -> StoreResult<usize> {
    let mut cursor = 0;
    let mut rewritten = 0;
    loop {
        let window = store
            .read_async(LABEL, move |connection| {
                compaction::scan_window(connection, column, Direction::Inflate, cursor)
            })
            .await?;
        let next = window.next_cursor;
        let done = window.done;
        if !window.candidates.is_empty() {
            let now = now_seconds();
            let batch = store
                .transaction(move |transaction| {
                    compaction::apply_window(transaction, column, &window, now)
                })
                .await?;
            rewritten += batch.converted;
        }
        if done {
            break;
        }
        cursor = next;
    }
    store
        .transaction(move |transaction| compaction::reset_progress(transaction, column))
        .await?;
    Ok(rewritten)
}

/// [`inflate`] for `analytics_items.record`, one analytics transaction per
/// batch. The dictionaries stay stored, because they are never deleted.
pub async fn inflate_analytics(store: &Arc<RuntimeStore>) -> StoreResult<usize> {
    let column = ANALYTICS_ITEMS_RECORD;
    let mut cursor = 0;
    let mut rewritten = 0;
    loop {
        let now = now_seconds();
        let (converted, next, done) = store
            .analytics_projection_transaction(move |transaction| {
                let window =
                    compaction::scan_window(transaction, column, Direction::Inflate, cursor)?;
                let converted = if window.candidates.is_empty() {
                    0
                } else {
                    compaction::apply_window(transaction, column, &window, now)?.converted
                };
                Ok::<_, StoreError>((converted, window.next_cursor, window.done))
            })
            .await?;
        rewritten += converted;
        if done {
            break;
        }
        cursor = next;
    }
    store
        .analytics_projection_transaction(move |transaction| {
            compaction::reset_progress(transaction, column)
        })
        .await?;
    Ok(rewritten)
}

/// Keep the cursor and record why the pass stopped.
async fn record_failure(store: &Arc<RuntimeStore>, column: CompactedColumn, error: &StoreError) {
    let message = error.to_string();
    let now = now_seconds();
    let result = store
        .transaction(move |transaction| {
            compaction::record_error(transaction, column, &message, now)
        })
        .await;
    if let Err(error) = result {
        eprintln!("stored text compaction status write failed: {error}");
    }
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use studio_runtime_store::stored_text::StoredText;

    async fn store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        (directory, Arc::new(store))
    }

    fn body(index: usize) -> String {
        format!("{{\"aggregatedOutput\":\"line {index}\\n\"}}").repeat(2_000)
    }

    /// Plain rows as an older release wrote them.
    async fn insert_plain(store: &Arc<RuntimeStore>, count: usize) {
        store
            .transaction(move |transaction| {
                for index in 0..count {
                    transaction.execute(
                        "INSERT INTO runtime_item_fulltext(id,body) VALUES(?1,?2)",
                        params![format!("agent:item-{index:03}"), body(index)],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("plain rows");
    }

    async fn rows(store: &Arc<RuntimeStore>) -> Vec<(String, String, String)> {
        store
            .read_async(LABEL, |connection| {
                let mut statement = connection.prepare(
                    "SELECT id,typeof(body),body FROM runtime_item_fulltext ORDER BY rowid",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get::<_, StoredText>(2)?.into_string(),
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, StoreError>(rows)
            })
            .await
            .expect("rows")
    }

    async fn status(store: &Arc<RuntimeStore>) -> Option<(i64, String)> {
        store
            .read_async(LABEL, |connection| {
                use rusqlite::OptionalExtension;
                Ok::<_, StoreError>(
                    connection
                        .query_row(
                            "SELECT complete,status FROM runtime_payload_migrations WHERE name=?1",
                            [FULLTEXT_BODY.progress],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?,
                )
            })
            .await
            .expect("status")
    }

    fn plenty() -> FreeSpace {
        Arc::new(|| Some(COMPACTION_MIN_FREE_BYTES * 4))
    }

    #[tokio::test]
    async fn pass_converts_every_plain_body_without_changing_its_text() {
        let (_directory, store) = store().await;
        insert_plain(&store, compaction::COMPACTION_BATCH_ROWS + 3).await;

        let outcome = run_pass(&store, FULLTEXT_BODY, &plenty())
            .await
            .expect("pass");

        assert_eq!(outcome, PassOutcome::Complete);
        let rows = rows(&store).await;
        assert_eq!(rows.len(), compaction::COMPACTION_BATCH_ROWS + 3);
        for (index, (_, kind, text)) in rows.iter().enumerate() {
            assert_eq!(kind, "blob");
            assert_eq!(text, &body(index));
        }
        assert_eq!(status(&store).await, Some((1, "complete".to_owned())));
        let batches = store
            .read_async(LABEL, |connection| {
                Ok::<_, StoreError>(connection.query_row(
                    "SELECT count(*),sum(rows) FROM runtime_stored_text_batches WHERE name=?1",
                    [FULLTEXT_BODY.progress],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )?)
            })
            .await
            .expect("batches");
        assert_eq!(batches, (2, (compaction::COMPACTION_BATCH_ROWS + 3) as i64));

        // A complete column only reads its progress row.
        let generation = store.committed_change_generation();
        assert_eq!(
            run_pass(&store, FULLTEXT_BODY, &plenty())
                .await
                .expect("idle pass"),
            PassOutcome::Complete
        );
        assert_eq!(store.committed_change_generation(), generation);
    }

    #[tokio::test]
    async fn pass_writes_nothing_while_free_space_is_low_or_unknown() {
        let (_directory, store) = store().await;
        insert_plain(&store, 3).await;
        for free in [
            Arc::new(|| Some(COMPACTION_MIN_FREE_BYTES - 1)) as FreeSpace,
            Arc::new(|| None) as FreeSpace,
        ] {
            let outcome = run_pass(&store, FULLTEXT_BODY, &free).await.expect("pass");
            assert_eq!(outcome, PassOutcome::WaitingForSpace);
            assert!(rows(&store).await.iter().all(|(_, kind, _)| kind == "text"));
            assert_eq!(
                status(&store).await,
                Some((0, "waitingForSpace".to_owned()))
            );
        }
        assert_eq!(
            run_pass(&store, FULLTEXT_BODY, &plenty())
                .await
                .expect("pass"),
            PassOutcome::Complete
        );
        assert!(rows(&store).await.iter().all(|(_, kind, _)| kind == "blob"));
    }

    #[tokio::test]
    async fn inflate_restores_every_plain_body_and_forgets_progress() {
        let (_directory, store) = store().await;
        insert_plain(&store, compaction::COMPACTION_BATCH_ROWS + 3).await;
        run_pass(&store, FULLTEXT_BODY, &plenty())
            .await
            .expect("pass");

        let rewritten = inflate(&store, FULLTEXT_BODY).await.expect("inflate");

        assert_eq!(rewritten, compaction::COMPACTION_BATCH_ROWS + 3);
        for (index, (_, kind, text)) in rows(&store).await.iter().enumerate() {
            assert_eq!(kind, "text");
            assert_eq!(text, &body(index));
        }
        assert_eq!(status(&store).await, None, "a later pass compresses again");
    }

    fn analytics_record(index: usize) -> String {
        serde_json::json!({"id": format!("item-{index:05}"), "agentId": "agent",
            "rootId": "root", "type": "agentMessage", "output": {"bytes": index, "chars": index}})
        .to_string()
    }

    async fn insert_analytics(store: &Arc<RuntimeStore>, rows: std::ops::Range<usize>) {
        store
            .analytics_projection_transaction(move |transaction| {
                for index in rows.clone() {
                    transaction.execute(
                        "INSERT INTO main.analytics_items VALUES \
                         (?1,'agent','root',NULL,NULL,?2,'agentMessage',NULL,0,?3)",
                        params![
                            format!("item-{index:05}"),
                            index as f64,
                            analytics_record(index)
                        ],
                    )?;
                }
                Ok::<_, StoreError>(())
            })
            .await
            .expect("analytics rows");
    }

    fn analytics_rows(store: &Arc<RuntimeStore>) -> Vec<(String, String)> {
        let connection = AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("analytics reader");
        connection
            .prepare(
                "SELECT typeof(m.record),v.record FROM main.analytics_items m \
                 JOIN analytics_items v ON v.id=m.id ORDER BY m.rowid",
            )
            .expect("statement")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("rows read")
    }

    #[tokio::test]
    async fn analytics_pass_trains_a_dictionary_and_converts_every_record() {
        let (_directory, store) = store().await;
        insert_analytics(&store, 0..10).await;
        assert_eq!(
            run_analytics_pass(&store, &plenty())
                .await
                .expect("small pass"),
            PassOutcome::WaitingForRows,
            "too few rows to train a dictionary"
        );
        assert!(
            analytics_rows(&store)
                .iter()
                .all(|(kind, _)| kind == "text")
        );

        insert_analytics(&store, 10..3_000).await;
        assert_eq!(
            run_analytics_pass(&store, &plenty()).await.expect("pass"),
            PassOutcome::Complete
        );
        let rows = analytics_rows(&store);
        assert_eq!(rows.len(), 3_000);
        for (index, (kind, text)) in rows.iter().enumerate() {
            assert_eq!(kind, "blob");
            assert_eq!(text, &analytics_record(index));
        }
        let dictionaries: i64 = AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("reader")
            .query_row(
                "SELECT count(*) FROM main.analytics_stored_text_dictionaries WHERE name=?1",
                [dictionaries::ANALYTICS_ITEMS_RECORD],
                |row| row.get(0),
            )
            .expect("dictionary count");
        assert_eq!(dictionaries, 1);

        let rewritten = inflate_analytics(&store).await.expect("inflate");
        assert_eq!(rewritten, 3_000);
        let rows = analytics_rows(&store);
        assert!(rows.iter().all(|(kind, _)| kind == "text"));
        assert_eq!(rows[42].1, analytics_record(42));
    }
}
