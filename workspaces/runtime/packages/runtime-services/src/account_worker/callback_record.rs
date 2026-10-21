//! The durable answer obligation of one provider callback request.
//!
//! A journal batch that commits a provider request writes this record in the
//! same transaction, so the event is durably accepted and its journal cursor
//! settles with the batch. The record, not the cursor, owns the obligation to
//! answer. Its lifecycle:
//!
//! - `accepted`: the request is stored; the answer has not been submitted.
//!   Dispatch is idempotent through the tool request's own durable identity,
//!   so an accepted record may be dispatched again after a restart.
//! - `submitted`: the answer was handed to the provider session. Without a
//!   write outcome this is uncertain and never leads to a second answer.
//! - `unknown`: the write outcome is uncertain; the record is kept.
//! - retired: the answer was written, or the provider generation that waited
//!   for it ended. A retired record is deleted.
//!
//! Records live under their own receipt prefix, outside the journal namespace,
//! so journal compaction neither deletes nor implies anything about them.
use super::{
    JournalEntry, JournalPayload, WorkerError, WriteOutcome,
    store::{delete_receipt, invalid, receipt, save},
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_providers::{DecodedJournalPayload, Frame, decode_journal_entry, encode_line};
use studio_runtime_store::RuntimeStore;

/// Receipt phases. `runtime-server` diagnostics count unanswered records by the
/// `:accepted` and `:unknown` id suffixes; `callback_handoff` pins both.
const ACCEPTED: &str = "accepted";
const SUBMITTED: &str = "submitted";
const UNKNOWN: &str = "unknown";
/// Written before records existed; only deleted, never read.
const LEGACY_WRITTEN: &str = "written";

/// Everything needed to dispatch the request and to answer it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Accepted {
    pub account: String,
    pub journal_scope: String,
    pub generation: u64,
    pub cursor: u64,
    /// The provider's request id, as sent.
    pub request_id: Value,
    pub method: String,
    /// The encoded request frame.
    pub frame: String,
    /// The emitting VM actor and its epoch; none for the account session.
    pub actor: Option<String>,
    pub epoch: Option<i64>,
}

impl Accepted {
    pub(super) fn operation(&self) -> String {
        operation(&self.journal_scope, self.generation, self.cursor)
    }

    pub(super) fn frame(&self) -> Result<Frame, WorkerError> {
        match decode_journal_entry(
            self.cursor,
            self.generation,
            self.frame.clone().into_bytes(),
        )
        .payload
        {
            DecodedJournalPayload::Frame(frame) => Ok(frame),
            DecodedJournalPayload::RawStdout { .. } => Err(invalid(&self.operation())),
        }
    }
}

/// The receipt operation of one request. It is also the provider write's
/// operation id, so a lost write outcome is attributed to this record.
pub(super) fn operation(journal_scope: &str, generation: u64, cursor: u64) -> String {
    format!("callback:{journal_scope}:{generation}:{cursor}")
}

/// Store the record of a provider request journaled in this transaction.
/// Repeating it for the same entry changes nothing. Entries that are not
/// provider requests have no record.
pub(super) fn accept(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    journal_scope: &str,
    actor: Option<(&str, i64)>,
    entry: &JournalEntry,
) -> Result<(), WorkerError> {
    let JournalPayload::Frame(frame) = &entry.payload else {
        return Ok(());
    };
    let Some(Frame::Response { id, .. }) = super::callback::response(frame) else {
        return Ok(());
    };
    let method = match frame {
        Frame::Request(request) => request.method.as_str().to_owned(),
        Frame::UnknownMethod(request) => request.method.clone(),
        _ => return Ok(()),
    };
    let operation = operation(journal_scope, entry.generation.0, entry.cursor.0);
    let line = encode_line(frame).map_err(|_| invalid(&operation))?;
    let record = Accepted {
        account: account.to_owned(),
        journal_scope: journal_scope.to_owned(),
        generation: entry.generation.0,
        cursor: entry.cursor.0,
        request_id: serde_json::to_value(id).map_err(|_| invalid(&operation))?,
        method,
        frame: String::from_utf8(line).map_err(|_| invalid(&operation))?,
        actor: actor.map(|(actor, _)| actor.to_owned()),
        epoch: actor.map(|(_, epoch)| epoch),
    };
    save(
        tx,
        &operation,
        ACCEPTED,
        serde_json::to_value(record).map_err(|_| invalid(&operation))?,
    )
}

/// A request journaled before records existed was held by its unready journal
/// receipt. Give it a record unless its answer was already written.
pub(super) fn adopt_legacy(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    journal_scope: &str,
    actor: Option<(&str, i64)>,
    entry: &JournalEntry,
) -> Result<(), WorkerError> {
    let operation = operation(journal_scope, entry.generation.0, entry.cursor.0);
    if receipt(tx, &operation, LEGACY_WRITTEN)?.is_some() {
        return retire(tx, &operation);
    }
    accept(tx, account, journal_scope, actor, entry)
}

fn retire(tx: &studio_runtime_store::Transaction<'_>, operation: &str) -> Result<(), WorkerError> {
    for phase in [ACCEPTED, SUBMITTED, UNKNOWN, LEGACY_WRITTEN] {
        delete_receipt(tx, operation, phase)?;
    }
    Ok(())
}

pub(super) async fn retire_async(store: &RuntimeStore, operation: &str) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| retire(tx, &operation))
        .await
}

/// Decide whether the answer may be sent. True means no answer is owed: the
/// record is retired. A record whose answer was already submitted is
/// uncertain, never permission to answer a second time.
pub(super) async fn admit(store: &RuntimeStore, operation: &str) -> Result<bool, WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| {
            if receipt(tx, &operation, ACCEPTED)?.is_none() {
                return Ok(true);
            }
            if receipt(tx, &operation, SUBMITTED)?.is_some()
                || receipt(tx, &operation, UNKNOWN)?.is_some()
            {
                return Err(WorkerError::CallbackUnknown);
            }
            save(tx, &operation, SUBMITTED, json!(true))?;
            Ok(false)
        })
        .await
}

/// Record how the provider write of the answer ended. A write that provably
/// did not leave returns the record to `accepted`, so it may be sent again.
pub(super) async fn written(
    store: &RuntimeStore,
    operation: &str,
    outcome: WriteOutcome,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| match outcome {
            WriteOutcome::NotSent => delete_receipt(tx, &operation, SUBMITTED),
            WriteOutcome::Written => retire(tx, &operation),
            _ => save(tx, &operation, UNKNOWN, json!(true)),
        })
        .await
}

fn unsent_prefix(account: &str) -> String {
    format!("account-worker:callback:{account}:")
}

const UNSENT_SQL: &str = "SELECT a.result FROM runtime_operation_receipts a \
    WHERE a.id>=?1 AND a.id<?2 AND a.signature='accepted' \
    AND json_extract(a.result,'$.account')=?3 \
    AND (json_extract(a.result,'$.generation')<?4 OR NOT EXISTS (SELECT 1 FROM runtime_operation_receipts s \
        WHERE s.id=substr(a.id,1,length(a.id)-8)||'submitted')) ";

/// Whether any accepted record of the account needs the worker: its answer
/// was not submitted, or its generation ended and it only waits to be retired.
pub(super) fn any_unsent(
    connection: &rusqlite::Connection,
    account: &str,
    generation: u64,
) -> Result<bool, WorkerError> {
    let prefix = unsent_prefix(account);
    Ok(connection.query_row(
        &format!("SELECT EXISTS({UNSENT_SQL})"),
        params![
            prefix,
            format!("{prefix}~"),
            account,
            i64::try_from(generation).map_err(|_| WorkerError::TaskFailed)?
        ],
        |row| row.get(0),
    )?)
}

/// Up to `limit` accepted records of the account whose answer was not
/// submitted, or whose generation ended before `generation`, oldest first.
pub(super) async fn unsent(
    store: &RuntimeStore,
    account: &str,
    generation: u64,
    limit: usize,
) -> Result<Vec<Accepted>, WorkerError> {
    let account = account.to_owned();
    let limit = i64::try_from(limit).map_err(|_| WorkerError::TaskFailed)?;
    let generation = i64::try_from(generation).map_err(|_| WorkerError::TaskFailed)?;
    store
        .read_async("runtime-services/account_worker_read", move |connection| {
            let prefix = unsent_prefix(&account);
            let mut statement = connection.prepare_cached(&format!(
                "{UNSENT_SQL} ORDER BY json_extract(a.result,'$.generation'), \
                 json_extract(a.result,'$.cursor') LIMIT ?5"
            ))?;
            let rows = statement
                .query_map(
                    params![prefix, format!("{prefix}~"), account, generation, limit],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|raw| serde_json::from_str(&raw).map_err(|_| invalid(&account)))
                .collect()
        })
        .await
}
