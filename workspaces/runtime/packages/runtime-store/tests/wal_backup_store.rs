//! The write transaction alert and the WAL backup hook on a real store.
use std::fs;
use std::time::Duration;

use studio_runtime_store::RuntimeStore;
use studio_runtime_store::wal_backup::{SNAPSHOT_FILE, restore};

async fn insert(store: &RuntimeStore, rows: i64) {
    store
        .transaction(move |tx| {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS backup_witness(value BLOB NOT NULL);")?;
            for _ in 0..rows {
                tx.execute("INSERT INTO backup_witness VALUES (zeroblob(1500))", [])?;
            }
            Ok(())
        })
        .await
        .expect("witness commit");
}

#[tokio::test]
async fn a_slow_write_transaction_raises_one_alert() {
    let state = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(state.path())).await.expect("open");
    store.set_write_alert_threshold(Duration::from_millis(50));
    let before = store.write_alerts();
    insert(&store, 1).await;
    assert_eq!(store.write_alerts(), before, "a fast transaction is quiet");
    store
        .transaction(|_| {
            // The injected slow transaction.
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(Duration::from_millis(120));
            Ok(())
        })
        .await
        .expect("slow transaction");
    assert_eq!(store.write_alerts(), before + 1);
}

#[tokio::test]
async fn checkpoints_ship_the_wal_and_the_generation_restores() {
    let state = tempfile::tempdir().expect("state");
    let store = RuntimeStore::open(Some(state.path())).await.expect("open");
    insert(&store, 20).await;
    let generation = store.begin_backup_generation().await.expect("begin");
    let backup = store.install_wal_backup().expect("backup");
    let received = state.path().join("received").join(&generation);
    fs::create_dir_all(&received).expect("receiver folder");
    fs::copy(
        state.path().join("canvas.sqlite3"),
        received.join(SNAPSHOT_FILE),
    )
    .expect("snapshot copy");
    assert!(backup.end_snapshot());
    insert(&store, 30).await;
    store.checkpoint_wal_passive().await.expect("flush");
    // The flush finished the WAL, so the next commit restarts it.
    insert(&store, 10).await;
    store.checkpoint_wal_passive().await.expect("flush");
    assert_eq!(backup.status().broken, None);
    for segment in backup.segments() {
        fs::copy(&segment.path, received.join(&segment.name)).expect("segment copy");
    }
    let out = state.path().join("restored.sqlite3");
    let restored = restore(&received, &out).expect("restore");
    assert!(restored.wal_files >= 2, "{restored:?}");
    let count = |path: &std::path::Path| -> i64 {
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open")
            .query_row("SELECT count(*) FROM backup_witness", [], |row| row.get(0))
            .expect("count")
    };
    assert_eq!(count(&out), 60);
}
