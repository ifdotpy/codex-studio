//! Busy-turn input for Codex and Claude agents.
//!
//! A running turn receives eligible current-turn input through `turn/steer`;
//! both providers speak the same request and
//! acknowledge it the same way.
//! The running turn keeps its `startAttempt`, which still routes the turn's
//! notifications and terminal outcome. Each steer owns one durable receipt in
//! the runtime outbox; its event rows move through these states:
//!
//! | Receipt stage | Event status  | Meaning                                         |
//! | ------------- | ------------- | ----------------------------------------------- |
//! | `reserved`    | `reserved`    | Claimed; not yet authorized for transport       |
//! | `submitted`   | `dispatching` | Request may have been written; awaiting the ack |
//! | `unconfirmed` | `failed`      | The turn ended without an ack; never resent     |
//! | (deleted)     | `delivered`   | The provider acknowledged the exact input       |
//! | (deleted)     | `pending`     | Proven not applied; delivered once, later       |
//!
//! Only the provider acknowledgement records delivery. A lost response leaves
//! the input `dispatching` until the acknowledgement or the turn's end decides
//! it. Nothing in this module ever resubmits an input whose outcome is unknown.
//! A turn end proves input unread when the provider reports it (the Claude
//! bridge's `unappliedSteers`), or when Studio wrote the steer after the time
//! at which the provider reports that the turn stopped taking input
//! ([`SchedulerRepository::release_steers_written_after_turn_end`]).

use super::*;
use crate::CachedSql;
use crate::outbox::{Outbox, OutboxRow};
use crate::request_results::{Admission, AdmittedRequest, RequestResults};
use studio_runtime_schema::DeliveryMoment;
use studio_runtime_schema::persistence::{
    OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, SteerStage,
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SteerPayload {
    agent: String,
    epoch: i64,
    account_key: String,
    thread_id: String,
    turn_id: String,
    events: Vec<String>,
    stage: SteerStage,
    created: f64,
    root_request_id: String,
    admission: AdmittedRequest,
    #[serde(default)]
    request_ticket: Option<String>,
    #[serde(default)]
    generation: Option<u64>,
    #[serde(default)]
    submitted_at: Option<f64>,
    #[serde(default)]
    prepared_submission: Option<Value>,
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    interrupt_still_queued: bool,
}

/// Prefix of the durable per-steer receipt: `claude-steer:{agent}:{steer}`.
/// The name predates Codex steering; it is kept so stored receipts stay readable.
const STEER_RECEIPT_PREFIX: &str = "claude-steer:";
const STEER_RECEIPT_SIGNATURE: &str = "claude-steer";
/// Visible error of input whose delivery to the running turn is unknown.
pub(super) const STOP_CANCELLED_ERROR: &str = "Cancelled by Stop before Claude read this input";
pub(super) const STOP_QUEUED_ERROR: &str =
    "Claude still holds this input after Stop. Delivery is unknown.";
pub const STEER_UNCONFIRMED_ERROR: &str = "The agent did not confirm this message before its turn ended. It may have been read; Studio did not send it again.";
/// Unit: seconds. The least time by which a steer's durable `submittedAt`
/// must follow the provider's reported turn end before the steer counts as
/// written after that end. The provider reports the end in whole seconds; the
/// rest of the margin absorbs a small wall-clock correction between the two
/// reads of the same host clock.
const STEER_AFTER_TURN_END_SECONDS: f64 = 5.0;

/// One claimed busy-turn input batch.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedSteer {
    pub steer_id: String,
    pub root_request_id: String,
    pub admission: AdmittedRequest,
    pub agent: AgentRecord,
    pub thread_id: String,
    pub turn_id: String,
    pub events: Vec<QueueEvent>,
    pub metadata: BTreeMap<String, Value>,
}

/// Input proven delivered by the provider acknowledgement.
#[derive(Clone, Debug, PartialEq)]
pub struct AcknowledgedSteer {
    pub agent_id: String,
    pub admission: AdmittedRequest,
    pub steer_id: String,
    /// Original batch identity, including input withdrawn after uncertainty.
    pub event_ids: Vec<String>,
    /// Opaque compiler-owned projection frozen before transport, retained until
    /// acknowledgement even after the ordinary request reply is retired.
    pub prepared_submission: Option<Value>,
    pub events: Vec<QueueEvent>,
    pub submitted_at: f64,
}

fn receipt_id(agent: &str, steer: &str) -> String {
    format!("{STEER_RECEIPT_PREFIX}{agent}:{steer}")
}

fn invalid_receipt(id: &str) -> StoreError {
    invalid_object("runtime_outbox", id)
}
fn outbox_error(error: crate::outbox::OutboxError) -> StoreError {
    StoreError::SchemaMismatch(error.to_string())
}
fn load(db: &Connection, id: &str) -> StoreResult<Option<SteerPayload>> {
    Outbox::get::<SteerPayload>(db, id)
        .map_err(outbox_error)?
        .map(|row| {
            if row.kind != OutboxKind::Steer
                || row.owner_kind != OwnerKind::Agent
                || row.owner_id != row.payload.agent
                || row.signature != STEER_RECEIPT_SIGNATURE
            {
                return Err(invalid_receipt(id));
            }
            Ok(row.payload)
        })
        .transpose()
}
fn store(tx: &Transaction<'_>, id: &str, value: &SteerPayload) -> StoreResult<()> {
    let status = match value.stage {
        SteerStage::Reserved => OutboxStatus::Pending,
        SteerStage::Submitted => OutboxStatus::Submitted,
        SteerStage::Unconfirmed => OutboxStatus::Unknown,
    };
    Outbox::put(
        tx,
        &OutboxRow {
            id: id.into(),
            kind: OutboxKind::Steer,
            owner_kind: OwnerKind::Agent,
            owner_id: value.agent.clone(),
            request_id: Some(value.admission.id().into()),
            signature: STEER_RECEIPT_SIGNATURE.into(),
            status,
            account: Some(value.account_key.clone()),
            stream_id: Some(value.thread_id.clone()),
            generation: None,
            ordinal: None,
            next_at: None,
            payload: value.clone(),
            created: (value.created * 1000.0) as i64,
        },
    )
    .map_err(outbox_error)
}
fn delete(tx: &Transaction<'_>, id: &str) -> StoreResult<()> {
    Outbox::delete_settled(tx, id, STEER_RECEIPT_SIGNATURE).map_err(outbox_error)?;
    Ok(())
}
fn receipts(tx: &Transaction<'_>, agent: &str) -> StoreResult<Vec<(String, SteerPayload)>> {
    Ok(
        Outbox::for_owner::<SteerPayload>(tx, OwnerKind::Agent, agent, OutboxKind::Steer)
            .map_err(outbox_error)?
            .into_iter()
            .map(|row| (row.id, row.payload))
            .collect(),
    )
}
fn receipt_events(id: &str, receipt: &SteerPayload) -> StoreResult<String> {
    if receipt.events.is_empty() {
        return Err(invalid_receipt(id));
    }
    Ok(serde_json::to_string(&receipt.events)?)
}

/// The running turn and thread that may receive input, or `None`.
fn steer_turn(agent: &AgentRecord) -> Option<(String, String)> {
    let text = |key: &str| agent.extra.get(key).and_then(Value::as_str);
    let flag = |key: &str| agent.extra.get(key).is_some_and(truthy);
    let turn = text("turnId").filter(|turn| !turn.is_empty())?;
    let thread = text("threadId").filter(|thread| !thread.is_empty())?;
    let attempt = agent.extra.get("startAttempt")?;
    let eligible = agent.status == AgentStatus::Running
        && agent.extra.get("autoWake") == Some(&json!(true))
        && agent.extra.get("inFlight") == Some(&json!(true))
        && !flag("remoteWorker")
        && !flag("remoteAnchor")
        && text("executionMode") != Some("vm")
        && continuation_allowed(agent)
        && text("steerRejectedTurnId") != Some(turn)
        && attempt["turnId"] == turn
        && attempt["threadId"] == thread
        && attempt["epoch"] == agent.epoch
        && attempt["submitted"] == true
        && attempt.get("action").is_none_or(|action| !truthy(action));
    eligible.then(|| (turn.to_owned(), thread.to_owned()))
}

/// Delivery eligibility is separate from event order: deferred input stays in
/// the same queue while later current-turn input can join the active turn.
fn steerable(event: &QueueEvent, metadata: Option<&Value>) -> StoreResult<bool> {
    let moment = DeliveryMoment::for_event(&event.kind, &event.text, metadata)
        .map_err(|_| invalid_object("runtime_event_meta", &event.id.0))?;
    Ok(moment == DeliveryMoment::CurrentTurn
        && shares_turn(&event.kind, true)
        && !native_input_command(event)
        && !event.text.trim().is_empty()
        && metadata.is_none_or(|meta| {
            meta.get("assets")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
                && meta.get("radioAnswerTurnId").is_none_or(Value::is_null)
        }))
}

/// Ordered pending input with its stored metadata. Callers select a delivery
/// moment before coalescing progress, so a deferred update cannot retire an
/// eligible current-turn update.
fn pending_steer_events(
    connection: &Connection,
    agent_id: &str,
    epoch: i64,
) -> StoreResult<(Vec<QueueEvent>, BTreeMap<String, Value>)> {
    let metadata_column = if table_exists(connection, "runtime_event_meta")? {
        "(SELECT m.record FROM runtime_event_meta m WHERE m.id=e.id)"
    } else {
        "NULL"
    };
    let mut statement = connection.prepare_cached(&format!(
        "SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,{metadata_column} \
         FROM runtime_events e WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
         ORDER BY e.created,e.rowid"
    ))?;
    let rows = statement.query_map(params![agent_id, epoch], |row| {
        Ok((
            crate::QueueEventRepository::from_row(row)?,
            row.get::<_, Option<String>>(9)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut metadata = BTreeMap::new();
    for row in rows {
        let (event, raw) = row?;
        let meta = pending_metadata(&event, raw.as_deref())?;
        if steerable(&event, meta.as_ref())? {
            if let Some(meta) = meta {
                metadata.insert(event.id.0.clone(), meta);
            }
            events.push(event);
        }
    }
    Ok((events, metadata))
}

/// Return unresolved input to the queue in its own epoch; stopped input is cancelled.
/// When the turn already ended, the idle agent is queued again so the
/// returned input starts the next turn; returns whether it was, so the caller
/// wakes it after commit.
fn release(tx: &Transaction<'_>, id: &str, receipt: &SteerPayload) -> StoreResult<bool> {
    let agent_id = receipt.agent.as_str();
    let epoch = receipt.epoch;
    let agent = load_agent(tx, agent_id)?
        .filter(|agent| agent.epoch == epoch && !agent.extra.get("deletedAt").is_some_and(truthy));
    let returned = tx.execute_cached(
        "UPDATE runtime_events SET status=?1,turn_id=NULL,error=NULL \
         WHERE agent=?2 AND epoch=?3 AND id IN (SELECT value FROM json_each(?4)) \
         AND (status IN ('reserved','dispatching') OR (status='failed' AND error=?5))",
        params![
            if agent.is_some() {
                "pending"
            } else {
                "cancelled"
            },
            agent_id,
            epoch,
            receipt_events(id, receipt)?,
            STEER_UNCONFIRMED_ERROR
        ],
    )?;
    let requeue = returned > 0
        && agent.as_ref().is_some_and(|agent| {
            agent.extra.get("autoWake") == Some(&json!(true))
                && !agent.extra.get("inFlight").is_some_and(truthy)
                && !matches!(
                    agent.status,
                    AgentStatus::Running
                        | AgentStatus::Starting
                        | AgentStatus::Queued
                        | AgentStatus::Approval
                )
                && continuation_allowed(agent)
        });
    if requeue && let Some(mut agent) = agent {
        agent.status = AgentStatus::Queued;
        publish_agent(tx, &agent)?;
    }
    delete(tx, id)?;
    Ok(requeue)
}

/// The turn ended or its transport closed without an acknowledgement.
fn unconfirm(tx: &Transaction<'_>, id: &str, mut receipt: SteerPayload) -> StoreResult<()> {
    let agent_id = receipt.agent.as_str();
    let epoch = receipt.epoch;
    tx.execute_cached(
        "UPDATE runtime_events SET status='failed',error=?1 \
         WHERE agent=?2 AND epoch=?3 AND status='dispatching' \
         AND id IN (SELECT value FROM json_each(?4))",
        params![
            STEER_UNCONFIRMED_ERROR,
            agent_id,
            epoch,
            receipt_events(id, &receipt)?
        ],
    )?;
    receipt.stage = SteerStage::Unconfirmed;
    store(tx, id, &receipt)
}

/// The running turn that may receive pending input of `epoch` now: the agent is
/// steerable, no earlier input of the agent is unresolved, and input is pending.
fn steer_target(
    connection: &Connection,
    agent: &AgentRecord,
    epoch: i64,
) -> StoreResult<Option<(String, String)>> {
    if agent.epoch != epoch {
        return Ok(None);
    }
    let Some(target) = steer_turn(agent) else {
        return Ok(None);
    };
    let ready: bool = connection.query_row_cached(
        "SELECT NOT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 \
         AND status IN ('reserved','dispatching','uncertain')) \
         AND EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending')",
        params![agent.id.0, epoch],
        |row| row.get(0),
    )?;
    Ok(ready.then_some(target))
}

impl SchedulerRepository {
    pub fn steer_root(db: &Connection, agent: &str, steer: &str) -> StoreResult<Option<String>> {
        Ok(load(db, &receipt_id(agent, steer))?.map(|receipt| receipt.root_request_id))
    }
    /// Read-only check whether `claim_steer` could reserve input now, so a busy
    /// turn's wake does not open a write transaction while its queue is blocked.
    pub fn steer_claimable(
        connection: &Connection,
        agent: &AgentRecord,
        epoch: i64,
    ) -> StoreResult<bool> {
        Ok(steer_target(connection, agent, epoch)?.is_some()
            && !pending_steer_events(connection, &agent.id.0, epoch)?
                .0
                .is_empty())
    }

    /// Reserve eligible current-turn input in durable queue order. Deferred
    /// input does not block it. A previous steer must resolve before another.
    pub fn claim_steer(
        tx: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        steer_id: &str,
        created: f64,
    ) -> StoreResult<Option<ClaimedSteer>> {
        let Some(agent) = load_agent(tx, agent_id)? else {
            return Ok(None);
        };
        let Some((turn_id, thread_id)) = steer_target(tx, &agent, epoch)? else {
            return Ok(None);
        };
        let (events, metadata) = pending_steer_events(tx, agent_id, epoch)?;
        let (complaints, _work) = load_policy_records(tx, &agent, &events)?;
        let obsolete_work = obsolete_work_review_ids(tx, &agent, &events)?;
        let prepared =
            prepare_pending_batch(&agent, events, metadata, &complaints, &obsolete_work)?;
        apply_preparation(tx, &prepared)?;
        let mut events = prepared.events;
        let metadata = prepared.metadata;
        if events.is_empty() {
            return Ok(None);
        }
        for event in &mut events {
            let changed = tx.execute_cached(
                "UPDATE runtime_events SET status='reserved' WHERE id=?1 AND agent=?2 \
                 AND epoch=?3 AND status='pending'",
                params![event.id.0, agent_id, epoch],
            )?;
            if changed != 1 {
                return Err(StoreError::SchemaMismatch(
                    "steer claim changed during its write transaction".into(),
                ));
            }
            event.status = "reserved".into();
        }
        let ids = events
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>();
        mark_timing(tx, &json!(ids).to_string(), "dispatchPickedAt")?;
        let root = RequestIdentity::issue_root();
        let identity = RequestIdentity::new("execution-steer", &[agent_id], &root, &[steer_id])
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
        let signature =
            json!({"agent":agent_id,"epoch":epoch,"thread":thread_id,"turn":turn_id,"events":ids})
                .to_string();
        let Admission::Fresh(admission) =
            RequestResults::admit(tx, &identity, &signature, (timestamp() * 1000.0) as i64)
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        else {
            return Err(invalid_receipt(steer_id));
        };
        store(
            tx,
            &receipt_id(agent_id, steer_id),
            &SteerPayload {
                agent: agent_id.into(),
                epoch,
                account_key: agent
                    .extra
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .into(),
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                events: ids,
                stage: SteerStage::Reserved,
                created,
                root_request_id: root.clone(),
                admission: admission.clone(),
                request_ticket: None,
                generation: None,
                submitted_at: None,
                prepared_submission: None,
                response: None,
                interrupt_still_queued: false,
            },
        )?;
        Ok(Some(ClaimedSteer {
            steer_id: steer_id.into(),
            root_request_id: root,
            admission,
            agent,
            thread_id,
            turn_id,
            events,
            metadata,
        }))
    }

    /// Retire resolved business notifications using the ordinary queue policy.
    /// A changed selection is released as a whole; it must be compiled again
    /// under a new exact batch identity before anything reaches the provider.
    pub fn reconcile_obsolete_steer_before_submit(
        tx: &Transaction<'_>,
        agent_id: &str,
        steer_id: &str,
    ) -> StoreResult<bool> {
        let id = receipt_id(agent_id, steer_id);
        let Some(receipt) = load(tx, &id)? else {
            return Ok(false);
        };
        if receipt.stage != SteerStage::Reserved {
            return Ok(false);
        }
        let Some(agent) = load_agent(tx, agent_id)? else {
            return Ok(false);
        };
        let ids = receipt_events(&id, &receipt)?;
        let mut statement = tx.prepare_cached(
            "SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events \
             WHERE agent=?1 AND id IN (SELECT value FROM json_each(?2)) ORDER BY created,rowid",
        )?;
        let rows = statement
            .query_map(
                params![agent_id, ids],
                crate::QueueEventRepository::from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let (complaints, _work) = load_policy_records(tx, &agent, &rows)?;
        let obsolete_work = obsolete_work_review_ids(tx, &agent, &rows)?;
        // Coalescing already ran at selection. Submission only retires resolved
        // business input; it must preserve the exact remaining metadata.
        let obsolete = obsolete_events(&agent, &rows, &complaints, &obsolete_work);
        if obsolete.is_empty() {
            return Ok(true);
        }
        release(tx, &id, &receipt)?;
        for (event, reason) in obsolete {
            tx.execute_cached(
                "UPDATE runtime_events SET status='stored_only',error=?1 WHERE id=?2 AND agent=?3 AND epoch=?4 AND status='pending'",
                params![reason,event,agent_id,agent.epoch],
            )?;
        }
        Ok(false)
    }

    /// Persist the uncertainty boundary before the steer request is written.
    /// A changed turn proves the request was never sent and releases the input.
    pub fn steer_submitted(
        tx: &Transaction<'_>,
        agent_id: &str,
        steer_id: &str,
        ticket: &str,
        generation: u64,
    ) -> StoreResult<bool> {
        let id = receipt_id(agent_id, steer_id);
        let Some(mut receipt) = load(tx, &id)? else {
            return Ok(false);
        };
        if receipt.stage != SteerStage::Reserved {
            return Ok(false);
        }
        let current = load_agent(tx, agent_id)?.is_some_and(|agent| {
            agent.epoch == receipt.epoch
                && agent.extra.get("accountKey").unwrap_or(&json!("default"))
                    == &json!(receipt.account_key)
                && steer_turn(&agent).is_some_and(|(turn, thread)| {
                    receipt.turn_id == turn && receipt.thread_id == thread
                })
        });
        if !current {
            release(tx, &id, &receipt)?;
            return Ok(false);
        }
        let events = receipt_events(&id, &receipt)?;
        let changed = tx.execute_cached(
            "UPDATE runtime_events SET status='dispatching' WHERE agent=?1 AND epoch=?2 \
             AND status='reserved' AND id IN (SELECT value FROM json_each(?3))",
            params![agent_id, receipt.epoch, events],
        )?;
        require_membership(changed, receipt.events.len(), agent_id)?;
        receipt.stage = SteerStage::Submitted;
        receipt.request_ticket = Some(ticket.into());
        receipt.generation = Some(generation);
        receipt.submitted_at = Some(timestamp());
        store(tx, &id, &receipt)?;
        Ok(true)
    }

    /// Freeze the compiler-owned projection with the durable steer receipt.
    /// Ordinary request receipts may be retired as soon as their reply arrives;
    /// this projection must survive until the independent input acknowledgement.
    pub fn freeze_steer_submission(
        tx: &Transaction<'_>,
        agent_id: &str,
        steer_id: &str,
        submission: &Value,
    ) -> StoreResult<()> {
        let id = receipt_id(agent_id, steer_id);
        let mut receipt = load(tx, &id)?.ok_or_else(|| invalid_receipt(&id))?;
        if receipt.stage != SteerStage::Submitted
            || !submission.is_object()
            || receipt
                .prepared_submission
                .as_ref()
                .is_some_and(|saved| saved != submission)
        {
            return Err(invalid_receipt(&id));
        }
        receipt.prepared_submission = Some(submission.clone());
        store(tx, &id, &receipt)
    }

    /// The transport proves it never wrote the request. Returns whether the
    /// agent's turn had already ended, so the agent was queued again to wake.
    pub fn steer_not_sent(
        tx: &Transaction<'_>,
        agent_id: &str,
        steer_id: &str,
    ) -> StoreResult<bool> {
        let id = receipt_id(agent_id, steer_id);
        match load(tx, &id)? {
            Some(receipt) => release(tx, &id, &receipt),
            None => Ok(false),
        }
    }

    /// The provider definitively refused the input. It returns to the queue once and
    /// waits for the next turn; this turn is not offered another steer.
    /// Returns whether the turn had already ended and the agent was queued again.
    pub fn steer_rejected(
        tx: &Transaction<'_>,
        agent_id: &str,
        steer_id: &str,
    ) -> StoreResult<bool> {
        let id = receipt_id(agent_id, steer_id);
        let Some(receipt) = load(tx, &id)? else {
            return Ok(false);
        };
        let requeued = release(tx, &id, &receipt)?;
        if let Some(mut agent) = load_agent(tx, agent_id)?
            && agent.epoch == receipt.epoch
            && agent.extra.get("turnId").and_then(Value::as_str) == Some(receipt.turn_id.as_str())
        {
            agent
                .extra
                .insert("steerRejectedTurnId".into(), json!(receipt.turn_id));
            publish_agent(tx, &agent)?;
        }
        Ok(requeued)
    }

    /// Record the provider's reply to the steer request. A reply is not a
    /// delivery receipt; only the acknowledgement notification is.
    pub fn steer_response(
        tx: &Transaction<'_>,
        agent_id: &str,
        steer_id: &str,
        response: &str,
    ) -> StoreResult<()> {
        let id = receipt_id(agent_id, steer_id);
        if let Some(mut receipt) = load(tx, &id)?
            && receipt.stage == SteerStage::Submitted
        {
            receipt.response = Some(response.into());
            store(tx, &id, &receipt)?;
        }
        Ok(())
    }

    /// Mark the exact acknowledged input delivered to its turn. An input already
    /// marked unconfirmed is corrected by a late acknowledgement.
    pub fn acknowledge_steer(
        tx: &Transaction<'_>,
        agent_id: &str,
        first_event: &str,
        thread_id: &str,
        turn_id: &str,
    ) -> StoreResult<Option<AcknowledgedSteer>> {
        let found = receipts(tx, agent_id)?.into_iter().find(|(_, receipt)| {
            receipt.agent == agent_id
                && receipt.events[0] == first_event
                && receipt.thread_id == thread_id
                && receipt.turn_id == turn_id
                && matches!(
                    receipt.stage,
                    SteerStage::Submitted | SteerStage::Unconfirmed
                )
        });
        let Some((id, receipt)) = found else {
            return Ok(None);
        };
        let epoch = receipt.epoch;
        let events = receipt_events(&id, &receipt)?;
        let changed = tx.execute_cached(
            "UPDATE runtime_events SET status='delivered',turn_id=?1,error=NULL \
             WHERE agent=?2 AND epoch=?3 AND id IN (SELECT value FROM json_each(?4)) \
             AND (status IN ('dispatching','uncertain') OR (status='failed' AND error=?5))",
            params![turn_id, agent_id, epoch, events, STEER_UNCONFIRMED_ERROR],
        )?;
        // Input a user already changed after it became unconfirmed keeps that change.
        delete(tx, &id)?;
        if changed == 0 {
            return Ok(None);
        }
        // `+status` keeps SQLite on the primary key for the acknowledged ids. A
        // plain `status='delivered'` lets the planner pick `runtime_event_queue`
        // and visit every delivered event of the state inside the writer.
        let mut statement = tx.prepare_cached(
            "SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events \
             WHERE id IN (SELECT value FROM json_each(?1)) AND +status='delivered' AND turn_id=?2 \
             ORDER BY created,rowid",
        )?;
        let delivered = statement
            .query_map(
                params![events, turn_id],
                crate::QueueEventRepository::from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let ids = delivered
            .iter()
            .map(|event| &event.id.0)
            .collect::<Vec<_>>();
        mark_timing(tx, &json!(ids).to_string(), "acceptedAt")?;
        // A steered agent message is delivered to its chat recipient, as at
        // turn acceptance.
        let delivered_ids = delivered
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>();
        Self::project_delivered_chat_events(tx, agent_id, epoch, turn_id, &delivered_ids)?;
        Ok(Some(AcknowledgedSteer {
            agent_id: agent_id.into(),
            admission: receipt.admission,
            steer_id: id
                .strip_prefix(&receipt_id(agent_id, ""))
                .ok_or_else(|| invalid_receipt(&id))?
                .to_owned(),
            event_ids: serde_json::from_str(&events).map_err(|_| invalid_receipt(&id))?,
            prepared_submission: receipt.prepared_submission.as_ref().cloned(),
            events: delivered,
            submitted_at: receipt.submitted_at.unwrap_or(receipt.created),
        }))
    }

    /// Settle an atomic Claude Stop receipt. Provider UUIDs have already been
    /// mapped to Studio identities by the bridge. Ignore unrelated identities
    /// and stale transport generations, including receipts after a newer turn.
    pub fn settle_interrupt_inputs(
        tx: &Transaction<'_>,
        agent_id: &str,
        account: &str,
        generation: u64,
        thread_id: &str,
        response: &Value,
    ) -> StoreResult<()> {
        let (Some(cancelled), Some(still_queued)) = (
            response["cancelled"].as_array(),
            response["still_queued"].as_array(),
        ) else {
            return Ok(());
        };
        if !cancelled.iter().chain(still_queued).all(Value::is_string) {
            return Ok(());
        }
        // The first prompt can still be in Claude's pending-dispatch window
        // even after turn/start was accepted. Refine that exact attempt too.
        if let Some(agent) = load_agent(tx, agent_id)? {
            let attempt = agent.extra.get("startAttempt").unwrap_or(&Value::Null);
            if attempt["accountKey"] == account
                && attempt["connectionGeneration"].as_u64() == Some(generation)
                && attempt["threadId"] == thread_id
                && let Some(events) = attempt["events"].as_array()
                && let Some(first) = events.first()
            {
                let status = if still_queued.contains(&json!(first)) {
                    Some("uncertain")
                } else if cancelled.contains(&json!(first)) {
                    Some("cancelled")
                } else {
                    None
                };
                if let Some(status) = status {
                    let epoch = attempt["epoch"]
                        .as_i64()
                        .ok_or_else(|| invalid_agent(agent_id))?;
                    let turn = attempt["turnId"].as_str().unwrap_or_default();
                    tx.execute_cached(
                        "UPDATE runtime_events SET status=?1,error=?2 WHERE agent=?3 AND epoch=?4 \
                         AND id IN (SELECT value FROM json_each(?5)) \
                         AND (status IN ('reserved','dispatching','uncertain') OR (status='delivered' AND turn_id=?6))",
                        params![status, if status == "cancelled" { STOP_CANCELLED_ERROR }
                            else { STOP_QUEUED_ERROR },
                            agent_id, epoch, attempt["events"].to_string(), turn],
                    )?;
                    let events: Vec<String> = serde_json::from_value(attempt["events"].clone())?;
                    Self::project_chat_event_status(tx, agent_id, epoch, turn, &events, status)?;
                }
            }
        }
        for (id, mut receipt) in receipts(tx, agent_id)? {
            if receipt.agent != agent_id
                || receipt.account_key != account
                || receipt.generation != Some(generation)
                || receipt.thread_id != thread_id
                || !matches!(
                    receipt.stage,
                    SteerStage::Submitted | SteerStage::Unconfirmed
                )
            {
                continue;
            }
            let first = &receipt.events[0];
            let status = if still_queued.contains(&json!(first)) {
                "uncertain"
            } else if cancelled.contains(&json!(first)) {
                "cancelled"
            } else {
                continue;
            };
            tx.execute_cached(
                "UPDATE runtime_events SET status=?1,error=?2 \
                 WHERE agent=?3 AND epoch=?4 AND id IN (SELECT value FROM json_each(?5)) \
                 AND (status IN ('dispatching','uncertain') OR (status='failed' AND error=?6))",
                params![
                    status,
                    if status == "cancelled" {
                        STOP_CANCELLED_ERROR
                    } else {
                        STOP_QUEUED_ERROR
                    },
                    agent_id,
                    receipt.epoch,
                    receipt_events(&id, &receipt)?,
                    STEER_UNCONFIRMED_ERROR
                ],
            )?;
            let event_ids: Vec<String> = serde_json::from_str(&receipt_events(&id, &receipt)?)?;
            Self::project_chat_event_status(
                tx,
                agent_id,
                receipt.epoch,
                receipt.turn_id.as_str(),
                &event_ids,
                status,
            )?;
            if status == "cancelled" {
                delete(tx, &id)?;
            } else {
                receipt.interrupt_still_queued = true;
                store(tx, &id, &receipt)?;
            }
        }
        Ok(())
    }

    /// Return the input of every steer to `turn_id` that Studio wrote after
    /// the provider stopped taking input into that turn, at `ended_at` (unix
    /// seconds, the provider's reported turn end). The caller passes it only
    /// for a provider that records every steer it took before that time and
    /// refuses a later one (`expectedTurnId`). `submittedAt` is written before
    /// the request, so such a steer was never applied, also when its reply was
    /// lost or a closed transport already made it unconfirmed. The input
    /// returns to the queue once; the request is never written again.
    pub fn release_steers_written_after_turn_end(
        tx: &Transaction<'_>,
        agent_id: &str,
        turn_id: &str,
        ended_at: f64,
    ) -> StoreResult<()> {
        if !ended_at.is_finite() {
            return Ok(());
        }
        for (id, receipt) in receipts(tx, agent_id)? {
            let written_after_end = receipt
                .submitted_at
                .is_some_and(|at| at >= ended_at + STEER_AFTER_TURN_END_SECONDS);
            if receipt.agent == agent_id
                && receipt.turn_id == turn_id
                && matches!(
                    receipt.stage,
                    SteerStage::Submitted | SteerStage::Unconfirmed
                )
                && !receipt.interrupt_still_queued
                && written_after_end
            {
                release(tx, &id, &receipt)?;
            }
        }
        Ok(())
    }

    /// Resolve every steer still open for a finished turn. Input the provider
    /// proves it never read returns to the queue; other input is unconfirmed.
    /// Unconfirmed receipts of the agent's earlier turns are dropped: their
    /// acknowledgement can no longer arrive, and the input stays visibly failed.
    /// Called by `settle_terminal`, which every terminal path goes through.
    pub fn resolve_steers_at_terminal(
        tx: &Transaction<'_>,
        agent_id: &str,
        turn_id: &str,
        unapplied: &[&str],
    ) -> StoreResult<()> {
        for (id, receipt) in receipts(tx, agent_id)? {
            if receipt.agent != agent_id {
                continue;
            }
            if receipt.turn_id != turn_id {
                if receipt.stage == SteerStage::Unconfirmed && !receipt.interrupt_still_queued {
                    delete(tx, &id)?;
                }
                continue;
            }
            let first = receipt
                .events
                .first()
                .map(String::as_str)
                .unwrap_or_default();
            match receipt.stage {
                SteerStage::Reserved => {
                    release(tx, &id, &receipt)?;
                }
                SteerStage::Submitted if unapplied.contains(&first) => {
                    release(tx, &id, &receipt)?;
                }
                SteerStage::Submitted if !receipt.interrupt_still_queued => {
                    unconfirm(tx, &id, receipt)?;
                }
                SteerStage::Submitted | SteerStage::Unconfirmed => {}
            }
        }
        Ok(())
    }

    /// A transport generation before `current` can no longer acknowledge input.
    /// Unsent reservations return to the queue; sent ones become unconfirmed.
    pub fn abandon_steers(tx: &Transaction<'_>, account: &str, current: u64) -> StoreResult<()> {
        let mut rows = Vec::new();
        for status in [
            OutboxStatus::Pending,
            OutboxStatus::Submitted,
            OutboxStatus::Unknown,
        ] {
            rows.extend(
                Outbox::for_account::<SteerPayload>(
                    tx,
                    account,
                    OutboxKind::Steer,
                    status,
                    u32::MAX,
                )
                .map_err(outbox_error)?,
            );
        }
        for row in rows {
            let (id, receipt) = (row.id, row.payload);
            if receipt.account_key != account {
                continue;
            }
            match receipt.stage {
                SteerStage::Reserved => {
                    release(tx, &id, &receipt)?;
                }
                SteerStage::Submitted if receipt.generation.is_none_or(|g| g < current) => {
                    unconfirm(tx, &id, receipt)?
                }
                SteerStage::Submitted | SteerStage::Unconfirmed => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
