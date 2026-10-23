//! Analytics reads return the text of a compressed `analytics_items.record`.

use serde_json::{Value, json};
use studio_runtime_store::{AnalyticsRepository, RuntimeStore, analytics, stored_text};

#[tokio::test]
async fn analytics_reads_decode_compressed_item_records() {
    let directory = tempfile::tempdir().expect("temporary state");
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("store opens");
    let record = json!({"id":"item-1","agentId":"agent","rootId":"root","itemId":"message-1",
        "type":"agentMessage","output":{"bytes":12}})
    .to_string();
    // A plain connection, as an older or newer writer, stores the compressed form.
    let raw = rusqlite::Connection::open(directory.path().join("analytics.sqlite3"))
        .expect("analytics opens");
    raw.execute(
        "INSERT INTO analytics_items VALUES ('item-1','agent','root',NULL,NULL,1.0,'agentMessage',NULL,0,?1)",
        [stored_text::encode(&record)],
    )
    .expect("compressed record is stored");

    let reader = AnalyticsRepository::new(directory.path())
        .open_read_only()
        .expect("analytics reader opens");
    let item = analytics::message_item(&reader, "agent", "message-1", None)
        .expect("item reads")
        .expect("item exists");

    assert_eq!(
        item,
        serde_json::from_str::<Value>(&record).expect("record JSON")
    );
    drop(store);
}
