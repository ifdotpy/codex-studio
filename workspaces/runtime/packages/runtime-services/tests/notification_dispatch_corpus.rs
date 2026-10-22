//! Account limits belong to the durable account journal, not thread dispatch.
use serde_json::{Value, json};
use studio_runtime_providers::LineDecoder;
use studio_runtime_services::notification_dispatch::{
    NotificationStreamBuffer, ProviderDeliveryIdentity, PublicationContext, dispatch_notification,
};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};

#[tokio::test]
async fn live_limits_thread_dispatch_records_only_the_ignored_receipt() {
    // Main removed the Python notification corpus rather than freezing it.
    // Preserve this branch's narrow exception with exact native assertions.
    // account_worker::tests::limits_updates covers journal ingestion and replay.
    let root = tempfile::tempdir().expect("temporary state");
    let store = RuntimeStore::open(Some(root.path())).await.expect("store");
    let agent = json!({"id":"agent","rootId":"agent","epoch":1,"status":"running","events":0,
        "threadId":"thread","accountKey":"default","provider":"codex","isLead":true});
    store
        .transaction_with(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(agent).expect("agent"))
        })
        .await
        .expect("seed agent");
    let notification = json!({"method":"account/rateLimits/updated","params":{
        "threadId":"thread","rateLimits":{"primary":{"usedPercent":100.0,"resetsAt":9000.0}}
    }});
    let mut bytes = serde_json::to_vec(&notification).expect("notification JSON");
    bytes.push(b'\n');
    let frame = LineDecoder::new().push(&bytes).expect("frame").remove(0);
    store
        .transaction_with_error(move |tx| {
            let before = AgentRepository::get(tx, &"agent".into())?
                .expect("agent")
                .value();
            for _ in 0..2 {
                let result = dispatch_notification(
                    tx,
                    "agent",
                    &PublicationContext::default(),
                    &NotificationStreamBuffer::default(),
                    &ProviderDeliveryIdentity {
                        provider_handle: "account:default".into(),
                        generation: 7,
                        cursor: 1,
                    },
                    &frame,
                    1234.5,
                )?;
                assert_eq!(
                    json!(result.outcome),
                    json!({"status":"ignored","reason":"account-journal/limits-observation"})
                );
                assert_eq!(
                    AgentRepository::get(tx, &"agent".into())?
                        .expect("agent")
                        .value(),
                    before
                );
            }
            let receipt: String = tx
                .query_row("SELECT result FROM runtime_operation_receipts", [], |r| {
                    r.get(0)
                })
                .expect("ignored delivery receipt");
            assert_eq!(
                serde_json::from_str::<Value>(&receipt).expect("receipt JSON"),
                json!({"status":"ignored","reason":"account-journal/limits-observation"})
            );
            for table in ["runtime_events", "runtime_items"] {
                assert_eq!(
                    tx.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                        .expect("row count"),
                    0
                );
            }
            assert_eq!(
                tx.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                    .get::<_, i64>(0))
                    .expect("row count"),
                1
            );
            Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
        })
        .await
        .expect("account notice stays outside thread state");
}
