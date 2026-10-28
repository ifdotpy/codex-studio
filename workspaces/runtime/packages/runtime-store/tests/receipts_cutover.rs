use rusqlite::Connection;
use studio_runtime_store::RuntimeStore;

#[tokio::test]
async fn receipt_cutover_unclassifiable_row_stops_open_without_deleting_source() {
    let root = tempfile::tempdir().expect("state");
    drop(RuntimeStore::open(Some(root.path())).await.expect("fresh"));
    let db = Connection::open(root.path().join("canvas.sqlite3")).expect("database");
    db.execute_batch("CREATE TABLE IF NOT EXISTS runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL); INSERT INTO runtime_operation_receipts VALUES('unclassified-operation','unclassified-signature','{}');").expect("old source");
    let error = RuntimeStore::open(Some(root.path()))
        .await
        .err()
        .expect("unknown family must stop startup");
    let detail = format!("{error:?}");
    assert!(detail.contains("runtime_operation_receipts"), "{detail}");
    assert!(detail.contains("unclassified-operation"), "{detail}");
    assert!(detail.contains("unclassified-signature"), "{detail}");
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_operation_receipts WHERE id='unclassified-operation'",
            [],
            |row| row.get(0),
        )
        .expect("source preserved");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn receipt_cutover_dropped_tables_stay_absent_on_second_open() {
    let root = tempfile::tempdir().expect("state");
    for _ in 0..2 {
        drop(RuntimeStore::open(Some(root.path())).await.expect("open"));
        let db = Connection::open(root.path().join("canvas.sqlite3")).expect("database");
        for table in [
            "runtime_operation_receipts",
            "runtime_native_action_receipts",
            "runtime_context_terminal_receipts",
            "user_terminal_receipts",
            "runtime_access_receipts",
            "runtime_tool_results",
            "runtime_tool_request_aliases",
        ] {
            let exists: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                    [table],
                    |row| row.get(0),
                )
                .expect("schema");
            assert!(!exists, "{table} must stay absent");
        }
    }
}
