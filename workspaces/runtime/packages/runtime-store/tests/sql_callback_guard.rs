#![cfg(debug_assertions)]

use std::path::Path;
use studio_runtime_store::{RuntimeStore, StoreError, debug_assert_outside_sql, path_identity};

fn guarded_path_read_panics() -> bool {
    std::panic::catch_unwind(|| path_identity::resolve(Path::new("/"))).is_err()
}

#[tokio::test]
async fn filesystem_owner_refuses_reads_and_writes_and_guard_unwinds() {
    let directory = tempfile::tempdir().expect("temporary state");
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("store");
    let refused = store
        .read_async("tests/sql-guard", |_| {
            Ok::<_, StoreError>(guarded_path_read_panics())
        })
        .await
        .expect("read callback");
    assert!(
        refused,
        "path identity must refuse filesystem work in a read callback"
    );
    let refused = store
        .transaction(|_| Ok(guarded_path_read_panics()))
        .await
        .expect("write callback");
    assert!(
        refused,
        "path identity must refuse filesystem work in a write callback"
    );
    debug_assert_outside_sql();
    assert!(path_identity::resolve(directory.path()).is_some());
}
