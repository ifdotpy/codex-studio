//! Frozen callback data and its canonical SQLite seed for both test binaries.
use serde::Deserialize;
use serde_json::Value;
use studio_runtime_store::RuntimeStore;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Seed {
    #[serde(rename = "canvas.sqlite3")]
    canvas: Vec<String>,
    #[serde(rename = "analytics.sqlite3")]
    analytics: Vec<String>,
}

pub(crate) async fn state() -> (tempfile::TempDir, RuntimeStore, Value) {
    let root = tempfile::tempdir().expect("isolated callback state");
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("callback store");
    let seed: Seed = serde_json::from_str(include_str!("seed.json")).expect("callback seed");
    let statements = seed.canvas.join("\n");
    store
        .transaction(move |tx| {
            tx.execute_batch(&statements)?;
            Ok(())
        })
        .await
        .expect("seed callback rows");
    let analytics = seed.analytics.join("\n");
    store
        .analytics_transaction::<_, studio_runtime_store::StoreError>(move |tx, _| {
            tx.execute_batch(&analytics)?;
            Ok(())
        })
        .await
        .expect("seed callback analytics");
    let value = serde_json::from_str(include_str!("expected.json")).expect("callback expectations");
    (root, store, value)
}
