use studio_runtime_store::{RuntimeStore, StoreError};

#[tokio::test]
async fn terminal_output_never_takes_the_canvas_writer() {
    let directory = tempfile::tempdir().expect("temporary state");
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("store");
    let before = store.writer_admissions();
    store
        .terminal_output_append("t", 0, "abc".to_owned())
        .await
        .expect("append");
    store
        .terminal_output_append("t", 3, "def".to_owned())
        .await
        .expect("append more");
    assert_eq!(
        store.terminal_output_start("t").await.expect("start"),
        Some(0)
    );
    assert_eq!(
        store
            .terminal_output_chunks("t", 0, 6, 10)
            .await
            .expect("chunks"),
        vec![(0, 6, "abcdef".to_owned())]
    );
    store.terminal_output_delete("t").await.expect("delete");
    assert_eq!(store.terminal_output_start("t").await.expect("start"), None);
    assert_eq!(store.writer_admissions(), before);
}

#[tokio::test]
async fn output_left_in_canvas_moves_to_the_archive_on_open() {
    let directory = tempfile::tempdir().expect("temporary state");
    {
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO user_terminal_output VALUES('old',0,3,'abc')",
                    [],
                )?;
                Ok::<(), StoreError>(())
            })
            .await
            .expect("legacy row");
    }
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("reopen");
    assert_eq!(
        store
            .terminal_output_chunks("old", 0, 3, 10)
            .await
            .expect("chunks"),
        vec![(0, 3, "abc".to_owned())]
    );
    let canvas_rows: i64 = store
        .test_reader()
        .expect("reader")
        .query_row("SELECT COUNT(*) FROM user_terminal_output", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(canvas_rows, 0);
}
