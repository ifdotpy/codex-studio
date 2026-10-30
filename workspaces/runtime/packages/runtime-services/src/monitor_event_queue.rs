//! Durable bounded monitor result stream stored with the P15 monitor record.
//!
//! Account-worker notification receipt and queue append share one SQLite
//! transaction. The consumer removes an event only after P15 committed its
//! output/result, so replay after restart is exact and never starts a command.

use base64::Engine;
use rusqlite::OptionalExtension;
#[cfg(test)]
use serde_json::{Value, json};
use studio_runtime_schema::monitor_record::{MonitorProbeResult, MonitorQueueEvent, MonitorStream};
use studio_runtime_store::{
    MonitorRecord, MonitorsRepository, RuntimeStore, StoreError, Table, Transaction,
};

use crate::monitor_policy::{MonitorError, Operation};
use crate::monitor_runtime_wiring::{Delivery, MonitorEvent};
use crate::notification_dispatch::NotificationDispatchError;

const MAX_EVENTS: usize = 128;
pub const STREAM_QUEUE_MAX_BYTES: usize = 1024 * 1024;
// Keep enough room for one bounded liveness result and the definitive exit
// receipt after the output stream reaches its normal admission limit.
const RESERVED_EVENTS: usize = 2;
pub const TERMINAL_RECEIPT_RESERVE_BYTES: usize = 64 * 1024;
pub const PROBE_RESULT_MAX_BYTES: usize = 32 * 1024;
/// Python stores at most this many characters in a command monitor's error/tail.
pub const EXIT_RECEIPT_MAX_ERROR_BYTES: usize = 12_000;

pub struct JournalEventScope<'a> {
    pub account: &'a str,
    pub cursor_scope: &'a str,
    pub generation: u64,
    pub cursor: u64,
    pub expected_agent: Option<&'a str>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputDelta {
    pub delta_base64: String,
    pub stream: Option<MonitorStream>,
}

/// Append `command/exec/outputDelta` while the provider notification receipt is
/// still in its caller's transaction. A full queue aborts that transaction, so
/// the account worker retains the exact journal entry and retries it later.
pub fn enqueue_output_in_transaction(
    tx: &Transaction<'_>,
    process_id: &str,
    account: &str,
    generation: u64,
    cursor: u64,
    params: &OutputDelta,
) -> Result<bool, NotificationDispatchError> {
    enqueue_output_with_scope_in_transaction(
        tx,
        process_id,
        JournalEventScope {
            account,
            cursor_scope: account,
            generation,
            cursor,
            expected_agent: None,
        },
        params,
    )
}

pub fn enqueue_output_with_scope_in_transaction(
    tx: &Transaction<'_>,
    process_id: &str,
    scope: JournalEventScope<'_>,
    params: &OutputDelta,
) -> Result<bool, NotificationDispatchError> {
    let Some(mut record) = Table::<MonitorRecord>::new(tx).get(&process_id.to_owned())? else {
        return Ok(false);
    };
    let Some(operation) = record.operation.as_ref() else {
        return Ok(false);
    };
    if record.status() != studio_runtime_schema::studio_errors::MonitorStatus::Running
        || operation.account_key.as_str() != scope.account
        || operation.agent.as_str() != record.agent()
        || record.epoch.get() != operation.epoch.get()
        || scope
            .expected_agent
            .is_some_and(|agent| record.agent() != agent)
    {
        return Ok(false);
    }
    let agent = record.agent();
    let owner_account = tx
        .query_row(
            "SELECT COALESCE(account_key,'default') FROM runtime_agents WHERE id=?1",
            [agent],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    if owner_account
        .as_deref()
        .is_some_and(|account| account != scope.account)
    {
        let original =
            crate::account_worker::retained::monitor_command_scope(tx, process_id, operation)
                .map_err(|error| match error {
                    crate::account_worker::WorkerError::Expired => {
                        NotificationDispatchError::RequestResult(
                            studio_runtime_store::request_results::RequestResultError::Expired,
                        )
                    }
                    crate::account_worker::WorkerError::Store(error) => {
                        NotificationDispatchError::Store(error)
                    }
                    crate::account_worker::WorkerError::Recovery(_)
                    | crate::account_worker::WorkerError::Scheduler(_)
                    | crate::account_worker::WorkerError::Session(_)
                    | crate::account_worker::WorkerError::Notification(_)
                    | crate::account_worker::WorkerError::PreparationRefused(_)
                    | crate::account_worker::WorkerError::StaleVmActor
                    | crate::account_worker::WorkerError::PreparationPending(_)
                    | crate::account_worker::WorkerError::PreparationAwaitsAgent(_)
                    | crate::account_worker::WorkerError::CleanupUnconfirmed(_)
                    | crate::account_worker::WorkerError::AlreadyRunning
                    | crate::account_worker::WorkerError::Closed
                    | crate::account_worker::WorkerError::ShutdownTimeout
                    | crate::account_worker::WorkerError::TaskFailed
                    | crate::account_worker::WorkerError::Capacity
                    | crate::account_worker::WorkerError::CallbackUnknown => {
                        NotificationDispatchError::invalid()
                    }
                })?;
        if original.is_none_or(|original| {
            original.generation != scope.generation || original.account != scope.account
        }) {
            return Ok(false);
        }
    }
    let encoded = &params.delta_base64;
    let stream = params.stream.unwrap_or(MonitorStream::Stdout);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| NotificationDispatchError::invalid())?;
    let key = format!(
        "{}:{}:{}",
        scope.cursor_scope, scope.generation, scope.cursor
    );
    let queue = record.stream_queue.get_or_insert_with(Vec::new);
    if let Some(existing) = queue.iter().find(|item| item.cursor() == key) {
        return if matches!(existing, MonitorQueueEvent::Output { delta_base64, stream: saved, .. } if delta_base64 == encoded && *saved == stream)
        {
            Ok(true)
        } else {
            Err(NotificationDispatchError::ConflictingDelivery)
        };
    }
    if record.stream_ack_cursor.as_deref() == Some(key.as_str()) {
        return Ok(true);
    }
    let queued_bytes = queue
        .iter()
        .map(MonitorQueueEvent::byte_length)
        .sum::<u64>();
    if queue.len() >= MAX_EVENTS - RESERVED_EVENTS
        || queued_bytes.saturating_add(bytes.len() as u64)
            > (STREAM_QUEUE_MAX_BYTES - TERMINAL_RECEIPT_RESERVE_BYTES) as u64
    {
        return Err(NotificationDispatchError::StreamCapacity);
    }
    let committed_offset = record.bytes.get() as u64;
    let offset = queue.last().map_or(committed_offset, |item| {
        let offset = match item {
            MonitorQueueEvent::Output { offset, .. } => *offset,
            MonitorQueueEvent::Probe { .. } | MonitorQueueEvent::Exit { .. } => committed_offset,
        };
        offset.saturating_add(item.byte_length())
    });
    queue.push(MonitorQueueEvent::Output {
        cursor: key,
        offset,
        byte_length: bytes.len() as u64,
        delta_base64: encoded.clone(),
        stream,
        operation: operation.clone(),
    });
    Table::<MonitorRecord>::new(tx).put(&record)?;
    Ok(true)
}

/// The agent that owns the running monitor `process_id`, whose output deltas
/// [`enqueue_output_with_scope_in_transaction`] appends.
pub fn output_agent(
    tx: &Transaction<'_>,
    process_id: &str,
) -> Result<Option<String>, NotificationDispatchError> {
    Ok(Table::<MonitorRecord>::new(tx)
        .get(&process_id.to_owned())?
        .map(|record| record.agent().to_owned())
        .filter(|agent| !agent.is_empty()))
}

/// Return the first durable per-monitor event in stable monitor/cursor order.
pub async fn next(store: &RuntimeStore) -> Result<Option<Delivery>, MonitorError> {
    let row = store
        .read_async("runtime-services/monitor_event_queue", |connection| {
            MonitorsRepository::first_queued_stream(connection)
        })
        .await?;
    let Some((id, encoded)) = row else {
        return Ok(None);
    };
    let record: MonitorRecord = serde_json::from_str(&encoded)
        .map_err(|_| MonitorError::InvalidDeliveryIdentity("monitor output queue"))?;
    if record.id != id {
        return Err(MonitorError::InvalidDeliveryIdentity(
            "monitor output owner",
        ));
    }
    let item = record
        .stream_queue
        .and_then(|queue| queue.into_iter().next())
        .ok_or(MonitorError::InvalidDeliveryIdentity("monitor output head"))?;
    let cursor = item.cursor().to_owned();
    if cursor.is_empty() {
        return Err(MonitorError::InvalidDeliveryIdentity(
            "monitor output cursor",
        ));
    }
    let operation = item.operation().clone();
    let event = match item {
        MonitorQueueEvent::Exit {
            code,
            error,
            finished,
            ..
        } => MonitorEvent::Exit {
            code,
            error,
            finished,
        },
        MonitorQueueEvent::Probe {
            generation, result, ..
        } => MonitorEvent::Probe { generation, result },
        MonitorQueueEvent::Output {
            offset,
            delta_base64,
            byte_length,
            stream,
            ..
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(delta_base64)
                .map_err(|_| MonitorError::InvalidDeliveryIdentity("monitor output encoding"))?;
            if byte_length != bytes.len() as u64 {
                return Err(MonitorError::InvalidDeliveryIdentity("monitor output size"));
            }
            MonitorEvent::OutputStream {
                offset,
                stream: match stream {
                    MonitorStream::Stdout => "stdout",
                    MonitorStream::Stderr => "stderr",
                }
                .to_owned(),
                bytes,
            }
        }
    };
    Ok(Some(Delivery {
        id,
        operation,
        cursor,
        event,
    }))
}

/// Persist a completed liveness request behind already-received output. The
/// cursor is stable per monitor generation, so a lost queue write is replayable.
pub async fn enqueue_probe(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    generation: i64,
    result: &MonitorProbeResult,
) -> Result<(), MonitorError> {
    let cursor = format!("liveness:{id}:{generation}");
    let id = id.to_owned();
    let operation = operation.clone();
    let result = result.clone();
    store
        .transaction_with(move |tx| {
            let Some(mut record) = MonitorsRepository::get(tx, &id)? else {
                return Err(MonitorError::Refused("Unknown command watch"));
            };
            if record.operation.as_ref() != Some(&operation) {
                return Err(MonitorError::InvalidDeliveryIdentity("monitor probe operation changed"));
            }
            if record.stream_ack_cursor.as_deref() == Some(cursor.as_str()) { return Ok(()); }
            let queue = record.stream_queue.get_or_insert_with(Vec::new);
            if let Some(existing) = queue.iter().find(|item| item.cursor() == cursor) {
                return if matches!(existing, MonitorQueueEvent::Probe { generation: saved, result: prior, .. } if *saved == generation && prior == &result) {
                    Ok(())
                } else {
                    Err(MonitorError::InvalidDeliveryIdentity("conflicting monitor probe"))
                };
            }
            let result_bytes = serde_json::to_vec(&result).map_err(StoreError::from)?.len();
            if queue.len() >= MAX_EVENTS - 1 || result_bytes > PROBE_RESULT_MAX_BYTES
                || queue.iter().map(MonitorQueueEvent::byte_length).sum::<u64>().saturating_add(result_bytes as u64) > (STREAM_QUEUE_MAX_BYTES - TERMINAL_RECEIPT_RESERVE_BYTES) as u64
            {
                return Err(MonitorError::Refused("Monitor output queue is full"));
            }
            queue.push(MonitorQueueEvent::Probe { cursor, generation, result, byte_length: result_bytes as u64, operation });
            Table::<MonitorRecord>::new(tx).put(&record)?;
            Ok(())
        })
        .await
}

/// Persist the definitive result after all previously journaled output deltas.
/// The broker receipt makes this operation replayable after a crash.
pub fn enqueue_exit_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    operation: &Operation,
    cursor: &str,
    code: Option<i64>,
    error: Option<&str>,
    finished: f64,
) -> Result<bool, MonitorError> {
    let error = error.map(|value| {
        let mut end = value.len().min(EXIT_RECEIPT_MAX_ERROR_BYTES);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value[..end].to_owned()
    });
    let Some(mut record) = Table::<MonitorRecord>::new(tx).get(&id.to_owned())? else {
        return Err(MonitorError::Refused("Unknown command watch"));
    };
    if record
        .operation
        .as_ref()
        .is_none_or(|saved| !saved.same_identity(operation))
    {
        return Err(MonitorError::InvalidDeliveryIdentity(
            "monitor operation changed",
        ));
    }
    if record.stream_ack_cursor.as_deref() == Some(cursor) {
        return Ok(true);
    }
    let queue = record.stream_queue.get_or_insert_with(Vec::new);
    if let Some(existing) = queue.iter().find(|item| item.cursor() == cursor) {
        return if matches!(existing, MonitorQueueEvent::Exit { code: saved_code, error: saved_error, .. } if *saved_code == code && *saved_error == error)
        {
            Ok(true)
        } else {
            Err(MonitorError::InvalidDeliveryIdentity(
                "conflicting monitor result",
            ))
        };
    }
    let queued_bytes = queue
        .iter()
        .map(MonitorQueueEvent::byte_length)
        .sum::<u64>();
    let error_bytes = error.as_ref().map_or(0, |value| value.len()) as u64;
    if queue.len() >= MAX_EVENTS
        || queued_bytes.saturating_add(error_bytes) > STREAM_QUEUE_MAX_BYTES as u64
    {
        return Ok(false);
    }
    queue.push(MonitorQueueEvent::Exit {
        cursor: cursor.to_owned(),
        code,
        error,
        finished,
        byte_length: error_bytes,
        operation: operation.clone(),
    });
    Table::<MonitorRecord>::new(tx).put(&record)?;
    Ok(true)
}

pub async fn enqueue_exit(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    cursor: &str,
    code: Option<i64>,
    error: Option<&str>,
    finished: f64,
) -> Result<(), MonitorError> {
    let id = id.to_owned();
    let operation = operation.clone();
    let cursor = cursor.to_owned();
    let error = error.map(str::to_owned);
    store
        .transaction_with(move |tx| {
            if enqueue_exit_in_transaction(
                tx,
                &id,
                &operation,
                &cursor,
                code,
                error.as_deref(),
                finished,
            )? {
                Ok(())
            } else {
                Err(MonitorError::Refused("Monitor output queue is full"))
            }
        })
        .await
}

/// Acknowledge only the exact head already applied by the P15 consumer. Repeated
/// ACK after a lost local response succeeds from `streamAckCursor`.
pub async fn acknowledge(store: &RuntimeStore, delivery: &Delivery) -> Result<(), MonitorError> {
    let delivery = delivery.clone();
    store
        .transaction_with(move |tx| {
            let Some(mut record) = MonitorsRepository::get(tx, &delivery.id)? else {
                return Err(MonitorError::Refused("Unknown command watch"));
            };
            if record.stream_ack_cursor.as_deref() == Some(delivery.cursor.as_str()) {
                return Ok(());
            }
            if record.operation.as_ref() != Some(&delivery.operation) {
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "monitor operation changed",
                ));
            }
            let queue =
                record
                    .stream_queue
                    .as_mut()
                    .ok_or(MonitorError::InvalidDeliveryIdentity(
                        "monitor output queue",
                    ))?;
            if queue.first().map(MonitorQueueEvent::cursor) != Some(delivery.cursor.as_str()) {
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "monitor output ACK order",
                ));
            }
            queue.remove(0);
            record.stream_ack_cursor = Some(delivery.cursor);
            Table::<MonitorRecord>::new(tx).put(&record)?;
            Ok(())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn operation() -> Value {
        json!({"agent":"worker","epoch":1,"accountKey":"default","connectionId":"connection"})
    }

    /// Store `count` finished monitors without a pending event, starting at
    /// `from`. Half carry an empty queue, as acknowledged monitors do.
    async fn finished_history(store: &RuntimeStore, from: usize, count: usize) {
        store
            .transaction_with(move |tx| {
                let mut insert =
                    tx.prepare("INSERT INTO runtime_monitors(id,record,monitor_status) VALUES(?1,?2,COALESCE(CASE WHEN json_valid(?2) THEN json_extract(?2,'$.status') END,'lost'))")?;
                for index in from..from + count {
                    let id = format!("history-{index:06}");
                    let mut record = json!({"id":id,"agent":format!("other-{index}"),"status":"completed","created":1,"operation":operation()});
                    if index % 2 == 0 {
                        record["streamQueue"] = json!([]);
                    }
                    insert.execute(rusqlite::params![id, record.to_string()])?;
                }
                Ok::<_, StoreError>(())
            })
            .await
            .expect("finished monitor history");
    }

    /// SQLite work of the queue head read that `next` runs, in virtual machine
    /// steps. The count is deterministic for a given database state.
    fn queue_head_work(store: &RuntimeStore) -> (u64, Option<String>) {
        let connection = store.test_reader().expect("reader");
        let steps = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&steps);
        connection
            .progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .expect("progress counter");
        let head = MonitorsRepository::first_queued_stream(&connection).expect("queue head");
        connection
            .progress_handler(1, None::<fn() -> bool>)
            .expect("progress counter");
        (steps.load(Ordering::Relaxed), head.map(|(id, _)| id))
    }

    /// The monitor tick reads the queue head once per pass and once per
    /// delivered event. On 22 October 2026 that read parsed every monitor
    /// (93,650 rows, 209 MB, about 0.55 s) and kept the backend near 80% CPU.
    /// Its work must not grow with finished monitor history, and a finished
    /// monitor with an undelivered exit must still be the delivered head.
    #[tokio::test]
    async fn queue_head_work_is_independent_of_finished_monitor_history() {
        const HISTORY: usize = 128;
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record,monitor_status) VALUES('live',?1,COALESCE(CASE WHEN json_valid(?1) THEN json_extract(?1,'$.status') END,'lost'))",
                    [json!({"id":"live","agent":"worker","epoch":1,"status":"running","created":2,"requestId":"req-live","activityAt":2,"activityGeneration":0,"bytes":0,"command":"true","cwd":std::env::temp_dir(),"interactive":false,"livenessCommand":"","log":std::env::temp_dir().join("live.log"),"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit","operation":operation(),"streamQueue":[]}).to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record,stream_pending) VALUES('zz-finished',?1,1)",
                    [json!({"id":"zz-finished","agent":"worker","epoch":1,"status":"completed","created":3,"requestId":"req-finished","activityAt":3,"activityGeneration":0,"bytes":0,"command":"true","cwd":std::env::temp_dir(),"interactive":false,"livenessCommand":"","log":std::env::temp_dir().join("finished.log"),"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit","operation":operation(),
                        "streamQueue":[{"kind":"exit","cursor":"result:zz-finished","code":0,"error":null,"finished":9.0,"byteLength":0,"operation":operation()}]}).to_string()],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("monitors");
        finished_history(&store, 0, HISTORY).await;
        let (small, head) = queue_head_work(&store);
        assert_eq!(head.as_deref(), Some("zz-finished"));
        finished_history(&store, HISTORY, HISTORY).await;
        let (large, head) = queue_head_work(&store);
        assert_eq!(head.as_deref(), Some("zz-finished"));
        assert!(
            large <= small + small / 20,
            "queue head work grew with history: {small} steps, then {large} with {HISTORY} more finished monitors"
        );

        let delivery = next(&store)
            .await
            .expect("queue head")
            .expect("finished monitor exit is delivered");
        assert_eq!(delivery.id, "zz-finished");
        assert_eq!(delivery.cursor, "result:zz-finished");
        assert!(matches!(
            delivery.event,
            MonitorEvent::Exit {
                code: Some(0),
                error: None,
                ..
            }
        ));
        acknowledge(&store, &delivery).await.expect("ack exit");
        assert!(next(&store).await.expect("drained queue").is_none());
    }
}
