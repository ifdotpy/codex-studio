//! Background Canvas maintenance: entity tombstone pruning in short writer
//! transactions.
//!
//! Studio keeps all user history. This pass never shortens or deletes task
//! output, transcripts or any other record. Releases from 2026-10-17
//! (`478b2e176`) up to its removal also trimmed the output of finished tasks
//! older than seven days here; the `payload-v1:tasks` row that it left in
//! `runtime_payload_migrations` is a historical marker only.

use std::sync::Arc;
use std::time::Duration;

use studio_runtime_store::{RuntimeStore, StoreResult, SyncDocumentsRepository};

/// Pause between prune batches so interactive writers keep the actor.
pub const RETENTION_BATCH_PAUSE: Duration = Duration::from_millis(25);
/// Delay between complete passes. A pass that finds nothing to do performs no
/// writes.
pub const RETENTION_PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Run maintenance passes until shutdown. Stopping between awaits is safe: a
/// submitted batch commits or rolls back on its own.
pub async fn run(store: Arc<RuntimeStore>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        tokio::select! {
            result = run_pass(&store) => {
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

/// One complete pass: once due, prune entity tombstones down to their limit,
/// one batch at a time.
pub async fn run_pass(store: &Arc<RuntimeStore>) -> StoreResult<()> {
    if !store
        .read_async("runtime-services/storage_retention", |connection| {
            SyncDocumentsRepository::entity_tombstone_prune_due(connection)
        })
        .await?
    {
        return Ok(());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{OptionalExtension, params};
    use serde_json::{Value, json};
    use studio_runtime_store::{ENTITY_TOMBSTONE_LIMIT, ENTITY_TOMBSTONE_PRUNE_BATCH};

    const NOW: f64 = 2_000_000_000.0;

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

    /// The `payload-v1:tasks` row of the removed task trim.
    async fn task_trim_marker(store: &Arc<RuntimeStore>) -> Option<i64> {
        store
            .read_async("runtime-services/storage_retention", |connection| {
                Ok::<_, studio_runtime_store::StoreError>(
                    connection
                        .query_row(
                            "SELECT cursor FROM runtime_payload_migrations \
                             WHERE name='payload-v1:tasks'",
                            [],
                            |row| row.get(0),
                        )
                        .optional()?,
                )
            })
            .await
            .expect("read marker")
    }

    fn long_tail() -> String {
        format!("{}{}", "a".repeat(4096), "end of output")
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

        run_pass(&store).await.expect("retention pass");

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
        assert_eq!(task_trim_marker(&store).await, None);
    }

    #[tokio::test]
    async fn idle_pass_on_fresh_state_writes_nothing() {
        let (_directory, store) = store().await;
        let generation = store.committed_change_generation();
        run_pass(&store).await.expect("idle pass");
        assert_eq!(store.committed_change_generation(), generation);
        assert_eq!(task_trim_marker(&store).await, None);
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

        run_pass(&store).await.expect("retention pass");

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
