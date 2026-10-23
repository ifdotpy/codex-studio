//! Background Canvas retention: finished-task output trimming and entity
//! tombstone pruning in short, resumable writer transactions.
//!
//! Task retention owns the `payload-v1:tasks` marker: candidate rows are read
//! on a reader connection in bounded rowid windows, and each writer batch
//! re-checks every row before it replaces the record. A restart resumes from
//! the durable cursor; a repeated batch is idempotent because a trimmed tail
//! is already within [`crate::payload_blobs::TASK_PREVIEW_BYTES`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use studio_runtime_store::{RuntimeStore, StoreResult, SyncDocumentsRepository};

/// Finished task output older than this keeps only its preview. Unit: seconds.
pub const TASK_RETENTION_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
/// The newest tasks of each agent keep their full output. Unit: tasks.
pub const TASK_NEWEST_PER_AGENT: i64 = 100;
/// Task rowids inspected by one reader scan window. Unit: rows.
pub const TASK_RETENTION_SCAN_ROWS: i64 = 512;
/// Pause between retention batches so interactive writers keep the actor.
pub const RETENTION_BATCH_PAUSE: Duration = Duration::from_millis(25);
/// Delay between complete retention passes. A pass that finds nothing to do
/// performs no writes.
pub const RETENTION_PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);

const TASK_MARKER: &str = "payload-v1:tasks";
/// Task statuses that are final, so their output can no longer change. Task
/// status is free-form; any other value (including active, approval, lost or
/// future statuses) keeps its full output.
const TRIMMABLE_TASK_STATUSES: &[&str] = &["completed", "failed", "interrupted"];

/// Outcome of one task retention scan window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaskRetentionBatch {
    /// Highest task rowid inspected; the next window starts after it.
    pub cursor: i64,
    pub trimmed: usize,
    /// True when the window reached the end of `runtime_tasks`.
    pub done: bool,
}

struct Candidate {
    rowid: i64,
    id: String,
    raw: String,
    agent: String,
    trimmed: String,
}

/// Run retention passes until shutdown. Stopping between awaits is safe: a
/// submitted batch commits or rolls back on its own, and the next start
/// resumes from the durable cursor.
pub async fn run(store: Arc<RuntimeStore>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        tokio::select! {
            result = run_pass(&store, now_seconds()) => {
                if let Err(error) = result {
                    eprintln!("storage retention pass failed: {error}");
                }
            }
            _ = shutdown.changed() => return,
        }
        tokio::select! {
            () = tokio::time::sleep(RETENTION_PASS_INTERVAL) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// One complete pass: resume task trimming from its durable cursor to the end
/// of the table, then prune entity tombstones down to their limit.
pub async fn run_pass(store: &Arc<RuntimeStore>, now: f64) -> StoreResult<()> {
    let mut cursor = store
        .read_async("runtime-services/storage_retention", |tx| {
            task_marker_cursor(tx)
        })
        .await?;
    loop {
        let batch = trim_task_batch(store, cursor, now).await?;
        cursor = batch.cursor;
        if batch.done {
            break;
        }
        tokio::time::sleep(RETENTION_BATCH_PAUSE).await;
    }
    if !store
        .read_async("runtime-services/storage_retention", |connection| {
            SyncDocumentsRepository::entity_tombstone_prune_due(connection)
        })
        .await?
    {
        return Ok(());
    }
    // Once due, prune all the way down to the limit, one batch at a time.
    loop {
        let removed = store
            .transaction(|transaction| {
                SyncDocumentsRepository::prune_entity_tombstones(transaction)
            })
            .await?;
        if removed == 0 {
            break;
        }
        tokio::time::sleep(RETENTION_BATCH_PAUSE).await;
    }
    Ok(())
}

/// Trim eligible tasks in the rowid window after `cursor`. The marker is
/// written only with trimmed rows or at the end of a pass whose saved marker
/// is not yet complete, so an idle pass submits no writer job at all.
pub async fn trim_task_batch(
    store: &Arc<RuntimeStore>,
    cursor: i64,
    now: f64,
) -> StoreResult<TaskRetentionBatch> {
    let (candidates, next_cursor, done) = store
        .read_async("runtime-services/storage_retention", move |connection| {
            scan_window(connection, cursor, now)
        })
        .await?;
    let finish_marker = done
        && store
            .read_async("runtime-services/storage_retention", |tx| {
                saved_marker_incomplete(tx)
            })
            .await?;
    if candidates.is_empty() && !finish_marker {
        return Ok(TaskRetentionBatch {
            cursor: next_cursor,
            trimmed: 0,
            done,
        });
    }
    store
        .transaction(move |transaction| {
            let trimmed = apply_candidates(transaction, &candidates)?;
            if trimmed > 0 || (done && saved_marker_incomplete(transaction)?) {
                save_marker(transaction, next_cursor, done, now)?;
            }
            Ok(TaskRetentionBatch {
                cursor: next_cursor,
                trimmed,
                done,
            })
        })
        .await
}

fn task_marker_cursor(connection: &Connection) -> StoreResult<i64> {
    if !table_exists(connection, "runtime_payload_migrations")? {
        return Ok(0);
    }
    let marker = connection
        .query_row(
            "SELECT cursor,complete FROM runtime_payload_migrations WHERE name=?1",
            [TASK_MARKER],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    // A completed pass starts over: older tasks age past the retention window.
    Ok(match marker {
        Some((cursor, 0)) => cursor.max(0),
        _ => 0,
    })
}

fn scan_window(
    connection: &Connection,
    cursor: i64,
    now: f64,
) -> StoreResult<(Vec<Candidate>, i64, bool)> {
    if !table_exists(connection, "runtime_tasks")? {
        return Ok((Vec::new(), cursor, true));
    }
    let mut statement = connection.prepare_cached(
        "SELECT rowid,id,record FROM runtime_tasks WHERE rowid>?1 ORDER BY rowid LIMIT ?2",
    )?;
    let rows = statement
        .query_map(params![cursor, TASK_RETENTION_SCAN_ROWS], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let done = rows.len() < TASK_RETENTION_SCAN_ROWS as usize;
    let next_cursor = rows.last().map_or(cursor, |(rowid, _, _)| *rowid);
    let candidates = rows
        .into_iter()
        .filter_map(|(rowid, id, raw)| candidate(rowid, id, raw, now))
        .collect();
    Ok((candidates, next_cursor, done))
}

/// A malformed, non-final, recent, or already short record is not a candidate;
/// in particular a missing or null `created` never ages out.
fn candidate(rowid: i64, id: String, raw: String, now: f64) -> Option<Candidate> {
    let record: Value = serde_json::from_str(&raw).ok()?;
    let status = record.get("status").and_then(Value::as_str)?;
    if !TRIMMABLE_TASK_STATUSES.contains(&status) {
        return None;
    }
    let created = record.get("created").and_then(Value::as_f64)?;
    if created >= now - TASK_RETENTION_SECONDS {
        return None;
    }
    let agent = record.get("agent").and_then(Value::as_str)?.to_owned();
    let tail = record.get("tail").and_then(Value::as_str)?;
    if crate::payload_blobs::task_preview(tail).len() == tail.len() {
        return None;
    }
    let trimmed = crate::payload_blobs::trim_task(&record).ok()?;
    Some(Candidate {
        rowid,
        id,
        raw,
        agent,
        trimmed: serde_json::to_string(&trimmed).ok()?,
    })
}

fn apply_candidates(transaction: &Transaction<'_>, candidates: &[Candidate]) -> StoreResult<usize> {
    let mut newest = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut trimmed = 0;
    for candidate in candidates {
        let current: Option<String> = transaction
            .query_row(
                "SELECT record FROM runtime_tasks WHERE rowid=?1 AND id=?2",
                params![candidate.rowid, candidate.id],
                |row| row.get(0),
            )
            .optional()?;
        // A task changed after the scan is re-evaluated by the next pass.
        if current.as_deref() != Some(candidate.raw.as_str()) {
            continue;
        }
        if !newest.contains_key(candidate.agent.as_str()) {
            let mut statement = transaction.prepare_cached(
                "SELECT id FROM runtime_tasks INDEXED BY runtime_task_agent_created_id \
                 WHERE json_extract(record,'$.agent')=?1 \
                 ORDER BY json_extract(record,'$.created') DESC,id DESC LIMIT ?2",
            )?;
            let ids = statement
                .query_map(params![candidate.agent, TASK_NEWEST_PER_AGENT], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<BTreeSet<_>, _>>()?;
            newest.insert(candidate.agent.as_str(), ids);
        }
        if newest[candidate.agent.as_str()].contains(&candidate.id) {
            continue;
        }
        transaction.execute(
            "UPDATE runtime_tasks SET record=?1 WHERE rowid=?2",
            params![candidate.trimmed, candidate.rowid],
        )?;
        trimmed += 1;
    }
    Ok(trimmed)
}

/// Whether a saved marker still reports an unfinished pass (including the
/// historical Python `error` and `waitingForSpace` states).
fn saved_marker_incomplete(connection: &Connection) -> StoreResult<bool> {
    Ok(connection
        .query_row(
            "SELECT complete=0 FROM runtime_payload_migrations WHERE name=?1",
            [TASK_MARKER],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}

fn save_marker(
    transaction: &Transaction<'_>,
    cursor: i64,
    done: bool,
    now: f64,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,error) \
         VALUES (?1,?2,?3,?4,?5,NULL) ON CONFLICT(name) DO UPDATE SET \
         cursor=excluded.cursor,complete=excluded.complete,status=excluded.status,\
         updated=excluded.updated,error=NULL",
        params![
            TASK_MARKER,
            cursor,
            i64::from(done),
            if done { "complete" } else { "running" },
            now
        ],
    )?;
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use studio_runtime_store::{ENTITY_TOMBSTONE_LIMIT, ENTITY_TOMBSTONE_PRUNE_BATCH};

    const NOW: f64 = 2_000_000_000.0;
    const OLD: f64 = NOW - TASK_RETENTION_SECONDS - 10.0;

    async fn store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open runtime store");
        (directory, Arc::new(store))
    }

    async fn insert_tasks(store: &Arc<RuntimeStore>, tasks: Vec<Value>) {
        store
            .transaction(move |transaction| {
                let mut statement =
                    transaction.prepare("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)")?;
                for task in &tasks {
                    statement.execute(params![
                        task["id"].as_str().expect("task id"),
                        task.to_string()
                    ])?;
                }
                Ok(())
            })
            .await
            .expect("insert tasks");
    }

    async fn task(store: &Arc<RuntimeStore>, id: &'static str) -> Value {
        store
            .read_async("runtime-services/storage_retention", move |connection| {
                let raw: String = connection.query_row(
                    "SELECT record FROM runtime_tasks WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                Ok::<_, studio_runtime_store::StoreError>(serde_json::from_str(&raw)?)
            })
            .await
            .expect("read task")
    }

    async fn marker(store: &Arc<RuntimeStore>) -> Option<(i64, i64, String, Option<String>)> {
        store.read_async("runtime-services/storage_retention", |connection| {
            Ok::<_, studio_runtime_store::StoreError>(connection
                .query_row(
                    "SELECT cursor,complete,status,error FROM runtime_payload_migrations WHERE name=?1",
                    [TASK_MARKER],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?)
        })
        .await
        .expect("read marker")
    }

    fn long_tail() -> String {
        format!("{}{}", "a".repeat(4096), "end of output")
    }

    #[tokio::test]
    async fn pass_trims_only_old_finished_output_outside_each_agents_newest_tasks() {
        let (_directory, store) = store().await;
        let tail = long_tail();
        let mut tasks = vec![
            json!({"id":"old-done","agent":"a","status":"completed","created":OLD - 1000.0,"tail":tail}),
            json!({"id":"old-running","agent":"a","status":"running","created":OLD - 900.0,"tail":tail}),
            json!({"id":"old-null-created","agent":"a","status":"failed","created":null,"tail":tail}),
            json!({"id":"old-short","agent":"a","status":"failed","created":OLD - 800.0,"tail":"short"}),
            json!({"id":"old-interrupted","agent":"a","status":"interrupted","created":OLD - 700.0,"tail":tail}),
            // Free-form non-final statuses keep their output.
            json!({"id":"old-approval","agent":"a","status":"approval","created":OLD - 600.0,"tail":tail}),
            json!({"id":"old-lost","agent":"a","status":"lost","created":OLD - 500.0,"tail":tail}),
            json!({"id":"old-no-status","agent":"a","created":OLD - 400.0,"tail":tail}),
            json!({"id":"recent-done","agent":"b","status":"completed","created":NOW - 60.0,"tail":tail}),
        ];
        // Agent "c" has one more old task than its newest-task window.
        for index in 0..=TASK_NEWEST_PER_AGENT {
            tasks.push(
                json!({"id":format!("c-{index:03}"),"agent":"c","status":"completed",
                "created":OLD - 500.0 + index as f64,"tail":tail}),
            );
        }
        // Newer tasks push agent "a"'s old tasks out of its newest window.
        for index in 0..TASK_NEWEST_PER_AGENT {
            tasks.push(
                json!({"id":format!("a-new-{index:03}"),"agent":"a","status":"completed",
                "created":NOW - 100.0 + index as f64,"tail":"x"}),
            );
        }
        insert_tasks(&store, tasks).await;
        // The retired Python retention run left an error marker at cursor 0.
        store
            .transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO runtime_payload_migrations(name,cursor,complete,status,error) \
                     VALUES(?1,0,0,'error','TypeError')",
                    [TASK_MARKER],
                )?;
                Ok(())
            })
            .await
            .expect("seed error marker");

        run_pass(&store, NOW).await.expect("retention pass");

        let trimmed = task(&store, "old-done").await;
        // The last 512 bytes of `long_tail()`.
        assert_eq!(
            trimmed["tail"],
            json!(format!("{}end of output", "a".repeat(499)))
        );
        assert_eq!(trimmed["outputPreviewOnly"], json!(true));
        assert_eq!(trimmed["outputTruncated"], json!(true));
        for trimmed in ["c-000", "old-interrupted"] {
            assert_eq!(
                task(&store, trimmed).await["outputPreviewOnly"],
                json!(true),
                "{trimmed}"
            );
        }
        for kept in [
            "old-running",
            "old-approval",
            "old-lost",
            "old-no-status",
            "old-null-created",
            "recent-done",
            "c-001",
            "c-100",
        ] {
            assert_eq!(task(&store, kept).await["tail"], json!(tail), "{kept}");
        }
        assert_eq!(task(&store, "old-short").await["tail"], json!("short"));
        let (_, complete, status, error) = marker(&store).await.expect("marker");
        assert_eq!((complete, status.as_str(), error), (1, "complete", None));

        // A repeated pass with nothing to trim submits no writer job.
        let generation = store.committed_change_generation();
        run_pass(&store, NOW).await.expect("idle pass");
        assert_eq!(store.committed_change_generation(), generation);
    }

    /// The owner keeps all history: a retention tick never shortens task
    /// output, whatever the task's age, status or position in its agent.
    #[tokio::test]
    async fn pass_keeps_the_full_output_of_old_finished_tasks() {
        const SEVEN_DAYS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
        const NEWEST_PER_AGENT: usize = 100;
        let (_directory, store) = store().await;
        let tail = long_tail();
        let mut tasks = ["completed", "failed", "interrupted"]
            .into_iter()
            .map(|status| {
                json!({"id":format!("old-{status}"),"agent":"a","status":status,
                    "created":NOW - SEVEN_DAYS - 1_000.0,"tail":tail})
            })
            .collect::<Vec<_>>();
        // Newer tasks put the old ones outside agent "a"'s newest tasks.
        for index in 0..NEWEST_PER_AGENT {
            tasks.push(
                json!({"id":format!("a-new-{index:03}"),"agent":"a","status":"completed",
                "created":NOW - 100.0 + index as f64,"tail":"x"}),
            );
        }
        insert_tasks(&store, tasks).await;
        let generation = store.committed_change_generation();

        run_pass(&store, NOW).await.expect("retention pass");

        for id in ["old-completed", "old-failed", "old-interrupted"] {
            let kept = task(&store, id).await;
            assert_eq!(kept["tail"], json!(tail), "{id} keeps its full tail");
            assert!(kept.get("outputPreviewOnly").is_none(), "{id}");
            assert!(kept.get("outputTruncated").is_none(), "{id}");
        }
        assert_eq!(
            store.committed_change_generation(),
            generation,
            "a pass without tombstones to prune writes nothing"
        );
    }

    #[tokio::test]
    async fn idle_pass_on_fresh_state_writes_nothing() {
        let (_directory, store) = store().await;
        let generation = store.committed_change_generation();
        run_pass(&store, NOW).await.expect("idle pass");
        assert_eq!(store.committed_change_generation(), generation);
        assert_eq!(marker(&store).await, None);
    }

    #[tokio::test]
    async fn batch_saves_a_resumable_cursor_and_a_repeated_window_is_a_no_op() {
        let (_directory, store) = store().await;
        let tail = long_tail();
        let tasks = (0..TASK_RETENTION_SCAN_ROWS + 3)
            .map(|index| {
                json!({"id":format!("t-{index:04}"),"agent":"a","status":"completed",
                    "created":OLD - index as f64,"tail":tail})
            })
            .collect::<Vec<_>>();
        insert_tasks(&store, tasks).await;

        let first = trim_task_batch(&store, 0, NOW).await.expect("first window");
        assert_eq!(first.cursor, TASK_RETENTION_SCAN_ROWS);
        assert_eq!(
            first.trimmed,
            (TASK_RETENTION_SCAN_ROWS - TASK_NEWEST_PER_AGENT) as usize
        );
        assert!(!first.done);
        let (cursor, complete, status, _) = marker(&store).await.expect("marker");
        assert_eq!(
            (cursor, complete, status.as_str()),
            (first.cursor, 0, "running")
        );
        let resumed = store
            .read_async("runtime-services/storage_retention", |tx| {
                task_marker_cursor(tx)
            })
            .await
            .expect("resume cursor");
        assert_eq!(resumed, first.cursor);
        let repeated = trim_task_batch(&store, 0, NOW)
            .await
            .expect("repeat window");
        assert_eq!(repeated.trimmed, 0);
    }

    #[tokio::test]
    async fn writer_skips_a_task_changed_after_the_reader_scan() {
        let (_directory, store) = store().await;
        let tail = long_tail();
        let mut tasks = vec![
            json!({"id":"changed","agent":"a","status":"completed","created":OLD,"tail":tail}),
        ];
        // Newer tasks push "changed" out of agent "a"'s newest window.
        for index in 0..TASK_NEWEST_PER_AGENT {
            tasks.push(
                json!({"id":format!("new-{index:03}"),"agent":"a","status":"completed",
                "created":NOW - 100.0 + index as f64,"tail":"x"}),
            );
        }
        insert_tasks(&store, tasks).await;
        let (candidates, _, _) = store
            .read_async("runtime-services/storage_retention", |connection| {
                scan_window(connection, 0, NOW)
            })
            .await
            .expect("scan");
        assert_eq!(candidates.len(), 1);

        // A writer appends output between the scan and the retention batch.
        let newer = format!("{tail} and more");
        let replacement =
            json!({"id":"changed","agent":"a","status":"completed","created":OLD,"tail":newer});
        store
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE runtime_tasks SET record=?1 WHERE id='changed'",
                    [replacement.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("update task after scan");

        let trimmed = store
            .transaction(move |transaction| apply_candidates(transaction, &candidates))
            .await
            .expect("apply scanned candidates");
        assert_eq!(trimmed, 0, "a stale scanned record is never written back");
        assert_eq!(task(&store, "changed").await["tail"], json!(newer));
    }

    #[tokio::test]
    async fn tombstone_pruning_keeps_the_limit_and_raises_the_floor() {
        const TRANSCRIPT_TOMBSTONES: i64 = 50;
        let (_directory, store) = store().await;
        let excess = ENTITY_TOMBSTONE_PRUNE_BATCH + 100;
        let total = ENTITY_TOMBSTONE_LIMIT + excess;
        store
            .transaction(move |transaction| {
                let mut statement = transaction.prepare(
                    "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) \
                     VALUES('agent',?1,?2,'h',NULL,1)",
                )?;
                for index in 0..total {
                    statement.execute(params![format!("gone-{index:05}"), 1_000 + index])?;
                }
                // Transcript tombstones are older than every entity tombstone
                // but belong to transcript sync; pruning never counts or
                // removes them.
                let mut transcript = transaction.prepare(
                    "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) \
                     VALUES('transcript:agent',?1,?2,'h',NULL,1)",
                )?;
                for index in 0..TRANSCRIPT_TOMBSTONES {
                    transcript.execute(params![format!("item-{index:03}"), 1 + index])?;
                }
                Ok(())
            })
            .await
            .expect("seed tombstones");

        run_pass(&store, NOW).await.expect("retention pass");

        let (count, floor, remaining) = store
            .read_async("runtime-services/storage_retention", |connection| {
                let count: String = connection.query_row(
                    "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'",
                    [],
                    |row| row.get(0),
                )?;
                let remaining: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM sync_entities \
                 WHERE deleted=1 AND collection NOT LIKE 'transcript:%'",
                    [],
                    |row| row.get(0),
                )?;
                let transcript: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM sync_entities \
                 WHERE deleted=1 AND collection LIKE 'transcript:%'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(transcript, TRANSCRIPT_TOMBSTONES);
                Ok::<_, studio_runtime_store::StoreError>((
                    count,
                    SyncDocumentsRepository::entity_tombstone_floor(connection)?,
                    remaining,
                ))
            })
            .await
            .expect("read tombstones");
        assert_eq!(count, ENTITY_TOMBSTONE_LIMIT.to_string());
        assert_eq!(remaining, ENTITY_TOMBSTONE_LIMIT);
        assert_eq!(floor, 1_000 + excess - 1);

        // A client whose cursor predates a pruned deletion resets instead of
        // silently missing it; a client at the floor already saw every one.
        let (behind, current) = store
            .transaction(move |transaction| {
                let pull = |after| {
                    SyncDocumentsRepository::pull_entities(
                        transaction,
                        after,
                        10,
                        false,
                        0,
                        true,
                        None,
                    )
                };
                Ok((pull(1_000)?, pull(floor)?))
            })
            .await
            .expect("pull entities");
        assert!(behind.reset, "a cursor below the floor resets");
        assert_eq!(behind.tombstone_floor, floor);
        assert!(!current.reset, "a cursor at the floor continues");
        assert!(current.rows.iter().all(|row| row.sequence > floor));
    }
}
