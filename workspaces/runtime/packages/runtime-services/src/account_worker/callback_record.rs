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
//! - `submitted`: the exact answer and connection identity are stored before
//!   handoff. A supervised retry uses the same durable operation identity.
//!   Legacy records without those bytes never permit another answer.
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
use crate::provider_session_task::SessionStatus;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_process::ProviderHandle;
use studio_runtime_providers::{
    DecodedJournalPayload, Frame, ResponseBody, decode_journal_entry, encode_line,
};
use studio_runtime_store::RuntimeStore;

/// Receipt phases. `runtime-server` diagnostics count unanswered records by the
/// `:accepted` and `:unknown` id suffixes; `callback_handoff` pins both.
const ACCEPTED: &str = "accepted";
const SUBMITTED: &str = "submitted";
const UNKNOWN: &str = "unknown";
/// A timed-out first handoff may recover only through the exact saved reply.
const RECOVERABLE: &str = "recoverable";
/// Written before the one supervised reconciliation attempt, not after it.
const REPLAYED: &str = "replayed";
/// Written before records existed; only deleted, never read.
const LEGACY_WRITTEN: &str = "written";
const SAVED_REPLY_VERSION: u64 = 1;
/// Receipt range used by callback scans and registry demand. Unit: receipt IDs.
const RECEIPT_PREFIX: &str = "account-worker:callback:";
/// Maximum wait for callback admission or its final write receipt. Unit: seconds.
/// The deadline makes a lost handoff visible without repeating its business tool.
pub(super) const WRITE_RECEIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedReply {
    version: u64,
    handle: String,
    generation: u64,
    pid: u32,
    frame: String,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority: Option<super::callback::ManagedReplyAuthority>,
}

impl SavedReply {
    pub(super) fn new(frame: &Frame, status: &SessionStatus) -> Result<Self, WorkerError> {
        let info = status
            .attachment
            .info
            .as_ref()
            .ok_or(WorkerError::CallbackUnknown)?;
        let bytes = encode_line(frame).map_err(|_| WorkerError::CallbackUnknown)?;
        let saved = Self {
            version: SAVED_REPLY_VERSION,
            handle: info.handle.as_str().to_owned(),
            generation: info.generation.0,
            pid: info.pid,
            sha256: studio_runtime_store::sha256_hex(&bytes),
            frame: String::from_utf8(bytes).map_err(|_| WorkerError::CallbackUnknown)?,
            authority: None,
        };
        if !saved.same_connection(status) {
            return Err(WorkerError::CallbackUnknown);
        }
        Ok(saved)
    }

    pub(super) fn same_connection(&self, status: &SessionStatus) -> bool {
        status.generation.0 == self.generation
            && status.initialization == studio_runtime_providers::InitializationStatus::Ready
            && status.attachment.open_attempted
            && status.attachment.initialization
                == studio_runtime_providers::InitializationStatus::Ready
            && status.attachment.handle.as_str() == self.handle
            && status.attachment.info.as_ref().is_some_and(|info| {
                info.handle.as_str() == self.handle
                    && info.generation.0 == self.generation
                    && info.pid == self.pid
                    && info.pid > 0
            })
    }

    fn response(&self, accepted: &Accepted) -> Result<Frame, WorkerError> {
        if self.version != SAVED_REPLY_VERSION
            || self.generation != accepted.generation
            || self.handle != ProviderHandle::for_account(&accepted.account).as_str()
            || self.sha256 != studio_runtime_store::sha256_hex(self.frame.as_bytes())
        {
            return Err(WorkerError::CallbackUnknown);
        }
        let DecodedJournalPayload::Frame(frame @ Frame::Response { .. }) = decode_journal_entry(
            accepted.cursor,
            accepted.generation,
            self.frame.clone().into_bytes(),
        )
        .payload
        else {
            return Err(WorkerError::CallbackUnknown);
        };
        let Frame::Response { id, .. } = &frame else {
            unreachable!()
        };
        if serde_json::to_value(id).map_err(|_| WorkerError::CallbackUnknown)?
            != accepted.request_id
        {
            return Err(WorkerError::CallbackUnknown);
        }
        Ok(frame)
    }
}

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
    /// Loaded beside the accepted row, never part of its stored request.
    #[serde(skip)]
    pub submitted: Option<Value>,
}

impl Accepted {
    /// Only an exact saved reply on a durable, same-process transport qualifies.
    pub(super) fn saved_response(
        &self,
        status: &SessionStatus,
    ) -> Result<Option<Frame>, WorkerError> {
        let Some(raw) = self.submitted.clone() else {
            return Ok(None);
        };
        let saved: SavedReply =
            serde_json::from_value(raw).map_err(|_| WorkerError::CallbackUnknown)?;
        if self.managed_frame()?.is_some() && saved.authority.is_none() {
            return Err(WorkerError::CallbackUnknown);
        }
        if self.actor.is_some()
            || !status.supervisor_write_deduplication
            || !saved.same_connection(status)
        {
            return Err(WorkerError::CallbackUnknown);
        }
        saved.response(self).map(Some)
    }
    pub(super) fn operation(&self) -> String {
        operation(&self.journal_scope, self.generation, self.cursor)
    }

    fn managed_frame(&self) -> Result<Option<Frame>, WorkerError> {
        let frame = self.frame()?;
        Ok(matches!(&frame, Frame::Request(request) if request.method.as_str() == "item/tool/call").then_some(frame))
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
        submitted: None,
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
    for phase in [
        ACCEPTED,
        SUBMITTED,
        UNKNOWN,
        RECOVERABLE,
        REPLAYED,
        LEGACY_WRITTEN,
    ] {
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

/// Legacy admission has no exact saved reply, so a lost outcome cannot retry.
/// True means that the record is retired and no answer is owed.
#[cfg(test)]
pub(super) async fn admit(store: &RuntimeStore, operation: &str) -> Result<bool, WorkerError> {
    admit_reply(store, operation, None).await
}

/// Persist exact bytes before admission. An existing match grants one retry
/// only when the transport owns durable supervisor deduplication.
/// True forbids a write: the record retired or current actor authority refused it.
pub(super) async fn admit_reply(
    store: &RuntimeStore,
    operation: &str,
    reply: Option<(SavedReply, bool)>,
) -> Result<bool, WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| {
            let Some(accepted) = receipt(tx, &operation, ACCEPTED)? else {
                return Ok(true);
            };
            if receipt(tx, &operation, UNKNOWN)?.is_some() {
                return Err(WorkerError::CallbackUnknown);
            }
            let value = if let Some((mut saved, deduplicated)) = reply {
                let accepted: Accepted =
                    serde_json::from_value(accepted).map_err(|_| invalid(&operation))?;
                if accepted.operation() != operation || accepted.actor.is_some() {
                    return Err(WorkerError::CallbackUnknown);
                }
                let response = saved.response(&accepted)?;
                let refusal = matches!(
                    &response,
                    Frame::Response {
                        body: ResponseBody::Error(_),
                        ..
                    }
                ) || matches!(&response, Frame::Response { body: ResponseBody::Result(value), .. }
                        if value.get("success") == Some(&Value::Bool(false)));
                let prior = receipt(tx, &operation, SUBMITTED)?;
                let managed = accepted.managed_frame()?;
                let current_authority = managed
                    .as_ref()
                    .map(|frame| {
                        super::callback::managed_reply_authority_in(
                            tx,
                            &accepted.account,
                            accepted.generation,
                            frame,
                        )
                    })
                    .transpose()?
                    .flatten();
                if let Some(prior) = prior {
                    let original: SavedReply = serde_json::from_value(prior.clone())
                        .map_err(|_| WorkerError::CallbackUnknown)?;
                    if managed.is_some()
                        && !original.authority.as_ref().is_some_and(|authority| {
                            current_authority
                                .as_ref()
                                .is_some_and(|current| authority.same_actor(current))
                        })
                    {
                        save(tx, &operation, UNKNOWN, json!(true))?;
                        return Ok(true);
                    }
                    saved.authority = original.authority;
                    let value = serde_json::to_value(saved).map_err(|_| invalid(&operation))?;
                    if !deduplicated
                        || prior != value
                        || receipt(tx, &operation, REPLAYED)?.is_some()
                    {
                        save(tx, &operation, UNKNOWN, json!(true))?;
                        return Ok(true);
                    }
                    save(tx, &operation, REPLAYED, json!(true))?;
                    return Ok(false);
                }
                saved.authority = current_authority;
                let missing_authority = managed.is_some() && saved.authority.is_none();
                let value = serde_json::to_value(saved).map_err(|_| invalid(&operation))?;
                // A malformed request still receives its first refusal. It
                // cannot recover that response without captured authority.
                if missing_authority && !refusal {
                    save(tx, &operation, SUBMITTED, value)?;
                    save(tx, &operation, UNKNOWN, json!(true))?;
                    return Ok(true);
                }
                value
            } else {
                if receipt(tx, &operation, SUBMITTED)?.is_some() {
                    return Err(WorkerError::CallbackUnknown);
                }
                json!(true)
            };
            save(tx, &operation, SUBMITTED, value)?;
            Ok(false)
        })
        .await
}

/// A timed-out first supervised handoff retains exact bytes for one deduplicated
/// reconciliation. A second lost outcome, a legacy row, or a different transport
/// stays unknown. Late shutdown evidence must not erase this distinction.
pub(super) async fn expired(
    store: &RuntimeStore,
    operation: &str,
    status: Option<SessionStatus>,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| {
            let accepted = receipt(tx, &operation, ACCEPTED)?
                .and_then(|value| serde_json::from_value::<Accepted>(value).ok());
            let submitted = receipt(tx, &operation, SUBMITTED)?;
            let recoverable = match (accepted, submitted, status) {
                (Some(mut accepted), Some(submitted), Some(status))
                    if receipt(tx, &operation, UNKNOWN)?.is_none()
                        && receipt(tx, &operation, REPLAYED)?.is_none() =>
                {
                    accepted.submitted = Some(submitted);
                    accepted.operation() == operation
                        && accepted
                            .saved_response(&status)
                            .is_ok_and(|reply| reply.is_some())
                }
                _ => false,
            };
            save(
                tx,
                &operation,
                if recoverable { RECOVERABLE } else { UNKNOWN },
                json!(true),
            )
        })
        .await
}

/// A refused recovery is not a late receipt from the timed-out attachment.
/// Keep the refusal even when the first timeout left a recoverable marker.
pub(super) async fn refuse_recovery(
    store: &RuntimeStore,
    operation: &str,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| save(tx, &operation, UNKNOWN, json!(true)))
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
            WriteOutcome::Written => retire(tx, &operation),
            _ if receipt(tx, &operation, RECOVERABLE)?.is_some()
                && receipt(tx, &operation, REPLAYED)?.is_none() =>
            {
                Ok(())
            }
            WriteOutcome::NotSent
                if receipt(tx, &operation, REPLAYED)?.is_none()
                    && receipt(tx, &operation, UNKNOWN)?.is_none() =>
            {
                delete_receipt(tx, &operation, SUBMITTED)
            }
            _ => save(tx, &operation, UNKNOWN, json!(true)),
        })
        .await
}

fn unsent_prefix(account: &str) -> String {
    format!("{RECEIPT_PREFIX}{account}:")
}

macro_rules! callback_rows_sql {
    () => {
        "FROM runtime_operation_receipts a \
    LEFT JOIN runtime_operation_receipts s ON s.id=substr(a.id,1,length(a.id)-8)||'submitted' \
    WHERE a.id>=?1 AND a.id<?2 AND a.signature='accepted' "
    };
}
macro_rules! callback_unknown_absent_sql {
    () => {
        "NOT EXISTS (SELECT 1 FROM runtime_operation_receipts u \
        WHERE u.id=substr(a.id,1,length(a.id)-8)||'unknown')"
    };
}
/// One owner for the recoverable reply predicate in both worker and registry.
macro_rules! saved_reply_available_sql {
    () => {
        concat!(
            "json_type(s.result)='object' AND ",
            callback_unknown_absent_sql!(),
            " \
        AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts r \
        WHERE r.id=substr(a.id,1,length(a.id)-8)||'replayed')"
        )
    };
}
const UNSENT_SQL: &str = concat!(
    "SELECT a.result,s.result ",
    callback_rows_sql!(),
    " \
    AND json_extract(a.result,'$.account')=?3 \
    AND (json_extract(a.result,'$.generation')<?4 OR ",
    callback_unknown_absent_sql!(),
    ") \
    AND (json_extract(a.result,'$.generation')<?4 OR s.id IS NULL OR (",
    saved_reply_available_sql!(),
    ")) "
);
const REPLY_DEMAND_ROWS_SQL: &str = concat!(
    callback_rows_sql!(),
    " \
    AND json_type(a.result,'$.account')='text' AND json_extract(a.result,'$.account')<>'' \
    AND json_extract(a.result,'$.actor') IS NULL AND ",
    saved_reply_available_sql!()
);

/// A saved response is account demand even when there is no queued user input.
/// This selects metadata only; the ready attachment validates bytes and identity.
pub(super) fn reply_accounts(
    connection: &rusqlite::Connection,
    after: &str,
    limit: usize,
) -> Result<Vec<String>, WorkerError> {
    let mut query = connection.prepare_cached(&format!(
        "SELECT DISTINCT json_extract(a.result,'$.account') {REPLY_DEMAND_ROWS_SQL} \
         AND json_extract(a.result,'$.account')>?3 ORDER BY 1 LIMIT ?4"
    ))?;
    Ok(query
        .query_map(
            params![
                RECEIPT_PREFIX,
                format!("{RECEIPT_PREFIX}~"),
                after,
                i64::try_from(limit).map_err(|_| WorkerError::TaskFailed)?
            ],
            |row| row.get(0),
        )?
        .collect::<Result<Vec<_>, _>>()?)
}

pub(super) fn has_reply_demand(
    connection: &rusqlite::Connection,
    account: &str,
) -> Result<bool, WorkerError> {
    let prefix = unsent_prefix(account);
    Ok(connection.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 {REPLY_DEMAND_ROWS_SQL} \
        AND json_extract(a.result,'$.account')=?3)"
        ),
        params![prefix, format!("{prefix}~"), account],
        |row| row.get(0),
    )?)
}

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
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(raw, submitted)| {
                    let mut accepted: Accepted =
                        serde_json::from_str(&raw).map_err(|_| invalid(&account))?;
                    accepted.submitted = submitted
                        .map(|raw| serde_json::from_str(&raw))
                        .transpose()
                        .map_err(|_| invalid(&account))?;
                    Ok(accepted)
                })
                .collect()
        })
        .await
}
