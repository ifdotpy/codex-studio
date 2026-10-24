//! Durable bounded monitor result stream stored with the P15 monitor record.
//!
//! Account-worker notification receipt and queue append share one SQLite
//! transaction. The consumer removes an event only after P15 committed its
//! output/result, so replay after restart is exact and never starts a command.

use base64::Engine;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
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

/// Append `command/exec/outputDelta` while the provider notification receipt is
/// still in its caller's transaction. A full queue aborts that transaction, so
/// the account worker retains the exact journal entry and retries it later.
pub fn enqueue_output_in_transaction(
    tx: &Transaction<'_>,
    process_id: &str,
    account: &str,
    generation: u64,
    cursor: u64,
    params: &Value,
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
    params: &Value,
) -> Result<bool, NotificationDispatchError> {
    let Some((record, _)) = Table::<MonitorRecord>::new(tx).get_with_raw(&process_id.to_owned())?
    else {
        return Ok(false);
    };
    let mut record = record.value().clone();
    if record["status"] != "running"
        || record["operation"]["accountKey"].as_str() != Some(scope.account)
        || record["operation"]["agent"].as_str() != record["agent"].as_str()
        || record["operation"]["epoch"] != record["epoch"]
        || scope
            .expected_agent
            .is_some_and(|agent| record["agent"].as_str() != Some(agent))
    {
        return Ok(false);
    }
    let agent = record["agent"]
        .as_str()
        .ok_or(NotificationDispatchError::InvalidNotification)?;
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
        let operation = serde_json::from_value::<Operation>(record["operation"].clone())
            .map_err(|_| NotificationDispatchError::InvalidNotification)?;
        let original =
            crate::account_worker::retained::monitor_command_scope(tx, process_id, &operation)
                .map_err(|error| match error {
                    crate::account_worker::WorkerError::Store(error) => {
                        NotificationDispatchError::Store(error)
                    }
                    _ => NotificationDispatchError::InvalidNotification,
                })?;
        if original.is_none_or(|original| {
            original.generation != scope.generation || original.account != scope.account
        }) {
            return Ok(false);
        }
    }
    let Some(encoded) = params["deltaBase64"].as_str() else {
        return Err(NotificationDispatchError::InvalidNotification);
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| NotificationDispatchError::InvalidNotification)?;
    if bytes.len() > crate::monitor_policy::CHUNK_BYTES {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let mut queue = record
        .get("streamQueue")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let key = format!(
        "{}:{}:{}",
        scope.cursor_scope, scope.generation, scope.cursor
    );
    if let Some(existing) = queue.iter().find(|item| item["cursor"] == key) {
        if existing["deltaBase64"] == encoded
            && existing["stream"].as_str().unwrap_or("stdout")
                == params["stream"]
                    .as_str()
                    .filter(|s| *s == "stderr")
                    .unwrap_or("stdout")
        {
            return Ok(true);
        }
        return Err(NotificationDispatchError::ConflictingDelivery);
    }
    if record["streamAckCursor"] == key {
        return Ok(true);
    }
    let queued_bytes = queue
        .iter()
        .filter_map(|item| item["byteLength"].as_u64())
        .sum::<u64>();
    if queue.len() >= MAX_EVENTS - RESERVED_EVENTS
        || queued_bytes.saturating_add(bytes.len() as u64)
            > (STREAM_QUEUE_MAX_BYTES - TERMINAL_RECEIPT_RESERVE_BYTES) as u64
    {
        return Err(NotificationDispatchError::StreamCapacity);
    }
    let committed_offset = record["bytes"].as_u64().unwrap_or(0);
    let offset = queue.last().map_or(committed_offset, |item| {
        item["offset"]
            .as_u64()
            .unwrap_or(committed_offset)
            .saturating_add(item["byteLength"].as_u64().unwrap_or(0))
    });
    let operation = record
        .get("operation")
        .cloned()
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    queue.push(json!({
        "cursor": key,
        "offset": offset,
        "byteLength": bytes.len(),
        "deltaBase64": encoded,
        "stream": params["stream"].as_str().filter(|s| *s == "stderr").unwrap_or("stdout"),
        "operation": operation,
    }));
    record["streamQueue"] = Value::Array(queue);
    Table::<MonitorRecord>::new(tx).put(&MonitorRecord::from_value(record)?)?;
    Ok(true)
}

/// Return the monitor's durable event queue. A command that never wrote output
/// has no queue yet; its first probe or exit receipt creates it, as the first
/// output delta does. Any other stored shape is refused.
fn stream_queue(record: &mut Value) -> Result<&mut Vec<Value>, MonitorError> {
    if record.get("streamQueue").is_none() {
        record["streamQueue"] = json!([]);
    }
    record
        .get_mut("streamQueue")
        .and_then(Value::as_array_mut)
        .ok_or(MonitorError::InvalidDeliveryIdentity(
            "monitor output queue",
        ))
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
    let record: Value = serde_json::from_str(&encoded)
        .map_err(|_| MonitorError::InvalidDeliveryIdentity("monitor output queue"))?;
    let item = record["streamQueue"]
        .as_array()
        .and_then(|queue| queue.first())
        .ok_or(MonitorError::InvalidDeliveryIdentity("monitor output head"))?;
    let cursor = item["cursor"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(MonitorError::InvalidDeliveryIdentity(
            "monitor output cursor",
        ))?
        .to_owned();
    let operation: Operation = serde_json::from_value(item["operation"].clone())
        .map_err(|_| MonitorError::InvalidDeliveryIdentity("monitor output operation"))?;
    let kind = item["kind"].as_str().unwrap_or("output");
    if kind == "exit" {
        return Ok(Some(Delivery {
            id,
            operation,
            cursor,
            event: MonitorEvent::Exit {
                code: item["code"].as_i64(),
                error: item["error"].as_str().map(str::to_owned),
                finished: item["finished"]
                    .as_f64()
                    .ok_or(MonitorError::InvalidDeliveryIdentity("monitor exit time"))?,
            },
        }));
    }
    if kind == "probe" {
        return Ok(Some(Delivery {
            id,
            operation,
            cursor,
            event: MonitorEvent::Probe {
                generation: item["generation"].as_i64().ok_or(
                    MonitorError::InvalidDeliveryIdentity("monitor probe generation"),
                )?,
                result: item["result"].clone(),
            },
        }));
    }
    if kind != "output" {
        return Err(MonitorError::InvalidDeliveryIdentity("monitor event kind"));
    }
    let offset = item["offset"]
        .as_u64()
        .ok_or(MonitorError::InvalidDeliveryIdentity(
            "monitor output offset",
        ))?;
    let encoded_bytes =
        item["deltaBase64"]
            .as_str()
            .ok_or(MonitorError::InvalidDeliveryIdentity(
                "monitor output bytes",
            ))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded_bytes)
        .map_err(|_| MonitorError::InvalidDeliveryIdentity("monitor output encoding"))?;
    if bytes.len() > crate::monitor_policy::CHUNK_BYTES
        || item["byteLength"].as_u64() != Some(bytes.len() as u64)
    {
        return Err(MonitorError::InvalidDeliveryIdentity("monitor output size"));
    }
    Ok(Some(Delivery {
        id,
        operation,
        cursor,
        event: MonitorEvent::OutputStream {
            offset,
            stream: item["stream"]
                .as_str()
                .filter(|s| *s == "stderr")
                .unwrap_or("stdout")
                .to_owned(),
            bytes,
        },
    }))
}

/// Persist a completed liveness request behind already-received output. The
/// cursor is stable per monitor generation, so a lost queue write is replayable.
pub async fn enqueue_probe(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    generation: i64,
    result: &Value,
) -> Result<(), MonitorError> {
    let cursor = format!("liveness:{id}:{generation}");
    let id = id.to_owned();
    let operation = operation.clone();
    let result = result.clone();
    store
        .transaction_with(move |tx| {
            let Some(record) = MonitorsRepository::get(tx, &id)? else {
                return Err(MonitorError::Refused("Unknown command watch"));
            };
            let mut value = record.value().clone();
            if value.get("operation")
                != Some(&serde_json::to_value(&operation).unwrap_or(Value::Null))
            {
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "monitor probe operation changed",
                ));
            }
            if value["streamAckCursor"].as_str() == Some(&cursor) {
                return Ok(());
            }
            let queue = stream_queue(&mut value)?;
            if let Some(existing) = queue.iter().find(|item| item["cursor"] == cursor) {
                if existing["kind"] == "probe"
                    && existing["generation"] == generation
                    && existing.get("result") == Some(&result)
                {
                    return Ok(());
                }
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "conflicting monitor probe",
                ));
            }
            let result_bytes = result.to_string().len();
            if queue.len() >= MAX_EVENTS - 1
                || result_bytes > PROBE_RESULT_MAX_BYTES
                || queue
                    .iter()
                    .filter_map(|item| item["byteLength"].as_u64())
                    .sum::<u64>()
                    .saturating_add(result_bytes as u64)
                    > (STREAM_QUEUE_MAX_BYTES - TERMINAL_RECEIPT_RESERVE_BYTES) as u64
            {
                return Err(MonitorError::Refused("Monitor output queue is full"));
            }
            queue.push(json!({
                "kind":"probe",
                "cursor":cursor,
                "generation":generation,
                "result":result,
                "byteLength":result_bytes,
                "operation":operation,
            }));
            Table::<MonitorRecord>::new(tx)
                .put(&MonitorRecord::from_value(value).map_err(StoreError::from)?)?;
            Ok(())
        })
        .await
}

/// Persist the definitive result after all previously journaled output deltas.
/// The broker receipt makes this operation replayable after a crash.
pub async fn enqueue_exit(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    cursor: &str,
    code: Option<i64>,
    error: Option<&str>,
    finished: f64,
) -> Result<(), MonitorError> {
    let error = error.map(|value| {
        let mut end = value.len().min(EXIT_RECEIPT_MAX_ERROR_BYTES);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        &value[..end]
    });
    let id = id.to_owned();
    let operation = operation.clone();
    let cursor = cursor.to_owned();
    let error = error.map(str::to_owned);
    store
        .transaction_with(move |tx| {
            let Some(record) = MonitorsRepository::get(tx, &id)? else {
                return Err(MonitorError::Refused("Unknown command watch"));
            };
            let mut value = record.value().clone();
            if value.get("operation")
                != Some(&serde_json::to_value(&operation).unwrap_or(Value::Null))
            {
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "monitor operation changed",
                ));
            }
            if value["streamAckCursor"].as_str() == Some(cursor.as_str()) {
                return Ok(());
            }
            let queue = stream_queue(&mut value)?;
            if let Some(existing) = queue.iter().find(|item| item["cursor"] == cursor) {
                if existing["kind"] == "exit"
                    && existing["code"] == code.map_or(Value::Null, |v| json!(v))
                    && existing["error"] == error.as_ref().map_or(Value::Null, |v| json!(v))
                {
                    return Ok(());
                }
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "conflicting monitor result",
                ));
            }
            let queued_bytes = queue
                .iter()
                .filter_map(|item| item["byteLength"].as_u64())
                .sum::<u64>();
            let error_bytes = error.as_ref().map_or(0, |value| value.len()) as u64;
            if queue.len() >= MAX_EVENTS
                || queued_bytes.saturating_add(error_bytes) > STREAM_QUEUE_MAX_BYTES as u64
            {
                return Err(MonitorError::Refused("Monitor output queue is full"));
            }
            queue.push(json!({
                "kind":"exit",
                "cursor":cursor,
                "code":code,
                "error":error,
                "finished":finished,
                "byteLength":error_bytes,
                "operation":operation,
            }));
            let record = MonitorRecord::from_value(value).map_err(StoreError::from)?;
            Table::<MonitorRecord>::new(tx).put(&record)?;
            Ok(())
        })
        .await
}

/// Acknowledge only the exact head already applied by the P15 consumer. Repeated
/// ACK after a lost local response succeeds from `streamAckCursor`.
pub async fn acknowledge(store: &RuntimeStore, delivery: &Delivery) -> Result<(), MonitorError> {
    let delivery = delivery.clone();
    store
        .transaction_with(move |tx| {
            let Some(record) = MonitorsRepository::get(tx, &delivery.id)? else {
                return Err(MonitorError::Refused("Unknown command watch"));
            };
            let mut value = record.value().clone();
            if value["streamAckCursor"].as_str() == Some(&delivery.cursor) {
                return Ok(());
            }
            if value["operation"]
                != serde_json::to_value(&delivery.operation).unwrap_or(Value::Null)
            {
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "monitor operation changed",
                ));
            }
            let queue = value["streamQueue"].as_array_mut().ok_or(
                MonitorError::InvalidDeliveryIdentity("monitor output queue"),
            )?;
            if queue.first().and_then(|item| item["cursor"].as_str()) != Some(&delivery.cursor) {
                return Err(MonitorError::InvalidDeliveryIdentity(
                    "monitor output ACK order",
                ));
            }
            queue.remove(0);
            value["streamAckCursor"] = json!(delivery.cursor);
            let record = MonitorRecord::from_value(value).map_err(StoreError::from)?;
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
                    tx.prepare("INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)")?;
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
        const HISTORY: usize = 2_000;
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES('live',?1)",
                    [json!({"id":"live","agent":"worker","status":"running","created":2,"operation":operation(),"streamQueue":[]}).to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_monitors(id,record) VALUES('zz-finished',?1)",
                    [json!({"id":"zz-finished","agent":"worker","status":"completed","created":3,"operation":operation(),
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
