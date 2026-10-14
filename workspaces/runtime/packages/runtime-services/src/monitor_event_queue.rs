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
const MAX_BYTES: usize = 1024 * 1024;

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
    let Some((record, _)) = Table::<MonitorRecord>::new(tx).get_with_raw(&process_id.to_owned())?
    else {
        return Ok(false);
    };
    let mut record = record.value().clone();
    if record["status"] != "running"
        || record["operation"]["accountKey"].as_str() != Some(account)
        || record["operation"]["agent"].as_str() != record["agent"].as_str()
        || record["operation"]["epoch"] != record["epoch"]
    {
        return Ok(false);
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
    let key = format!("{account}:{generation}:{cursor}");
    if let Some(existing) = queue.iter().find(|item| item["cursor"] == key) {
        if existing["deltaBase64"] == encoded {
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
    if queue.len() >= MAX_EVENTS
        || queued_bytes.saturating_add(bytes.len() as u64) > MAX_BYTES as u64
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
        "operation": operation,
    }));
    record["streamQueue"] = Value::Array(queue);
    Table::<MonitorRecord>::new(tx).put(&MonitorRecord::from_value(record)?)?;
    Ok(true)
}

/// Return the first durable per-monitor event in stable monitor/cursor order.
pub fn next(store: &RuntimeStore) -> Result<Option<Delivery>, MonitorError> {
    let connection = store.read()?;
    let row = connection
        .query_row(
            "SELECT id,record FROM runtime_monitors WHERE json_array_length(record,'$.streamQueue')>0 ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(StoreError::from)?;
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
        event: MonitorEvent::Output { offset, bytes },
    }))
}

/// Persist the definitive result after all previously journaled output deltas.
/// The broker receipt makes this operation replayable after a crash.
pub fn enqueue_exit(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    cursor: &str,
    code: Option<i64>,
    error: Option<&str>,
    finished: f64,
) -> Result<(), MonitorError> {
    store.transaction_with(|tx| {
        let Some(record) = MonitorsRepository::get(tx, &id.to_owned())? else {
            return Err(MonitorError::Refused("Unknown command watch"));
        };
        let mut value = record.value().clone();
        if value.get("operation") != Some(&serde_json::to_value(operation).unwrap_or(Value::Null)) {
            return Err(MonitorError::InvalidDeliveryIdentity(
                "monitor operation changed",
            ));
        }
        if value["streamAckCursor"].as_str() == Some(cursor) {
            return Ok(());
        }
        let queue = value
            .get_mut("streamQueue")
            .and_then(Value::as_array_mut)
            .ok_or(MonitorError::InvalidDeliveryIdentity(
                "monitor output queue",
            ))?;
        if let Some(existing) = queue.iter().find(|item| item["cursor"] == cursor) {
            if existing["kind"] == "exit"
                && existing["code"] == code.map_or(Value::Null, |v| json!(v))
                && existing["error"] == error.map_or(Value::Null, |v| json!(v))
                && existing["finished"] == json!(finished)
            {
                return Ok(());
            }
            return Err(MonitorError::InvalidDeliveryIdentity(
                "conflicting monitor result",
            ));
        }
        if queue.len() > MAX_EVENTS {
            return Err(MonitorError::Refused("Monitor output queue is full"));
        }
        queue.push(json!({
            "kind":"exit",
            "cursor":cursor,
            "code":code,
            "error":error,
            "finished":finished,
            "operation":operation,
        }));
        let record = MonitorRecord::from_value(value).map_err(StoreError::from)?;
        Table::<MonitorRecord>::new(tx).put(&record)?;
        Ok(())
    })
}

/// Acknowledge only the exact head already applied by the P15 consumer. Repeated
/// ACK after a lost local response succeeds from `streamAckCursor`.
pub fn acknowledge(store: &RuntimeStore, delivery: &Delivery) -> Result<(), MonitorError> {
    store.transaction_with(|tx| {
        let Some(record) = MonitorsRepository::get(tx, &delivery.id)? else {
            return Err(MonitorError::Refused("Unknown command watch"));
        };
        let mut value = record.value().clone();
        if value["streamAckCursor"].as_str() == Some(&delivery.cursor) {
            return Ok(());
        }
        if value["operation"] != serde_json::to_value(&delivery.operation).unwrap_or(Value::Null) {
            return Err(MonitorError::InvalidDeliveryIdentity(
                "monitor operation changed",
            ));
        }
        let queue =
            value["streamQueue"]
                .as_array_mut()
                .ok_or(MonitorError::InvalidDeliveryIdentity(
                    "monitor output queue",
                ))?;
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
}
