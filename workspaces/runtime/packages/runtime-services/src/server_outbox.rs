//! Signed requests to paired servers in the shared outbox (effect `server_request`).
//!
//! One entry has the envelope key as request id, the paired server as target, and the
//! fingerprint of the envelope as signature. A queued request is `pending`; a request
//! that was claimed for sending is `submitted`. The record keeps the exact body, so each
//! retry sends the same request id and the same bytes. A settled request leaves the table.
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use studio_runtime_schema::persistence::{OutboxStatus, ServerDeliveryState};
use studio_runtime_store::StoreError;
use studio_runtime_store::outbox::OutboxError;
use studio_runtime_store::remote_outbox::{RemoteEffect, RemoteEntry, RemoteOutbox};
use studio_runtime_store::request_results::AdmittedRequest;

use crate::multi_server_auth::AccessError;

const EFFECT: RemoteEffect = RemoteEffect::ServerRequest;

/// The SQL condition that selects the entries of this family.
const FAMILY: &str = "kind='remote_delivery' AND json_extract(payload,'$.effect')='server_request'";
/// The action inside the stored envelope body.
const ACTION: &str = "json_extract(json_extract(payload,'$.record.body'),'$.action')";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ServerRequestRecord {
    /// The exact envelope text. A retry sends these bytes.
    pub body: String,
    pub admission: AdmittedRequest,
    pub attempts: i64,
    pub error: Option<String>,
}

pub(crate) type Entry = RemoteEntry<ServerRequestRecord>;

fn failed(error: OutboxError) -> AccessError {
    AccessError::Store(StoreError::from(error))
}

pub(crate) fn state(status: OutboxStatus) -> ServerDeliveryState {
    match status {
        OutboxStatus::Pending => ServerDeliveryState::Queued,
        _ => ServerDeliveryState::Running,
    }
}

fn status(state: ServerDeliveryState) -> OutboxStatus {
    match state {
        ServerDeliveryState::Queued => OutboxStatus::Pending,
        _ => OutboxStatus::Submitted,
    }
}

fn millis(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

/// The next attempt time of an entry in Unix seconds.
pub(crate) fn next_at(entry: &Entry) -> f64 {
    entry.next_at.map_or(0.0, |at| at as f64 / 1000.0)
}

/// The entry with this envelope key, whatever its server.
pub(crate) fn find(db: &Connection, key: &str) -> Result<Option<Entry>, AccessError> {
    RemoteOutbox::find(db, EFFECT, key).map_err(failed)
}

/// The entry with this envelope key that belongs to this server.
pub(crate) fn find_for(
    db: &Connection,
    server: &str,
    key: &str,
) -> Result<Option<Entry>, AccessError> {
    Ok(find(db, key)?.filter(|entry| entry.target == server))
}

/// Every request that is not settled, in insertion order.
pub(crate) fn all(db: &Connection) -> Result<Vec<Entry>, AccessError> {
    RemoteOutbox::entries(db, EFFECT, None).map_err(failed)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert(
    tx: &Transaction<'_>,
    server: &str,
    key: &str,
    signature: &str,
    state: ServerDeliveryState,
    next_at: f64,
    record: &ServerRequestRecord,
    now: i64,
) -> Result<(), AccessError> {
    RemoteOutbox::put(
        tx,
        EFFECT,
        server,
        key,
        signature,
        status(state),
        Some(millis(next_at)),
        record,
        now.saturating_mul(1000),
    )
    .map_err(failed)
}

/// Change the state, the next attempt time and the record of an entry.
pub(crate) fn update(
    tx: &Transaction<'_>,
    entry: &Entry,
    state: ServerDeliveryState,
    next_at: f64,
    record: &ServerRequestRecord,
) -> Result<(), AccessError> {
    RemoteOutbox::update(
        tx,
        EFFECT,
        &entry.target,
        &entry.request_id,
        &entry.signature,
        status(state),
        Some(millis(next_at)),
        record,
    )
    .map_err(failed)?;
    Ok(())
}

pub(crate) fn delete(tx: &Transaction<'_>, entry: &Entry) -> Result<(), AccessError> {
    RemoteOutbox::delete(
        tx,
        EFFECT,
        &entry.target,
        &entry.request_id,
        &entry.signature,
    )
    .map_err(failed)?;
    Ok(())
}

/// Requests that are due at `now` (Unix seconds): queued ones, and claimed ones whose action
/// the receiver can replay. `retry_actions` is an SQL list of quoted action names.
pub(crate) fn due(
    db: &Connection,
    now: i64,
    limit: i64,
    retry_actions: &str,
) -> rusqlite::Result<Vec<(String, String, ServerDeliveryState)>> {
    let sql = format!(
        "SELECT target,json_extract(payload,'$.record.body'),status FROM runtime_outbox \
         WHERE {FAMILY} AND next_at<=?1 \
           AND (status='pending' OR (status='submitted' AND {ACTION} IN ({retry_actions}))) \
         ORDER BY rowid LIMIT ?2"
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map(params![now.saturating_mul(1000), limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            state(row.get::<_, OutboxStatus>(2)?),
        ))
    })?;
    rows.collect()
}

/// The earliest next attempt of a request that the pump may send, in whole Unix seconds
/// rounded up, so the scheduler never wakes before the entry is due.
pub(crate) fn next_due_at(db: &Connection, retry_actions: &str) -> rusqlite::Result<Option<i64>> {
    let sql = format!(
        "SELECT MIN(next_at) FROM runtime_outbox WHERE {FAMILY} \
         AND (status='pending' OR (status='submitted' AND {ACTION} IN ({retry_actions})))"
    );
    db.query_row(&sql, [], |row| row.get::<_, Option<i64>>(0))
        .map(|at| at.map(|at| (at + 999).div_euclid(1000)))
}

/// A restored connection: the queued requests of this server become due at `now` (seconds).
pub(crate) fn release(tx: &Transaction<'_>, server: &str, now: i64) -> rusqlite::Result<usize> {
    RemoteOutbox::release_target(tx, server, now.saturating_mul(1000))
}

/// After an unclean stop the commit that claimed a request can be lost, so a queued request
/// whose action the receiver cannot replay may have been sent. It becomes a claimed request
/// and the pump never sends it again by itself. `exec` stays queued: its probe asks the
/// target for the receipt.
pub(crate) fn hold_unclean(tx: &Transaction<'_>, retry_actions: &str) -> rusqlite::Result<usize> {
    let sql = format!(
        "UPDATE runtime_outbox SET status='submitted' WHERE {FAMILY} AND status='pending' \
         AND {ACTION} NOT IN ({retry_actions},'exec')"
    );
    tx.execute(&sql, [])
}

/// The servers that have a queued request with a failed attempt.
pub(crate) fn offline_servers(db: &Connection) -> rusqlite::Result<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT target FROM runtime_outbox WHERE {FAMILY} AND status='pending' \
         AND json_extract(payload,'$.record.error') IS NOT NULL"
    );
    let mut statement = db.prepare(&sql)?;
    statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()
}

/// Whether a request of one of these actions for this project is not settled.
pub(crate) fn project_action_pending(
    db: &Connection,
    actions: &str,
    project: &str,
) -> rusqlite::Result<bool> {
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM runtime_outbox WHERE {FAMILY} AND {ACTION} IN ({actions}) \
         AND json_extract(json_extract(payload,'$.record.body'),'$.payload.project')=?1)"
    );
    db.query_row(&sql, [project], |row| row.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn database() -> Connection {
        let db = Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("shared outbox");
        db
    }

    fn record(action: &str) -> ServerRequestRecord {
        ServerRequestRecord {
            body: json!({"action": action, "payload": {"project": "p"}}).to_string(),
            admission: AdmittedRequest::from_persisted_fields("id".into(), "sig".into(), 1),
            attempts: 0,
            error: None,
        }
    }

    #[test]
    fn retry_keeps_the_key_the_signature_and_the_exact_body() {
        let mut db = database();
        let tx = db.transaction().unwrap();
        let first = record("event");
        insert(
            &tx,
            "peer",
            "key",
            "sig",
            ServerDeliveryState::Queued,
            0.0,
            &first,
            10,
        )
        .unwrap();
        let entry = find_for(&tx, "peer", "key").unwrap().expect("entry");
        let retried = ServerRequestRecord {
            attempts: 1,
            error: Some("unknown".into()),
            ..entry.record.clone()
        };
        update(&tx, &entry, ServerDeliveryState::Queued, 5.0, &retried).unwrap();
        let again = find(&tx, "key").unwrap().expect("entry");
        assert_eq!(again.id, entry.id);
        assert_eq!(again.signature, "sig");
        assert_eq!(again.record.body, first.body);
        assert_eq!(again.record.attempts, 1);
        assert_eq!(next_at(&again), 5.0);
        // The same key with another signature is a conflict.
        assert!(
            insert(
                &tx,
                "peer",
                "key",
                "other",
                ServerDeliveryState::Queued,
                0.0,
                &first,
                10
            )
            .is_err()
        );
    }

    #[test]
    fn only_replayable_claimed_requests_are_due_and_unclean_stop_holds_the_rest() {
        let mut db = database();
        let tx = db.transaction().unwrap();
        for (key, action, state) in [
            ("queued-event", "event", ServerDeliveryState::Queued),
            ("claimed-event", "event", ServerDeliveryState::Running),
            (
                "claimed-stop-turn",
                "interrupt",
                ServerDeliveryState::Running,
            ),
            ("queued-interrupt", "interrupt", ServerDeliveryState::Queued),
        ] {
            insert(&tx, "peer", key, key, state, 1.0, &record(action), 10).unwrap();
        }
        let due_keys = |tx: &Transaction<'_>| {
            due(tx, 5, 10, "'event'")
                .unwrap()
                .into_iter()
                .map(|(_, body, _)| body)
                .count()
        };
        // queued-event, claimed-event and queued-interrupt are due.
        assert_eq!(due_keys(&tx), 3);
        assert_eq!(hold_unclean(&tx, "'event'").unwrap(), 1);
        // queued-interrupt is now claimed and its action is not replayable.
        assert_eq!(due_keys(&tx), 2);
        assert_eq!(next_due_at(&tx, "'event'").unwrap(), Some(1));
    }
}
