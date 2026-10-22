//! Count publication work at the real journal and timer callers, before hash deduplication.
use super::journal_cost::{entry, journal, running_store};
use super::*;
use crate::scheduler::publication_work::{self, Calls};
use std::collections::BTreeMap;

const AGENTS: [(&str, &str); 2] = [("agent", "thread"), ("second", "second-thread")];
const FIRST_CURSOR: u64 = 1;
const LONG_ANSWER_CHARS: usize = crate::sync_views::AGENT_SYNC_LAST_ANSWER_CHARS + 1;

async fn fixture() -> Arc<RuntimeStore> {
    let (_directory, store) = running_store().await;
    store
        .transaction(|tx| {
            let mut second = AgentRepository::get(tx, &AgentId::from("agent"))?
                .expect("fixture agent")
                .value();
            second["id"] = json!("second");
            second["rootId"] = json!("second");
            second["threadId"] = json!("second-thread");
            second["startAttempt"]["threadId"] = json!("second-thread");
            AgentRepository::put(tx, &AgentRecord::from_value(second)?)
        })
        .await
        .expect("second agent");
    store
}

async fn start_counting(store: &RuntimeStore) {
    store
        .transaction(|_| {
            publication_work::start();
            drop(studio_runtime_store::agent_volatile::publication_work::take());
            Ok(())
        })
        .await
        .expect("start writer accounting");
}

async fn calls(store: &RuntimeStore) -> BTreeMap<String, Calls> {
    store
        .transaction(|_| Ok(publication_work::take()))
        .await
        .expect("read writer accounting")
}

fn assert_once(calls: BTreeMap<String, Calls>) {
    let expected = AGENTS
        .into_iter()
        .map(|(id, _)| {
            (
                id.to_owned(),
                Calls {
                    projections: 1,
                    validations: 1,
                },
            )
        })
        .collect();
    assert_eq!(
        calls, expected,
        "one projection and final-row validation per changed agent"
    );
}

fn notifications() -> Vec<JournalEntry> {
    AGENTS
        .into_iter()
        .flat_map(|(_, thread)| {
            [
                json!({"method":"item/started","params":{"threadId":thread,"turnId":"turn",
            "item":{"id":"answer","type":"agentMessage","text":""}}}),
                json!({"method":"item/completed","params":{"threadId":thread,"turnId":"turn",
            "item":{"id":"answer","type":"agentMessage","text":"x".repeat(LONG_ANSWER_CHARS)}}}),
            ]
        })
        .enumerate()
        .map(|(index, frame)| entry(FIRST_CURSOR + index as u64, frame))
        .collect()
}

/// The canonical projection witness now exercises the outer journal owner; a batch
/// with repeated source writes must still project and validate each agent once.
#[tokio::test]
async fn long_answers_use_canonical_bounded_agent_sync_projection() {
    let store = fixture().await;
    start_counting(&store).await;
    let streams = journal(&store, NotificationStreamBuffer::default(), notifications()).await;
    assert_once(calls(&store).await);
    store
        .transaction(|tx| {
            for (id, _) in AGENTS {
                let raw: String = tx.query_row(
                    "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                let envelope: Value = serde_json::from_str(&raw)?;
                let payload = &envelope["value"];
                assert_eq!(
                    payload["lastAnswer"].as_str().map(str::len),
                    Some(crate::sync_views::AGENT_SYNC_LAST_ANSWER_CHARS)
                );
                assert!(payload.get("turnEpoch").is_none());
                assert!(payload["startAttempt"].get("id").is_none());
            }
            Ok(())
        })
        .await
        .expect("canonical bounded projection");
    start_counting(&store).await;
    journal(&store, streams, notifications()).await;
    assert!(
        calls(&store).await.is_empty(),
        "exact replay does no publication work"
    );
}

#[tokio::test]
async fn stream_flush_batch_publishes_each_agent_once() {
    let store = fixture().await;
    let entries = AGENTS
        .into_iter()
        .enumerate()
        .map(|(index, (_, thread))| {
            entry(
                FIRST_CURSOR + index as u64,
                json!({"method":"item/agentMessage/delta","params":{"threadId":thread,
            "turnId":"turn","itemId":"answer","delta":"pending"}}),
            )
        })
        .collect();
    journal(&store, NotificationStreamBuffer::default(), entries).await;
    start_counting(&store).await;
    store::flush_streams(Arc::clone(&store), "default", Vec::new())
        .await
        .expect("timer flush");
    assert_once(calls(&store).await);
    start_counting(&store).await;
    store::flush_streams(Arc::clone(&store), "default", Vec::new())
        .await
        .expect("empty timer flush");
    assert!(
        calls(&store).await.is_empty(),
        "a settled flush does not republish"
    );
}

#[tokio::test]
async fn journal_projection_failure_rolls_back_sources_and_receipts() {
    let store = fixture().await;
    store.transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER reject_agent_projection BEFORE INSERT ON sync_entities WHEN NEW.collection='agent' BEGIN SELECT RAISE(ABORT,'projection failure'); END;")?;
        Ok(())
    }).await.expect("publication failpoint");
    let result = store::journal_batch(
        Arc::clone(&store),
        "default",
        "default",
        &NotificationStreamBuffer::default(),
        notifications(),
        &Value::Null,
        None,
        None,
        Vec::new(),
        None,
        false,
    )
    .await;
    assert!(
        result.is_err(),
        "outer publication failure must fail the batch"
    );
    store
        .transaction(|tx| {
            for (id, _) in AGENTS {
                let agent = AgentRepository::get(tx, &AgentId::from(id))?.expect("source agent");
                assert_eq!(agent.value()["events"], 0);
            }
            let receipt_count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM runtime_operation_receipts",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                receipt_count, 0,
                "projection failure rolls back every receipt and stream record"
            );
            tx.execute_batch("DROP TRIGGER reject_agent_projection")?;
            Ok(())
        })
        .await
        .expect("atomic rollback");
    start_counting(&store).await;
    journal(&store, NotificationStreamBuffer::default(), notifications()).await;
    assert_once(calls(&store).await);
}

#[tokio::test]
async fn changed_agents_decode_once_through_final_publication() {
    let store = fixture().await;
    store
        .transaction(|tx| {
            let mut unrelated = AgentRepository::get(tx, &AgentId::from("agent"))?
                .expect("source agent")
                .value();
            unrelated["id"] = json!("unrelated");
            unrelated["threadId"] = json!("unrelated-thread");
            unrelated["startAttempt"]["threadId"] = json!("unrelated-thread");
            AgentRepository::put(tx, &AgentRecord::from_value(unrelated)?)
        })
        .await
        .expect("unrelated agent");
    store
        .transaction(|tx| {
            let changes = crate::sync_policy::EntityChanges {
                agents: AGENTS
                    .into_iter()
                    .map(|(id, _)| id.to_owned())
                    .chain(["unrelated".to_owned()])
                    .collect(),
                ..Default::default()
            };
            crate::sync_policy::publish_entity_changes(tx, &changes, None, &|_, _, _, _| true)
        })
        .await
        .expect("initial projections");
    start_counting(&store).await;
    let streams = journal(&store, NotificationStreamBuffer::default(), notifications()).await;
    assert_once(calls(&store).await);
    let measured = store
        .transaction(|_| Ok(studio_runtime_store::agent_volatile::publication_work::take()))
        .await
        .expect("decode accounting");
    let expected = AGENTS
        .into_iter()
        .map(|(id, _)| {
            (
                id.to_owned(),
                studio_runtime_store::agent_volatile::publication_work::Calls {
                    decodes: 1,
                    volatile_loads: 1,
                    text_round_trips: 0,
                },
            )
        })
        .collect();
    assert_eq!(
        measured, expected,
        "decode/merge once, no intermediate text, no unrelated agent"
    );
    start_counting(&store).await;
    journal(&store, streams, notifications()).await;
    assert!(calls(&store).await.is_empty());
    assert!(
        store
            .transaction(|_| {
                Ok(studio_runtime_store::agent_volatile::publication_work::take().is_empty())
            })
            .await
            .expect("replay accounting"),
        "replay does no decoding"
    );
}
