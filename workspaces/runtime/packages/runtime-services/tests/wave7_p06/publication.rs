//! Private durable journal and idempotent consumer: commit/publication/ACK crashes.
use super::{Oracle, State, post_commit_value};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use studio_runtime_providers::LineDecoder;
use studio_runtime_services::notification_dispatch::{
    NotificationDispatchError, NotificationPublicationBatch, NotificationStreamBuffer,
    ProviderDeliveryIdentity, PublicationContext, dispatch_notification_with_effects,
};
use studio_runtime_services::notification_runtime_effects::RuntimeNotificationEffects;
use studio_runtime_store::RuntimeStore;

fn journal(
    store: &RuntimeStore,
    frame: &Value,
    cursor: u64,
    fail: bool,
) -> Result<(), NotificationDispatchError> {
    let mut bytes = serde_json::to_vec(frame).expect("test invariant");
    bytes.push(b'\n');
    let frame = LineDecoder::new()
        .push(&bytes)
        .expect("test invariant")
        .remove(0);
    let publication = PublicationContext::load(store.root()).expect("test invariant");
    store.transaction_with(|tx| {
        let raw: String = tx
            .query_row("SELECT payload FROM p06_buffer WHERE id=1", [], |row| {
                row.get(0)
            })
            .map_err(studio_runtime_store::StoreError::from)?;
        let streams: NotificationStreamBuffer = serde_json::from_str(&raw)?;
        let dispatched = dispatch_notification_with_effects(
            tx,
            "agent",
            &publication,
            &streams,
            &ProviderDeliveryIdentity {
                provider_handle: "account:default".into(),
                generation: 7,
                cursor,
            },
            &frame,
            1234.5,
            &RuntimeNotificationEffects,
        )?;
        let next = dispatched.stream_update.unwrap_or(streams);
        if !dispatched.post_commit.is_empty() {
            if fail {
                return Err(NotificationDispatchError::InvalidNotification);
            }
            tx.execute(
                "INSERT INTO p06_journal(id,sequence,payload,ack) VALUES(?1,?2,?3,0)",
                params![
                    format!(
                        "hash-order-{:020}",
                        u64::MAX - dispatched.post_commit.sequence
                    ),
                    i64::try_from(dispatched.post_commit.sequence).expect("test invariant"),
                    serde_json::to_string(&dispatched.post_commit)?
                ],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        }
        tx.execute(
            "UPDATE p06_buffer SET payload=?1 WHERE id=1",
            [serde_json::to_string(&next)?],
        )
        .map_err(studio_runtime_store::StoreError::from)?;
        Ok(())
    })
}

fn drain(root: &Path, crash_after_publish: bool) {
    let store = RuntimeStore::open(Some(root)).expect("test invariant");
    let mut sink = Connection::open(root.join("p06-sink.sqlite3")).expect("test invariant");
    sink.execute_batch("CREATE TABLE IF NOT EXISTS applied(position INTEGER PRIMARY KEY AUTOINCREMENT,sequence INTEGER,ordinal INTEGER,payload TEXT,UNIQUE(sequence,ordinal));").expect("test invariant");
    let batches: Vec<NotificationPublicationBatch> = {
        let read = store.read().expect("test invariant");
        read.prepare("SELECT payload FROM p06_journal WHERE ack=0 ORDER BY sequence")
            .expect("test invariant")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("test invariant")
            .map(|row| serde_json::from_str(&row.expect("test invariant")).expect("test invariant"))
            .collect()
    };
    for batch in batches {
        for (ordinal, effect) in batch.effects.iter().enumerate() {
            let tx = sink.transaction().expect("test invariant");
            tx.execute(
                "INSERT OR IGNORE INTO applied(sequence,ordinal,payload) VALUES(?1,?2,?3)",
                params![
                    i64::try_from(batch.sequence).expect("test invariant"),
                    ordinal as i64,
                    serde_json::to_string(effect).expect("test invariant")
                ],
            )
            .expect("test invariant");
            tx.commit().expect("test invariant");
            // Simulate abrupt process loss after the consumer durably applied an
            // effect, before the producer acknowledges any part of this batch.
            if crash_after_publish {
                return;
            }
        }
        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE p06_journal SET ack=1 WHERE sequence=?1",
                    [i64::try_from(batch.sequence).expect("test invariant")],
                )?;
                Ok(())
            })
            .expect("test invariant");
    }
}

#[test]
fn wave7_p06_commit_publication_restart_preserves_order() {
    if let Some(root) = std::env::var_os("P06_PUBLICATION_CHILD") {
        drain(Path::new(&root), true);
        return;
    }
    let root = State(std::env::temp_dir().join(format!("p06-publication-{}", std::process::id())));
    fs::create_dir(&root.0).expect("test invariant");
    let mut oracle = Oracle::new(&root.0);
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../wave7_p06_notification_cases.json"))
            .expect("test invariant");
    let case = cases
        .into_iter()
        .find(|case| case["id"] == "p06-DSC-ticks0")
        .expect("test invariant");
    let rust = root.0.join("case/rust");
    let expected = oracle
        .call(json!({"command":"seed","root":root.0.join("case/python"),"rust":rust,"case":case}));
    let store = RuntimeStore::open(Some(&rust)).expect("test invariant");
    store.transaction(|tx| {
        tx.execute_batch("CREATE TABLE p06_journal(id TEXT PRIMARY KEY,sequence INTEGER UNIQUE,payload TEXT,ack INTEGER); CREATE TABLE p06_buffer(id INTEGER PRIMARY KEY,payload TEXT);")?;
        tx.execute("INSERT INTO p06_buffer VALUES(1,?1)",[serde_json::to_string(&NotificationStreamBuffer::default()).expect("test invariant")])?; Ok(())
    }).expect("test invariant");
    let first = &case["steps"][0]["frame"];
    assert!(journal(&store, first, 1, true).is_err());
    assert_eq!(store.read().expect("test invariant").query_row("SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'provider-notification:%'",[],|r|r.get::<_,i64>(0)).expect("test invariant"),0,"journal staging failure must roll receipt back");
    for (index, step) in case["steps"]
        .as_array()
        .expect("test invariant")
        .iter()
        .enumerate()
    {
        journal(&store, &step["frame"], index as u64 + 1, false).expect("test invariant");
    }
    let order: Vec<i64> = store
        .read()
        .expect("test invariant")
        .prepare("SELECT sequence FROM p06_journal ORDER BY id")
        .expect("test invariant")
        .query_map([], |r| r.get(0))
        .expect("test invariant")
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        order,
        vec![3, 2, 1],
        "fixture must expose hash-order recovery"
    );
    drop(store); // No publications have happened: recover only durable state.
    assert!(!rust.join("p06-sink.sqlite3").exists());
    let child = Command::new(std::env::current_exe().expect("test invariant"))
        .args([
            "--exact",
            "publication::wave7_p06_commit_publication_restart_preserves_order",
            "--nocapture",
        ])
        .env("P06_PUBLICATION_CHILD", &rust)
        .output()
        .expect("test invariant");
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let reopened = RuntimeStore::open(Some(&rust)).expect("test invariant");
    assert_eq!(
        reopened
            .read()
            .expect("test invariant")
            .query_row("SELECT sum(ack) FROM p06_journal", [], |r| r
                .get::<_, i64>(0))
            .expect("test invariant"),
        0,
        "publication crash precedes ACK"
    );
    // Redelivered frames cannot append a second batch or advance its order.
    for (index, step) in case["steps"]
        .as_array()
        .expect("test invariant")
        .iter()
        .enumerate()
    {
        journal(&reopened, &step["frame"], index as u64 + 1, false).expect("test invariant");
    }
    drop(reopened);
    drain(&rust, false);
    drain(&rust, false); // ACKed batches cannot republish.
    let sink = Connection::open(rust.join("p06-sink.sqlite3")).expect("test invariant");
    let published: Vec<Value> = sink
        .prepare("SELECT payload FROM applied ORDER BY position")
        .expect("test invariant")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("test invariant")
        .map(|row| {
            post_commit_value(
                &serde_json::from_str(&row.expect("test invariant")).expect("test invariant"),
            )
        })
        .collect();
    let methods: Vec<_> = published
        .iter()
        .filter(|effect| effect["kind"] == "analytics")
        .map(|effect| effect["method"].as_str().expect("test invariant"))
        .collect();
    assert_eq!(
        methods,
        ["item/started", "item/agentMessage/delta", "item/completed"]
    );
    assert_eq!(
        published.len(),
        6,
        "publication-before-ACK replay must be deduplicated"
    );
    let analytics=oracle.call(json!({"command":"post_commit","module":"notification_dispatch","root":rust,"effects":published}));
    assert_eq!(
        analytics, expected["analytics"],
        "recovered publication order preserves real Python analytics rows"
    );
}
