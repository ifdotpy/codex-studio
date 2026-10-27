use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_store::{StateCheckTable, analytics_capture::check_state_readonly};
use tempfile::tempdir;

fn report_for(root: &std::path::Path, table: &str) -> StateCheckTable {
    check_state_readonly(root)
        .expect("readonly analytics state check succeeds")
        .into_iter()
        .find(|report| report.table == table)
        .expect("expected analytics state report")
}

fn create_history(root: &std::path::Path) -> Connection {
    let db = Connection::open(root.join("analytics.sqlite3")).expect("open fixture database");
    db.execute_batch(
        "CREATE TABLE analytics_history(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL);",
    )
    .expect("create fixture history table");
    db
}

#[test]
fn compressed_item_records_receive_the_same_readonly_validation() {
    let root = tempdir().expect("temporary state");
    let db = create_history(root.path());
    db.execute_batch("CREATE TABLE analytics_items (id TEXT PRIMARY KEY, record TEXT NOT NULL);")
        .expect("item table");
    let text = json!({"id":"item","agentId":"agent","type":"agentMessage"}).to_string();
    db.execute("INSERT INTO analytics_items VALUES ('item',?1)", [&text])
        .expect("plain item");
    let plain = report_for(root.path(), "analytics_items");
    db.execute(
        "UPDATE analytics_items SET record=?1",
        [studio_runtime_store::stored_text::encode(&text)],
    )
    .expect("compressed item");
    let compressed = report_for(root.path(), "analytics_items");
    assert_eq!(compressed.row_count, plain.row_count);
    assert_eq!(compressed.rejected_count, plain.rejected_count);
}

fn add_history(db: &Connection, id: &str, agent: &str, record: Value) {
    db.execute(
        "INSERT INTO analytics_history(id,agent,record) VALUES (?1,?2,?3)",
        (id, agent, record.to_string()),
    )
    .expect("insert fixture history row");
}

#[test]
fn importer_diagnostic_writer_variants_have_a_separate_report() {
    let variants = [
        json!({"status":"error","error":"fixture error","updated":1.0}),
        json!({"status":"current","error":null,"lastError":"fixture error","updated":2.0}),
    ];
    for record in variants {
        let root = tempdir().expect("create temporary state root");
        let db = create_history(root.path());
        add_history(&db, "importer", "", record);
        drop(db);

        let history = report_for(root.path(), "analytics_history");
        assert_eq!(history.row_count, 0);
        assert_eq!(history.rejected_count, 0);

        let importer = report_for(root.path(), "analytics_history_importer");
        assert_eq!(importer.row_count, 1);
        assert_eq!(importer.rejected_count, 0);
    }
}

#[test]
fn malformed_importer_and_unknown_history_rows_still_fail_validation() {
    let root = tempdir().expect("create temporary state root");
    let db = create_history(root.path());
    add_history(
        &db,
        "importer",
        "",
        json!({"status":"error","error":"fixture error","updated":"bad"}),
    );
    add_history(
        &db,
        "unknown-diagnostic",
        "",
        json!({"status":"error","error":"fixture error","updated":1.0}),
    );
    drop(db);

    let importer = report_for(root.path(), "analytics_history_importer");
    assert_eq!(importer.row_count, 1);
    assert_eq!(importer.rejected_count, 1);
    assert!(
        importer
            .failures
            .iter()
            .any(|failure| { failure.keyword == "anyOf" && failure.count == 1 })
    );

    let history = report_for(root.path(), "analytics_history");
    assert_eq!(history.row_count, 1);
    assert_eq!(history.rejected_count, 1);
}

#[test]
fn importer_reserved_key_requires_the_python_owned_empty_agent_column() {
    let root = tempdir().expect("create temporary state root");
    let db = create_history(root.path());
    add_history(
        &db,
        "importer",
        "unexpected-agent",
        json!({"status":"error","error":"fixture error","updated":1.0}),
    );
    drop(db);

    let importer = report_for(root.path(), "analytics_history_importer");
    assert_eq!(importer.row_count, 1);
    assert_eq!(importer.rejected_count, 1);
    assert!(importer.failures.iter().any(|failure| {
        failure.pointer == "/agent" && failure.keyword == "rowKey" && failure.count == 1
    }));
}
