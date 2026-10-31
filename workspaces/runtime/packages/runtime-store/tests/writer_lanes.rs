use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use studio_runtime_store::{RuntimeStore, StoreError};

/// The row callback runs on the writer's dedicated blocking thread, so a blocking delay is safe.
#[allow(clippy::disallowed_methods)]
fn spend_row_cost() {
    std::thread::sleep(Duration::from_millis(1));
}

#[tokio::test]
async fn analytics_write_does_not_take_the_domain_writer() {
    let directory = tempfile::tempdir().expect("temporary state");
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("store");
    let before = store.writer_admissions();
    store
        .analytics_projection_transaction(|_| Ok::<(), StoreError>(()))
        .await
        .expect("analytics write");
    assert_eq!(store.writer_admissions(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_arriving_during_a_batch_commits_before_the_batch_ends() {
    const ROWS: u32 = 300;
    let directory = tempfile::tempdir().expect("temporary state");
    let store = Arc::new(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store"),
    );
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let command = Arc::new(Mutex::new(None));
    let started = Arc::new(AtomicBool::new(false));
    let handle = tokio::runtime::Handle::current();
    let batch_store = Arc::clone(&store);
    let batch_log = Arc::clone(&log);
    let batch_command = Arc::clone(&command);
    store
        .background_batch(0..ROWS, move |_, row| {
            // A test-only delay stands in for row cost; the part limit then splits the batch.
            spend_row_cost();
            batch_log.lock().expect("log").push(format!("row {row}"));
            if !started.swap(true, Ordering::SeqCst) {
                let store = Arc::clone(&batch_store);
                let log = Arc::clone(&batch_log);
                *batch_command.lock().expect("command") = Some(handle.spawn(async move {
                    store
                        .transaction(move |_| {
                            log.lock().expect("log").push("command".to_owned());
                            Ok::<(), StoreError>(())
                        })
                        .await
                }));
            }
            Ok(())
        })
        .await
        .expect("batch");
    let task = command
        .lock()
        .expect("command")
        .take()
        .expect("command task");
    task.await.expect("command task").expect("command commit");
    let log = log.lock().expect("log");
    let command_at = log
        .iter()
        .position(|entry| entry == "command")
        .expect("command ran");
    assert!(
        command_at < log.len() - 1,
        "the command waited for the whole batch"
    );
    assert_eq!(log.len(), ROWS as usize + 1);
}
