//! Journal cost against receipt backlog: a provider request that is not yet
//! settled holds compaction, so later settled receipts accumulate behind it.
use super::*;
use studio_runtime_providers::{DecodedJournalPayload, decode_journal_value};

const GENERATION: u64 = 7;
const SCOPE: &str = "default";

fn entry(cursor: u64, frame: Value) -> JournalEntry {
    let DecodedJournalPayload::Frame(frame) =
        decode_journal_value(cursor, GENERATION, frame).payload
    else {
        panic!("fixture frame decodes");
    };
    JournalEntry {
        received_at_ms: cursor,
        cursor: EventCursor(cursor),
        generation: Generation(GENERATION),
        payload: JournalPayload::Frame(frame),
    }
}

fn delta(cursor: u64) -> JournalEntry {
    entry(
        cursor,
        json!({"method":"item/agentMessage/delta","params":{
            "threadId":"thread","turnId":"turn","itemId":"answer","delta":"x"}}),
    )
}

async fn running_store() -> Arc<RuntimeStore> {
    let directory = tempfile::tempdir().expect("temporary store").keep();
    let store = Arc::new(RuntimeStore::open(Some(&directory)).await.expect("store"));
    let agent = AgentRecord::from_value(json!({
        "id":"agent","name":"Agent","rootId":"agent","epoch":4,"status":"running",
        "accountHistory":[],"model":"gpt-5","effort":"high","fastMode":false,
        "daybreakEnabled":false,"cyberAccessProgram":false,"cwd":"/disposable","deletedAt":null,"events":0,"activeTools":[],
        "provider":"codex","accountKey":"default","executionMode":"host",
        "threadId":"thread","turnId":"turn","autoWake":true,"inFlight":true,"turnEpoch":4,
        "startAttempt":{"id":"attempt","epoch":4,"submitted":true,
            "accountKey":"default","threadId":"thread","turnId":"turn",
            "connectionGeneration":GENERATION,"requestTicket":"ticket",
            "executionOutcome":"unknown","events":[]}
    }))
    .expect("agent");
    store
        .transaction(move |tx| AgentRepository::put(tx, &agent))
        .await
        .expect("seed agent");
    store
}

async fn journal(
    store: &Arc<RuntimeStore>,
    streams: NotificationStreamBuffer,
    entries: Vec<JournalEntry>,
) -> NotificationStreamBuffer {
    let (streams, committed, deferred) = store::journal_batch(
        Arc::clone(store),
        "default",
        SCOPE,
        &streams,
        entries,
        &Value::Null,
        None,
        None,
    )
    .await
    .expect("journal batch")
    .expect("journal batch commits");
    assert!(deferred.is_empty());
    assert!(!committed.is_empty());
    streams
}

fn journal_receipts(store: &RuntimeStore) -> i64 {
    store
        .read()
        .expect("reader")
        .query_row(
            "SELECT COUNT(*) FROM runtime_operation_receipts WHERE id LIKE 'account-worker:journal:%'",
            [],
            |row| row.get(0),
        )
        .expect("count")
}

/// SQLite virtual machine steps between progress callbacks.
const VM_STEPS_PER_TICK: std::ffi::c_int = 1;

/// SQLite work of one journal batch, in [`VM_STEPS_PER_TICK`] units. The
/// count is deterministic for a given database state, unlike wall time.
async fn batch_work(
    store: &Arc<RuntimeStore>,
    streams: NotificationStreamBuffer,
    entries: Vec<JournalEntry>,
) -> (NotificationStreamBuffer, u64) {
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = Arc::clone(&ticks);
    store
        .transaction_with(move |tx| -> Result<(), WorkerError> {
            tx.progress_handler(
                VM_STEPS_PER_TICK,
                Some(move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    false
                }),
            )?;
            Ok(())
        })
        .await
        .expect("install progress counter");
    let streams = journal(store, streams, entries).await;
    store
        .transaction_with(|tx| -> Result<(), WorkerError> {
            tx.progress_handler(VM_STEPS_PER_TICK, None::<fn() -> bool>)?;
            // The worker drains publications after each batch; this fixture
            // measures journal receipts only.
            tx.execute(
                "DELETE FROM runtime_operation_receipts WHERE id LIKE 'account-worker:notification-publication:%'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("remove progress counter");
    (streams, ticks.load(std::sync::atomic::Ordering::Relaxed))
}

/// An unanswered provider request keeps the checkpoint below it while later
/// notifications settle. Journal work per batch must not grow with that
/// backlog, and the backlog must drain once the request settles.
#[tokio::test]
async fn journal_batch_work_is_independent_of_held_receipt_backlog() {
    const BACKLOG: u64 = 4_000;
    const BATCH: u64 = super::JOURNAL_BATCH_MAX_EVENTS as u64;
    let store = running_store().await;
    let streams = journal(
        &store,
        NotificationStreamBuffer::default(),
        vec![entry(
            1,
            json!({"id":"approval","method":"item/commandExecution/requestApproval",
                "params":{"threadId":"thread","turnId":"turn","itemId":"command"}}),
        )],
    )
    .await;
    // The first batch opens the stream; compare two batches that extend it.
    let streams = journal(&store, streams, (2..2 + BATCH).map(delta).collect()).await;
    let (streams, small) = batch_work(
        &store,
        streams,
        (2 + BATCH..2 + 2 * BATCH).map(delta).collect(),
    )
    .await;
    let base = 2 + 2 * BATCH;
    store
        .transaction_with(move |tx| -> Result<(), WorkerError> {
            for cursor in base..base + BACKLOG {
                store::replace(
                    tx,
                    &store::journal_operation(SCOPE, GENERATION, cursor),
                    "settled",
                    json!({"cursor":cursor,"digest":"d","ready":true,"committed":true}),
                )?;
            }
            Ok(())
        })
        .await
        .expect("held backlog");
    let next = base + BACKLOG;
    let (mut streams, large) =
        batch_work(&store, streams, (next..next + BATCH).map(delta).collect()).await;
    assert!(
        large <= small + small / 20,
        "journal work grew with the held backlog: {small} -> {large} ticks"
    );
    assert_eq!(journal_receipts(&store), (3 * BATCH + BACKLOG + 1) as i64);

    store::callback_settled(&store, SCOPE, GENERATION, 1)
        .await
        .expect("settle the request");
    let mut cursor = next + BATCH;
    while journal_receipts(&store) > 0 {
        streams = journal(&store, streams, vec![delta(cursor)]).await;
        cursor += 1;
    }
    let checkpoint = store
        .transaction_with(move |tx| store::checkpoint(tx, SCOPE, GENERATION))
        .await
        .expect("checkpoint");
    assert_eq!(checkpoint, cursor - 1);
}

/// Receipts written before cursor padding keep their exactly-once evidence.
#[tokio::test]
async fn legacy_journal_receipts_are_padded_once() {
    let store = running_store().await;
    let legacy = |cursor: u64| {
        (
            format!("account-worker:journal:{SCOPE}:{GENERATION}:{cursor}:settled"),
            cursor,
        )
    };
    store
        .transaction_with(move |tx| -> Result<(), WorkerError> {
            for (id, cursor) in [legacy(2), legacy(10)] {
                tx.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'settled',?2)",
                    rusqlite::params![
                        id,
                        json!({"cursor":cursor,"digest":"d","ready":false,"committed":true}).to_string()
                    ],
                )?;
            }
            store::pad_legacy_journal_receipts(tx, "default")?;
            store::pad_legacy_journal_receipts(tx, "default")?;
            for cursor in [2, 10] {
                assert!(store::journal_committed(tx, SCOPE, GENERATION, cursor)?);
            }
            let ids = tx
                .prepare("SELECT id FROM runtime_operation_receipts WHERE id LIKE 'account-worker:journal:%' ORDER BY id")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(
                ids,
                [2, 10].map(|cursor| format!(
                    "account-worker:{}:settled",
                    store::journal_operation(SCOPE, GENERATION, cursor)
                ))
            );
            Ok(())
        })
        .await
        .expect("legacy receipts");
}
