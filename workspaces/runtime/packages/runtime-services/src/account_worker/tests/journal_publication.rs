//! Count publication work at the real journal and timer callers, before hash deduplication.
use super::journal_cost::{entry, journal, running_store};
use super::*;
use crate::scheduler::publication_work::{self, Calls};
use std::collections::BTreeMap;

const AGENTS: [(&str, &str); 2] = [("agent", "thread"), ("second", "second-thread")];
const FIRST_CURSOR: u64 = 1;
const LONG_ANSWER_CHARS: usize = crate::sync_views::AGENT_SYNC_LAST_ANSWER_CHARS + 1;

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let (directory, store) = running_store().await;
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
    (directory, store)
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
    let (_directory, store) = fixture().await;
    drop(
        store
            .test_reader()
            .expect("fixture database remains readable"),
    );
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
    let (_directory, store) = fixture().await;
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
    let (_directory, store) = fixture().await;
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
    let (_directory, store) = fixture().await;
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

/// Recovery may commit while the account worker still holds an older cache.
/// The next journal must build on the committed buffer, preserving its receipts.
#[tokio::test]
async fn journal_uses_committed_streams_after_recovery_advances_publications() {
    let (_directory, store) = running_store().await;
    let stale = NotificationStreamBuffer::default();
    let committed = journal(
        &store,
        stale.clone(),
        vec![entry(
            1,
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread",
            "turnId":"turn","itemId":"answer","delta":"before"}}),
        )],
    )
    .await;
    let updated = journal(
        &store,
        stale,
        vec![entry(
            2,
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread",
            "turnId":"turn","itemId":"answer","delta":"after"}}),
        )],
    )
    .await;
    assert!(updated.changed_from(&committed));
    store::flush_streams(Arc::clone(&store), "default", Vec::new())
        .await
        .expect("flush preserved text");
    let (record, _) = store
        .transaction(|tx| {
            studio_runtime_store::NotificationDispatchRepository::transcript_item(
                tx,
                "agent",
                "agent:answer",
            )
        })
        .await
        .expect("transcript read")
        .expect("answer transcript");
    assert_eq!(record["text"], "beforeafter");
    // Exact journal replay leaves the committed buffer untouched even with stale cache input.
    let replay = journal(
        &store,
        committed,
        vec![entry(
            2,
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread",
            "turnId":"turn","itemId":"answer","delta":"after"}}),
        )],
    )
    .await;
    assert_eq!(
        replay,
        store
            .transaction_with(|tx| store::load_streams(tx, "default"))
            .await
            .expect("durable streams")
    );
}

async fn recovery_preserves_interleaved_journal(drain: bool) {
    use crate::recovery::{CompletionMode, RecoveryAction, RecoveryEffects, RecoveryOutcome};
    use crate::recovery_history::ConnectionScope;
    use crate::recovery_policy::RecoverySnapshot;
    let store = fixture().await;
    store.transaction(|tx| {
        let mut a = AgentRepository::get(tx, &AgentId::from("agent"))?.expect("agent A").value();
        a["startAttempt"]["events"] = json!(["recovery-input-a"]);
        AgentRepository::put(tx, &AgentRecord::from_value(a)?)?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id) VALUES('recovery-input-a','agent','user','input A','delivered',1,4,'turn')", [])?;
        Ok(())
    }).await.expect("authorized running input for A");
    journal(
        &store,
        NotificationStreamBuffer::default(),
        vec![entry(
            1,
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread",
            "turnId":"turn","itemId":"recovered-one","delta":"partial A"}}),
        )],
    )
    .await;
    let completion = super::super::connection_recovery::lifecycle_completion(
        &store,
        "default",
        "interleaved-recovery".into(),
        1000.0,
    )
    .await
    .expect("recovery writer");
    let expected = store
        .transaction(|tx| {
            Ok(AgentRepository::get(tx, &AgentId::from("agent"))?
                .expect("agent A")
                .value())
        })
        .await
        .expect("snapshot A before history read");
    let snapshot = RecoverySnapshot::from_record(expected).expect("recovery snapshot");
    let streams = journal(
        &store,
        NotificationStreamBuffer::default(),
        vec![entry(
            2,
            json!({"method":"item/agentMessage/delta","params":{"threadId":"second-thread",
            "turnId":"turn","itemId":"answer-b","delta":"pending B"}}),
        )],
    )
    .await;
    let before: Value = serde_json::to_value(&streams).expect("stream JSON");
    if drain {
        store.transaction(|tx| {
            tx.execute("DELETE FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:%'", [])?;
            Ok(())
        }).await.expect("receipt deletion after successful delivery");
    }
    let mut recovery = crate::recovery_store::RecoveryStore {
        store: Arc::clone(&store),
        connection_current: Arc::new(|_| true),
        completion,
        at: 1000.0,
        observation_current: Arc::new(|_| None),
    };
    let scope = ConnectionScope {
        account: "default".into(),
        connection: "fixture-connection".into(),
        generation: 7,
    };
    let action = RecoveryAction::Terminal {
        turn: json!({"id":"turn","threadId":"thread","status":"completed","items":[
            {"id":"recovered-one","type":"agentMessage","text":"recovered one"},
            {"id":"recovered-two","type":"agentMessage","text":"recovered two"}]}),
        native_state: "idle".into(),
        mode: CompletionMode::ObservedTurn,
    };
    let result = recovery
        .apply(&snapshot, &scope, action.clone())
        .await
        .expect("A recovery commits after B journal");
    assert!(matches!(result.outcome, RecoveryOutcome::Applied(_)));
    let committed = store
        .transaction_with(|tx| store::load_streams(tx, "default"))
        .await
        .expect("committed streams");
    let after = serde_json::to_value(&committed).expect("stream JSON");
    assert!(
        after["publication_sequence"].as_u64().expect("sequence")
            > before["publication_sequence"].as_u64().expect("sequence")
    );
    assert_eq!(
        committed.pending_bytes(),
        "pending B".len(),
        "A recovery preserves B pending text"
    );
    store::flush_streams(Arc::clone(&store), "default", Vec::new())
        .await
        .expect("flush B");
    let item = store
        .transaction(|tx| {
            studio_runtime_store::NotificationDispatchRepository::transcript_item(
                tx,
                "second",
                "second:answer-b",
            )
        })
        .await
        .expect("B transcript")
        .expect("B answer")
        .0;
    assert_eq!(item["text"], "pending B");
    // The exact successful recovery retry must not publish either recovered item twice.
    let retried = recovery
        .apply(&snapshot, &scope, action)
        .await
        .expect("stale recovery retry");
    assert!(matches!(retried.outcome, RecoveryOutcome::Superseded));
}

#[tokio::test]
async fn recovery_uses_committed_streams_with_pending_publications() {
    recovery_preserves_interleaved_journal(false).await;
}

#[tokio::test]
async fn recovery_uses_committed_streams_after_publications_are_delivered() {
    recovery_preserves_interleaved_journal(true).await;
}

/// A fresh cache must require only one scalar lookup, independent of stream count.
#[tokio::test]
async fn stream_cache_revision_keeps_allocations_and_bounds_validation_work() {
    use std::sync::atomic::{AtomicU64, Ordering};
    const MAX_REVISION_LOOKUP_STEPS: u64 = 64;
    for count in [32, 256] {
        let (_directory, store) = running_store().await;
        let base = journal(
            &store,
            NotificationStreamBuffer::default(),
            vec![entry(
                1,
                json!({"method":"item/agentMessage/delta","params":{"threadId":"thread",
                "turnId":"turn","itemId":"answer","delta":"pending"}}),
            )],
        )
        .await;
        let mut value = serde_json::to_value(base).expect("stream JSON");
        let prototype = value["entries"][0].clone();
        value["entries"] = Value::Array(
            (0..count)
                .map(|index| {
                    let mut item = prototype.clone();
                    item["key"]["item"] = json!(format!("item-{index}"));
                    item
                })
                .collect(),
        );
        let mut cached: NotificationStreamBuffer =
            serde_json::from_value(value).expect("many streams");
        let ticks = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&ticks);
        let elapsed = store
            .transaction_with(move |tx| {
                store::persist_streams(tx, "default", &mut cached)?;
                tx.progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                )?;
                let started = std::time::Instant::now();
                let validated = store::current_streams(tx, "default", Some(&cached))?;
                let elapsed = started.elapsed();
                tx.progress_handler(1, None::<fn() -> bool>)?;
                assert!(
                    !validated.changed_from(&cached),
                    "no JSON reconstruction or entry copies"
                );
                Ok::<_, WorkerError>(elapsed)
            })
            .await
            .expect("validate cached streams");
        let steps = ticks.load(Ordering::Relaxed);
        eprintln!(
            "stream revision validation: {count} streams, {steps} SQLite VM steps, {elapsed:?}"
        );
        assert!(
            steps <= MAX_REVISION_LOOKUP_STEPS,
            "revision lookup grew with streams: {steps}"
        );
    }
}

#[tokio::test]
async fn stream_cache_revision_handles_legacy_snapshots_and_rolled_back_handoffs() {
    let (_directory, store) = running_store().await;
    journal(
        &store,
        NotificationStreamBuffer::default(),
        vec![entry(
            1,
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread",
            "turnId":"turn","itemId":"answer","delta":"pending"}}),
        )],
    )
    .await;
    // An installed older server stored the phase as the metadata signature.
    let canonical = store.transaction_with(|tx| {
        tx.execute("UPDATE runtime_operation_receipts SET signature='metadata' WHERE id='account-worker:streams:default:metadata'", [])?;
        store::load_streams(tx, "default")
    }).await.expect("legacy snapshot");
    let staged = Arc::new(std::sync::Mutex::new(None));
    let capture = Arc::clone(&staged);
    let original = canonical.clone();
    let failed = store
        .transaction_with(move |tx| {
            let mut next = store::current_streams(tx, "default", Some(&original))?;
            store::persist_streams(tx, "default", &mut next)?;
            *capture.lock().expect("staged handoff") = Some(next);
            Err::<(), _>(store::invalid("rollback-fixture"))
        })
        .await;
    assert!(failed.is_err());
    let rolled_back = staged
        .lock()
        .expect("staged handoff")
        .take()
        .expect("buffer");
    let restored = store
        .transaction_with(move |tx| store::current_streams(tx, "default", Some(&rolled_back)))
        .await
        .expect("rollback invalidates staged handoff");
    assert_eq!(restored, canonical);
    // A commit can change pending text without advancing publication_sequence.
    let old = canonical.clone();
    let mut value = serde_json::to_value(&canonical).expect("stream JSON");
    value["entries"][0]["pending"] = json!("new pending text");
    let mut replacement: NotificationStreamBuffer =
        serde_json::from_value(value).expect("new buffer");
    let current = store
        .transaction_with(move |tx| {
            store::persist_streams(tx, "default", &mut replacement)?;
            let current = store::current_streams(tx, "default", Some(&old))?;
            assert_eq!(
                current, replacement,
                "delayed handoff must reload even at the same publication sequence"
            );
            Ok::<_, WorkerError>(current)
        })
        .await
        .expect("pending-only commit invalidates cache");
    assert_eq!(current.pending_bytes(), "new pending text".len());
}
