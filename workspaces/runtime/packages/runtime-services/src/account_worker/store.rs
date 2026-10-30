//! Account database operations. Transaction callbacks stay synchronous and are
//! submitted to RuntimeStore's bounded writer actor.
use super::retained::storage::TransportRequestExt;
use super::*;
use crate::continuation_runtime::CapacityStartOutcome;
use crate::diagnostics::measure_runtime_stage as stage;
use crate::notification_dispatch::{
    NotificationEffects, NotificationPublicationBatch, PreparedSubmission,
    ProviderDeliveryIdentity, dispatch_notification_with_effects, flush_notification_streams,
};
use rusqlite::{OptionalExtension, params};
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, OwnerKind};
use studio_runtime_schema::shared_values::{
    PROVIDER_RESUME_UNKNOWN_ERROR, PROVIDER_SESSION_MISSING_ERROR,
    PROVIDER_THREAD_START_UNKNOWN_ERROR,
};
use studio_runtime_store::CachedSql;
use studio_runtime_store::outbox::{Outbox, OutboxError, OutboxRow, Publication, PublicationClass};
use studio_runtime_store::{AgentRecord, NotificationDispatchRepository};

pub(super) const PUBLICATION_DRAIN_LIMIT: usize = 64;
/// Pending publication receipts above which token-rate observations are
/// coalesced. Complete usage observations may exceed this count. Unit: receipts.
pub(super) const MAX_PUBLICATION_RECEIPTS: usize = 256;
pub(super) const MAX_COALESCED_TOKEN_RATE_EFFECTS: usize = 64;
const MAX_COALESCED_TOKEN_RATE_BYTES: usize = 256 * 1024;
/// Decimal digits of `u64::MAX`. Journal receipt IDs zero-pad the provider
/// cursor to this width so receipt key order is cursor order.
const JOURNAL_CURSOR_DIGITS: usize = 20;
/// Settled journal receipts one compaction reclaims: two full batches plus the
/// receipt that held the checkpoint, so a released backlog shrinks faster than
/// new batches add to it.
const JOURNAL_COMPACTION_LIMIT: usize = 2 * super::JOURNAL_BATCH_MAX_EVENTS + 1;

/// Signature of a pending publication receipt that holds token-rate effects
/// and receives coalesced ones at the receipt bound.
const PUBLICATION_TOKEN_RATE: PublicationClass = PublicationClass::TokenRate;
/// Signature of the pending publication receipt that carries the coalesced
/// state invalidation.
const PUBLICATION_INVALIDATION: PublicationClass = PublicationClass::Invalidation;
/// Signature of every other pending publication receipt.
const PUBLICATION_PENDING: PublicationClass = PublicationClass::Pending;

pub(super) fn save_publication(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    payload: Publication,
) -> Result<(), WorkerError> {
    type Effect = crate::notification_dispatch::NotificationPostCommit;
    let old = Outbox::get::<Publication<Effect>>(tx, operation).map_err(|error| match error {
        OutboxError::Json(_) => StoreError::InvalidStoredObject {
            table: "runtime_outbox",
            id: operation.to_owned(),
        }
        .into(),
        error @ (OutboxError::Sql(_) | OutboxError::Conflict(_)) => outbox_error(error),
    })?;
    if let Some(old) = old {
        let mut stored = old.payload;
        let mut incoming: Publication<Effect> =
            serde_json::from_str(&serde_json::to_string(&payload).map_err(StoreError::from)?)
                .map_err(StoreError::from)?;
        for value in [&mut stored, &mut incoming] {
            value.analytics_overflow = Some(value.analytics_overflow.unwrap_or_default());
            value.publication_overflow = Some(value.publication_overflow.unwrap_or_default());
        }
        if stored != incoming {
            return Err(invalid(operation));
        }
        return Ok(());
    }
    Outbox::put(
        tx,
        &OutboxRow {
            id: operation.to_owned(),
            kind: OutboxKind::Publication,
            owner_kind: OwnerKind::Account,
            owner_id: payload.account.clone(),
            request_id: None,
            signature: studio_runtime_store::sha256_hex(operation.as_bytes()),
            status: OutboxStatus::Pending,
            account: Some(payload.account.clone()),
            stream_id: None,
            generation: None,
            ordinal: Some(payload.sequence as i64),
            next_at: Some(0),
            payload,
            created: (now() * 1000.0) as i64,
        },
    )
    .map_err(outbox_error)
}

/// The publication receipt operation of one effect. Sequence and index are
/// zero-padded like journal cursors, so receipt key order is drain order.
pub(super) fn publication_operation(account: &str, sequence: u64, index: usize) -> String {
    format!(
        "notification-publication:{account}:{sequence:0JOURNAL_CURSOR_DIGITS$}:{index:0JOURNAL_CURSOR_DIGITS$}"
    )
}

/// The journal receipt operation of one provider cursor.
pub(super) fn journal_operation(journal_scope: &str, generation: u64, cursor: u64) -> String {
    format!("journal:{journal_scope}:{generation}:{cursor:0JOURNAL_CURSOR_DIGITS$}")
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct JournalSettlement {
    cursor: u64,
    digest: String,
    ready: bool,
    committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notification: Option<crate::notification_dispatch::NotificationSettlement>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "family", content = "payload", rename_all = "snake_case")]
pub(super) enum ProviderCall {
    Account(Box<AccountCall>),
    Retained(Box<super::retained::storage::RetainedCall>),
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct AccountCall {
    pub context: Pending,
    pub request: super::retained::storage::TransportRequest,
    pub written: Option<super::retained::storage::TransportRequest>,
    pub restarted: bool,
}

pub(super) fn execution_admission(
    tx: &rusqlite::Connection,
    attempt_id: &str,
) -> Result<
    (
        String,
        studio_runtime_store::request_results::AdmittedRequest,
    ),
    WorkerError,
> {
    let attempt = studio_runtime_store::ExecutionHistoryRepository::attempt(tx, attempt_id)?
        .ok_or_else(|| invalid(attempt_id))?;
    let root = attempt
        .start
        .root_request_id
        .ok_or_else(|| invalid(attempt_id))?;
    let admitted = studio_runtime_store::ExecutionHistoryRepository::admission(tx, attempt_id)?
        .ok_or_else(|| invalid(attempt_id))?;
    Ok((root, admitted))
}

pub(super) fn account_submission(
    tx: &rusqlite::Connection,
    operation: &str,
) -> Result<Option<AccountCall>, WorkerError> {
    match Outbox::get::<ProviderCall>(tx, operation).map_err(outbox_error)? {
        Some(OutboxRow {
            payload: ProviderCall::Account(call),
            ..
        }) => Ok(Some(*call)),
        Some(_) => Err(invalid(operation)),
        None => Ok(None),
    }
}

pub(super) fn save_account_submission(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    request: &RecoveredRequest,
) -> Result<(), WorkerError> {
    if account_submission(tx, &context.operation)?.is_some() {
        return Err(invalid(&context.operation));
    }
    let vm = AgentRepository::get(tx, &AgentId::from(context.intent.agent_id.as_str()))?
        .is_some_and(|agent| {
            agent.execution_mode == Some(studio_runtime_store::AgentExecutionMode::Vm)
        });
    let account = &context.intent.account_key;
    let request = super::retained::storage::TransportRequest::from(request);
    let signature = studio_runtime_store::sha256_hex(
        &serde_json::to_vec(&json!({"context":context,"request":request}))
            .map_err(StoreError::from)?,
    );
    Outbox::put(
        tx,
        &OutboxRow {
            id: context.operation.clone(),
            kind: OutboxKind::ProviderCall,
            owner_kind: OwnerKind::Agent,
            owner_id: context.intent.agent_id.clone(),
            request_id: None,
            signature,
            status: OutboxStatus::Unknown,
            account: Some(account.clone()),
            stream_id: Some(if vm {
                super::vm_journal_scope(account, &context.intent.agent_id)
            } else {
                account.clone()
            }),
            generation: Some(request.generation as i64),
            ordinal: None,
            next_at: None,
            payload: ProviderCall::Account(Box::new(AccountCall {
                context: context.clone(),
                request,
                written: None,
                restarted: false,
            })),
            created: (now() * 1000.0) as i64,
        },
    )
    .map_err(outbox_error)
}

fn account_written(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    request: &RecoveredRequest,
) -> Result<(), WorkerError> {
    let Some(mut row) = Outbox::get::<ProviderCall>(tx, operation).map_err(outbox_error)? else {
        return Err(invalid(operation));
    };
    let ProviderCall::Account(call) = &mut row.payload else {
        return Err(invalid(operation));
    };
    let written = super::retained::storage::TransportRequest::from(request);
    if call.written.as_ref().is_some_and(|old| old != &written) {
        return Err(invalid(operation));
    }
    call.written = Some(written);
    row.status = OutboxStatus::Submitted;
    Outbox::put(tx, &row).map_err(outbox_error)
}

fn save_journal(
    tx: &studio_runtime_store::Transaction<'_>,
    identity: JournalIdentity<'_>,
    entry: &JournalEntry,
    digest: String,
    committed: bool,
) -> Result<(), WorkerError> {
    let id = journal_operation(identity.journal_scope, entry.generation.0, entry.cursor.0);
    let old = Outbox::get::<JournalSettlement>(tx, &id).map_err(outbox_error)?;
    let mut row = old.unwrap_or_else(|| OutboxRow {
        signature: studio_runtime_store::sha256_hex(id.as_bytes()),
        id,
        kind: OutboxKind::JournalEntry,
        owner_kind: if identity.expected_vm_actor.is_some() {
            OwnerKind::Agent
        } else {
            OwnerKind::Account
        },
        owner_id: identity
            .expected_vm_actor
            .map_or(identity.account, |(actor, _)| actor)
            .to_owned(),
        request_id: None,
        status: OutboxStatus::Pending,
        account: Some(identity.account.to_owned()),
        stream_id: Some(identity.journal_scope.to_owned()),
        generation: Some(entry.generation.0 as i64),
        ordinal: Some(entry.cursor.0 as i64),
        next_at: Some(0),
        payload: JournalSettlement {
            cursor: entry.cursor.0,
            digest: digest.clone(),
            ready: committed,
            committed,
            notification: None,
        },
        created: (now() * 1000.0) as i64,
    });
    if row.payload.digest != digest && !row.payload.digest.is_empty() {
        return Err(invalid(&row.id));
    }
    row.payload.digest = digest;
    row.payload.ready = committed;
    row.payload.committed = committed;
    row.status = if committed {
        OutboxStatus::Committed
    } else {
        OutboxStatus::Pending
    };
    row.next_at = if committed { None } else { Some(0) };
    Outbox::put(tx, &row).map_err(outbox_error)
}

fn notification_scope(
    identity: &ProviderDeliveryIdentity,
) -> studio_runtime_store::StoreResult<&str> {
    identity
        .provider_handle
        .strip_prefix("account:")
        .ok_or(StoreError::RecordContract(
            "notification provider handle is not an account stream",
        ))
}

pub(crate) fn notification_acknowledged(
    tx: &rusqlite::Connection,
    identity: &ProviderDeliveryIdentity,
) -> studio_runtime_store::StoreResult<bool> {
    let scope = notification_scope(identity)?;
    checkpoint(tx, scope, identity.generation).map(|cursor| identity.cursor <= cursor)
}

pub fn get_notification(
    tx: &rusqlite::Connection,
    identity: &ProviderDeliveryIdentity,
) -> studio_runtime_store::StoreResult<Option<crate::notification_dispatch::NotificationSettlement>>
{
    let scope = notification_scope(identity)?;
    Ok(Outbox::get::<JournalSettlement>(
        tx,
        &journal_operation(scope, identity.generation, identity.cursor),
    )
    .map_err(StoreError::from)?
    .and_then(|row| row.payload.notification))
}

pub fn put_notification(
    tx: &studio_runtime_store::Transaction<'_>,
    identity: &ProviderDeliveryIdentity,
    notification: crate::notification_dispatch::NotificationSettlement,
) -> studio_runtime_store::StoreResult<()> {
    let scope = notification_scope(identity)?;
    let id = journal_operation(scope, identity.generation, identity.cursor);
    let mut row =
        if let Some(row) = Outbox::get::<JournalSettlement>(tx, &id).map_err(StoreError::from)? {
            row
        } else {
            let (account, actor) = scope
                .rsplit_once(":vm:")
                .map_or((scope, None), |(account, actor)| (account, Some(actor)));
            OutboxRow {
                signature: studio_runtime_store::sha256_hex(id.as_bytes()),
                id,
                kind: OutboxKind::JournalEntry,
                owner_kind: if actor.is_some() {
                    OwnerKind::Agent
                } else {
                    OwnerKind::Account
                },
                owner_id: actor.unwrap_or(account).to_owned(),
                request_id: None,
                status: OutboxStatus::Pending,
                account: Some(account.to_owned()),
                stream_id: Some(scope.to_owned()),
                generation: Some(identity.generation as i64),
                ordinal: Some(identity.cursor as i64),
                next_at: Some(0),
                payload: JournalSettlement {
                    cursor: identity.cursor,
                    digest: String::new(),
                    ready: false,
                    committed: false,
                    notification: None,
                },
                created: (now() * 1000.0) as i64,
            }
        };
    if let Some(old) = &row.payload.notification
        && old != &notification
    {
        return Err(StoreError::RecordContract(
            "notification identity conflicts with its committed payload",
        ));
    }
    row.payload.notification = Some(notification);
    Outbox::put(tx, &row).map_err(StoreError::from)
}

pub(super) struct SubmissionEvidence {
    pub turn: Option<String>,
    pub submission: Option<PreparedSubmission>,
    pub guard: Option<preparation::CommitGuard>,
    pub connection_id: String,
    /// The durable effect that owes this submission; an authorized commit
    /// deletes it, since a replay would find the submitted receipt.
    pub effect: Option<String>,
}

pub(super) fn invalid(id: &str) -> WorkerError {
    StoreError::InvalidSyncObject {
        scope: "account_worker".into(),
        id: id.into(),
    }
    .into()
}

const JOURNAL_FRAME_PAYLOAD_PREFIX: &[u8] = b"{\"frame\":";
const JOURNAL_FRAME_PAYLOAD_SUFFIX: &[u8] = b"}";

fn journal_frame_payload(frame: &Frame, operation: &str) -> Result<Vec<u8>, WorkerError> {
    let encoded = studio_runtime_providers::encode_line(frame).map_err(|_| invalid(operation))?;
    let encoded = encoded
        .strip_suffix(b"\n")
        .ok_or_else(|| invalid(operation))?;
    let mut payload = Vec::with_capacity(
        encoded.len() + JOURNAL_FRAME_PAYLOAD_PREFIX.len() + JOURNAL_FRAME_PAYLOAD_SUFFIX.len(),
    );
    payload.extend_from_slice(JOURNAL_FRAME_PAYLOAD_PREFIX);
    payload.extend_from_slice(encoded);
    payload.extend_from_slice(JOURNAL_FRAME_PAYLOAD_SUFFIX);
    Ok(payload)
}

/// Whether `attempt` is the attempt of the agent's last completed turn. Such
/// an attempt is history, not a live start: an earlier build restored it from
/// a stale disconnect record.
fn completed_attempt(agent: &studio_runtime_store::AgentRecord, attempt: &Value) -> bool {
    attempt["turnId"].as_str().is_some_and(|turn| {
        !turn.is_empty()
            && agent.extra.get("lastCompletedTurn").and_then(Value::as_str) == Some(turn)
    })
}
/// Hold reason of an agent whose thread request outcome is unknown after a
/// restart. The request is never sent again; a later message continues.
pub(super) const RECEIPT_PENDING_HOLD: &str = "thread-preparation/receipt-pending";
/// Outstanding provider calls belonging to one account, without completed history.
pub(super) fn account_obligations(
    connection: &rusqlite::Connection,
    account: &str,
) -> Result<Vec<OutboxRow<ProviderCall>>, WorkerError> {
    let mut rows = Vec::new();
    for status in [OutboxStatus::Submitted, OutboxStatus::Unknown] {
        rows.extend(
            Outbox::for_account(
                connection,
                account,
                OutboxKind::ProviderCall,
                status,
                u32::MAX,
            )
            .map_err(outbox_error)?,
        );
    }
    Ok(rows)
}

pub(super) fn record_supervisor_written(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    operation: &str,
    generation: Option<i64>,
    remote_id: Option<u64>,
) -> Result<(), WorkerError> {
    let Some(mut row) = Outbox::get::<ProviderCall>(tx, operation).map_err(outbox_error)? else {
        return Ok(());
    };
    if row.account.as_deref() != Some(account)
        || row.stream_id.as_deref() != Some(account)
        || row.generation != generation
        || !matches!(row.status, OutboxStatus::Unknown | OutboxStatus::Submitted)
    {
        return Ok(());
    }
    match &mut row.payload {
        ProviderCall::Account(call) => {
            let mut request = call.request.clone();
            if request.remote_id.is_some() && remote_id.is_some() && request.remote_id != remote_id
            {
                return Err(invalid(operation));
            }
            if remote_id.is_some() {
                request.remote_id = remote_id;
            }
            if call.written.as_ref().is_some_and(|old| old != &request) {
                return Err(invalid(operation));
            }
            call.written = Some(request);
            row.status = OutboxStatus::Submitted;
        }
        ProviderCall::Retained(call) => {
            let Some(request) = &mut call.request else {
                return Ok(());
            };
            if request.remote_id.is_some() && remote_id.is_some() && request.remote_id != remote_id
            {
                return Err(invalid(operation));
            }
            if remote_id.is_some() {
                request.remote_id = remote_id;
            }
            call.written = true;
        }
    }
    Outbox::put(tx, &row).map_err(outbox_error)
}

pub(super) fn has_pending_thread_start(
    connection: &rusqlite::Connection,
    agent: &str,
) -> Result<bool, WorkerError> {
    Ok(Outbox::for_owner::<ProviderCall>(connection,OwnerKind::Agent,agent,OutboxKind::ProviderCall).map_err(|error| if let studio_runtime_store::outbox::OutboxError::Json(_) = error {
        invalid(agent)
    } else {
        outbox_error(error)
    })?
        .into_iter().any(|row| matches!(row.payload,ProviderCall::Account(call) if matches!(call.context.method,Method::ThreadStart|Method::ThreadResume))))
}

#[derive(serde::Serialize)]
enum LateProviderReply<'a> {
    Result(&'a Value),
    Error(&'a Value),
}

fn continued_thread_call(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
) -> Result<bool, WorkerError> {
    let agent = stored_agent(tx, &context.intent.agent_id)?;
    let attempt =
        studio_runtime_store::ExecutionHistoryRepository::attempt(tx, &context.intent.attempt_id)?
            .ok_or_else(|| invalid(&context.intent.agent_id))?;
    let begun = attempt
        .start
        .created
        .ok_or_else(|| invalid(&context.intent.agent_id))?;
    Ok(tx.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending' AND kind IN (?3,?4) AND created>?5)",
        params![context.intent.agent_id, agent.epoch, studio_runtime_schema::persistence::EventKind::User, studio_runtime_schema::persistence::EventKind::Followup, begun], |row| row.get(0),
    )?)
}

/// A later user message explicitly supersedes a restarted, uncertain thread call.
fn supersede_thread_calls(
    tx: &studio_runtime_store::Transaction<'_>,
    agent_id: &str,
) -> Result<bool, WorkerError> {
    let rows =
        Outbox::for_owner::<ProviderCall>(tx, OwnerKind::Agent, agent_id, OutboxKind::ProviderCall)
            .map_err(outbox_error)?;
    let mut superseded = false;
    for row in rows {
        let ProviderCall::Account(call) = &row.payload else {
            continue;
        };
        let context = &call.context;
        if !matches!(context.method, Method::ThreadStart | Method::ThreadResume) {
            continue;
        }
        if row.status != OutboxStatus::Unknown
            || !call.restarted
            || !continued_thread_call(tx, context)?
        {
            return Ok(false);
        }
        studio_runtime_store::request_results::RequestResults::settle_abandoned(tx, &row)?;
        let mut agent = stored_agent(tx, agent_id)?;
        if let Some(hold) = &agent.start_outcome_hold
            && hold.operation_id() == Some(context.operation.as_str())
        {
            agent.start_outcome_hold = None;
            AgentRepository::put(tx, &agent)?;
        }
        superseded = true;
    }
    Ok(superseded)
}
/// The stored agent `id`; a row that vanished inside the transaction is invalid.
fn stored_agent(
    tx: &studio_runtime_store::Transaction<'_>,
    id: &str,
) -> Result<studio_runtime_store::AgentRecord, StoreError> {
    AgentRepository::get(tx, &AgentId::from(id))?.ok_or_else(|| StoreError::InvalidSyncObject {
        scope: "runtime_agents".into(),
        id: id.to_owned(),
    })
}
/// `restart` is true for the account's first worker in this backend process;
/// a later worker follows a provider disconnect while the server kept running.
pub(super) async fn recover(
    store: &RuntimeStore,
    account: &str,
    generation: Generation,
    restart: bool,
) -> Result<Vec<(Pending, RecoveredRequest)>, WorkerError> {
    let account = account.to_owned();
    scheduler::lifecycle_transaction_with(store, move |tx| {
        SchedulerRepository::recover_unsubmitted_for_account(tx, &account)?;
        let at = now();
        // Before generation reconciliation reads it as unresolved, a settled
        // attempt that a stale recovery receipt restored returns its agent to
        // the outcome of its finished turn.
        SchedulerRepository::settle_stale_recovery(tx, &account, at)?;
        // The repository preserves Python recovery records. Journal delivery needs
        // the scoped attempt restored before notifications can be acknowledged.
        // Only an open receipt of an unfinished turn is restored. A settled
        // attempt owes no notification; restored, it would hide a
        // receipt-pending hold and let the next generation interrupt the
        // finished agent again. Openness is read before reconciliation, which
        // can continue an interrupted turn and so close its receipt.
        let mut statement = tx.prepare("SELECT a.id FROM runtime_agents a WHERE a.account_key=?1 AND json_extract(a.record,'$.startAttempt') IS NULL AND COALESCE(json_extract(a.record,'$.disconnectRecovery.startAttempt.connectionGeneration'),json_extract(a.record,'$.restartRecovery.startAttempt.connectionGeneration'))=?2 AND a.hold_status='clear'")?;
        let ids = statement
            .query_map(
                params![
                    account,
                    i64::try_from(generation.0)
                        .map_err(|_| StoreError::SchemaMismatch("generation overflow".into()))?,
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut restore = Vec::new();
        for id in ids {
            let agent = stored_agent(tx, &id)?;
            if let Some(attempt) = SchedulerRepository::open_recovery_attempt(tx, &agent)? {
                restore.push((id, attempt));
            }
        }
        let interrupted = SchedulerRepository::reconcile_generation(
            tx,
            &account,
            generation.0,
            at,
            restart,
        )?;
        // A native question or approval of another provider generation can no
        // longer reach its sender; its card must not wait for the user.
        crate::provider_callbacks::expire_ended_generations(tx, &account, generation.0).map_err(
            |error| match error {
                crate::provider_callbacks::CallbackError::Store(error) => WorkerError::Store(error),
                crate::provider_callbacks::CallbackError::Expired => WorkerError::Expired,
                crate::provider_callbacks::CallbackError::Invalid(_) | crate::provider_callbacks::CallbackError::InvalidResultSchema(_) | crate::provider_callbacks::CallbackError::Dependency(_) | crate::provider_callbacks::CallbackError::Stale | crate::provider_callbacks::CallbackError::Conflict | crate::provider_callbacks::CallbackError::Unknown | crate::provider_callbacks::CallbackError::Stored(_) => invalid(&account),
            },
        )?;
        // A provider replacement in this backend is a disconnect. Actual backend
        // restarts keep their separate history-reconciliation notification path.
        if !restart {
            for agent_id in interrupted {
                crate::lifecycle_effects::notify_parent_of_disconnect(tx, &agent_id, at)?;
            }
        }
        for (id, attempt) in restore {
            let mut agent = stored_agent(tx, &id)?;
            if agent.extra.get("startAttempt").is_none_or(Value::is_null) {
                agent.extra.insert("startAttempt".into(), attempt);
                AgentRepository::put(tx, &agent)?;
            }
        }
        let rows = account_obligations(tx,&account)?;
        let mut recovered = Vec::new();
        for mut row in rows {
            let ProviderCall::Account(call) = &mut row.payload else {continue;};
            let context = call.context.clone();
            let held: bool = tx.query_row_cached("SELECT hold_status!='clear' FROM runtime_agents WHERE id=?1",[&context.intent.agent_id],|r|r.get(0)).optional()?.unwrap_or(false);
            if held {continue;}
            let request = call.written.as_ref().unwrap_or(&call.request).recovered();
            let vm_owned =
                AgentRepository::get(tx, &AgentId::from(context.intent.agent_id.as_str()))?
                    .is_some_and(|agent| {
                        agent.execution_mode == Some(studio_runtime_store::AgentExecutionMode::Vm)
                    });
            if vm_owned {
                continue;
            }
            if context.method == Method::TurnStart {
                uncertain(tx, &context, Some(&request.ticket), false)?;
            }
            if matches!(
                context.method,
                Method::ThreadStart | Method::ThreadResume
            ) {
                call.restarted = true;
                row.status = OutboxStatus::Unknown;
                Outbox::put(tx,&row).map_err(outbox_error)?;
                let current =
                    AgentRepository::get(tx, &AgentId::from(context.intent.agent_id.as_str()))?;
                if !continued_thread_call(tx, &context)? && current.as_ref().is_some_and(|a| {
                    a.epoch == context.intent.epoch
                        && account_key(a) == account
                        && a.extra.get("startAttempt").is_none_or(|attempt| {
                            attempt.is_null() || completed_attempt(a, attempt)
                        })
                        && (a.auto_wake == Some(true)
                            || a.start_outcome_hold.as_ref()
                                .and_then(studio_runtime_store::StoredStartOutcomeHold::operation_id)
                                == Some(context.operation.as_str()))
                }) {
                    hold_thread_receipt(
                        tx,
                        &context,
                        request.ticket.generation,
                        RECEIPT_PENDING_HOLD,
                    )?;
                }
            }
            // An old generation's ticket cannot be restored into this transport.
            // Its durable receipt and visible Unknown hold remain for reconciliation.
            if request.ticket.generation != generation.0 {
                continue;
            }
            recovered.push((context, request));
        }
        Ok(recovered)
    })
    .await
}

/// Restore outstanding tickets belonging to one VM actor into that actor's
/// guest session. Host tickets are handled by `recover` and never cross this
/// boundary.
pub(super) fn recover_vm_actor_requests(
    connection: &rusqlite::Connection,
    account: &str,
    agent: &str,
    generation: u64,
) -> Result<Vec<(Pending, RecoveredRequest)>, WorkerError> {
    let Some(agent_record) = AgentRepository::get(connection, &AgentId::from(agent))? else {
        return Ok(Vec::new());
    };
    if agent_record.execution_mode != Some(studio_runtime_store::AgentExecutionMode::Vm) {
        return Ok(Vec::new());
    }
    let mut recovered = Vec::new();
    for row in Outbox::for_owner::<ProviderCall>(
        connection,
        OwnerKind::Agent,
        agent,
        OutboxKind::ProviderCall,
    )
    .map_err(outbox_error)?
    {
        let ProviderCall::Account(call) = row.payload else {
            continue;
        };
        if call.context.intent.account_key != account {
            continue;
        }
        let request = call.written.as_ref().unwrap_or(&call.request).recovered();
        if request.ticket.generation == generation {
            recovered.push((call.context, request));
        }
    }
    Ok(recovered)
}

pub(super) async fn claim(
    store: Arc<RuntimeStore>,
    account: &str,
    agent: &str,
    epoch: i64,
) -> Result<Option<TurnIntent>, WorkerError> {
    let account = account.to_owned();
    let agent = agent.to_owned();
    let read_agent = agent.clone();
    let (snapshot, has_radio_turn) = store
        .read_async("runtime-services/account_worker_claim_read", move |tx| {
            let snapshot = AgentRepository::get(tx, &AgentId::from(read_agent.as_str()))?;
            let has_radio_turn = tx.query_row_cached(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 \
                 AND status='pending' AND kind='radio_turn')",
                rusqlite::params![read_agent.as_str(), epoch],
                |row| row.get::<_, bool>(0),
            )?;
            Ok::<_, WorkerError>((snapshot, has_radio_turn))
        })
        .await?;
    let prepared = if has_radio_turn {
        let prepared = crate::host_project_runtime::PreparedHost::load(&store)
            .await
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker_claim_host",
            move || {
                prepared
                    .recheck_paths()
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                Ok::<_, WorkerError>(Some(prepared))
            },
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??
    } else {
        None
    };
    if snapshot.as_ref().is_none_or(|record| {
        record.epoch != epoch
            || account_key(record) != account.as_str()
            || studio_runtime_store::truthy(record.extra.get("moveImportPending"))
    }) {
        return Ok(None);
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::TaskFailed)?;
    let attempt = studio_runtime_store::sha256_hex(&bytes);
    scheduler::lifecycle_transaction_with(
        &store,
        move |tx| -> Result<Option<TurnIntent>, WorkerError> {
            let record = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?;
            if record
                .as_ref()
                .is_none_or(|a| account_key(a) != account.as_str())
            {
                return Ok(None);
            }
            if record
                .as_ref()
                .is_some_and(|a| studio_runtime_store::truthy(a.extra.get("moveImportPending")))
            {
                return Ok(None);
            }
            if record.as_ref().is_some_and(|a| {
                a.extra
                    .get("claudePreInputRetry")
                    .is_some_and(|v| !v.is_null())
            }) {
                hold_before_claim(
                    tx,
                    &agent,
                    "preparation/claude-frozen-input-recovery-required",
                )?;
                return Ok(None);
            }
            let guarding = has_pending_thread_start(tx, agent.as_str())?;
            if guarding {
                tx.execute_batch("SAVEPOINT supersede_thread")?;
            }
            let claim = (|| -> Result<Option<TurnIntent>, WorkerError> {
                if guarding
                    && (!supersede_thread_calls(tx, &agent)?
                        || has_pending_thread_start(tx, &agent)?)
                {
                    return Ok(None);
                }
                let pending = record
                    .as_ref()
                    .and_then(|a| a.extra.get("startAttempt"))
                    .filter(|a| {
                        a["submitted"] == false
                            && matches!(
                                a["prepareError"].as_str(),
                                Some(
                                    "workspace-readiness/pending" | "model-catalog/refresh-pending"
                                )
                            )
                    });
                let attempt = if guarding {
                    &attempt
                } else {
                    pending.and_then(|a| a["id"].as_str()).unwrap_or(&attempt)
                };
                if let Some(prepared) = &prepared {
                    prepared
                        .recheck(tx)
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                    let mut statement = tx.prepare(
                "SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events \
                 WHERE agent=?1 AND epoch=?2 AND status='pending' ORDER BY created,rowid",
            )?;
                    let events = statement
                        .query_map(rusqlite::params![agent.as_str(), epoch], |row| {
                            Ok(studio_runtime_store::QueueEvent {
                                id: studio_runtime_store::QueueEventId(row.get(0)?),
                                agent: studio_runtime_store::AgentId(row.get(1)?),
                                kind: row.get(2)?,
                                text: row.get(3)?,
                                status: row.get(4)?,
                                created: row.get(5)?,
                                epoch: row.get(6)?,
                                turn_id: row.get(7)?,
                                error: row.get(8)?,
                            })
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    drop(statement);
                    let selected = crate::radio_runtime::select_pending(
                        tx,
                        prepared,
                        &record.as_ref().ok_or(WorkerError::Closed)?.value(),
                        &events,
                        now(),
                    )
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                    // `select_pending` owns the radio rule: one radio turn, the exact
                    // answer to the live turn, or all ordinary rows. Nothing selected
                    // means the live turn holds the floor: wait without a hold; the
                    // agent is claimable again once that turn ends.
                    if selected.is_empty() {
                        return Ok(None);
                    }
                    let ordinary = events.iter().filter(|e| e.kind != "radio_turn").count();
                    if selected.len() < ordinary || selected[0].kind == "radio_turn" {
                        return scheduler::claim_selected_event_in_transaction(
                            tx,
                            &agent,
                            epoch,
                            attempt,
                            now(),
                            &selected[0].id.0,
                        )
                        .map_err(Into::into);
                    }
                }
                match scheduler::claim_preparation_in_transaction(tx, &agent, epoch, attempt, now())
                {
                    Ok(intent) => Ok(intent),
                    Err(scheduler::SchedulerError::PolicyRefused(reason)) => {
                        hold_before_claim(tx, &agent, &reason)?;
                        Ok(None)
                    }
                    Err(error) => Err(error.into()),
                }
            })();
            if guarding {
                if matches!(claim, Ok(Some(_))) {
                    tx.execute_batch("RELEASE supersede_thread")?;
                } else {
                    tx.execute_batch("ROLLBACK TO supersede_thread; RELEASE supersede_thread")?;
                }
            }
            claim
        },
    )
    .await
}

/// Persist a visible fail-closed state for one malformed agent without decoding
/// it through the typed repository. The original invalid field is retained for
/// repair, while this agent can no longer be selected for provider work.
pub async fn hold_invalid_agent_record(
    store: &RuntimeStore,
    agent_id: &str,
    expected_pointer: &str,
    expected_reason: &str,
    diagnostic: &str,
) -> Result<(), WorkerError> {
    let server_id = store.local_server_id()?;
    let agent_id = agent_id.to_owned();
    let expected_pointer = expected_pointer.to_owned();
    let expected_reason = expected_reason.to_owned();
    let diagnostic = diagnostic.to_owned();
    store
        .transaction_with(move |tx| {
            hold_invalid_agent_record_in(
                tx,
                server_id.as_deref(),
                &agent_id,
                &expected_pointer,
                &expected_reason,
                &diagnostic,
            )
        })
        .await
}
/// [`hold_invalid_agent_record`] inside the caller's transaction.
pub(super) fn hold_invalid_agent_record_in(
    tx: &studio_runtime_store::Transaction<'_>,
    server_id: Option<&str>,
    agent_id: &str,
    expected_pointer: &str,
    expected_reason: &str,
    diagnostic: &str,
) -> Result<(), WorkerError> {
    let raw: Option<String> = tx
        .query_row_cached(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(());
    };
    match AgentRepository::get(tx, &AgentId::from(agent_id)) {
        Ok(_) => return Ok(()),
        Err(StoreError::InvalidSyncObjectField {
            scope,
            id,
            pointer,
            reason,
        }) if scope == "runtime_agents"
            && id == agent_id
            && pointer == expected_pointer
            && reason == expected_reason => {}
        Err(StoreError::InvalidSyncObjectField { .. }) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let parsed: Value = serde_json::from_str(&raw).map_err(|_| StoreError::InvalidSyncObject {
        scope: "runtime_agents".into(),
        id: agent_id.to_owned(),
    })?;
    AgentRepository::hold(
        tx,
        agent_id,
        studio_runtime_schema::persistence::HoldStatus::Invalid,
        Some(diagnostic),
    )?;

    // Publish a minimal safe agent view so the renderer can show that this
    // row is held while the malformed persisted bytes remain untouched.
    let public = crate::sync_views::invalid_agent_view(agent_id, &parsed, diagnostic);
    let context = crate::sync_views::AgentViewContext {
        project_server_id: server_id.map(str::to_owned),
        ..Default::default()
    };
    let projection = crate::sync_views::agent_entity_view(&public, &context).ok_or_else(|| {
        StoreError::InvalidSyncObject {
            scope: "runtime_agents".into(),
            id: agent_id.to_owned(),
        }
    })?;
    crate::sync_policy::publish_agent_projection(
        tx,
        agent_id,
        projection,
        false,
        &|_, _, _, _| true,
    )?;
    Ok(())
}
fn account_key(agent: &studio_runtime_store::AgentRecord) -> &str {
    agent.account_key.as_str()
}

pub(super) async fn submitted(
    store: &RuntimeStore,
    context: impl Into<Arc<Pending>>,
    account: &str,
    request: RecoveredRequest,
    evidence: SubmissionEvidence,
) -> Result<bool, WorkerError> {
    let context: Arc<Pending> = context.into();
    let SubmissionEvidence {
        turn,
        submission,
        guard,
        connection_id,
        effect,
    } = evidence;
    let account = account.to_owned();
    let blocked_intent = context.intent.clone();
    let blocked_method = context.method;
    let result = scheduler::lifecycle_transaction_with(store, move |tx| {
        if account_submission(tx, &context.operation)?.is_some() {
            return Ok(false);
        }
        let i = &context.intent;
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(i.agent_id.as_str()))? else {
            return Ok(false);
        };
        if account_key(&agent) != account.as_str() || i.account_key != account {
            return Ok(false);
        }
        // Retirement is a committed outcome, whereas a stale preparation must
        // roll back its freeze. Reconcile before making any preparation writes.
        if context.method == Method::TurnStart
            && guard.is_some()
            && !SchedulerRepository::reconcile_obsolete_before_submit(
                tx,
                &i.agent_id,
                i.epoch,
                &i.attempt_id,
            )?
        {
            return Ok(false);
        }
        if let Some(guard) = guard.as_ref() {
            let checked = agent_fault::in_savepoint(tx, "preparation_commit", || {
                stage("turn_submission.submitted.commit", || {
                    preparation::commit(tx, &context, guard)
                })
            })?;
            if let Err(error) = checked {
                if let WorkerError::PreparationRefused(reason) = &error
                    && preparation::refuse_prepared_idle(tx, i, guard, reason)?
                {
                    return Ok(false);
                }
                return Err(error);
            }
        }
        let authorized = stage("turn_submission.submitted.authorize", || {
            Ok::<_, WorkerError>(match context.method {
                Method::TurnStart
                    if i.provider == studio_runtime_providers::ProviderKind::Claude =>
                {
                    preparation::commit_claude(
                        tx,
                        i,
                        &id_string(&request.ticket.id),
                        request.ticket.generation,
                        &connection_id,
                        submission.as_ref().ok_or_else(|| invalid(&i.attempt_id))?,
                        guard
                            .as_ref()
                            .ok_or_else(|| invalid("Claude commit guard"))?,
                    )?
                }
                Method::TurnStart => commit_prepared_turn(
                    tx,
                    i,
                    &id_string(&request.ticket.id),
                    request.ticket.generation,
                    &connection_id,
                    submission.as_ref().ok_or_else(|| invalid(&i.attempt_id))?,
                )?,
                Method::TurnInterrupt => SchedulerRepository::prepare_interrupt(
                    tx,
                    &i.agent_id,
                    i.thread_id.as_deref().unwrap_or(""),
                    turn.as_deref().unwrap_or(""),
                    request.ticket.generation,
                    &id_string(&request.ticket.id),
                )?,
                Method::Initialize
                | Method::Initialized
                | Method::Ack
                | Method::AccountLoginCancel
                | Method::AccountLoginStart
                | Method::AccountRateLimitResetCreditConsume
                | Method::ThreadLoadedList
                | Method::ConfigRead
                | Method::SkillsExtraRootsSet
                | Method::ModelList
                | Method::AccountRead
                | Method::AccountRateLimitsRead
                | Method::ThreadStart
                | Method::ThreadRealtimeStart
                | Method::ThreadRealtimeStop
                | Method::ThreadRealtimeAppendSpeech
                | Method::ThreadRealtimeItemStarted
                | Method::ThreadRealtimeTranscriptDelta
                | Method::ThreadRealtimeTranscriptDone
                | Method::ThreadRealtimeSdp
                | Method::ThreadRealtimeError
                | Method::ThreadRealtimeClosed
                | Method::ThreadResume
                | Method::ThreadRead
                | Method::ThreadTurnsList
                | Method::TurnSteer
                | Method::ThreadList
                | Method::ThreadArchive
                | Method::ThreadFork
                | Method::ThreadNameSet
                | Method::ThreadMetadataUpdate
                | Method::ThreadSettingsUpdate
                | Method::ThreadCompactStart
                | Method::ThreadRollback
                | Method::ThreadItemsList
                | Method::ThreadGoalSet
                | Method::ThreadGoalUpdated
                | Method::ThreadUnarchive
                | Method::ThreadUnsubscribe
                | Method::ProcessSpawn
                | Method::ProcessWriteStdin
                | Method::ProcessKill
                | Method::ProcessResizePty
                | Method::CommandExec
                | Method::CommandExecWrite
                | Method::CommandExecResize
                | Method::CommandExecTerminate
                | Method::ThreadBackgroundTerminalsList
                | Method::ThreadBackgroundTerminalsTerminate
                | Method::ThreadBackgroundTerminalsClean
                | Method::ThreadCommandExecutionBackground
                | Method::ThreadQueueList
                | Method::ClaudeState
                | Method::ClaudeRepairReport
                | Method::ClaudeFileBackups
                | Method::ClaudeSettings
                | Method::ClaudeActivity
                | Method::ClaudeStopTask
                | Method::ClaudeShellPolicy
                | Method::ClaudeDiagnostics
                | Method::ClaudeMoveVersions
                | Method::McpServerReconnect
                | Method::McpServerStatusList
                | Method::SkillsList
                | Method::OperationStatus
                | Method::ReviewStart
                | Method::BrowserAnswer
                | Method::BrowserCallback
                | Method::BrowserClose
                | Method::BrowserWait
                | Method::ClaudeCommands
                | Method::ClaudeMoveExport
                | Method::ClaudeMoveIdentity
                | Method::ClaudeMovePreflight
                | Method::ClaudeMoveProof
                | Method::Detach
                | Method::EventRead
                | Method::Exec
                | Method::Generate
                | Method::Health
                | Method::Info
                | Method::LayrHealth
                | Method::Next
                | Method::Open
                | Method::ProjectEnsure
                | Method::ProjectExec
                | Method::ProjectImport
                | Method::ProviderList
                | Method::Replay
                | Method::ResponseStatus
                | Method::RetireCommand
                | Method::SecretsList
                | Method::SecretsRemove
                | Method::SecretsSet
                | Method::ShareConfigure
                | Method::ShareName
                | Method::ShareStatus
                | Method::Sign
                | Method::Status
                | Method::SyncMacApply
                | Method::SyncMacHandoff
                | Method::SyncMacHashes
                | Method::SyncMacHead
                | Method::SyncMacRead
                | Method::SyncMacWorktree
                | Method::VaultConfigure
                | Method::VaultPush
                | Method::VaultStatus
                | Method::UploadBegin
                | Method::UploadChunk
                | Method::UploadCommit
                | Method::Verify
                | Method::Write
                | Method::CurrentTimeRead
                | Method::ItemToolCall
                | Method::ItemCommandExecutionRequestApproval
                | Method::ItemFileChangeRequestApproval
                | Method::ItemPermissionsRequestApproval
                | Method::McpServerElicitationRequest
                | Method::ItemToolRequestUserInput
                | Method::ItemStarted
                | Method::ItemCompleted
                | Method::ItemAgentMessageDelta
                | Method::ItemReasoningTextDelta
                | Method::ItemCommandExecutionOutputDelta
                | Method::CommandExecOutputDelta
                | Method::TurnStarted
                | Method::TurnCompleted
                | Method::TurnPlanUpdated
                | Method::TurnDiffUpdated
                | Method::ThreadClosed
                | Method::ThreadStatusChanged
                | Method::ThreadTokenUsageUpdated
                | Method::ThreadShellPolicyUpdated
                | Method::ThreadIdleCompactionUpdated
                | Method::AccountLoginCompleted
                | Method::AccountUpdated
                | Method::AccountRateLimitsUpdated
                | Method::ModelSafetyBufferingUpdated
                | Method::ProviderGenerationStarted
                | Method::ProtocolError => {
                    agent.epoch == i.epoch
                        && agent.extra.get("startAttempt").unwrap_or(&Value::Null)["id"]
                            == i.attempt_id
                        && agent.extra.get("startAttempt").unwrap_or(&Value::Null)["submitted"]
                            == false
                        && agent.auto_wake == Some(true)
                        && agent.extra.get("nativeFailureHold").unwrap_or(&Value::Null) != true
                }
            })
        })?;
        if !authorized {
            if guard.is_some() {
                return Err(WorkerError::PreparationRefused(
                    "preparation/submission-identity-changed".into(),
                ));
            }
            return Ok(false);
        }
        stage("turn_submission.submitted.persist", || {
            if let Some(compiled) = &context.compiled {
                preparation::persist_compilation(tx, compiled)?;
            }
            if let Some(key) = &effect {
                delete_effect(tx, key)?;
            }
            save_account_submission(tx, &context, &request)
        })?;
        Ok(true)
    })
    .await;
    match result {
        Err(WorkerError::Notification(
            crate::notification_dispatch::NotificationDispatchError::ConflictingInput,
        )) => {
            let reason = "Submitted input conflicts with an earlier record".to_owned();
            scheduler::settle_start_outcome(store, &blocked_intent, TurnStart::Blocked { reason })
                .await?;
            Ok(false)
        }
        Err(WorkerError::PreparationRefused(reason)) => {
            let reason = if preparation::snapshot_changed(&reason) {
                let reason = changed_start_reason(blocked_method.as_str(), &reason);
                if return_changed_start(store, &blocked_intent, &reason).await? {
                    return Ok(false);
                }
                reason
            } else {
                reason
            };
            scheduler::settle_start_outcome(store, &blocked_intent, TurnStart::Blocked { reason })
                .await?;
            Ok(false)
        }
        result => result,
    }
}

/// Return the input batch of a start whose commit was refused because the
/// agent state changed after its preparation. The refused commit rolled back
/// and its request was never written, so a new preparation from the current
/// state cannot repeat a message; the caller wakes the queued agent. Returns
/// false when the start cannot return (see
/// `SchedulerRepository::return_unstarted_input`, bounded per run).
async fn return_changed_start(
    store: &RuntimeStore,
    intent: &TurnIntent,
    reason: &str,
) -> Result<bool, WorkerError> {
    let identity = intent.identity();
    let reason = reason.to_owned();
    Ok(scheduler::lifecycle_transaction(store, move |tx| {
        SchedulerRepository::return_unstarted_input(tx, &identity, &reason)
    })
    .await?)
}

/// The stored error of a start whose `method` request was refused at its
/// commit because the agent state changed while Studio prepared it. `refusal`
/// is the refusal code with the names of the changed fields.
fn changed_start_reason(method: &str, refusal: &str) -> String {
    format!(
        "Provider did not send {method}: the agent state changed while Studio prepared it \
         ({refusal}). The input is kept."
    )
}
#[derive(Default)]
pub(super) struct SettledOutcome {
    /// The request is settled and leaves the pending set.
    pub retired: bool,
    pub effect: Option<Effect>,
    /// The committed thread acceptance; published only after its commit.
    pub loaded: Option<crate::thread_prepare::LoadedIdentity>,
}

/// `session_closed` is true while the worker settles its ended session's outcomes.
pub(super) async fn outcome(
    store: Arc<RuntimeStore>,
    account: String,
    resources: Arc<preparation::PreparationResources>,
    pending: Option<Arc<Pending>>,
    outcome: SessionOutcome,
    session_closed: bool,
) -> Result<(SettledOutcome, Option<agent_fault::AgentFault>), WorkerError> {
    let Some(context) = pending else {
        if let SessionOutcome::GenerationClosed { generation } = outcome {
            scheduler::lifecycle_transaction(&store, move |tx| {
                disconnect_provider_generation(tx, &account, generation, false)
            })
            .await?;
        }
        return Ok((SettledOutcome::default(), None));
    };
    let resources = Arc::new(
        resources
            .for_receipt(
                &store,
                &context.intent.agent_id,
                context
                    .thread
                    .as_ref()
                    .map(|attempt| attempt.snapshot.actor.cwd.as_str()),
            )
            .await?,
    );
    let state_root = store.root().to_path_buf();
    let server_id = store.local_server_id()?;
    let step = OutcomeStep {
        account,
        resources,
        context,
        outcome,
        session_closed,
    };
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        settle_outcome_isolated(tx, &state_root, server_id.as_deref(), step)
    })
    .await
}

/// [`settle_outcome`] in its agent's savepoint. A reply whose stored data
/// fails for its agent holds that agent; the request still retires when its
/// provider outcome is final (a reply, or proven not sent), so it is never
/// sent again and never waits forever.
fn settle_outcome_isolated(
    tx: &studio_runtime_store::Transaction<'_>,
    state_root: &std::path::Path,
    server_id: Option<&str>,
    step: OutcomeStep,
) -> Result<(SettledOutcome, Option<agent_fault::AgentFault>), WorkerError> {
    let context = Arc::clone(&step.context);
    let account = step.account.clone();
    let final_outcome = matches!(
        step.outcome,
        SessionOutcome::Resolved {
            outcome: RequestOutcome::Reply { .. },
            ..
        } | SessionOutcome::Write {
            outcome: WriteOutcome::NotSent,
            ..
        }
    );
    let mut subject = Some(context.intent.agent_id.clone());
    // A final outcome retires its request before the hold, which then sees
    // no request in flight. A thread/start keeps its unresolved receipt: the
    // provider may have created the thread, and another start would create a
    // second one. Its agent stays held by that receipt as well (see `recover`).
    let retire = || {
        if final_outcome && context.method != Method::ThreadStart {
            if context.method == Method::TurnStart
                && let Some(key) = effect_key(&account, &Effect::Submit(context.intent.clone()))
            {
                delete_effect(tx, &key)?;
            }
            forget_request(tx, &context.operation)?;
        }
        Ok(())
    };
    let fault = match agent_fault::isolate(
        tx,
        server_id,
        agent_fault::FaultStage::ProviderReply,
        &mut subject,
        |_| settle_outcome(tx, state_root, step),
        retire,
    )? {
        Ok(settled) => return Ok((settled, None)),
        Err(fault) => fault.with_method(Some(context.method.as_str())),
    };
    Ok((
        SettledOutcome {
            retired: final_outcome,
            ..SettledOutcome::default()
        },
        Some(fault),
    ))
}

/// One request outcome to settle; `session_closed` is true while the worker
/// settles its ended session's outcomes.
pub(super) struct OutcomeStep {
    pub account: String,
    pub resources: Arc<preparation::PreparationResources>,
    pub context: Arc<Pending>,
    pub outcome: SessionOutcome,
    pub session_closed: bool,
}

/// Settle one request outcome inside a canvas transaction.
fn settle_outcome(
    tx: &studio_runtime_store::Transaction<'_>,
    state_root: &std::path::Path,
    step: OutcomeStep,
) -> Result<SettledOutcome, WorkerError> {
    let OutcomeStep {
        account,
        resources,
        context,
        outcome,
        session_closed,
    } = step;
    let i = &context.intent;
    if let SessionOutcome::Resolved {
        outcome: RequestOutcome::Reply { body, .. },
        ..
    } = &outcome
        && matches!(context.method, Method::ThreadStart | Method::ThreadResume)
        && account_submission(tx, &context.operation)?.is_none()
    {
        let (_, source) = execution_admission(tx, &i.attempt_id)?;
        let abandoned: Option<String> = tx
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [&context.operation],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(abandoned) = abandoned {
            let _: studio_runtime_store::request_results::ResultEnvelope<
                studio_runtime_store::request_results::AbandonedResult,
            > = serde_json::from_str(&abandoned).map_err(StoreError::from)?;
            let reply = match body {
                ResponseBody::Result(value) => LateProviderReply::Result(value),
                ResponseBody::Error(value) => LateProviderReply::Error(value),
            };
            let signature = serde_json::to_string(&reply).map_err(StoreError::from)?;
            let token =
                source.derive_result("account-call-late", &[&context.operation], &signature);
            studio_runtime_store::request_results::RequestResults::settle(
                tx,
                &token,
                &studio_runtime_store::request_results::ResultEnvelope {
                    actor: Some(i.agent_id.clone()),
                    reply,
                    references: vec![i.attempt_id.clone()],
                },
            )
            .map_err(StoreError::from)?;
            return Ok(SettledOutcome {
                retired: true,
                ..SettledOutcome::default()
            });
        }
    }
    if context.method == Method::TurnSteer {
        let wake = steer::outcome(tx, &context, &outcome)?;
        if let SessionOutcome::Write {
            outcome: WriteOutcome::Written,
            recovery: Some(request),
            ..
        } = &outcome
        {
            account_written(tx, &context.operation, request)?;
        }
        let retired = matches!(
            outcome,
            SessionOutcome::Resolved { .. }
                | SessionOutcome::Write {
                    outcome: WriteOutcome::NotSent,
                    ..
                }
        );
        if retired {
            forget_request(tx, &context.operation)?;
        }
        return Ok(SettledOutcome {
            retired,
            effect: wake.map(|(agent, epoch)| Effect::Wake(agent, epoch)),
            loaded: None,
        });
    }
    let mut effect = None;
    let mut loaded = None;
    let retired = match outcome {
        SessionOutcome::Write {
            outcome: WriteOutcome::Written,
            recovery: Some(request),
            ..
        } => {
            account_written(tx, &context.operation, &request)?;
            false
        }
        SessionOutcome::Write {
            outcome: WriteOutcome::Written,
            ..
        } => false,
        SessionOutcome::Write {
            outcome: WriteOutcome::NotSent,
            ticket,
            not_sent,
            ..
        } => {
            if let Some((agent, epoch)) =
                settle_not_sent(tx, &context, ticket.as_ref(), not_sent, session_closed)?
            {
                effect = Some(Effect::Wake(agent, epoch));
            }
            true
        }
        SessionOutcome::Write { ticket, .. } => uncertain(tx, &context, ticket.as_ref(), false)?,
        SessionOutcome::Resolved {
            outcome: RequestOutcome::Unknown,
            ticket,
        } => uncertain(tx, &context, Some(&ticket), false)?,
        SessionOutcome::Resolved {
            ticket,
            outcome: RequestOutcome::Reply { body, .. },
        } => {
            match context.method {
                Method::ThreadResume | Method::ThreadStart if start_settled(tx, &i.attempt_id)? => {
                    // A reply after its attempt's start outcome settled (for
                    // example held at the reply deadline) is stale for that
                    // attempt, as in Python, which drops a reply whose
                    // preparation already finished. It neither prepares nor
                    // settles the attempt a second time.
                }
                Method::ThreadResume | Method::ThreadStart if matches!(&body, ResponseBody::Error(error) if session_missing(error)) =>
                {
                    // The saved session is gone, so no retry can resume it. The input
                    // was never sent: it settles as not sent, and the transcript stays.
                    SchedulerRepository::settle_start_outcome(
                        tx,
                        &i.identity(),
                        "failed",
                        Some(PROVIDER_SESSION_MISSING_ERROR),
                    )?;
                }
                Method::ThreadResume | Method::ThreadStart => {
                    // Name the provider's own error; a rejected reply names
                    // the check that failed. The input is never sent here.
                    let accepted = match &body {
                        ResponseBody::Result(value) => preparation::accept_thread(
                            tx, state_root, &resources, &context, &ticket, value,
                        )?
                        .map_err(|rejection| {
                            format!("the reply does not match the prepared thread: {rejection}")
                        }),
                        ResponseBody::Error(error) => Err(error["message"]
                            .as_str()
                            .map(|message| message.chars().take(300).collect::<String>())
                            .unwrap_or_else(|| "provider error".to_owned())),
                    };
                    let rejected = match accepted {
                        Ok((next, thread)) => {
                            effect = Some(next);
                            loaded = Some(thread);
                            None
                        }
                        Err(detail) => Some(detail),
                    };
                    if let Some(detail) = rejected {
                        let reason = format!(
                            "Provider thread resume failed ({detail}); input remains pending."
                        );
                        let reason = reason.as_str();
                        let current =
                            AgentRepository::get(tx, &AgentId::from(i.agent_id.as_str()))?;
                        if current.as_ref().is_some_and(|a| {
                            a.epoch == i.epoch
                                && account_key(a) == account
                                && a.start_outcome_hold.as_ref().and_then(
                                    studio_runtime_store::StoredStartOutcomeHold::operation_id,
                                ) == Some(context.operation.as_str())
                        }) {
                            hold_thread_receipt(tx, &context, ticket.generation, reason)?;
                        } else {
                            SchedulerRepository::settle_start_outcome(
                                tx,
                                &i.identity(),
                                "held",
                                Some(reason),
                            )?;
                        }
                    }
                }
                Method::TurnStart => match body {
                    ResponseBody::Result(value) => {
                        if let Some(turn) = value["turn"]["id"].as_str().filter(|s| !s.is_empty()) {
                            stage_capacity_start(
                                tx,
                                i,
                                CapacityStartOutcome::Accepted { turn_id: turn },
                            )?;
                            if SchedulerRepository::settle_started_provider(
                                tx,
                                &i.agent_id,
                                i.epoch,
                                &i.attempt_id,
                                &id_string(&ticket.id),
                                &i.account_key,
                                ticket.generation,
                                i.thread_id.as_deref().unwrap_or(""),
                                turn,
                            )? == Some(true)
                            {
                                effect = Some(Effect::Interrupt {
                                    agent: i.agent_id.clone(),
                                    thread: i.thread_id.clone().unwrap_or_default(),
                                    turn: turn.into(),
                                });
                            }
                            preparation::accept_turn(tx, &context, &ticket, turn)?;
                        } else {
                            uncertain(tx, &context, Some(&ticket), false)?;
                            return Ok(SettledOutcome::default());
                        }
                    }
                    ResponseBody::Error(error) => {
                        if crate::disk_space::is_disk_full_error(&error) {
                            // Hold new starts for the recheck interval.
                            crate::disk_space::observe_full();
                        }
                        SchedulerRepository::settle_start_rejected_provider(
                            tx,
                            &i.agent_id,
                            i.epoch,
                            &i.attempt_id,
                            &id_string(&ticket.id),
                            &i.account_key,
                            ticket.generation,
                            i.thread_id.as_deref().unwrap_or(""),
                            error,
                        )?;
                        stage_capacity_start(
                            tx,
                            i,
                            CapacityStartOutcome::Failed {
                                reason: "The provider rejected the turn start.",
                                unknown: false,
                            },
                        )?;
                    }
                },
                Method::TurnInterrupt => {
                    let response = match &body {
                        ResponseBody::Result(response) => response,
                        ResponseBody::Error(error) => &error["data"]["interruptReceipt"],
                    };
                    SchedulerRepository::settle_interrupt_inputs(
                        tx,
                        &i.agent_id,
                        &i.account_key,
                        ticket.generation,
                        i.thread_id.as_deref().unwrap_or_default(),
                        response,
                    )?;
                    SchedulerRepository::settle_interrupt(
                        tx,
                        &id_string(&ticket.id),
                        ticket.generation,
                        if matches!(body, ResponseBody::Error(_)) {
                            "rejected"
                        } else {
                            "accepted"
                        },
                    )?;
                }
                Method::Initialize
                | Method::Initialized
                | Method::Ack
                | Method::AccountLoginCancel
                | Method::AccountLoginStart
                | Method::AccountRateLimitResetCreditConsume
                | Method::ThreadLoadedList
                | Method::ConfigRead
                | Method::SkillsExtraRootsSet
                | Method::ModelList
                | Method::AccountRead
                | Method::AccountRateLimitsRead
                | Method::ThreadRealtimeStart
                | Method::ThreadRealtimeStop
                | Method::ThreadRealtimeAppendSpeech
                | Method::ThreadRealtimeItemStarted
                | Method::ThreadRealtimeTranscriptDelta
                | Method::ThreadRealtimeTranscriptDone
                | Method::ThreadRealtimeSdp
                | Method::ThreadRealtimeError
                | Method::ThreadRealtimeClosed
                | Method::ThreadRead
                | Method::ThreadTurnsList
                | Method::TurnSteer
                | Method::ThreadList
                | Method::ThreadArchive
                | Method::ThreadFork
                | Method::ThreadNameSet
                | Method::ThreadMetadataUpdate
                | Method::ThreadSettingsUpdate
                | Method::ThreadCompactStart
                | Method::ThreadRollback
                | Method::ThreadItemsList
                | Method::ThreadGoalSet
                | Method::ThreadGoalUpdated
                | Method::ThreadUnarchive
                | Method::ThreadUnsubscribe
                | Method::ProcessSpawn
                | Method::ProcessWriteStdin
                | Method::ProcessKill
                | Method::ProcessResizePty
                | Method::CommandExec
                | Method::CommandExecWrite
                | Method::CommandExecResize
                | Method::CommandExecTerminate
                | Method::ThreadBackgroundTerminalsList
                | Method::ThreadBackgroundTerminalsTerminate
                | Method::ThreadBackgroundTerminalsClean
                | Method::ThreadCommandExecutionBackground
                | Method::ThreadQueueList
                | Method::ClaudeState
                | Method::ClaudeRepairReport
                | Method::ClaudeFileBackups
                | Method::ClaudeSettings
                | Method::ClaudeActivity
                | Method::ClaudeStopTask
                | Method::ClaudeShellPolicy
                | Method::ClaudeDiagnostics
                | Method::ClaudeMoveVersions
                | Method::McpServerReconnect
                | Method::McpServerStatusList
                | Method::SkillsList
                | Method::OperationStatus
                | Method::ReviewStart
                | Method::BrowserAnswer
                | Method::BrowserCallback
                | Method::BrowserClose
                | Method::BrowserWait
                | Method::ClaudeCommands
                | Method::ClaudeMoveExport
                | Method::ClaudeMoveIdentity
                | Method::ClaudeMovePreflight
                | Method::ClaudeMoveProof
                | Method::Detach
                | Method::EventRead
                | Method::Exec
                | Method::Generate
                | Method::Health
                | Method::Info
                | Method::LayrHealth
                | Method::Next
                | Method::Open
                | Method::ProjectEnsure
                | Method::ProjectExec
                | Method::ProjectImport
                | Method::ProviderList
                | Method::Replay
                | Method::ResponseStatus
                | Method::RetireCommand
                | Method::SecretsList
                | Method::SecretsRemove
                | Method::SecretsSet
                | Method::ShareConfigure
                | Method::ShareName
                | Method::ShareStatus
                | Method::Sign
                | Method::Status
                | Method::SyncMacApply
                | Method::SyncMacHandoff
                | Method::SyncMacHashes
                | Method::SyncMacHead
                | Method::SyncMacRead
                | Method::SyncMacWorktree
                | Method::VaultConfigure
                | Method::VaultPush
                | Method::VaultStatus
                | Method::UploadBegin
                | Method::UploadChunk
                | Method::UploadCommit
                | Method::Verify
                | Method::Write
                | Method::CurrentTimeRead
                | Method::ItemToolCall
                | Method::ItemCommandExecutionRequestApproval
                | Method::ItemFileChangeRequestApproval
                | Method::ItemPermissionsRequestApproval
                | Method::McpServerElicitationRequest
                | Method::ItemToolRequestUserInput
                | Method::ItemStarted
                | Method::ItemCompleted
                | Method::ItemAgentMessageDelta
                | Method::ItemReasoningTextDelta
                | Method::ItemCommandExecutionOutputDelta
                | Method::CommandExecOutputDelta
                | Method::TurnStarted
                | Method::TurnCompleted
                | Method::TurnPlanUpdated
                | Method::TurnDiffUpdated
                | Method::ThreadClosed
                | Method::ThreadStatusChanged
                | Method::ThreadTokenUsageUpdated
                | Method::ThreadShellPolicyUpdated
                | Method::ThreadIdleCompactionUpdated
                | Method::AccountLoginCompleted
                | Method::AccountUpdated
                | Method::AccountRateLimitsUpdated
                | Method::ModelSafetyBufferingUpdated
                | Method::ProviderGenerationStarted
                | Method::ProtocolError => {}
            }
            true
        }
        SessionOutcome::GenerationClosed { .. } => false,
    };
    if retired {
        // A retained Submit owns the same prepared/request identity. Retire
        // it with that identity's guard, never in an independent commit.
        if context.method == Method::TurnStart
            && let Some(key) = effect_key(&account, &Effect::Submit(i.clone()))
        {
            delete_effect(tx, &key)?;
        }
        forget_request(tx, &context.operation)?;
    }
    if let Some(effect) = &effect {
        persist_effect(tx, &account, effect)?;
    }
    Ok(SettledOutcome {
        retired,
        effect,
        loaded,
    })
}

pub(super) async fn disconnect_vm_actor(
    store: &RuntimeStore,
    agent_id: &str,
    epoch: i64,
    account: &str,
    generation: u64,
) -> Result<bool, WorkerError> {
    let agent_id = agent_id.to_owned();
    let account = account.to_owned();
    let disconnected = scheduler::lifecycle_transaction_with(store, move |tx| {
        SchedulerRepository::disconnect_vm_actor(tx, &agent_id, epoch, &account, generation, now())
    })
    .await?;
    Ok(disconnected)
}
/// Disconnect one ended provider generation and tell each interrupted child's
/// parent. The notice id is the stop id a later connection recovery would use
/// for the same turn or start attempt, so each parent hears of the stop once.
///
/// A retained provider (a supervised process that outlives this worker) keeps
/// its turns: the next worker attaches to the same generation and continues
/// them, and a replaced generation disconnects them then
/// (`SchedulerRepository::reconcile_generation`). Only the VM actors end here:
/// their sessions belong to this worker.
pub(super) fn disconnect_provider_generation(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    generation: u64,
    provider_retained: bool,
) -> studio_runtime_store::StoreResult<()> {
    let at = now();
    let interrupted = if provider_retained {
        SchedulerRepository::disconnect_vm_actors(tx, account, generation, at)?
    } else {
        SchedulerRepository::disconnect_account(tx, account, generation, at, false)?
    };
    for agent in interrupted {
        crate::lifecycle_effects::notify_parent_of_disconnect(tx, &agent, at)?;
    }
    Ok(())
}
/// Whether the start outcome of `attempt` is already settled: its execution
/// attempt holds an immutable `startResult` receipt.
fn start_settled(
    tx: &studio_runtime_store::Transaction<'_>,
    attempt: &str,
) -> Result<bool, WorkerError> {
    Ok(studio_runtime_store::ExecutionHistoryRepository::start_settled(tx, attempt)?)
}
/// Settle a request proven not written to the provider. Returns the agent to
/// wake when its exact input batch went back to the queue.
///
/// The provider never received the input, so a start request (thread or turn)
/// returns its batch and the agent claims it again (for example after its
/// reply deadline elapsed while Studio prepared it). The stored reason names
/// the request and the cause. A batch that returned too often, or an attempt
/// that is no longer current, settles failed with that reason; its input stays
/// pending.
fn settle_not_sent(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    ticket: Option<&studio_runtime_providers::RequestTicket>,
    cause: Option<crate::provider_session_task::NotSentCause>,
    session_closed: bool,
) -> Result<Option<(String, i64)>, WorkerError> {
    let method = context.method;
    if method == Method::TurnInterrupt {
        uncertain(tx, context, ticket, true)?;
        return Ok(None);
    }
    let identity = context.intent.identity();
    // A thread request that died with its provider process never submitted the
    // input; the attempt retries on the next session instead of holding it.
    if method != Method::TurnStart
        && session_closed
        && SchedulerRepository::requeue_unsubmitted_attempt(tx, &identity)?
    {
        return Ok(None);
    }
    let reason = not_sent_reason(method, cause);
    if SchedulerRepository::return_unstarted_input(tx, &identity, &reason)? {
        return Ok(Some((identity.agent_id, identity.epoch)));
    }
    SchedulerRepository::settle_start_outcome(tx, &identity, "pending", Some(&reason))?;
    Ok(None)
}

/// The stored error of a start whose `method` request was not sent.
fn not_sent_reason(
    method: Method,
    cause: Option<crate::provider_session_task::NotSentCause>,
) -> String {
    format!(
        "Provider did not send {}: {}. The input is kept.",
        method.as_str(),
        cause.map_or("Studio did not write it", |cause| cause.describe())
    )
}

/// The error data flag of a missing saved session. The Claude bridge owns the
/// name, so it is read from the bridge source instead of being copied here.
fn session_missing_flag() -> &'static str {
    static FLAG: std::sync::LazyLock<&'static str> = std::sync::LazyLock::new(|| {
        include_str!("../../../../../providers/apps/claude-bridge/session-store.ts")
            .split_once("export const SESSION_MISSING = \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(flag, _)| flag)
            .expect("Claude bridge session missing flag")
    });
    *FLAG
}

/// Whether a provider error reply says that the thread's saved session is gone.
fn session_missing(error: &Value) -> bool {
    error
        .get("data")
        .and_then(|data| data.get(session_missing_flag()))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Preserve uncertainty on the source attempt until exact provider evidence arrives.
fn uncertain(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    ticket: Option<&studio_runtime_providers::RequestTicket>,
    not_sent: bool,
) -> Result<bool, WorkerError> {
    if context.method == Method::TurnInterrupt {
        let ticket = ticket.ok_or_else(|| invalid(&context.operation))?;
        SchedulerRepository::settle_interrupt(
            tx,
            &id_string(&ticket.id),
            ticket.generation,
            if not_sent { "unsent" } else { "unknown" },
        )?;
    } else {
        let outcome = if not_sent {
            TurnStart::NotSubmitted
        } else if context.method == Method::TurnStart {
            TurnStart::Unknown
        } else {
            TurnStart::Blocked {
                reason: if context.method == Method::ThreadStart {
                    PROVIDER_THREAD_START_UNKNOWN_ERROR
                } else {
                    PROVIDER_RESUME_UNKNOWN_ERROR
                }
                .into(),
            }
        };
        SchedulerRepository::settle_start_outcome(
            tx,
            &context.intent.identity(),
            match &outcome {
                TurnStart::NotSubmitted => "pending",
                TurnStart::Unknown => "uncertain",
                TurnStart::Blocked { .. } => "held",
            },
            match &outcome {
                TurnStart::Blocked { reason } => Some(reason.as_str()),
                TurnStart::NotSubmitted | TurnStart::Unknown => None,
            },
        )?;
        if matches!(outcome, TurnStart::Unknown) {
            stage_capacity_start(
                tx,
                &context.intent,
                CapacityStartOutcome::Failed {
                    reason: "The provider turn start outcome is unknown. Check the existing request before continuing.",
                    unknown: true,
                },
            )?;
        }
    }
    Ok(false)
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn journal_batch(
    store: Arc<RuntimeStore>,
    account: &str,
    journal_scope: &str,
    streams: &NotificationStreamBuffer,
    entries: Vec<JournalEntry>,
    claude_account_pin: &Value,
    expected_vm_actor: Option<(&str, i64)>,
    native_source: Option<crate::notification_dispatch::NativeProcessSource>,
    delivered: Vec<DeliveredPublication>,
    outcome: Option<Box<OutcomeStep>>,
    park_unbound: bool,
) -> Result<JournalBatch, WorkerError> {
    if entries.is_empty() && outcome.is_none() {
        return Ok(JournalBatch {
            streams: streams.clone(),
            ..JournalBatch::default()
        });
    }
    let outcome = match outcome {
        Some(mut step) => {
            step.resources = Arc::new(
                step.resources
                    .for_receipt(
                        &store,
                        &step.context.intent.agent_id,
                        step.context
                            .thread
                            .as_ref()
                            .map(|attempt| attempt.snapshot.actor.cwd.as_str()),
                    )
                    .await?,
            );
            Some(step)
        }
        None => None,
    };
    let needs_limits = entries.iter().any(|entry| {
        matches!(
            &entry.payload,
            JournalPayload::Frame(Frame::Notification(frame))
                if frame.method.as_str() == "turn/completed"
        )
    });
    let account_for_read = account.to_owned();
    let (publication, limits_observation) =
        crate::diagnostics::measure_runtime_stage_async("journal.snapshot", async {
            let mut publication = crate::notification_dispatch::PublicationContext::default()
                .with_native_source(native_source);
            if let Some((actor, epoch)) = expected_vm_actor {
                publication = publication.with_vm_journal(account, actor, epoch, journal_scope)?;
            } else if journal_scope != account {
                return Err(invalid(journal_scope));
            }
            let limits_observation = if needs_limits {
                Some(
                    store
                        .read_async("runtime-services/account_worker_journal_snapshot", move |tx| {
                            let limits = studio_runtime_store::account_state::limits_snapshots(tx)?
                                .snapshots
                                .into_iter()
                                .find(|(key, _)| key == &account_for_read)
                                .map(|(_, limits)| limits)
                                .unwrap_or_else(|| {
                                    json!({"accountKey":account_for_read,"at":null,"error":null,"data":null,"stale":true})
                                });
                            crate::usage_resume::LimitsObservation::from_record(limits, None)
                                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
                        })
                        .await?,
                )
            } else {
                None
            };
            Ok::<_, WorkerError>((publication, limits_observation))
        })
        .await?;
    let account = account.to_owned();
    let journal_scope = journal_scope.to_owned();
    let claude_account_pin = claude_account_pin.clone();
    let expected_vm_actor = expected_vm_actor.map(|(actor, epoch)| (actor.to_owned(), epoch));
    let streams = streams.clone();
    let state_root = store.root().to_path_buf();
    let server_id = store.local_server_id()?;
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        let server_id = server_id.as_deref();
        // A background recovery can commit before its cache handoff arrives.
        // Validate the cached snapshot under the writer, reconstructing only
        // after a newer commit or rollback invalidated its revision.
        let streams = current_streams(tx, &account, Some(&streams))?;
        let mut faults = Vec::new();
        // Before this batch's receipts are counted against their bound.
        delete_delivered_publications(tx, &delivered)?;
        // The outcome a reply carries settles before its own journal entry.
        let settled = match outcome {
            Some(step) => {
                let (settled, fault) = settle_outcome_isolated(tx, &state_root, server_id, *step)?;
                faults.extend(fault);
                Some(settled)
            }
            None => None,
        };
        let mut next = streams.clone();
        let mut committed = Vec::with_capacity(entries.len());
        let mut parked = Vec::new();
        let mut deferred = Vec::new();
        let mut deferred_agent = None;
        let mut refused = Vec::new();
        let mut outbox = PublicationOutbox::default();
        let mut entries = entries.into_iter();
        while let Some(entry) = entries.next() {
            let stage = journal_stage(&entry);
            let identity = JournalIdentity {
                account: &account,
                journal_scope: &journal_scope,
                expected_vm_actor: expected_vm_actor
                    .as_ref()
                    .map(|(actor, epoch)| (actor.as_str(), *epoch)),
            };
            let mut subject = None;
            let faults_before = faults.len();
            let applied = crate::diagnostics::measure_runtime_stage(stage, || {
                agent_fault::isolate(
                    tx,
                    server_id,
                    agent_fault::FaultStage::JournalFrame,
                    &mut subject,
                    |subject| {
                        journal_one(
                            tx,
                            identity,
                            &publication,
                            &next,
                            &entry,
                            &claude_account_pin,
                            limits_observation.as_ref(),
                            &mut outbox,
                            JournalPolicy {
                                server_id,
                                park_unbound,
                                subject,
                                faults: &mut faults,
                            },
                        )
                    },
                    // The worker settles the parked entry below.
                    || Ok(()),
                )
            });
            let applied = match applied {
                // Backpressure, not a data fault: the entry's savepoint rolled
                // back, so it and every later entry wait in journal order.
                Err(error) if stream_capacity_refused(&error) => {
                    faults.truncate(faults_before);
                    refused.push(entry);
                    refused.extend(entries);
                    break;
                }
                applied => applied?,
            };
            match applied {
                Ok(Some((updated, effect))) => {
                    next = updated;
                    committed.push((entry, effect));
                }
                Ok(None) => {
                    deferred_agent = subject;
                    deferred.push(entry);
                    deferred.extend(entries);
                    break;
                }
                Err(fault) => {
                    // The rolled-back entry settles without its effects, so
                    // it neither holds the journal nor applies again. The
                    // outbox only caches the receipt count and invalidation
                    // row; earlier entries' receipts are rows of this
                    // transaction and stay. Reload the cache from them.
                    outbox = PublicationOutbox::default();
                    park_entry(tx, identity, &entry)?;
                    faults.push(fault.with_method(journal_method(&entry)));
                    parked.push(entry);
                }
            }
        }
        // Entries that left every stream untouched share each allocation
        // with the batch's input, so the comparison reads no stream text.
        if next.changed_from(&streams) {
            crate::diagnostics::measure_runtime_stage("journal.persist_streams", || {
                persist_streams(tx, &account, &mut next)
            })?;
        }
        // One compaction per batch: every entry of a generation shares the checkpoint.
        let mut generations = committed
            .iter()
            .map(|(entry, _)| entry.generation.0)
            .collect::<Vec<_>>();
        generations.extend(parked.iter().map(|entry| entry.generation.0));
        generations.sort_unstable();
        generations.dedup();
        crate::diagnostics::measure_runtime_stage("journal.compact", || {
            generations
                .into_iter()
                .try_for_each(|generation| compact(tx, &journal_scope, generation))
        })?;
        Ok(JournalBatch {
            streams: next,
            committed,
            parked,
            deferred,
            deferred_agent,
            refused,
            settled,
            faults,
        })
    })
    .await
}

/// A monitor's durable output queue had no room for an output delta (see
/// `monitor_event_queue`). Its consumer frees room, so the entry is journaled
/// again later instead of ending the worker.
fn stream_capacity_refused(error: &WorkerError) -> bool {
    matches!(
        error,
        WorkerError::Notification(
            crate::notification_dispatch::NotificationDispatchError::StreamCapacity
        )
    )
}

/// Settle a journal entry whose application was rolled back for its agent.
fn park_entry(
    tx: &studio_runtime_store::Transaction<'_>,
    identity: JournalIdentity<'_>,
    entry: &JournalEntry,
) -> Result<(), WorkerError> {
    let operation = journal_operation(identity.journal_scope, entry.generation.0, entry.cursor.0);
    let digest = journal_digest(entry, &operation)?;
    save_journal(tx, identity, entry, digest, true)
}

/// The provider method of one journal entry, if it is a notification or request.
fn journal_method(entry: &JournalEntry) -> Option<&str> {
    match &entry.payload {
        JournalPayload::Frame(Frame::Notification(frame)) => Some(frame.method.as_str()),
        JournalPayload::Frame(Frame::Request(frame)) => Some(frame.method.as_str()),
        JournalPayload::Frame(Frame::UnknownMethod(frame)) => Some(frame.method.as_str()),
        JournalPayload::Frame(_)
        | JournalPayload::RawStdout { .. }
        | JournalPayload::Stderr(_)
        | JournalPayload::Exit(_)
        | JournalPayload::Other { .. } => None,
    }
}

/// Fixed stage label of one journal entry, by provider method class.
fn journal_stage(entry: &JournalEntry) -> &'static str {
    match &entry.payload {
        JournalPayload::Frame(Frame::Notification(frame)) => match frame.method.as_str() {
            "turn/started" => "journal.entry.turn_started",
            "turn/completed" => "journal.entry.turn_completed",
            method if method.ends_with("Delta") || method.ends_with("/delta") => {
                "journal.entry.delta"
            }
            _ => "journal.entry.notification",
        },
        JournalPayload::Frame(Frame::Request(_)) => "journal.entry.request",
        JournalPayload::Frame(_)
        | JournalPayload::RawStdout { .. }
        | JournalPayload::Stderr(_)
        | JournalPayload::Exit(_)
        | JournalPayload::Other { .. } => "journal.entry.other",
    }
}

/// Consume only manual release closure, including a completed worker with no
/// current turn attempt. Other notifications retain their attempt fence.
fn record_manual_release_closure(
    tx: &rusqlite::Transaction<'_>,
    identity: &JournalIdentity<'_>,
    publication: &crate::notification_dispatch::PublicationContext,
    entry: &JournalEntry,
    method: &str,
    params: &Value,
) -> Result<bool, WorkerError> {
    if method != "thread/closed"
        && !(method == "thread/status/changed" && params["status"]["type"] == "notLoaded")
    {
        return Ok(false);
    }
    let Some(thread) = params["threadId"]
        .as_str()
        .filter(|thread| !thread.is_empty())
    else {
        return Ok(false);
    };
    let mut statement = tx.prepare_cached(
        "SELECT record FROM runtime_agents WHERE thread_id=?1 AND account_key=?2 ORDER BY rowid LIMIT 2",
    )?;
    let rows = statement
        .query_map(params![thread, identity.account], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let [raw] = rows.as_slice() else {
        return Ok(false);
    };
    let mut agent: AgentRecord = serde_json::from_str(raw).map_err(|_| invalid(thread))?;
    let record = agent.value();
    let release = &record["nativeRelease"];
    if !release["id"]
        .as_str()
        .is_some_and(|id| id.starts_with(crate::agent_management_recovery::MANUAL_RELEASE_PREFIX))
    {
        return Ok(false);
    }
    // Never let the legacy turn route accept weaker closure evidence for a
    // manual release, even when this notification fails the exact scope check.
    let Some(source) = publication.native_source.as_ref() else {
        return Ok(true);
    };
    let scope = crate::account_session_runtime::ConnectionScope {
        account: identity.account.to_owned(),
        connection: source.connection.clone(),
        generation: entry.generation.0,
    };
    if identity.expected_vm_actor.is_some()
        || source.provider != "codex"
        || source.account != identity.account
        || source.connection.is_empty()
        || source.generation != Some(entry.generation.0)
        || agent
            .native_release
            .as_ref()
            .and_then(|release| release.scope.as_ref())
            != Some(&crate::agent_management_recovery::stored_scope(&scope))
        || release["connectionId"] != source.connection
        || release["accountKey"] != identity.account
        || release["threadId"] != thread
        || release["targetEpoch"] != record["epoch"]
        || release["targetRootId"] != record["rootId"]
        || release["targetParentId"] != record["parentId"]
        || release["resetPending"] != true
        || release["submittedAt"].as_f64().is_none()
        || !matches!(
            release["phase"].as_str(),
            Some("unsubscribing" | "released" | "unknown")
        )
    {
        return Ok(true);
    }
    let release = agent
        .native_release
        .as_mut()
        .ok_or_else(|| invalid(thread))?;
    release.extra.insert("closedAt".into(), json!(now()));
    release.extra.insert("resetPending".into(), json!(false));
    // Closure does not replace the exact unsubscribe receipt. In particular,
    // an unknown unsubscribe keeps its phase and cannot enable preparation.
    AgentRepository::put(tx, &agent)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn journal_one(
    tx: &rusqlite::Transaction<'_>,
    identity: JournalIdentity<'_>,
    publication: &crate::notification_dispatch::PublicationContext,
    streams: &NotificationStreamBuffer,
    entry: &JournalEntry,
    claude_account_pin: &Value,
    limits_observation: Option<&crate::usage_resume::LimitsObservation>,
    outbox: &mut PublicationOutbox,
    policy: JournalPolicy<'_>,
) -> Result<Option<(NotificationStreamBuffer, Option<Effect>)>, WorkerError> {
    let JournalIdentity {
        account,
        journal_scope,
        expected_vm_actor,
    } = identity;
    let operation = journal_operation(journal_scope, entry.generation.0, entry.cursor.0);
    let digest = journal_digest(entry, &operation)?;
    if let Some((actor_id, expected_epoch)) = expected_vm_actor
        && !crate::linux_vm_provider::vm_actor_matches(tx, actor_id, expected_epoch, account, None)?
    {
        return Err(WorkerError::StaleVmActor);
    }
    let stream_generation = current_stream_generation(tx, journal_scope, entry.generation.0)?;
    if entry.cursor.0 <= checkpoint(tx, journal_scope, stream_generation)? {
        return Ok(Some((streams.clone(), None)));
    }
    if let Some(old) = Outbox::get::<JournalSettlement>(tx, &operation).map_err(outbox_error)? {
        if !old.payload.digest.is_empty() && old.payload.digest != digest {
            return Err(invalid(&operation));
        }
        if old.payload.committed {
            return Ok(Some((streams.clone(), None)));
        }
    }
    let mut next = streams.clone();
    let mut effect = None;
    let mut publications = Vec::new();
    if let JournalPayload::Frame(frame) = &entry.payload {
        let notice = match frame {
            Frame::Notification(f) => {
                Some((f.method.as_str(), f.params.as_ref().unwrap_or(&Value::Null)))
            }
            Frame::UnknownMethod(f) if !f.fields.contains_key("id") => Some((
                f.method.as_str(),
                f.fields.get("params").unwrap_or(&Value::Null),
            )),
            Frame::Request(_) | Frame::UnknownMethod(_) | Frame::Response { .. } => None,
        };
        if let Some(("command/exec/outputDelta", params)) = notice
            && let Some(process_id) = params
                .get("processId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            && let Err(error) =
                <crate::monitor_event_queue::OutputDelta as serde::Deserialize>::deserialize(params)
                    .map_err(|_| crate::notification_dispatch::NotificationDispatchError::invalid())
                    .and_then(|delta| {
                        crate::monitor_event_queue::enqueue_output_with_scope_in_transaction(
                            tx,
                            process_id,
                            crate::monitor_event_queue::JournalEventScope {
                                account,
                                cursor_scope: journal_scope,
                                generation: entry.generation.0,
                                cursor: entry.cursor.0,
                                expected_agent: expected_vm_actor.map(|(actor, _)| actor),
                            },
                            &delta,
                        )
                    })
        {
            // A refused delta of a running monitor belongs to its agent.
            if matches!(
                error,
                crate::notification_dispatch::NotificationDispatchError::InvalidNotification { .. }
            ) {
                *policy.subject = crate::monitor_event_queue::output_agent(tx, process_id)?;
            }
            return Err(error.into());
        }
        if let Some(("account/rateLimits/updated", params)) = notice {
            if let Some(source) = publication.native_source.as_ref()
                && source.account == account
                && source.generation == Some(entry.generation.0)
            {
                let update = crate::account_session_runtime::LimitsUpdate {
                    scope: crate::account_session_runtime::ConnectionScope {
                        account: account.to_owned(),
                        connection: source.connection.clone(),
                        generation: entry.generation.0,
                    },
                    cursor: entry.cursor.0,
                    observed_at: now(),
                    data: params.clone(),
                };
                crate::account_session_runtime::LimitsSnapshot::read(
                    account,
                    json!({"accountKey":account,"at":update.observed_at,"data":update.data}),
                )
                .map_err(|_| invalid(&operation))?;
                effect = Some(Effect::LimitsUpdated(update));
            }
        } else if let Some((method, params)) = notice
            && record_manual_release_closure(
                tx,
                &JournalIdentity {
                    account,
                    journal_scope,
                    expected_vm_actor,
                },
                publication,
                entry,
                method,
                params,
            )?
        {
            // The manual release is independent of a live model turn.
        } else if let Some((method, params)) = notice
            && let Some((agent, epoch)) = steer::acknowledge(tx, account, method, params)?
        {
            // Delivered busy-turn input frees the agent for its next queued input.
            effect = Some(Effect::Wake(agent, epoch));
        } else if let Some((method, params)) = notice
            && let Some(thread) = params["threadId"].as_str()
        {
            let turn = params["turn"]["id"]
                .as_str()
                .or_else(|| params["turnId"].as_str());
            let (_, attempt) = SchedulerRepository::find_attempt_for_thread(
                tx,
                account,
                entry.generation.0,
                thread,
                turn,
            )?;
            let attempt = attempt.filter(|(identity, _)| {
                expected_vm_actor.is_none_or(|(actor_id, _)| identity.agent_id == actor_id)
            });
            let attempt = match attempt {
                Some(attempt) => Some(attempt),
                None if method == "turn/completed" => {
                    crate::context_repair::source_terminal_attempt(
                        tx,
                        account,
                        entry.generation.0,
                        params,
                    )
                    .map_err(|_| studio_runtime_store::StoreError::InvalidSyncObject {
                        scope: "runtime_context_terminal_receipts".into(),
                        id: thread.to_owned(),
                    })?
                    .map(|identity| (identity, true))
                }
                // A turn the provider started on its own binds the idle agent.
                None if method == "turn/started" => match turn {
                    Some(turn) => SchedulerRepository::adopt_provider_turn(
                        tx,
                        account,
                        entry.generation.0,
                        thread,
                        turn,
                        expected_vm_actor.map(|(actor_id, _)| actor_id),
                    )?
                    .map(|identity| (identity, true)),
                    None => None,
                },
                None => None,
            };
            if let Some((identity, bound)) = attempt {
                *policy.subject = Some(identity.agent_id.clone());
                match method {
                    "turn/completed" if !bound && policy.park_unbound => {
                        // No outstanding request can bind this completion any
                        // more. Settle it and hold the agent, whose turn would
                        // otherwise stay open forever.
                        let detail = format!(
                            "the completion of turn {} on thread {thread} has no bound attempt {}",
                            turn.unwrap_or("(none)"),
                            identity.attempt_id
                        );
                        let hold = agent_fault::hold_agent(
                            tx,
                            policy.server_id,
                            &identity.agent_id,
                            agent_fault::FaultStage::JournalFrame,
                            &detail,
                        )?;
                        policy.faults.push(agent_fault::AgentFault {
                            agent_id: identity.agent_id.clone(),
                            stage: agent_fault::FaultStage::JournalFrame,
                            method: Some(method.to_owned()),
                            detail,
                            hold,
                        });
                    }
                    "turn/started" => {
                        if let Some(turn) = turn
                            && let Some(interrupt) =
                                SchedulerRepository::observe_started(tx, &identity, turn)?
                        {
                            crate::shell_policy::note_activity(tx, &identity.agent_id, now())?;
                            let agent = NotificationDispatchRepository::agent_record(
                                tx,
                                &identity.agent_id,
                            )?
                            .ok_or_else(|| invalid(&identity.agent_id))?;
                            let frame = json!({"method":method,"params":params});
                            let delivery = ProviderDeliveryIdentity {
                                provider_handle: ProviderHandle::for_account(journal_scope)
                                    .as_str()
                                    .into(),
                                generation: entry.generation.0,
                                cursor: entry.cursor.0,
                            };
                            next.queue_effects(
                                crate::notification_runtime_effects::RuntimeNotificationEffects
                                    .post_commit(
                                        Some(&agent),
                                        Some(&agent),
                                        &frame,
                                        &delivery,
                                        now(),
                                        &crate::notification_dispatch::DispatchOutcome::Written,
                                    ),
                            );
                            publications.push(next.take_post_commit()?);
                            if interrupt {
                                effect = Some(Effect::Interrupt {
                                    agent: identity.agent_id,
                                    thread: thread.into(),
                                    turn: turn.into(),
                                });
                            }
                        }
                    }
                    "turn/completed" => {
                        if !bound {
                            // The deferred entry holds the checkpoint below
                            // itself until it is journaled again.
                            save_journal(
                                tx,
                                JournalIdentity {
                                    account,
                                    journal_scope,
                                    expected_vm_actor,
                                },
                                entry,
                                digest,
                                false,
                            )?;
                            return Ok(None);
                        }
                        next = flush_notification_streams(
                            tx,
                            &identity.agent_id,
                            publication,
                            streams,
                            now(),
                        )?
                        .unwrap_or(next);
                        publications.push(next.take_post_commit()?);
                        let agent_before =
                            NotificationDispatchRepository::agent_record(tx, &identity.agent_id)?
                                .ok_or_else(|| invalid(&identity.agent_id))?;
                        if scheduler::settle_terminal_in_transaction(tx, &identity, params)? {
                            if agent_before["lastCompletedTurn"] != params["turn"]["id"] {
                                crate::shell_policy::note_activity(tx, &identity.agent_id, now())?;
                            }
                            invalidate_auth_on_failure(
                                &agent_before,
                                claude_account_pin,
                                &params["turn"],
                            );
                            crate::continuation_runtime::stage(
                                tx,
                                crate::continuation_runtime::Completion::TerminalTurn {
                                    agent_id: &identity.agent_id,
                                    turn: &params["turn"],
                                    known: true,
                                    now: now(),
                                    limits: limits_observation,
                                },
                            )
                            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                            let agent_after = NotificationDispatchRepository::agent_record(
                                tx,
                                &identity.agent_id,
                            )?
                            .ok_or_else(|| invalid(&identity.agent_id))?;
                            let frame = json!({"method":method,"params":params});
                            let delivery = ProviderDeliveryIdentity {
                                provider_handle: ProviderHandle::for_account(journal_scope)
                                    .as_str()
                                    .into(),
                                generation: entry.generation.0,
                                cursor: entry.cursor.0,
                            };
                            next.queue_effects(
                                crate::notification_runtime_effects::RuntimeNotificationEffects
                                    .post_commit(
                                        Some(&agent_before),
                                        Some(&agent_after),
                                        &frame,
                                        &delivery,
                                        now(),
                                        &crate::notification_dispatch::DispatchOutcome::Written,
                                    ),
                            );
                            publications.push(next.take_post_commit()?);
                            crate::notification_dispatch::prune_inactive_streams(
                                tx,
                                &mut next,
                                now(),
                                false,
                            )?;
                            if let Some(turn) = turn {
                                next.settle_turn(&identity.agent_id, account, thread, turn);
                            }
                            effect = Some(
                                if params["turn"]["status"] == "failed"
                                    && matches!(
                                        crate::continuation_runtime::classify_usage_failure(
                                            &params["turn"]["error"],
                                            None
                                        ),
                                        Some(
                                            crate::usage_resume::Cause::UsageLimit
                                                | crate::usage_resume::Cause::RateLimit
                                        )
                                    )
                                {
                                    Effect::LimitFailure {
                                        agent: identity.agent_id,
                                        epoch: identity.epoch,
                                        turn: params["turn"]["id"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .into(),
                                    }
                                } else {
                                    Effect::Wake(identity.agent_id, identity.epoch)
                                },
                            );
                        }
                    }
                    _ => {
                        let dispatched = dispatch_notification_with_effects(
                            tx,
                            &identity.agent_id,
                            publication,
                            streams,
                            &ProviderDeliveryIdentity {
                                provider_handle: ProviderHandle::for_account(account)
                                    .as_str()
                                    .into(),
                                generation: entry.generation.0,
                                cursor: entry.cursor.0,
                            },
                            frame,
                            now(),
                            &crate::notification_runtime_effects::RuntimeNotificationEffects,
                        )?;
                        publications.push(dispatched.post_commit);
                        let staged = dispatched.stream_update.unwrap_or(next);
                        next = if staged.pending_bytes() >= STREAM_FLUSH_BYTES {
                            let flushed = flush_notification_streams(
                                tx,
                                &identity.agent_id,
                                publication,
                                &staged,
                                now(),
                            )?;
                            if let Some(mut flushed) = flushed {
                                publications.push(flushed.take_post_commit()?);
                                flushed
                            } else {
                                staged
                            }
                        } else {
                            staged
                        };
                    }
                }
            }
        }
    }
    crate::diagnostics::measure_runtime_stage("journal.publications", || {
        publications
            .iter()
            .try_for_each(|batch| persist_publication_batch_in(tx, account, batch, outbox))
    })?;
    if let Some(effect) = &effect {
        persist_effect(tx, account, effect)?;
    }
    // A provider request is accepted with its batch: its record owns the
    // answer, so the cursor settles without waiting for it.
    super::callback_record::accept(tx, account, journal_scope, expected_vm_actor, entry)?;
    save_journal(tx, identity, entry, digest, true)?;
    Ok(Some((next, effect)))
}
pub(super) async fn save_recovery(
    store: &RuntimeStore,
    account: &str,
    recovery: &studio_runtime_process::provider_seam::InitializationRecovery,
) -> Result<(), WorkerError> {
    let account = account.to_owned();
    let generation = recovery.generation.map(|g| g.0 as i64);
    store.transaction_with(move|tx| {
        studio_runtime_store::account_state::ensure_account(tx,&account)?;
        tx.execute_cached("UPDATE runtime_account_state SET recovery_required=1,connection_generation=COALESCE(connection_generation,?2) WHERE account=?1",params![account,generation])?;
        Ok(())
    }).await
}
pub(super) async fn stop(
    store: &RuntimeStore,
    account: &str,
    agent: &str,
) -> Result<Option<Effect>, WorkerError> {
    let account = account.to_owned();
    let agent = agent.to_owned();
    scheduler::lifecycle_transaction_with(store, move |tx| {
        let Some(record) = AgentRepository::get(tx, &AgentId::from(agent.as_str()))? else {
            return Ok(None);
        };
        if record.account_key.as_str() != account.as_str() {
            return Ok(None);
        }
        SchedulerRepository::stop_agent(tx, &agent, "Stopped by user")?;
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(agent.as_str()))? else {
            return Ok(None);
        };
        let attempt = &agent.extra.get("startAttempt").unwrap_or(&Value::Null);
        let turn = attempt["turnId"]
            .as_str()
            .or_else(|| attempt["observedTurnId"].as_str());
        let effect = turn
            .zip(agent.thread_id.as_deref())
            .map(|(turn, thread)| Effect::Interrupt {
                agent: agent.id.0.clone(),
                thread: thread.into(),
                turn: turn.into(),
            });
        if let Some(effect) = &effect {
            persist_effect(tx, &account, effect)?;
        }
        Ok(effect)
    })
    .await
}
pub(super) fn interrupt_intent(
    connection: &rusqlite::Connection,
    agent: &str,
    thread: &str,
) -> Result<Option<(TurnIntent, i64)>, WorkerError> {
    let Some(a) = AgentRepository::get(connection, &AgentId::from(agent))? else {
        return Ok(None);
    };
    let provider = studio_runtime_providers::ProviderKind::of_field(a.extra.get("provider"))
        .map_err(|_| WorkerError::PreparationRefused("agent/provider".into()))?;
    let attempt = &a.extra.get("startAttempt").unwrap_or(&Value::Null);
    let interrupt = &attempt["interrupt"];
    if interrupt["threadId"] == thread
        && matches!(interrupt["outcome"].as_str(), Some("unknown" | "accepted"))
    {
        return Ok(None);
    }
    Ok(attempt["id"]
        .as_str()
        .zip(attempt["epoch"].as_i64())
        .map(|(id, epoch)| {
            (
                TurnIntent {
                    agent_id: agent.into(),
                    epoch,
                    attempt_id: id.into(),
                    account_key: a.account_key.as_str().into(),
                    provider,
                    thread_id: Some(thread.into()),
                    cwd: None,
                    model: None,
                    events: vec![],
                    radio_authorized: false,
                },
                a.epoch,
            )
        }))
}
pub(super) fn id_string(id: &RequestId) -> String {
    match id {
        RequestId::String(id) => id.clone(),
        RequestId::Number(id) => id.to_string(),
    }
}

pub(super) async fn was_submitted(
    store: &RuntimeStore,
    operation: &str,
) -> Result<bool, WorkerError> {
    let operation = operation.to_owned();
    store
        .read_async("runtime-services/account_worker_was_submitted", move |tx| {
            Ok(account_submission(tx, &operation)?.is_some())
        })
        .await
}

/// A persisted login bootstrap is a host-authenticated external effect. A
/// submitted row with no completion is uncertain and must never be resent.
// The follow-on effect is part of the transition that owes it. Repeating an
// effect after a crash is safe: submission checks its durable operation identity.
pub(super) fn effect_key(account: &str, effect: &Effect) -> Option<String> {
    if matches!(effect, Effect::Callback(..)) {
        return None;
    }
    let raw = serde_json::to_vec(effect).ok()?;
    Some(format!(
        "effect:{account}:{}",
        studio_runtime_store::sha256_hex(&raw)
    ))
}
pub(super) fn persist_effect(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    effect: &Effect,
) -> Result<(), WorkerError> {
    if let Some(key) = effect_key(account, effect) {
        if Outbox::get::<Effect>(tx, &key)
            .map_err(outbox_error)?
            .is_some()
        {
            return Ok(());
        }
        let order: i64 = tx.query_row_cached(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM runtime_outbox WHERE account=?1 AND kind=?2 AND status=?3",
            params![account, OutboxKind::FollowupEffect, OutboxStatus::Pending], |row| row.get(0),
        )?;
        Outbox::put(
            tx,
            &OutboxRow {
                signature: studio_runtime_store::sha256_hex(
                    &serde_json::to_vec(effect).map_err(StoreError::from)?,
                ),
                id: key,
                kind: OutboxKind::FollowupEffect,
                owner_kind: OwnerKind::Account,
                owner_id: account.to_owned(),
                request_id: None,
                status: OutboxStatus::Pending,
                account: Some(account.to_owned()),
                stream_id: None,
                generation: None,
                ordinal: Some(order),
                next_at: Some(0),
                payload: effect,
                created: (now() * 1000.0) as i64,
            },
        )
        .map_err(outbox_error)?;
    }
    Ok(())
}
fn delete_effect(tx: &studio_runtime_store::Transaction<'_>, key: &str) -> Result<(), WorkerError> {
    if let Some(mut row) = Outbox::get::<Effect>(tx, key).map_err(outbox_error)? {
        row.status = OutboxStatus::Committed;
        row.next_at = None;
        Outbox::put(tx, &row).map_err(outbox_error)?;
        Outbox::delete_settled(tx, key, &row.signature).map_err(outbox_error)?;
    }
    Ok(())
}

fn outbox_error(error: studio_runtime_store::outbox::OutboxError) -> WorkerError {
    WorkerError::Store(error.into())
}

#[cfg(test)]
mod effect_obligation_tests {
    use super::*;

    #[test]
    fn followup_obligations_recover_in_order_without_receipt_history() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&connection).expect("target schema");
        let tx = connection.transaction().expect("transaction");
        let first = Effect::Wake("first".into(), 1);
        let second = Effect::Wake("second".into(), 2);
        persist_effect(&tx, "account", &first).expect("first obligation");
        persist_effect(&tx, "account", &second).expect("second obligation");
        persist_effect(&tx, "account", &first).expect("same obligation");
        let recovered = recover_effects(&tx, "account").expect("recover");
        assert_eq!(
            serde_json::to_value(recovered).expect("effects"),
            json!([first, second])
        );
        delete_effect(&tx, &effect_key("account", &first).expect("effect key"))
            .expect("settlement");
        assert_eq!(recover_effects(&tx, "account").expect("remaining").len(), 1);
        tx.commit().expect("commit");
    }

    #[test]
    fn committed_journal_gap_does_not_advance_ack_or_lose_later_evidence() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&connection).expect("target schema");
        connection.execute("INSERT INTO runtime_account_state VALUES('account',3,3,'connection',0,0,'null',NULL,NULL)", []).expect("account");
        let tx = connection.transaction().expect("transaction");
        for cursor in [2, 1] {
            Outbox::put(&tx, &OutboxRow {
                id: journal_operation("account", 3, cursor),
                kind: OutboxKind::JournalEntry,
                owner_kind: OwnerKind::Account,
                owner_id: "account".into(),
                request_id: None,
                signature: cursor.to_string(),
                status: OutboxStatus::Committed,
                account: Some("account".into()),
                stream_id: Some("account".into()),
                generation: Some(3),
                ordinal: Some(cursor as i64),
                next_at: None,
                payload: json!({"cursor":cursor,"digest":cursor.to_string(),"ready":true,"committed":true}),
                created: 1,
            }).expect("journal gap");
            compact(&tx, "account", 3).expect("compact ready prefix");
            assert_eq!(
                checkpoint(&tx, "account", 3).expect("ack"),
                if cursor == 2 { 0 } else { 2 }
            );
        }
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |row| row
                .get::<_, i64>(0))
                .expect("remaining gaps"),
            0
        );
    }

    #[test]
    fn mixed_generation_journal_compacts_only_the_contiguous_ready_prefix() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("schema");
        db.execute("INSERT INTO runtime_account_state VALUES('account',7,6,'connection',0,0,'null',NULL,NULL)", []).expect("account");
        let tx = db.transaction().expect("transaction");
        for (generation, cursor, ready) in [(6, 1, true), (5, 2, false), (6, 3, true)] {
            let row = OutboxRow {
                id: journal_operation("account", generation, cursor),
                kind: OutboxKind::JournalEntry,
                owner_kind: OwnerKind::Account,
                owner_id: "account".into(),
                request_id: None,
                signature: cursor.to_string(),
                status: if ready {
                    OutboxStatus::Committed
                } else {
                    OutboxStatus::Pending
                },
                account: Some("account".into()),
                stream_id: Some("account".into()),
                generation: Some(generation as i64),
                ordinal: Some(cursor as i64),
                next_at: (!ready).then_some(0),
                payload: JournalSettlement {
                    cursor,
                    digest: cursor.to_string(),
                    ready,
                    committed: ready,
                    notification: None,
                },
                created: 1,
            };
            Outbox::put(&tx, &row).expect("journal");
        }
        compact(&tx, "account", 5).expect("compact");
        compact(&tx, "account", 6).expect("compact");
        assert_eq!(
            checkpoint(&tx, "account", 6).expect("ack"),
            1,
            "unready old entry holds current cursor"
        );
        let id = journal_operation("account", 5, 2);
        let mut row: OutboxRow<JournalSettlement> =
            Outbox::get(&tx, &id).expect("read").expect("gap");
        row.payload.ready = true;
        row.payload.committed = true;
        row.status = OutboxStatus::Committed;
        row.next_at = None;
        Outbox::put(&tx, &row).expect("settle gap");
        compact(&tx, "account", 5).expect("compact");
        assert_eq!(
            checkpoint(&tx, "account", 6).expect("ack"),
            3,
            "ready later generation drains behind gap"
        );
    }
}
pub(super) async fn complete_effect(store: &RuntimeStore, key: &str) -> Result<(), WorkerError> {
    let key = key.to_owned();
    store
        .transaction_with(move |tx| delete_effect(tx, &key))
        .await
}
pub(super) fn recover_effects(
    connection: &rusqlite::Connection,
    account: &str,
) -> Result<VecDeque<Effect>, WorkerError> {
    Ok(Outbox::for_account::<Effect>(
        connection,
        account,
        OutboxKind::FollowupEffect,
        OutboxStatus::Pending,
        u32::MAX,
    )
    .map_err(outbox_error)?
    .into_iter()
    .map(|row| row.payload)
    .collect())
}

fn forget_request(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
) -> Result<(), WorkerError> {
    if let Some(mut row) = Outbox::get::<ProviderCall>(tx, operation).map_err(outbox_error)? {
        row.status = OutboxStatus::Committed;
        row.next_at = None;
        Outbox::put(tx, &row).map_err(outbox_error)?;
        Outbox::delete_settled(tx, operation, &row.signature).map_err(outbox_error)?;
    }
    Ok(())
}
pub(crate) fn checkpoint(
    tx: &rusqlite::Connection,
    journal_scope: &str,
    generation: u64,
) -> Result<u64, StoreError> {
    if let Some((account, actor)) = journal_scope.rsplit_once(":vm:")
        && let Some(agent) = AgentRepository::get(tx, &AgentId::from(actor))?
        && account_key(&agent) == account
        && agent.execution_mode == Some(studio_runtime_store::AgentExecutionMode::Vm)
    {
        let state = agent.extra.get("providerStream").unwrap_or(&Value::Null);
        return Ok(if state["generation"].as_u64() == Some(generation) {
            state["ack"].as_u64().unwrap_or(0)
        } else {
            0
        });
    }
    studio_runtime_store::account_state::provider_checkpoint(tx, journal_scope, generation)
}

fn current_stream_generation(
    tx: &rusqlite::Connection,
    scope: &str,
    origin: u64,
) -> Result<u64, WorkerError> {
    let generation = if let Some((account, actor)) = scope.rsplit_once(":vm:") {
        let agent = AgentRepository::get(tx, &AgentId::from(actor))?
            .filter(|agent| account_key(agent) == account)
            .ok_or_else(|| invalid(scope))?;
        let (state, _) = studio_runtime_store::account_state::provider_state(
            tx,
            studio_runtime_store::account_state::ProviderOwner::Agent(&agent),
        )?
        .ok_or_else(|| invalid(scope))?;
        Some(state.generation())
    } else {
        tx.query_row(
            "SELECT connection_generation FROM runtime_account_state WHERE account=?1",
            [scope],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .map(u64::try_from)
        .transpose()
        .map_err(|_| invalid(scope))?
    }
    .unwrap_or(origin);
    if generation < origin {
        return Err(invalid(scope));
    }
    Ok(generation)
}

fn set_checkpoint(
    tx: &studio_runtime_store::Transaction<'_>,
    scope: &str,
    generation: u64,
    cursor: u64,
) -> Result<(), WorkerError> {
    if let Some((account, actor)) = scope.rsplit_once(":vm:")
        && let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(actor))?
        && account_key(&agent) == account
        && agent.execution_mode == Some(studio_runtime_store::AgentExecutionMode::Vm)
    {
        let state = agent
            .extra
            .entry("providerStream".into())
            .or_insert_with(|| json!({"generation":generation,"ack":0,"streams":null}));
        if state.get("generation").is_none() {
            state["generation"] = json!(generation);
        }
        if state["generation"].as_u64() != Some(generation) {
            return Err(invalid(scope));
        }
        state["ack"] = json!(state["ack"].as_u64().unwrap_or(0).max(cursor));
        AgentRepository::put(tx, &agent)?;
        return Ok(());
    }
    Ok(
        studio_runtime_store::account_state::set_provider_checkpoint(
            tx, scope, generation, cursor,
        )?,
    )
}

/// A proved attachment replaces only current stream provenance, never uncertain calls.
pub(super) async fn attach_provider_stream(
    store: &RuntimeStore,
    account: &str,
    actor: Option<&str>,
    generation: u64,
    connection: &str,
) -> Result<(), WorkerError> {
    let account = account.to_owned();
    let actor = actor.map(str::to_owned);
    let connection = connection.to_owned();
    store.transaction_with(move|tx| {
        if let Some(actor)=actor {
            let agent = stored_agent(tx, &actor)?;
            if account_key(&agent) != account || agent.execution_mode != Some(studio_runtime_store::AgentExecutionMode::Vm) { return Err(invalid(&actor)); }
            let current = studio_runtime_store::account_state::provider_state(tx, studio_runtime_store::account_state::ProviderOwner::Agent(&agent))?;
            let previous = current.as_ref().map(|(state, _)| state.generation());
            if previous.is_some_and(|old| old > generation) { return Err(invalid(&actor)); }
            if let Some(previous) = previous && previous != generation {
                let ack = current.as_ref().map_or(0, |(_, ack)| *ack);
                super::retained::storage::checkpoint_covered(tx, &super::vm_journal_scope(&account, &actor), previous, ack)?;
            }
            // Settlement above can update this same source owner.
            let mut agent = stored_agent(tx, &actor)?;
            let state = agent.extra.entry("providerStream".into()).or_insert_with(|| json!({}));
            if previous != Some(generation) || state.get("ack").is_none() { state["ack"] = json!(0); }
            state["generation"] = json!(generation);
            state["connection"] = json!(connection);
            AgentRepository::put(tx, &agent)?;
        } else {
            studio_runtime_store::account_state::ensure_account(tx,&account)?;
            let current=studio_runtime_store::account_state::provider_state(tx,studio_runtime_store::account_state::ProviderOwner::Account(&account))?;
            let previous=current.as_ref().map(|(state,_)|state.generation());
            let ack=current.as_ref().map_or(0,|(_,ack)|*ack);
            let generation=i64::try_from(generation).map_err(|_|invalid(&account))?;
            let next_generation=generation.checked_add(1).ok_or_else(||invalid(&account))?;
            if previous.is_some_and(|old|old>generation as u64){return Err(invalid(&account));}
            if let Some(previous)=previous && previous!=generation as u64 {super::retained::storage::checkpoint_covered(tx,&account,previous,ack)?;}
            tx.execute_cached("UPDATE runtime_account_state SET connection_generation=?2,connection_id=?3,journal_ack=CASE WHEN connection_generation=?2 THEN journal_ack ELSE 0 END,recovery_required=0,next_generation=MAX(next_generation,?4) WHERE account=?1",params![account,generation,connection,next_generation])?;
        }
        Ok(())
    }).await
}

#[cfg(any(test, feature = "test-support"))]
pub async fn attach_provider_stream_for_test(
    store: &RuntimeStore,
    account: &str,
    actor: Option<&str>,
    generation: u64,
    connection: &str,
) -> Result<(), WorkerError> {
    attach_provider_stream(store, account, actor, generation, connection).await
}
pub(super) fn journal_committed(
    tx: &studio_runtime_store::Transaction<'_>,
    journal_scope: &str,
    generation: u64,
    cursor: u64,
) -> Result<bool, WorkerError> {
    if cursor <= checkpoint(tx, journal_scope, generation)? {
        return Ok(true);
    }
    Ok(
        Outbox::get::<JournalSettlement>(tx, &journal_operation(journal_scope, generation, cursor))
            .map_err(outbox_error)?
            .is_some_and(|row| row.payload.committed),
    )
}
/// Advance the checkpoint over the ready prefix of settled receipts and delete
/// what it covers. Keys are in cursor order, so the work is bounded by
/// [`JOURNAL_COMPACTION_LIMIT`], not by receipts held behind an unready one.
fn compact(
    tx: &studio_runtime_store::Transaction<'_>,
    journal_scope: &str,
    generation: u64,
) -> Result<(), WorkerError> {
    let mut stmt = tx.prepare_cached("SELECT id,payload,generation FROM runtime_outbox WHERE stream_id=?1 AND kind=?2 ORDER BY ordinal,id LIMIT ?3")?;
    let rows = stmt
        .query_map(
            params![
                journal_scope,
                OutboxKind::JournalEntry,
                JOURNAL_COMPACTION_LIMIT as i64
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let stream_generation = current_stream_generation(tx, journal_scope, generation)?;
    let before = checkpoint(tx, journal_scope, stream_generation)?;
    let mut cursor = before;
    let mut covered = Vec::new();
    let mut generations = std::collections::BTreeMap::new();
    for (id, raw, origin) in rows {
        let row: JournalSettlement =
            serde_json::from_str(&raw).map_err(|_| invalid(journal_scope))?;
        let origin = u64::try_from(origin).map_err(|_| invalid(journal_scope))?;
        if origin > stream_generation || !row.ready || !row.committed {
            break;
        }
        if row.cursor <= before {
            covered.push(id);
            generations.insert(origin, row.cursor);
            continue;
        }
        if cursor.checked_add(1) != Some(row.cursor) {
            break;
        }
        cursor = row.cursor;
        covered.push(id);
        generations.insert(origin, row.cursor);
    }
    for (origin, cursor) in generations {
        super::retained::storage::checkpoint_covered(tx, journal_scope, origin, cursor)?;
    }
    if cursor > before {
        set_checkpoint(tx, journal_scope, stream_generation, cursor)?;
    }
    for id in covered {
        tx.execute_cached(
            "DELETE FROM runtime_outbox WHERE id=?1 AND kind=?2 AND status=?3",
            params![id, OutboxKind::JournalEntry, OutboxStatus::Committed],
        )?;
    }
    Ok(())
}

// Python codex_streaming.FLUSH_SECONDS is .75. The additional byte threshold
// bounds a durable batch when producers outpace that timer; answers are uncut.
pub(super) const STREAM_FLUSH_BYTES: usize = 64 * 1024;
/// One committed journal transaction.
#[derive(Default)]
pub(super) struct JournalBatch {
    pub streams: NotificationStreamBuffer,
    /// Applied entries in journal order, with the effect each one owes.
    pub committed: Vec<(JournalEntry, Option<Effect>)>,
    /// Entries settled without their effects: their agent's data failed.
    pub parked: Vec<JournalEntry>,
    /// Entries that wait for a reply to bind them, in journal order.
    pub deferred: Vec<JournalEntry>,
    /// The agent of the first deferred entry, an unbound turn completion.
    pub deferred_agent: Option<String>,
    /// The entry that a full monitor output queue refused, and every entry
    /// after it, in journal order. None of them is applied or settled; the
    /// worker journals them again after the queue drains.
    pub refused: Vec<JournalEntry>,
    pub settled: Option<SettledOutcome>,
    /// Agent-scoped failures this transaction rolled back.
    pub faults: Vec<agent_fault::AgentFault>,
}
/// How one journal entry treats data it cannot apply.
pub(super) struct JournalPolicy<'a> {
    server_id: Option<&'a str>,
    /// Settle an unbound turn completion instead of deferring it.
    park_unbound: bool,
    /// The agent the entry applies to, once known.
    subject: &'a mut Option<String>,
    faults: &'a mut Vec<agent_fault::AgentFault>,
}
/// Digest of one journal entry's payload; a replay must match it.
pub(super) fn journal_digest(entry: &JournalEntry, operation: &str) -> Result<String, WorkerError> {
    Ok(match &entry.payload {
        JournalPayload::Frame(frame) => {
            studio_runtime_store::sha256_hex(&journal_frame_payload(frame, operation)?)
        }
        JournalPayload::RawStdout { bytes, .. } => {
            studio_runtime_store::sha256_hex(json!({"raw":bytes}).to_string().as_bytes())
        }
        JournalPayload::Stderr(value) => {
            studio_runtime_store::sha256_hex(json!({"stderr":value}).to_string().as_bytes())
        }
        JournalPayload::Exit(value) => {
            studio_runtime_store::sha256_hex(json!({"exit":value}).to_string().as_bytes())
        }
        JournalPayload::Other { kind, payload } => studio_runtime_store::sha256_hex(
            json!({"kind":kind,"payload":payload})
                .to_string()
                .as_bytes(),
        ),
    })
}
#[derive(Clone, Copy)]
struct JournalIdentity<'a> {
    account: &'a str,
    journal_scope: &'a str,
    expected_vm_actor: Option<(&'a str, i64)>,
}
/// One account's pending publication receipts as seen inside one writer
/// transaction. Loaded on first use and kept current by its writes, so each
/// published effect costs key lookups instead of an outbox scan.
#[derive(Default)]
pub(crate) struct PublicationOutbox {
    pending: Option<usize>,
    invalidation: Option<Option<String>>,
}

/// Persist one publication batch; its analytics effects are logged for the
/// analytics projector in the same Canvas transaction.
pub(crate) fn persist_publication_batch(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    batch: &NotificationPublicationBatch,
) -> Result<(), WorkerError> {
    persist_publication_batch_in(tx, account, batch, &mut PublicationOutbox::default())
}

fn persist_publication_batch_in(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    batch: &NotificationPublicationBatch,
    outbox: &mut PublicationOutbox,
) -> Result<(), WorkerError> {
    if batch.is_empty() {
        return Ok(());
    }
    let captured = batch
        .effects
        .iter()
        .cloned()
        .map(|effect| crate::analytics_projection::log_effect(tx, effect))
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, _>>()?;
    let batch = &NotificationPublicationBatch {
        sequence: batch.sequence,
        effects: captured,
        analytics_overflow: batch.analytics_overflow,
    };
    if batch.is_empty() {
        return Ok(());
    }
    let prefix = account.to_owned();
    let pending = match outbox.pending {
        Some(pending) => pending,
        None => {
            let pending: i64 = tx.query_row_cached(
                "SELECT count(*) FROM runtime_outbox WHERE account=?1 AND kind=?2 AND status=?3",
                params![account, OutboxKind::Publication, OutboxStatus::Pending],
                |row| row.get(0),
            )?;
            usize::try_from(pending).map_err(|_| WorkerError::Capacity)?
        }
    };
    let available = MAX_PUBLICATION_RECEIPTS.saturating_sub(pending);
    let mut retained = 0;
    let overflow = batch.analytics_overflow;
    let mut publication_overflow = 0_u64;
    let mut last_saved = None;
    for (index, effect) in batch.effects.iter().enumerate() {
        if retained >= available
            && !matches!(
                effect,
                crate::notification_dispatch::NotificationPostCommit::AnalyticsCaptured(_)
            )
        {
            // The pending token-rate queue is full. Coalesce this observation
            // into the newest token-rate receipt rather than dropping the
            // committed token-rate update. When that receipt is full too, count
            // the volatile observation as publication overflow: Python never
            // stops an account for its observers. A complete usage observation
            // persists beyond the bound; its analytics rows are in the Canvas
            // projection log either way.
            if !coalesce_token_rate_receipt(tx, &prefix, effect)? {
                publication_overflow = publication_overflow.saturating_add(1);
            }
            continue;
        }
        last_saved = Some(save_publication_effect(tx, account, batch, index, effect)?);
        retained += 1;
    }
    if overflow > 0 || publication_overflow > 0 {
        if last_saved.is_none() && available > 0 {
            let operation = publication_operation(account, batch.sequence, batch.effects.len());
            save_publication(
                tx,
                &operation,
                Publication {
                    account: account.to_owned(),
                    sequence: batch.sequence,
                    index: batch.effects.len(),
                    analytics_overflow: Some(overflow),
                    publication_overflow: Some(publication_overflow),
                    ..Default::default()
                },
            )?;
            last_saved = Some(operation);
            retained += 1;
        }
        let target = if let Some(operation) = last_saved {
            Some(operation)
        } else {
            pending_publication(tx, &prefix, None, true)?
        };
        if let Some(id) = target {
            let pending_invalidation = match outbox.invalidation.take() {
                Some(known) => known,
                None => pending_publication(tx, &prefix, Some(PUBLICATION_INVALIDATION), false)?,
            };
            tx.execute_cached(
                "UPDATE runtime_outbox SET payload=json_set(json_set(payload,'$.analyticsOverflow',COALESCE(json_extract(payload,'$.analyticsOverflow'),0)+?2),'$.publicationOverflow',COALESCE(json_extract(payload,'$.publicationOverflow'),0)+?3) WHERE id=?1",
                params![
                    id,
                    overflow.min(i64::MAX as u64) as i64,
                    publication_overflow.min(i64::MAX as u64) as i64
                ],
            )?;
            if let Some(existing) = &pending_invalidation {
                // Changing the receipt result makes an in-flight drain retry
                // this same coalesced invalidation after the commit.
                tx.execute_cached(
                    "UPDATE runtime_outbox SET payload=json_set(payload,'$.stateInvalidationGeneration',COALESCE(json_extract(payload,'$.stateInvalidationGeneration'),0)+1) WHERE id=?1",
                    [existing],
                )?;
            } else {
                // The class marks the receipt that carries the invalidation.
                tx.execute_cached(
                    "UPDATE runtime_outbox SET payload=json_set(payload,'$.class',?2,'$.stateInvalidation',json('true')) WHERE id=?1",
                    params![id, PUBLICATION_INVALIDATION.as_str()],
                )?;
            }
            outbox.invalidation = Some(pending_invalidation.or(Some(id)));
        }
    }
    outbox.pending = Some(pending + retained);
    Ok(())
}

fn save_publication_effect(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    batch: &NotificationPublicationBatch,
    index: usize,
    effect: &crate::notification_dispatch::NotificationPostCommit,
) -> Result<String, WorkerError> {
    use crate::notification_dispatch::NotificationPostCommit as Effect;
    let operation = publication_operation(account, batch.sequence, index);
    let signature = match effect {
        Effect::TokenRateStream(_)
        | Effect::AnalyticsStreamCaptured(_)
        | Effect::TokenRateAssociation(_)
        | Effect::TokenRateUsage(_) => PUBLICATION_TOKEN_RATE,
        Effect::Analytics { .. }
        | Effect::AnalyticsStream { .. }
        | Effect::AnalyticsCaptured(_)
        | Effect::Recovered { .. } => PUBLICATION_PENDING,
    };
    save_publication(
        tx,
        &operation,
        Publication {
            account: account.to_owned(),
            sequence: batch.sequence,
            index,
            class: signature,
            effect: Some(serde_json::value::to_raw_value(effect).map_err(StoreError::from)?),
            ..Default::default()
        },
    )?;
    Ok(operation)
}

/// The oldest (or, with `newest`, the newest) pending publication receipt with
/// `signature`, or of any signature when it is empty. Key order is drain order,
/// so the lookup reads the primary key and never decodes receipt JSON.
fn pending_publication(
    tx: &studio_runtime_store::Transaction<'_>,
    prefix: &str,
    signature: Option<PublicationClass>,
    newest: bool,
) -> Result<Option<String>, WorkerError> {
    let order = if newest { "DESC" } else { "ASC" };
    Ok(tx
        .prepare_cached(&format!(
            "SELECT id FROM runtime_outbox WHERE account=?1 AND kind=?2 AND status=?3 AND (?4 IS NULL OR json_extract(payload,'$.class')=?4) ORDER BY ordinal {order},id {order} LIMIT 1"
        ))?
        .query_row(params![prefix, OutboxKind::Publication, OutboxStatus::Pending, signature.map(PublicationClass::as_str)], |row| {
            row.get::<_, String>(0)
        })
        .optional()?)
}

fn coalesce_token_rate_receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    prefix: &str,
    effect: &crate::notification_dispatch::NotificationPostCommit,
) -> Result<bool, WorkerError> {
    let target = match pending_publication(tx, prefix, Some(PUBLICATION_TOKEN_RATE), true)? {
        Some(id) => Some(id),
        None => pending_publication(tx, prefix, None, true)?,
    };
    let row = target
        .map(|id| {
            tx.query_row_cached(
                "SELECT payload FROM runtime_outbox WHERE id=?1",
                [&id],
                |row| row.get::<_, String>(0),
            )
            .map(|result| (id, result))
        })
        .transpose()?;
    if let Some((id, result)) = row {
        let mut publication: Publication =
            serde_json::from_str(&result).map_err(|_| invalid(&id))?;
        let count = publication.coalesced_effects.len() + usize::from(publication.effect.is_some());
        if count.saturating_sub(1) >= MAX_COALESCED_TOKEN_RATE_EFFECTS {
            return Ok(false);
        }
        // Keep each provider delivery identity intact. Rewriting earlier deltas
        // under a later cursor can double-project them after a partial drain.
        publication
            .coalesced_effects
            .push(serde_json::value::to_raw_value(effect).map_err(StoreError::from)?);
        if publication.effect.is_none() {
            publication.effect = Some(publication.coalesced_effects.remove(0));
        }
        publication.token_rate_coalesced = true;
        let serialized = serde_json::to_string(&publication).map_err(StoreError::from)?;
        if serialized.len() > MAX_COALESCED_TOKEN_RATE_BYTES {
            return Ok(false);
        }
        tx.execute_cached(
            "UPDATE runtime_outbox SET payload=?2 WHERE id=?1",
            params![id, serialized],
        )?;
    } else {
        return Ok(false);
    }
    Ok(true)
}

/// One delivered publication receipt: its id and the result the sink accepted.
pub(super) type DeliveredPublication = (String, String);

/// One drain pass over an account's publication receipts.
pub(super) struct PublicationPass {
    /// Receipts the sink accepted, in drain order.
    pub delivered: Vec<DeliveredPublication>,
    /// The pass read a full page, so more receipts may be pending.
    pub full: bool,
    /// The sink refused the receipt after the delivered prefix.
    pub failure: Option<WorkerError>,
}

/// Deliver pending receipts in sequence/index order without a writer
/// transaction. Publication is outside SQLite. The worker deletes the
/// delivered receipts in its next canvas transaction; until then they are in
/// `skip` and not delivered again. A crash before that deletion replays them,
/// which the sink's delivery identity makes safe.
pub(super) async fn deliver_publications(
    store: Arc<RuntimeStore>,
    account: String,
    sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
    skip: Vec<DeliveredPublication>,
    mut after_publish: impl FnMut() -> Result<(), WorkerError> + Send,
) -> Result<PublicationPass, WorkerError> {
    let page = PUBLICATION_DRAIN_LIMIT + skip.len();
    let skip = skip.into_iter().collect::<BTreeSet<_>>();
    let pending = store
        .read_async("runtime-services/account_worker_publication_read", move |connection| {
            let mut statement = connection.prepare(
                "SELECT id,payload FROM runtime_outbox WHERE account=?1 AND kind=?2 AND status=?3 ORDER BY ordinal,id LIMIT ?4",
            )?;
            let rows = statement
                .query_map(
                    params![account,OutboxKind::Publication,OutboxStatus::Pending,page as i64],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, WorkerError>(
                rows.into_iter()
                    .filter(|row| !skip.contains(row))
                    .take(PUBLICATION_DRAIN_LIMIT)
                    .collect::<Vec<_>>(),
            )
        })
        .await?;
    let full = pending.len() == PUBLICATION_DRAIN_LIMIT;
    let mut delivered = Vec::with_capacity(pending.len());
    let mut failure = None;
    for (id, result) in pending {
        if let Err(error) =
            publish_pending(&store, sink.as_ref(), &id, &result, &mut after_publish).await
        {
            failure = Some(error);
            break;
        }
        delivered.push((id, result));
    }
    Ok(PublicationPass {
        delivered,
        full,
        failure,
    })
}

/// Delete delivered receipts inside a canvas transaction. A changed result is
/// a newer coalesced effect that still has to be delivered, so it stays.
pub(super) fn delete_delivered_publications(
    tx: &studio_runtime_store::Transaction<'_>,
    delivered: &[DeliveredPublication],
) -> Result<(), WorkerError> {
    let mut delete = tx.prepare_cached("DELETE FROM runtime_outbox WHERE id=?1 AND payload=?2")?;
    for (id, result) in delivered {
        delete.execute(params![id, result])?;
    }
    Ok(())
}

/// Settle a capacity retry's record with the outcome of the `turn/start` that
/// carried its follow-up input; other starts stage nothing.
pub(super) fn stage_capacity_start(
    tx: &studio_runtime_store::Transaction<'_>,
    intent: &crate::scheduler::TurnIntent,
    outcome: CapacityStartOutcome<'_>,
) -> Result<(), StoreError> {
    crate::continuation_runtime::stage_capacity_start(
        tx,
        &intent.agent_id,
        intent.events.iter().map(|event| event.id.as_str()),
        outcome,
        now(),
    )
    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))
}

/// One pass that deletes its delivered prefix in its own transaction.
#[cfg(test)]
pub(super) async fn drain_publications(
    store: Arc<RuntimeStore>,
    account: String,
    sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
) -> Result<usize, WorkerError> {
    drain_publications_with_hook(store, account, sink, || Ok(())).await
}

#[cfg(test)]
pub(super) async fn drain_publications_with_hook(
    store: Arc<RuntimeStore>,
    account: String,
    sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
    before_delete: impl FnMut() -> Result<(), WorkerError> + Send,
) -> Result<usize, WorkerError> {
    let pass =
        deliver_publications(Arc::clone(&store), account, sink, Vec::new(), before_delete).await?;
    let count = pass.delivered.len();
    if count > 0 {
        let delivered = pass.delivered;
        store
            .transaction_with(move |tx| delete_delivered_publications(tx, &delivered))
            .await?;
    }
    pass.failure.map_or(Ok(count), Err)
}

/// Publish one pending receipt. Overflow diagnostics are persisted to
/// analytics_meta before the receipt is deleted, matching Python's lossy
/// analytics queue accounting.
async fn publish_pending(
    store: &Arc<RuntimeStore>,
    sink: &dyn crate::notification_runtime_effects::NotificationPostCommitSink,
    id: &str,
    result: &str,
    after_publish: &mut (impl FnMut() -> Result<(), WorkerError> + Send),
) -> Result<(), WorkerError> {
    let publication: Publication = serde_json::from_str(result).map_err(|_| invalid(id))?;
    let analytics_overflow = publication.analytics_overflow.unwrap_or_default();
    let publication_overflow = publication.publication_overflow.unwrap_or_default();
    if analytics_overflow > 0 || publication_overflow > 0 {
        crate::analytics_capture::record_publication_overflow(
            store,
            id.to_owned(),
            analytics_overflow,
            publication_overflow,
        )
        .await
        .map_err(|_| WorkerError::Capacity)?;
    }
    if publication.effect.is_some() || !publication.coalesced_effects.is_empty() {
        // The sink's delivery identity is the effect's provider identity.
        // It must make a replay after a crash between publish and delete safe.
        for raw in publication
            .effect
            .iter()
            .chain(&publication.coalesced_effects)
        {
            let effect: crate::notification_dispatch::NotificationPostCommit =
                serde_json::from_str(raw.get()).map_err(|_| StoreError::InvalidStoredObject {
                    table: "runtime_outbox",
                    id: id.to_owned(),
                })?;
            if let crate::notification_dispatch::NotificationPostCommit::TokenRateUsage(observation) =
                &effect
                && observation.method == "turn/completed"
                && let Some(agent) = observation.agent.as_ref().and_then(|a| a["id"].as_str())
                && let Some(turn) = observation
                    .params
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
            {
                crate::layr_native::save_turn(store, agent, turn)
                    .await
                    .map_err(|error| WorkerError::PreparationPending(error.to_string().into()))?;
            }
            sink.publish(&effect).await?;
        }
    } else if publication.analytics_overflow.is_none() && publication.publication_overflow.is_none()
    {
        return Err(invalid(id));
    }
    if publication.state_invalidation {
        let generation = publication.state_invalidation_generation;
        let request_id = format!("coalesced-state-invalidation:{id}:{generation}");
        sink.invalidate_state(&request_id).await?;
    }
    after_publish()
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct AccountStreams {
    pub(super) revision: String,
    metadata: NotificationStreamBuffer,
}

const VM_STREAM_OWNERS_SQL: &str = "SELECT id FROM runtime_agents WHERE account_key=?1 AND execution_mode='vm' AND hold_status='clear'";
const VM_STREAM_LANES_SQL: &str = "SELECT id,json_extract(record,'$.providerStream.streams') FROM runtime_agents WHERE account_key=?1 AND execution_mode='vm' AND json_type(record,'$.providerStream.streams')='object' AND hold_status='clear'";

fn persist_stream_lane(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    scope: &str,
    actor: Option<&str>,
    revision: &str,
    lane: &NotificationStreamBuffer,
) -> Result<AccountStreams, WorkerError> {
    let (metadata, pending) = lane.persisted_parts().map_err(|_| invalid(scope))?;
    let id = format!("streams:{scope}");
    let old = Outbox::get::<crate::notification_dispatch::NotificationPendingStreams>(tx, &id)
        .map_err(outbox_error)?;
    match pending {
        Some(pending) => {
            let row = if let Some(mut row) = old {
                row.payload = pending;
                row
            } else {
                OutboxRow {
                    signature: studio_runtime_store::sha256_hex(id.as_bytes()),
                    id,
                    kind: OutboxKind::StreamFlush,
                    owner_kind: if actor.is_some() {
                        OwnerKind::Agent
                    } else {
                        OwnerKind::Account
                    },
                    owner_id: actor.unwrap_or(account).to_owned(),
                    request_id: None,
                    status: OutboxStatus::Pending,
                    account: Some(account.to_owned()),
                    stream_id: Some(scope.to_owned()),
                    generation: None,
                    ordinal: None,
                    next_at: Some(0),
                    payload: pending,
                    created: (now() * 1000.0) as i64,
                }
            };
            Outbox::put(tx, &row).map_err(outbox_error)?;
        }
        None => {
            if let Some(mut row) = old {
                row.status = OutboxStatus::Committed;
                row.next_at = None;
                Outbox::put(tx, &row).map_err(outbox_error)?;
                Outbox::delete_settled(tx, &row.id, &row.signature).map_err(outbox_error)?;
            }
        }
    }
    Ok(AccountStreams {
        revision: revision.to_owned(),
        metadata: serde_json::from_str(&metadata).map_err(|_| invalid(scope))?,
    })
}

pub(crate) fn persist_streams(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    streams: &mut NotificationStreamBuffer,
) -> Result<(), WorkerError> {
    let revision = uuid::Uuid::new_v4().to_string();
    let mut host = streams.clone();
    let mut query = tx.prepare_cached(VM_STREAM_OWNERS_SQL)?;
    let actors = query
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(query);
    for actor in actors {
        let lane = host.take_owner(&actor);
        let scope = super::vm_journal_scope(account, &actor);
        let saved = persist_stream_lane(tx, account, &scope, Some(&actor), &revision, &lane)?;
        stored_agent(tx, &actor)?;
        tx.execute_cached("UPDATE runtime_agents SET record=json_set(record,'$.providerStream.streams',json(?2)) WHERE id=?1", params![actor, serde_json::to_string(&saved).map_err(StoreError::from)?])?;
    }
    let saved = persist_stream_lane(tx, account, account, None, &revision, &host)?;
    studio_runtime_store::account_state::put_streams(
        tx,
        account,
        &serde_json::to_string(&saved).map_err(StoreError::from)?,
    )?;
    streams.store_revision = Some((account.to_owned(), revision));
    Ok(())
}

pub(super) fn current_streams(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    cached: Option<&NotificationStreamBuffer>,
) -> Result<NotificationStreamBuffer, WorkerError> {
    if let Some(cached) = cached
        && let Some((cached_account, revision)) = &cached.store_revision
        && cached_account == account
    {
        let current:Option<String>=tx.query_row_cached("SELECT json_extract(streams,'$.revision') FROM runtime_account_state WHERE account=?1",[account],|row|row.get(0)).optional()?.flatten();
        if current.as_ref() == Some(revision) {
            return Ok(cached.clone());
        }
    }
    load_streams(tx, account)
}

fn restore_stream_lane(
    tx: &rusqlite::Connection,
    scope: &str,
    mut saved: AccountStreams,
) -> Result<AccountStreams, WorkerError> {
    if let Some(row) = Outbox::get::<crate::notification_dispatch::NotificationPendingStreams>(
        tx,
        &format!("streams:{scope}"),
    )
    .map_err(outbox_error)?
    {
        saved.metadata.restore_pending(row.payload)?;
    }
    Ok(saved)
}

pub(super) fn load_streams(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
) -> Result<NotificationStreamBuffer, WorkerError> {
    let saved = studio_runtime_store::account_state::streams(tx, account)?;
    let mut host = if let Some(raw) = saved {
        restore_stream_lane(
            tx,
            account,
            serde_json::from_str(&raw).map_err(|_| invalid(account))?,
        )?
    } else {
        AccountStreams {
            revision: String::new(),
            metadata: NotificationStreamBuffer::default(),
        }
    };
    let mut query = tx.prepare_cached(VM_STREAM_LANES_SQL)?;
    let lanes = query
        .query_map([account], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(query);
    for (actor, raw) in lanes {
        let scope = super::vm_journal_scope(account, &actor);
        let saved = restore_stream_lane(
            tx,
            &scope,
            serde_json::from_str(&raw).map_err(|_| invalid(&actor))?,
        )?;
        host.metadata.merge_owner(saved.metadata)?;
    }
    host.metadata.store_revision = Some((account.to_owned(), host.revision));
    Ok(host.metadata)
}
pub(super) async fn restore_streams(
    store: &RuntimeStore,
    account: &str,
) -> Result<NotificationStreamBuffer, WorkerError> {
    let account = account.to_owned();
    store
        .transaction_with(move |tx| {
            let mut streams = load_streams(tx, &account)?;
            crate::notification_dispatch::prune_inactive_streams(tx, &mut streams, now(), true)?;
            persist_streams(tx, &account, &mut streams)?;
            Ok(streams)
        })
        .await
}
pub(super) async fn flush_streams(
    store: Arc<RuntimeStore>,
    account: &str,
    delivered: Vec<DeliveredPublication>,
) -> Result<NotificationStreamBuffer, WorkerError> {
    let publication = crate::notification_dispatch::PublicationContext::default();
    let account = account.to_owned();
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        delete_delivered_publications(tx, &delivered)?;
        // A failed/cancelled journal job may have moved the live buffer. The
        // committed pending record, never that possibly empty cache, owns text.
        let mut streams = load_streams(tx, &account)?;
        crate::notification_dispatch::prune_inactive_streams(tx, &mut streams, now(), false)?;
        for (actor, owner, generation, thread) in streams.threads() {
            let identity = SchedulerRepository::find_attempt_for_actor(
                tx, &actor, &owner, generation, &thread, None,
            )?;
            if let Some(identity) = identity
                && let Some(next) = flush_notification_streams(
                    tx,
                    &identity.agent_id,
                    &publication,
                    &streams,
                    now(),
                )?
            {
                let mut next = next;
                let batch = next.take_post_commit()?;
                persist_publication_batch(tx, &account, &batch)?;
                streams = next;
            }
        }
        persist_streams(tx, &account, &mut streams)?;
        Ok(streams)
    })
    .await
}

/// Durable retry registration survives releasing an unsubmitted claim on restart.
pub(super) fn pending_preparations(
    db: &rusqlite::Connection,
    account: &str,
) -> Result<Vec<(String, i64)>, WorkerError> {
    // Python persists deferred preparation on startAttempt.prepareError and
    // clears it when preparation resumes (codex_runtime.py:7316, 7502-7505).
    let mut query = db.prepare("SELECT id FROM runtime_agents WHERE account_key=?1 AND auto_wake=1 AND deleted_at IS NULL AND COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.autoWake')=1 AND (json_type(record,'$.startAttempt.prepareError')='text' OR (json_type(record,'$.startAttempt') IS NULL AND json_type(record,'$.lastWorkspaceWait.attemptId')='text')) ORDER BY id")?;
    let ids = query
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .map(|id| {
            let row = AgentRepository::get(db, &AgentId::from(id.as_str()))?
                .ok_or_else(|| invalid(&id))?;
            Ok((id, row.epoch))
        })
        .collect()
}

#[cfg(test)]
mod stream_owner_query_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn vm_stream_queries_skip_json_for_host_agents() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(studio_runtime_store::agent_columns::agent_table_sql())
            .unwrap();
        for n in 0..20 {
            let id = format!("host-{n}");
            let raw = serde_json::json!({"id":id,"rootId":id,"provider":"codex","epoch":1,"status":"idle","accountKey":"default","executionMode":"host"});
            db.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![id, raw.to_string()],
            )
            .unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        db.create_scalar_function(
            "json_extract",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
            move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(None::<i64>)
            },
        )
        .unwrap();
        for sql in [super::VM_STREAM_OWNERS_SQL, super::VM_STREAM_LANES_SQL] {
            assert!(
                db.prepare(sql)
                    .unwrap()
                    .query(["default"])
                    .unwrap()
                    .next()
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod pending_preparation_scan_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn pending_scan_skips_json_for_agents_with_autowake_disabled() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(studio_runtime_store::agent_columns::agent_table_sql())
            .unwrap();
        for n in 0..20 {
            let id = format!("disabled-{n}");
            let raw = serde_json::json!({"id":id,"rootId":id,"provider":"codex","epoch":1,"status":"idle","accountKey":"default","autoWake":false});
            db.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![id, raw.to_string()],
            )
            .unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        db.create_scalar_function(
            "json_extract",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
            move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(None::<i64>)
            },
        )
        .unwrap();
        assert!(
            super::pending_preparations(&db, "default")
                .unwrap()
                .is_empty()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pending_scan_preserves_account_and_legacy_wait_selection() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(studio_runtime_store::agent_columns::agent_table_sql())
            .unwrap();
        db.execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .unwrap();
        for (id, account, auto_wake, extra) in [
            (
                "a",
                "default",
                true,
                serde_json::json!({"startAttempt":{"prepareError":"waiting"}}),
            ),
            (
                "b",
                "default",
                true,
                serde_json::json!({"lastWorkspaceWait":{"attemptId":"old"}}),
            ),
            (
                "c",
                "default",
                true,
                serde_json::json!({"startAttempt":null,"lastWorkspaceWait":{"attemptId":"old"}}),
            ),
            (
                "d",
                "other",
                true,
                serde_json::json!({"startAttempt":{"prepareError":"waiting"}}),
            ),
            (
                "e",
                "default",
                false,
                serde_json::json!({"startAttempt":{"prepareError":"waiting"}}),
            ),
        ] {
            let mut row = serde_json::json!({"id":id,"rootId":id,"provider":"codex","epoch":2,"status":"idle","accountKey":account,"autoWake":auto_wake});
            row.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            db.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![id, row.to_string()],
            )
            .unwrap();
        }
        assert_eq!(
            super::pending_preparations(&db, "default").unwrap(),
            vec![("a".into(), 2), ("b".into(), 2)]
        );
    }
}

/// A failed turn whose error proves the account's credentials rejected ends
/// the account's cached auth status, for every provider; the next read probes
/// once. `account_pin` is the worker account's auth pin (provider and home).
fn invalidate_auth_on_failure(agent: &Value, account_pin: &Value, turn: &Value) {
    use crate::account_driver::{AuthProfile, ProviderAuthReader, SystemProviderAuthReader};
    let Ok(provider) = studio_runtime_providers::ProviderKind::of_record(agent) else {
        return;
    };
    if turn["status"] != "failed"
        || crate::provider_adapter::adapter(provider).classify_error(&turn["error"])
            != Some(crate::usage_resume::Cause::Auth)
    {
        return;
    }
    let Some(home) = account_pin["home"].as_str() else {
        return;
    };
    SystemProviderAuthReader.invalidate(&AuthProfile {
        provider,
        home: home.into(),
        claude_options: account_pin
            .get("claudeOptions")
            .filter(|options| !options.is_null())
            .cloned(),
    });
}

/// Outcome of one preparation stage.
pub(super) enum PreparationStep<T> {
    Ready(T),
    /// Durably recorded as `startAttempt.prepareError`; wake when it unblocks.
    Pending(PendingWait),
    /// The start outcome was settled (refused); nothing further to do.
    Settled,
    /// Event-local failures were quarantined; reclaim safe input under a new id.
    Retry,
}

/// A pending preparation's reason and the agent rows whose change may clear it.
pub(super) struct PendingWait {
    pub reason: crate::account_worker::PreparationWaitReason,
    pub rows: super::agent_wakes::WakeRows,
}

impl<T> PreparationStep<T> {
    /// The ready value, or this non-ready step for a later stage.
    pub(super) fn into_ready<U>(self) -> Result<T, PreparationStep<U>> {
        match self {
            Self::Ready(value) => Ok(value),
            Self::Pending(pending) => Err(PreparationStep::Pending(pending)),
            Self::Settled => Err(PreparationStep::Settled),
            Self::Retry => Err(PreparationStep::Retry),
        }
    }
}

pub(super) async fn preparation_step<T: Send + 'static>(
    store: &Arc<RuntimeStore>,
    resources: &preparation::PreparationResources,
    intent: &TurnIntent,
    action: impl Future<Output = Result<T, WorkerError>> + Send,
) -> Result<PreparationStep<T>, WorkerError> {
    let (block, own_row) = match action.await {
        Ok(value) => return Ok(PreparationStep::Ready(value)),
        Err(WorkerError::PreparationPending(reason)) => (reason, false),
        Err(WorkerError::PreparationAwaitsAgent(reason)) => (reason, true),
        Err(WorkerError::PreparationRefused(reason)) => {
            if preparation::refuse_idle(store, resources, intent, &reason).await? {
                return Ok(PreparationStep::Retry);
            }
            scheduler::settle_start_outcome(store, intent, TurnStart::Blocked { reason }).await?;
            return Ok(PreparationStep::Settled);
        }
        Err(error) => return Err(error),
    };
    let owner = block.reason;
    let reason = block.diagnostic;
    // A repeated attempt with the same blocker reads only: the writer opens
    // once per blocker change, so a woken but still blocked preparation
    // leaves the row and its observers untouched.
    let agent_id = intent.agent_id.clone();
    let attempt_id = intent.attempt_id.clone();
    let epoch = intent.epoch;
    let blocker = reason.clone();
    let (row_id, needs_record) = store
        .read_async("runtime-services/account_worker_pending_read", move |db| {
            let row_id = db
                .query_row(
                    "SELECT rowid FROM runtime_agents WHERE id=?1",
                    [&agent_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            let row = AgentRepository::get(db, &AgentId::from(agent_id.as_str()))?
                .ok_or_else(|| invalid(&agent_id))?;
            Ok::<_, WorkerError>((
                row_id,
                pending_needs_record(&row, &blocker, owner, &attempt_id, epoch),
            ))
        })
        .await?;
    if needs_record {
        record_pending(store, intent, reason.clone(), owner).await?;
    }
    let rows = match row_id {
        Some(row) if own_row => super::agent_wakes::WakeRows::Own(row),
        _ => super::agent_wakes::WakeRows::Any,
    };
    Ok(PreparationStep::Pending(PendingWait {
        reason: owner,
        rows,
    }))
}

/// Whether the unsubmitted attempt at `epoch` does not record `reason` as its
/// pending blocker yet.
fn pending_needs_record(
    row: &studio_runtime_store::AgentRecord,
    reason: &str,
    owner: crate::account_worker::PreparationWaitReason,
    attempt_id: &str,
    epoch: i64,
) -> bool {
    let Some(attempt) = row.extra.get("startAttempt") else {
        return false;
    };
    let wait = owner.wait(epoch);
    let recorded = row.start_wait.as_ref() == Some(&wait)
        && attempt.get("prepareError").and_then(Value::as_str) == Some(reason)
        && (reason != preparation::WORKSPACE_PENDING_REASON
            || row
                .extra
                .get("lastWorkspaceWait")
                .is_some_and(|wait| wait["attemptId"] == attempt_id));
    !recorded && row.epoch == epoch && attempt["id"] == attempt_id && attempt["submitted"] == false
}

/// Record a changed pending blocker on the unsubmitted attempt.
async fn record_pending(
    store: &RuntimeStore,
    intent: &TurnIntent,
    reason: String,
    owner: crate::account_worker::PreparationWaitReason,
) -> Result<(), WorkerError> {
    let agent_id = intent.agent_id.clone();
    let attempt_id = intent.attempt_id.clone();
    let events = intent
        .events
        .iter()
        .map(|event| event.id.clone())
        .collect::<Vec<_>>();
    let epoch = intent.epoch;
    scheduler::lifecycle_transaction_with(store, move |tx| {
        let mut row = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
            .ok_or_else(|| invalid(&agent_id))?;
        if pending_needs_record(&row, &reason, owner, &attempt_id, epoch)
            && let Some(attempt) = row
                .extra
                .get_mut("startAttempt")
                .and_then(Value::as_object_mut)
        {
            attempt.insert("prepareError".into(), json!(reason));
            row.start_wait = Some(owner.wait(epoch));
            if reason == preparation::WORKSPACE_PENDING_REASON {
                // Python's WorkspaceWaitRecord uses these fields
                // (codex_workspace_delivery.py:43-46).
                row.extra.insert(
                    "lastWorkspaceWait".into(),
                    json!({
                        "attemptId": attempt_id,
                        "events": events,
                        "blockers": [],
                        "at": now(),
                    }),
                );
            }
            AgentRepository::put(tx, &row)?;
        }
        Ok::<_, WorkerError>(())
    })
    .await
}

fn hold_thread_receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    generation: u64,
    reason: &str,
) -> Result<(), WorkerError> {
    hold_before_claim(tx, &context.intent.agent_id, reason)?;
    let mut agent = AgentRepository::get(tx, &AgentId::from(context.intent.agent_id.as_str()))?
        .ok_or_else(|| invalid(&context.intent.agent_id))?;
    agent.start_outcome_hold = Some(studio_runtime_store::StoredStartOutcomeHold::Known(
        studio_runtime_store::StartOutcomeHold::Held {
            reason: Some(reason.into()),
            operation_id: Some(context.operation.clone()),
            connection_generation: Some(generation),
            attempt_id: None,
            extra: Default::default(),
        },
    ));
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

fn hold_before_claim(
    tx: &studio_runtime_store::Transaction<'_>,
    id: &str,
    reason: &str,
) -> Result<(), WorkerError> {
    let mut agent = AgentRepository::get(tx, &AgentId::from(id))?.ok_or_else(|| invalid(id))?;
    agent.status = studio_runtime_store::AgentStatus::Waiting;
    agent.extra.insert("nativeFailureHold".into(), json!(true));
    agent.auto_wake = Some(false);
    agent.in_flight = Some(false);
    agent.extra.insert("error".into(), json!(reason));
    agent.start_outcome_hold = Some(studio_runtime_store::StoredStartOutcomeHold::Known(
        studio_runtime_store::StartOutcomeHold::Held {
            reason: Some(reason.into()),
            operation_id: None,
            connection_generation: None,
            attempt_id: None,
            extra: Default::default(),
        },
    ));
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

#[cfg(test)]
mod journal_frame_payload_tests {
    use super::*;
    use serde_json::{Map, json};
    use studio_runtime_providers::{NotificationFrame, RequestFrame, UnknownMethodFrame};

    fn assert_legacy_payload_bytes(frame: &Frame) {
        let encoded = studio_runtime_providers::encode_line(frame).expect("frame encoding");
        let frame_value: Value = serde_json::from_slice(&encoded).expect("encoded frame JSON");
        let legacy_payload = json!({"frame": frame_value}).to_string();
        assert_eq!(
            journal_frame_payload(frame, "test journal").expect("journal payload"),
            legacy_payload.as_bytes()
        );
    }

    #[test]
    fn payload_bytes_match_legacy_digest_input_for_all_frame_variants() {
        let params = json!({
            "zLast": {
                "newline": "first\nsecond",
                "unicode": "café 雪 😀",
                "escaped": "quote: \\\" slash: / tab: \\t"
            },
            "aFirst": [1, -2, 1.25, 1e30, true, null]
        });
        let mut request_fields = Map::new();
        request_fields.insert("zExtension".into(), json!({"z": 2, "a": 1}));
        request_fields.insert("id".into(), json!("superseded-id"));
        request_fields.insert("method".into(), json!("superseded/method"));
        request_fields.insert("params".into(), json!({"superseded": true}));
        let request = Frame::Request(RequestFrame {
            id: RequestId::Number(u64::MAX),
            method: Method::TurnStarted,
            params: Some(params.clone()),
            fields: request_fields.clone(),
        });
        let string_request = Frame::Request(RequestFrame {
            id: RequestId::String("claude-request-id".into()),
            method: Method::TurnStarted,
            params: Some(params.clone()),
            fields: request_fields,
        });

        let mut notification_fields = Map::new();
        notification_fields.insert("zExtension".into(), json!({"z": 2, "a": 1}));
        notification_fields.insert("method".into(), json!("superseded/method"));
        let notification = Frame::Notification(NotificationFrame {
            method: Method::TurnCompleted,
            params: Some(params.clone()),
            fields: notification_fields,
        });

        let mut result_fields = Map::new();
        result_fields.insert("zExtension".into(), json!({"nested": {"z": 1, "a": 2}}));
        result_fields.insert("id".into(), json!("superseded-result-id"));
        result_fields.insert("result".into(), json!({"superseded": true}));
        let response_result = Frame::Response {
            id: RequestId::Number(9_007_199_254_740_991),
            body: ResponseBody::Result(json!({"z": params.clone(), "a": 1e-30})),
            fields: result_fields,
        };

        let mut error_fields = Map::new();
        error_fields.insert("aExtension".into(), json!("before\nafter 雪"));
        error_fields.insert("id".into(), json!(0));
        error_fields.insert("error".into(), json!({"superseded": true}));
        let response_error = Frame::Response {
            id: RequestId::Number(17),
            body: ResponseBody::Error(json!({
                "code": -32_601,
                "message": "provider \"failed\"\n雪",
                "data": {"z": 1.0, "a": -0.0}
            })),
            fields: error_fields,
        };

        let mut unknown_fields = Map::new();
        unknown_fields.insert(
            "zExtension".into(),
            json!({"last": 0.000001, "first": -0.0}),
        );
        unknown_fields.insert("method".into(), json!("future/notification"));
        unknown_fields.insert("params".into(), params);
        unknown_fields.insert("aExtension".into(), json!("雪\n😀"));
        let unknown = Frame::UnknownMethod(UnknownMethodFrame {
            method: "future/notification".into(),
            fields: unknown_fields,
        });

        assert_legacy_payload_bytes(&request);
        assert_legacy_payload_bytes(&string_request);
        assert_legacy_payload_bytes(&notification);
        assert_legacy_payload_bytes(&response_result);
        assert_legacy_payload_bytes(&response_error);
        assert_legacy_payload_bytes(&unknown);
    }
}

#[cfg(test)]
mod pending_thread_start_tests {
    use super::*;
    fn database() -> rusqlite::Connection {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("target schema");
        db
    }
    fn submit(db: &rusqlite::Connection, account: &str, agent: &str, method: Method) -> String {
        let operation = format!("account:{account}:{agent}:{}", method.as_str());
        let request = RecoveredRequest {
            ticket: studio_runtime_providers::RequestTicket {
                id: RequestId::String(operation.clone()),
                generation: 1,
                method,
            },
            operation_id: OperationId(operation.clone()),
            thread_id: None,
            lightweight: false,
            deadline_ms: 1,
            unknown: true,
            remote_id: None,
        };
        let context = Pending {
            intent: TurnIntent {
                agent_id: agent.into(),
                epoch: 0,
                attempt_id: operation.clone(),
                account_key: account.into(),
                provider: ProviderKind::Codex,
                thread_id: None,
                cwd: None,
                model: None,
                events: Vec::new(),
                radio_authorized: false,
            },
            method,
            operation: operation.clone(),
            thread: None,
            compiled: None,
        };
        let tx = db.unchecked_transaction().expect("transaction");
        Outbox::put(
            &tx,
            &OutboxRow {
                id: operation.clone(),
                kind: OutboxKind::ProviderCall,
                owner_kind: OwnerKind::Agent,
                owner_id: agent.into(),
                request_id: None,
                signature: operation.clone(),
                status: OutboxStatus::Unknown,
                account: Some(account.into()),
                stream_id: Some(account.into()),
                generation: Some(1),
                ordinal: None,
                next_at: None,
                payload: ProviderCall::Account(Box::new(AccountCall {
                    context,
                    request: super::super::retained::storage::TransportRequest::from(&request),
                    written: None,
                    restarted: true,
                })),
                created: 1,
            },
        )
        .expect("obligation");
        tx.commit().expect("commit");
        operation
    }
    #[test]
    fn supervisor_written_is_fenced_to_the_original_provider_generation() {
        let db = database();
        let id = submit(&db, "account", "lead", Method::TurnStart);
        let tx = db.unchecked_transaction().expect("transaction");
        record_supervisor_written(&tx, "account", &id, Some(2), Some(9)).expect("stale generation");
        let row = Outbox::get::<ProviderCall>(&tx, &id)
            .expect("read")
            .expect("row");
        assert_eq!(row.status, OutboxStatus::Unknown);
        record_supervisor_written(&tx, "account", &id, Some(1), Some(9)).expect("written");
        let row = Outbox::get::<ProviderCall>(&tx, &id)
            .expect("read")
            .expect("row");
        assert_eq!(row.status, OutboxStatus::Submitted);
        let ProviderCall::Account(call) = row.payload else {
            panic!("account call")
        };
        assert_eq!(call.written.expect("written request").remote_id, Some(9));
    }
    #[test]
    fn unresolved_thread_requests_block_only_their_agent() {
        for method in [Method::ThreadStart, Method::ThreadResume] {
            let db = database();
            let id = submit(&db, "account", "lead", method);
            assert!(has_pending_thread_start(&db, "lead").expect("pending"));
            assert!(!has_pending_thread_start(&db, "other").expect("other"));
            let tx = db.unchecked_transaction().expect("transaction");
            forget_request(&tx, &id).expect("positive settlement");
            tx.commit().expect("commit");
            assert!(!has_pending_thread_start(&db, "lead").expect("settled"));
        }
    }
    #[test]
    fn old_account_unknown_cannot_be_replaced_by_another_account() {
        let db = database();
        submit(&db, "old", "lead", Method::ThreadStart);
        submit(&db, "new", "lead", Method::TurnStart);
        assert!(has_pending_thread_start(&db, "lead").expect("old uncertainty"));
    }
    #[test]
    fn turn_submission_is_not_a_pending_thread_request() {
        let db = database();
        submit(&db, "account", "lead", Method::TurnStart);
        assert!(!has_pending_thread_start(&db, "lead").expect("turn"));
    }
    #[test]
    fn pending_owner_lookup_uses_the_outbox_owner_index() {
        let db = database();
        submit(&db, "account", "lead", Method::ThreadStart);
        let plan:String=db.query_row("EXPLAIN QUERY PLAN SELECT payload FROM runtime_outbox WHERE owner_kind=?1 AND owner_id=?2 AND kind=?3 ORDER BY id",params![OwnerKind::Agent,"lead",OutboxKind::ProviderCall],|row|row.get(3)).expect("plan");
        assert!(plan.contains("runtime_outbox_owner"), "{plan}");
    }
}
