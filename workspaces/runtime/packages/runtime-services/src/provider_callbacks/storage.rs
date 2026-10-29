//! Callback-generation identity fences and their single response obligation.
use super::model::*;
use crate::tool_requests::{self, CallbackTask};
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_providers::{Frame, ResponseBody, encode_line};
use studio_runtime_schema::persistence::{
    OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, ToolRequestStatus,
};
use studio_runtime_store::{
    RuntimeStore,
    outbox::{Outbox, OutboxRow},
    request_results::{Admission, AdmittedRequest, RequestResults},
    system_reads as records,
};

/// Retained until the source generation is fenced, independently of response TTL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallbackOwner {
    pub identity: ReplyIdentity,
    pub signature: Option<String>,
    pub card: Option<String>,
    pub tool_request: Option<String>,
    pub admission: Option<AdmittedRequest>,
    pub credential_refresh_admission: Option<AdmittedRequest>,
    pub reply_identity: Option<ReplyIdentity>,
    pub frame_fingerprint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackBody {
    Result(Value),
    Error(Value),
}
impl From<&ResponseBody> for CallbackBody {
    fn from(body: &ResponseBody) -> Self {
        match body {
            ResponseBody::Result(value) => Self::Result(value.clone()),
            ResponseBody::Error(value) => Self::Error(value.clone()),
        }
    }
}
impl From<CallbackBody> for ResponseBody {
    fn from(body: CallbackBody) -> Self {
        match body {
            CallbackBody::Result(value) => Self::Result(value),
            CallbackBody::Error(value) => Self::Error(value),
        }
    }
}

/// One canonical response body; retained-frame delivery uses this same row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallbackResponse {
    pub identity: ReplyIdentity,
    pub body: CallbackBody,
    pub card: Option<String>,
    pub tool_request: Option<String>,
    pub unanswered: Option<Value>,
    pub delete_requested_at: Option<f64>,
    pub frame_fingerprint: Option<String>,
}

impl From<CallbackResponse> for Reply {
    fn from(response: CallbackResponse) -> Self {
        Self {
            identity: response.identity,
            body: response.body.into(),
        }
    }
}

type Owner = CallbackTask<CallbackOwner>;
type Response = OutboxRow<CallbackResponse>;

pub(super) fn snapshot(
    tx: &Transaction<'_>,
    context: &CallbackContext,
) -> Result<CallbackSnapshot, CallbackError> {
    let raw: String = tx.query_row(
        "SELECT record FROM runtime_agents WHERE id=?1",
        [&context.agent],
        |row| row.get(0),
    )?;
    let raw = studio_runtime_store::agent_volatile::merged_text(tx, &context.agent, raw)?;
    CallbackSnapshot::from_record(&raw, context)
}
pub(super) fn save_actor(tx: &Transaction<'_>, actor: &Value) -> Result<(), CallbackError> {
    let id = actor["id"]
        .as_str()
        .ok_or(CallbackError::Stored("runtime_agents/id".into()))?;
    records::validate("runtime_agents", id, actor.clone())?;
    if !studio_runtime_store::agent_volatile::update(tx, id, actor)? {
        return Err(CallbackError::Stale);
    }
    crate::agents_create::publish_agent(tx, actor)?;
    Ok(())
}
pub(super) fn owner(
    tx: &Connection,
    identity: &ReplyIdentity,
) -> Result<Option<Owner>, CallbackError> {
    Ok(tool_requests::get_callback(tx, &identity.key())?)
}
pub(super) fn put_owner(tx: &Transaction<'_>, owner: &Owner) -> Result<(), CallbackError> {
    tool_requests::put_callback(tx, owner)?;
    Ok(())
}
pub(super) fn admit_owner(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    signature: &str,
) -> Result<Owner, CallbackError> {
    if let Some(owner) = owner(tx, identity)? {
        check_origin(&owner, identity, signature)?;
        return Ok(owner);
    }
    let id = RequestIdentity::issue_root();
    let now = chrono::Utc::now().timestamp_millis();
    let result_identity =
        RequestIdentity::new("provider_callback", &[&identity.key()], &id, &["response"])
            .map_err(|_| CallbackError::Invalid("Invalid callback request identity"))?;
    let Admission::Fresh(admission) = RequestResults::admit(tx, &result_identity, signature, now)?
    else {
        return Err(CallbackError::Conflict);
    };
    let mut origin = identity.clone();
    origin.attempt.clear();
    let owner = CallbackTask {
        id,
        origin_key: identity.key(),
        status: ToolRequestStatus::Queued,
        updated: now,
        payload: CallbackOwner {
            identity: origin,
            signature: Some(signature.into()),
            card: None,
            tool_request: None,
            admission: Some(admission),
            credential_refresh_admission: None,
            reply_identity: None,
            frame_fingerprint: None,
        },
    };
    put_owner(tx, &owner)?;
    Ok(owner)
}
fn check_origin(
    owner: &Owner,
    identity: &ReplyIdentity,
    signature: &str,
) -> Result<(), CallbackError> {
    if owner.origin_key != identity.key() || owner.payload.identity.key() != identity.key() {
        return Err(CallbackError::Stale);
    }
    if owner
        .payload
        .signature
        .as_deref()
        .is_some_and(|saved| saved != signature)
    {
        return Err(CallbackError::Conflict);
    }
    Ok(())
}
pub(super) fn response(
    tx: &Connection,
    identity: &ReplyIdentity,
) -> Result<Option<Response>, CallbackError> {
    Ok(Outbox::get(tx, &identity.key())?)
}
pub(super) fn put_response(tx: &Transaction<'_>, response: &Response) -> Result<(), CallbackError> {
    if serde_json::to_vec(&response.payload)?.len() > MAX_BYTES {
        return Err(CallbackError::Invalid(
            "Callback result exceeds durable reply bound",
        ));
    }
    Outbox::put(tx, response)?;
    Ok(())
}
pub(super) fn save_reply(
    tx: &Transaction<'_>,
    input: &Input,
    reply: &Reply,
) -> Result<(), CallbackError> {
    input.check_context(&reply.identity.context)?;
    let mut owner = admit_owner(tx, &reply.identity, &input.signature())?;
    if let Some(previous) = response(tx, &reply.identity)? {
        if previous.payload.identity != reply.identity
            || previous.payload.body != CallbackBody::from(&reply.body)
        {
            return Err(CallbackError::Conflict);
        }
        return Ok(());
    }
    if matches!(
        owner.status,
        ToolRequestStatus::Completed | ToolRequestStatus::Unknown
    ) {
        return Err(CallbackError::Unknown);
    }
    owner.status = ToolRequestStatus::Running;
    owner.payload.reply_identity = Some(reply.identity.clone());
    put_owner(tx, &owner)?;
    put_response(
        tx,
        &OutboxRow {
            id: reply.identity.key(),
            kind: OutboxKind::ProviderCallback,
            owner_kind: OwnerKind::Task,
            owner_id: owner.id.clone(),
            request_id: owner
                .payload
                .admission
                .as_ref()
                .map(|admission| admission.id().into()),
            signature: input.signature(),
            status: OutboxStatus::Pending,
            account: Some(reply.identity.context.account.clone()),
            stream_id: Some(reply.identity.context.stream_id.clone()),
            generation: Some(
                i64::try_from(reply.identity.context.generation)
                    .map_err(|_| CallbackError::Stale)?,
            ),
            ordinal: None,
            next_at: Some(0),
            created: owner.updated,
            payload: CallbackResponse {
                identity: reply.identity.clone(),
                body: CallbackBody::from(&reply.body),
                card: owner.payload.card,
                tool_request: None,
                unanswered: None,
                delete_requested_at: None,
                frame_fingerprint: None,
            },
        },
    )
}
pub(super) fn saved_reply(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    input: &Input,
) -> Result<Option<Reply>, CallbackError> {
    input.check_context(&identity.context)?;
    let Some(owner) = owner(tx, identity)? else {
        return Ok(None);
    };
    check_origin(&owner, identity, &input.signature())?;
    if owner.status == ToolRequestStatus::Completed {
        let admission = owner
            .payload
            .admission
            .as_ref()
            .ok_or(CallbackError::Unknown)?;
        let saved = RequestResults::read::<CallbackResponse>(
            tx,
            admission.id(),
            "provider_callback",
            &[&owner.origin_key],
            &owner.id,
            &["response"],
            chrono::Utc::now().timestamp_millis(),
        )?
        .ok_or(CallbackError::Unknown)?;
        if saved.signature != admission.signature() {
            return Err(CallbackError::Conflict);
        }
        return Ok(Some(saved.reply.into()));
    }
    let Some(mut saved) = response(tx, identity)? else {
        return Err(CallbackError::Unknown);
    };
    if saved.status == OutboxStatus::Pending {
        rebind_prepared(tx, &mut saved, &identity.context)?;
    }
    if saved.signature != input.signature() || saved.payload.identity.key() != identity.key() {
        return Err(CallbackError::Conflict);
    }
    match saved.status {
        OutboxStatus::Pending => Ok(Some(saved.payload.into())),
        OutboxStatus::Committed => Err(CallbackError::Stored(
            "completed callback still has a response obligation".into(),
        )),
        OutboxStatus::Submitted | OutboxStatus::Unknown | OutboxStatus::Unrecognized => {
            Err(CallbackError::Unknown)
        }
    }
}

pub async fn submission_ready(store: &RuntimeStore, reply: &Reply) -> Result<bool, CallbackError> {
    let reply = reply.clone();
    store
        .transaction_with(move |tx| {
            let owner = owner(tx, &reply.identity)?.ok_or(CallbackError::Unknown)?;
            if owner.status == ToolRequestStatus::Completed {
                return Ok(false);
            }
            let saved = response(tx, &reply.identity)?.ok_or(CallbackError::Unknown)?;
            if saved.payload.identity != reply.identity {
                return Err(CallbackError::Stale);
            }
            match saved.status {
                OutboxStatus::Committed => return Ok(false),
                OutboxStatus::Pending => {}
                OutboxStatus::Submitted | OutboxStatus::Unknown | OutboxStatus::Unrecognized => {
                    return Err(CallbackError::Unknown);
                }
            }
            if ResponseBody::from(saved.payload.body.clone()) != reply.body {
                return Err(CallbackError::Conflict);
            }
            if saved.payload.card.is_some() || saved.payload.tool_request.is_some() {
                snapshot(tx, &reply.identity.context)?.active()?;
                if let Some(id) = saved.payload.card {
                    let card =
                        records::get(tx, "runtime_requests", &id)?.ok_or(CallbackError::Stale)?;
                    if super::cards::status(&card)?
                        != studio_runtime_schema::persistence::RequestCardStatus::Answering
                        || studio_runtime_store::truthy(card.get("deletedAt"))
                    {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        })
        .await
}
#[derive(Clone, Copy, Debug)]
pub enum DeliveryOutcome {
    Submitted,
    Written,
    NotSent,
    Unknown,
}

pub async fn delivery(
    store: &RuntimeStore,
    identity: &ReplyIdentity,
    outcome: DeliveryOutcome,
) -> Result<bool, CallbackError> {
    let identity = identity.clone();
    store
        .transaction_with(move |tx| delivery_in(tx, &identity, outcome))
        .await
}
fn delivery_in(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    outcome: DeliveryOutcome,
) -> Result<bool, CallbackError> {
    let mut owner = owner(tx, identity)?.ok_or(CallbackError::Unknown)?;
    if owner.status == ToolRequestStatus::Completed {
        if matches!(outcome, DeliveryOutcome::Written) && owner.payload.frame_fingerprint.is_none()
        {
            return Err(CallbackError::Unknown);
        }
        return Ok(false);
    }
    let Some(mut saved) = response(tx, identity)? else {
        if matches!(outcome, DeliveryOutcome::NotSent) && owner.status == ToolRequestStatus::Queued
        {
            return Ok(false);
        }
        return Err(CallbackError::Unknown);
    };
    if saved.payload.identity != *identity {
        if matches!(outcome, DeliveryOutcome::NotSent) && saved.status == OutboxStatus::Pending {
            return Ok(false);
        }
        return Err(CallbackError::Stale);
    }
    if matches!(outcome, DeliveryOutcome::Unknown | DeliveryOutcome::NotSent)
        && saved.status == OutboxStatus::Unknown
    {
        return Ok(false);
    }
    if matches!(outcome, DeliveryOutcome::Submitted)
        && (saved.payload.card.is_some() || saved.payload.tool_request.is_some())
    {
        snapshot(tx, &identity.context)?.active()?;
    }
    saved.status = match (saved.status, outcome) {
        (OutboxStatus::Pending, DeliveryOutcome::Submitted) => OutboxStatus::Submitted,
        (OutboxStatus::Submitted | OutboxStatus::Unknown, DeliveryOutcome::Written) => {
            OutboxStatus::Committed
        }
        (OutboxStatus::Submitted, DeliveryOutcome::NotSent) => OutboxStatus::Pending,
        (OutboxStatus::Submitted, DeliveryOutcome::Unknown) => OutboxStatus::Unknown,
        _ => return Err(CallbackError::Unknown),
    };
    saved.next_at = (saved.status == OutboxStatus::Pending).then_some(0);
    if saved.status == OutboxStatus::Pending {
        saved.payload.frame_fingerprint = None;
        if let Some(unanswered) = &saved.payload.unanswered {
            super::cards::save(tx, unanswered)?;
            owner.status = ToolRequestStatus::Queued;
            owner.payload.reply_identity = None;
            put_owner(tx, &owner)?;
            Outbox::delete_settled(tx, &saved.id, &saved.signature)?;
            return Ok(true);
        }
        saved.payload.identity =
            Reply::new(identity.clone(), saved.payload.body.clone().into())?.identity;
        owner.payload.reply_identity = Some(saved.payload.identity.clone());
    }
    if saved.status == OutboxStatus::Unknown {
        owner.status = ToolRequestStatus::Unknown;
    }
    if saved.status == OutboxStatus::Committed {
        owner.status = ToolRequestStatus::Completed;
        owner.payload.frame_fingerprint = Some(studio_runtime_store::sha256_hex(
            &encode_line(&Reply::from(saved.payload.clone()).frame())
                .map_err(|_| CallbackError::Invalid("Invalid callback response frame"))?,
        ));
        let admission = owner
            .payload
            .admission
            .as_ref()
            .ok_or(CallbackError::Unknown)?;
        crate::event_write::settle_request(
            tx,
            admission,
            Some(&identity.context.agent),
            &saved.payload,
        )?;
    }
    if let Some(card_id) = &saved.payload.card {
        let mut card =
            records::get(tx, "runtime_requests", card_id)?.ok_or(CallbackError::Stale)?;
        use studio_runtime_schema::persistence::RequestCardStatus;
        card["status"] = json!(if saved.status == OutboxStatus::Committed {
            RequestCardStatus::Answered
        } else if saved.status == OutboxStatus::Unknown {
            RequestCardStatus::Uncertain
        } else {
            RequestCardStatus::Answering
        });
        if saved.status == OutboxStatus::Unknown {
            card["answerError"] = json!(
                "Answer delivery could not be confirmed. Inspect the agent conversation before taking further action."
            );
        }
        if saved.status == OutboxStatus::Committed {
            if let Some(at) = saved.payload.delete_requested_at {
                card["status"] = json!(RequestCardStatus::Deleted);
                card["deletedAt"] = json!(at);
                card["deletedBy"] = json!("user");
                card["deferred"] = json!(false);
            }
            let mut actor = records::get(tx, "runtime_agents", &identity.context.agent)?
                .ok_or(CallbackError::Stale)?;
            if CallbackSnapshot::from_record(&actor.to_string(), &identity.context)
                .and_then(|snapshot| snapshot.active())
                .is_ok()
                && matches!(actor["status"].as_str(), Some("approval" | "running"))
            {
                actor["status"] = json!("running");
                save_actor(tx, &actor)?;
            }
        }
        super::cards::save(tx, &card)?;
    }
    put_owner(tx, &owner)?;
    if saved.status == OutboxStatus::Committed {
        Outbox::delete_settled(tx, &saved.id, &saved.signature)?;
    } else {
        put_response(tx, &saved)?;
    }
    Ok(true)
}

pub(super) fn saved_decision(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    input: &Input,
) -> Result<Option<CallbackDecision>, CallbackError> {
    input.check_context(&identity.context)?;
    let Some(owner) = owner(tx, identity)? else {
        return Ok(None);
    };
    check_origin(&owner, identity, &input.signature())?;
    if owner.status == ToolRequestStatus::Completed {
        return Ok(Some(CallbackDecision::Settled(
            owner
                .payload
                .reply_identity
                .unwrap_or_else(|| identity.clone()),
        )));
    }
    if owner.status == ToolRequestStatus::Queued
        && owner.payload.card.is_none()
        && owner.payload.reply_identity.is_none()
    {
        return Ok(None);
    }
    if let Some(card) = owner.payload.card.as_ref()
        && response(tx, identity)?.is_none()
    {
        return if owner.status == ToolRequestStatus::Queued {
            Ok(Some(CallbackDecision::Pending(card.clone())))
        } else {
            Err(CallbackError::Unknown)
        };
    }
    saved_reply(tx, identity, input).map(|reply| reply.map(CallbackDecision::Answer))
}

pub async fn prepared_replies(
    store: &RuntimeStore,
    context: &CallbackContext,
) -> Result<Vec<Reply>, CallbackError> {
    let context = context.clone();
    store
        .transaction_with(move |tx| {
            snapshot(tx, &context)?.active()?;
            let rows = Outbox::for_account::<CallbackResponse>(
                tx,
                &context.account,
                OutboxKind::ProviderCallback,
                OutboxStatus::Pending,
                MAX_PENDING as u32,
            )?;
            let mut replies = Vec::new();
            for mut row in rows {
                if !row.payload.identity.key_context_matches(&context) {
                    continue;
                }
                rebind_prepared(tx, &mut row, &context)?;
                replies.push(row.payload.into());
            }
            Ok(replies)
        })
        .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameAdmission {
    Send,
    Written,
    Unknown,
}

/// Atomically fence the exact encoded frame before the retained transport sees it.
pub fn begin_frame(
    tx: &Transaction<'_>,
    stream: &str,
    generation: u64,
    operation: &str,
    frame: &Frame,
) -> Result<FrameAdmission, CallbackError> {
    let (key, _) = operation.rsplit_once(':').ok_or(CallbackError::Stale)?;
    let fingerprint = studio_runtime_store::sha256_hex(
        &encode_line(frame)
            .map_err(|_| CallbackError::Invalid("Invalid callback response frame"))?,
    );
    let Some(mut row): Option<Response> = Outbox::get(tx, key)? else {
        let owner: Option<Owner> = tool_requests::get_callback(tx, key)?;
        let Some(owner) = owner else {
            return Err(CallbackError::Unknown);
        };
        let Some(identity) = owner.payload.reply_identity else {
            return Err(CallbackError::Unknown);
        };
        if identity.operation_id() != operation
            || identity.context.stream_id != stream
            || identity.context.generation != generation
        {
            return Err(CallbackError::Stale);
        }
        if owner.payload.frame_fingerprint.as_deref() != Some(&fingerprint) {
            return Err(CallbackError::Conflict);
        }
        return Ok(if owner.status == ToolRequestStatus::Completed {
            FrameAdmission::Written
        } else {
            FrameAdmission::Unknown
        });
    };
    if row.payload.identity.operation_id() != operation
        || row.stream_id.as_deref() != Some(stream)
        || row.generation != i64::try_from(generation).ok()
    {
        return Err(CallbackError::Stale);
    }
    let reply = Reply::from(row.payload.clone());
    if reply.frame() != *frame {
        return Err(CallbackError::Conflict);
    }
    if let Some(saved) = &row.payload.frame_fingerprint {
        if *saved != fingerprint {
            return Err(CallbackError::Conflict);
        }
        return Ok(FrameAdmission::Unknown);
    }
    if !matches!(row.status, OutboxStatus::Pending | OutboxStatus::Submitted) {
        return Ok(FrameAdmission::Unknown);
    }
    row.payload.frame_fingerprint = Some(fingerprint);
    row.status = OutboxStatus::Submitted;
    row.next_at = None;
    put_response(tx, &row)?;
    Ok(FrameAdmission::Send)
}
pub fn record_frame_outcome(
    tx: &Transaction<'_>,
    operation: &str,
    outcome: DeliveryOutcome,
) -> Result<(), CallbackError> {
    let (key, _) = operation.rsplit_once(':').ok_or(CallbackError::Stale)?;
    let row: Response = Outbox::get(tx, key)?.ok_or(CallbackError::Unknown)?;
    if row.payload.identity.operation_id() != operation {
        return Err(CallbackError::Stale);
    }
    delivery_in(tx, &row.payload.identity, outcome)?;
    Ok(())
}

pub(super) fn hold_card(tx: &Transaction<'_>, card: &str) -> Result<(), CallbackError> {
    let mut owners = tx.prepare_cached("SELECT record FROM runtime_tool_requests WHERE kind=?1 AND status IN (?2,?3) AND json_extract(record,'$.payload.card')=?4")?;
    let owners = owners
        .query_map(
            params![
                studio_runtime_schema::persistence::ToolRequestKind::ProviderCallback,
                ToolRequestStatus::Queued,
                ToolRequestStatus::Running,
                card
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for raw in owners {
        let mut owner: Owner = serde_json::from_str(&raw)?;
        owner.status = ToolRequestStatus::Unknown;
        if let Some(mut saved) = response(tx, &owner.payload.identity)? {
            saved.status = OutboxStatus::Unknown;
            saved.next_at = None;
            put_response(tx, &saved)?;
        }
        put_owner(tx, &owner)?;
    }
    Ok(())
}
pub(super) fn resolve_owner(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    signature: Option<&str>,
) -> Result<(), CallbackError> {
    let mut owner = if let Some(owner) = owner(tx, identity)? {
        if let Some(signature) = signature {
            check_origin(&owner, identity, signature)?;
        }
        owner
    } else {
        CallbackTask {
            id: RequestIdentity::issue_root(),
            origin_key: identity.key(),
            status: ToolRequestStatus::Completed,
            updated: chrono::Utc::now().timestamp_millis(),
            payload: CallbackOwner {
                identity: identity.clone(),
                signature: signature.map(str::to_owned),
                card: None,
                tool_request: None,
                admission: None,
                credential_refresh_admission: None,
                reply_identity: None,
                frame_fingerprint: None,
            },
        }
    };
    owner.status = ToolRequestStatus::Completed;
    if owner.payload.signature.is_none() {
        owner.payload.signature = signature.map(str::to_owned);
    }
    put_owner(tx, &owner)?;
    if let Some(saved) = response(tx, identity)? {
        Outbox::delete_settled(tx, &saved.id, &saved.signature)?;
    }
    Ok(())
}

pub fn callback_origin_key(
    context: &CallbackContext,
    rpc_id: &studio_runtime_providers::RequestId,
) -> String {
    ReplyIdentity::new(context, rpc_id).key()
}
pub fn response_frame(tx: &Connection, key: &str) -> Result<Option<Frame>, CallbackError> {
    let saved: Option<Response> = Outbox::get(tx, key)?;
    Ok(saved.map(|row| Reply::from(row.payload).frame()))
}
pub fn save_frame_response(
    tx: &Transaction<'_>,
    context: &CallbackContext,
    input_frame: &Frame,
    response_frame: &Frame,
) -> Result<ReplyIdentity, CallbackError> {
    let input = Input::from_frame(input_frame)?
        .ok_or(CallbackError::Invalid("Expected callback request frame"))?;
    if let Some(saved) = response(tx, &ReplyIdentity::new(context, &input.id))? {
        let reply = Reply::from(saved.payload);
        if reply.frame() != *response_frame {
            return Err(CallbackError::Conflict);
        }
        return Ok(reply.identity);
    }
    let Frame::Response { id, body, .. } = response_frame else {
        return Err(CallbackError::Invalid("Expected callback response frame"));
    };
    if *id != input.id {
        return Err(CallbackError::Stale);
    }
    let reply = Reply::new(ReplyIdentity::new(context, id), body.clone())?;
    save_reply(tx, &input, &reply)?;
    Ok(reply.identity)
}

#[cfg(test)]
pub(crate) fn seed_card_owner(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    card: &Value,
) -> Result<(), CallbackError> {
    let input = Input {
        id: identity.rpc_id.clone(),
        method: card["method"].as_str().ok_or(CallbackError::Stale)?.into(),
        params: card["params"].clone(),
    };
    let mut owner = admit_owner(tx, identity, &input.signature())?;
    owner.payload.card = card["id"].as_str().map(str::to_owned);
    put_owner(tx, &owner)
}

/// Settle the canonical response from an accepted-callback handoff owner.
pub fn settle_response(
    tx: &Transaction<'_>,
    key: &str,
    outcome: DeliveryOutcome,
) -> Result<(), CallbackError> {
    let row: Response = Outbox::get(tx, key)?.ok_or(CallbackError::Unknown)?;
    delivery_in(tx, &row.payload.identity, outcome)?;
    Ok(())
}

/// Accepted logical callbacks still owed by this account, counted once even
/// after their response enters the delivery outbox.
pub fn pending_callback_count(
    db: &Connection,
    account: &str,
) -> studio_runtime_store::StoreResult<usize> {
    let count:i64 = db.query_row("SELECT count(*) FROM runtime_tool_requests WHERE kind=?1 AND status IN (?2,?3,?4) AND json_extract(record,'$.payload.accepted.account')=?5",
        params![studio_runtime_schema::persistence::ToolRequestKind::ProviderCallback,ToolRequestStatus::Queued,ToolRequestStatus::Running,ToolRequestStatus::Unknown,account],|row|row.get(0))?;
    usize::try_from(count).map_err(|_| {
        studio_runtime_store::StoreError::RecordContract("callback pending count exceeds usize")
    })
}

/// Allocate a provider callback's Studio root exactly once, before any effect.
pub fn admit_callback_request(
    tx: &Transaction<'_>,
    context: &CallbackContext,
    frame: &Frame,
) -> Result<String, CallbackError> {
    let input = Input::from_frame(frame)?
        .ok_or(CallbackError::Invalid("Expected callback request frame"))?;
    input.check_context(context)?;
    Ok(admit_owner(
        tx,
        &ReplyIdentity::new(context, &input.id),
        &input.signature(),
    )?
    .id)
}
fn rebind_prepared(
    tx: &Transaction<'_>,
    row: &mut Response,
    context: &CallbackContext,
) -> Result<(), CallbackError> {
    if row.payload.identity.context.connection == context.connection {
        return Ok(());
    }
    if !row.payload.identity.key_context_matches(context) {
        return Err(CallbackError::Stale);
    }
    if !context.agent.is_empty() {
        snapshot(tx, context)?.active()?;
    }
    row.payload
        .identity
        .context
        .connection
        .clone_from(&context.connection);
    let mut owner = owner(tx, &row.payload.identity)?.ok_or(CallbackError::Unknown)?;
    owner.payload.reply_identity = Some(row.payload.identity.clone());
    put_owner(tx, &owner)?;
    put_response(tx, row)
}

/// Trusted callback task lookup; never creates admission on a missing owner.
pub fn callback_admission(
    db: &Connection,
    root: &str,
) -> Result<Option<AdmittedRequest>, CallbackError> {
    Ok(owner_by_root(db, root)?.and_then(|task| task.payload.admission))
}

fn owner_by_root(db: &Connection, root: &str) -> Result<Option<Owner>, CallbackError> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM runtime_tool_requests WHERE id=?1 AND kind=?2",
            params![
                root,
                studio_runtime_schema::persistence::ToolRequestKind::ProviderCallback
            ],
            |row| row.get(0),
        )
        .optional()?;
    let task: Option<Owner> = raw.map(|raw| serde_json::from_str(&raw)).transpose()?;
    Ok(task)
}

/// First dispatch reserves and points at the effect atomically. Retries return
/// the persisted final pointer and never call the reservation API again.
pub fn reserve_credential_refresh(
    tx: &Transaction<'_>,
    root: &str,
    source: &AdmittedRequest,
    intent: crate::account_worker::retained::ProviderCallIntent,
) -> Result<AdmittedRequest, CallbackError> {
    let mut owner = owner_by_root(tx, root)?.ok_or(CallbackError::Unknown)?;
    if owner.payload.admission.as_ref() != Some(source)
        || intent.root_request_id != root
        || intent.scope.account != owner.payload.identity.context.account
    {
        return Err(CallbackError::Conflict);
    }
    if let Some(final_effect) = owner.payload.credential_refresh_admission {
        if final_effect.signature()
            != intent.fingerprint(&intent.scope.account, intent.method.as_str())
        {
            return Err(CallbackError::Conflict);
        }
        return Ok(final_effect);
    }
    if owner.status != ToolRequestStatus::Queued {
        return Err(CallbackError::Unknown);
    }
    let final_effect = crate::account_worker::retained::reserve_provider_call(tx, source, intent)
        .map_err(|error| match error {
        crate::account_worker::WorkerError::Expired => CallbackError::Expired,
        crate::account_worker::WorkerError::Store(error) => CallbackError::Store(error),
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
        | crate::account_worker::WorkerError::CallbackUnknown => CallbackError::Unknown,
    })?;
    owner.payload.credential_refresh_admission = Some(final_effect.clone());
    put_owner(tx, &owner)?;
    Ok(final_effect)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let db = Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("receipt schema");
        db.execute_batch("CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL,kind TEXT GENERATED ALWAYS AS (json_extract(record,'$.kind')) VIRTUAL,origin_key TEXT GENERATED ALWAYS AS (json_extract(record,'$.originKey')) VIRTUAL,status TEXT GENERATED ALWAYS AS (coalesce(json_extract(record,'$.status'),json_extract(record,'$.stage'))) VIRTUAL,agent TEXT GENERATED ALWAYS AS (coalesce(json_extract(record,'$.agent'),json_extract(record,'$.payload.identity.context.agent'))) VIRTUAL); CREATE INDEX runtime_tool_request_current_status ON runtime_tool_requests(status,agent,id); CREATE UNIQUE INDEX callback_origin ON runtime_tool_requests(origin_key) WHERE origin_key IS NOT NULL; CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL);") .expect("owner schema");
        db
    }
    fn input() -> (ReplyIdentity, Input) {
        let context = CallbackContext {
            agent: "agent".into(),
            account: "account".into(),
            stream_id: "account".into(),
            connection: "attachment".into(),
            generation: 4,
            epoch: 1,
            thread: "thread".into(),
            turn: "turn".into(),
        };
        let input = Input {
            id: studio_runtime_providers::RequestId::String("opaque".into()),
            method: "currentTime/read".into(),
            params: json!({}),
        };
        (ReplyIdentity::new(&context, &input.id), input)
    }
    #[test]
    fn callback_response_retirement_keeps_origin_and_expired_body_held() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let (identity, input) = input();
        admit_owner(&tx, &identity, &input.signature()).expect("origin before effects");
        assert!(
            saved_decision(&tx, &identity, &input)
                .expect("first admission")
                .is_none()
        );
        let reply = Reply::new(
            identity.clone(),
            ResponseBody::Result(json!({"original":true})),
        )
        .expect("reply");
        save_reply(&tx, &input, &reply).expect("admit response");
        assert_eq!(
            saved_reply(&tx, &identity, &input).expect("exact response"),
            Some(reply.clone())
        );
        delivery_in(&tx, &reply.identity, DeliveryOutcome::Submitted).expect("submit");
        delivery_in(&tx, &reply.identity, DeliveryOutcome::Written).expect("settle");
        let owner = owner(&tx, &identity)
            .expect("owner")
            .expect("retained owner");
        assert_eq!(owner.status, ToolRequestStatus::Completed);
        let expiry = owner.payload.admission.as_ref().unwrap().created()
            + studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS;
        assert_eq!(
            RequestResults::gc(&tx, expiry).expect("expire cached response"),
            1
        );
        Outbox::delete_settled(&tx, &identity.key(), &input.signature()).expect("retire transport");
        assert!(matches!(
            saved_reply(&tx, &identity, &input),
            Err(CallbackError::Unknown)
        ));
        assert!(matches!(
            saved_decision(&tx, &identity, &input).expect("fence"),
            Some(CallbackDecision::Settled(_))
        ));
        assert!(matches!(
            save_reply(&tx, &input, &reply),
            Err(CallbackError::Unknown)
        ));
        let changed = Input {
            params: json!({"different":true}),
            ..input
        };
        assert!(matches!(
            saved_decision(&tx, &identity, &changed),
            Err(CallbackError::Conflict)
        ));
    }
    #[test]
    fn native_written_keeps_exact_frame_fingerprint_without_retained_transport() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let (identity, input) = input();
        let reply = Reply::new(
            identity.clone(),
            ResponseBody::Result(json!({"native":true})),
        )
        .expect("reply");
        save_reply(&tx, &input, &reply).expect("response");
        delivery_in(&tx, &reply.identity, DeliveryOutcome::Submitted).expect("native handoff");
        delivery_in(&tx, &reply.identity, DeliveryOutcome::Written).expect("written proof");
        assert!(response(&tx, &identity).expect("outbox").is_none());
        assert_eq!(
            begin_frame(
                &tx,
                "account",
                4,
                &reply.identity.operation_id(),
                &reply.frame()
            )
            .expect("same exact frame"),
            FrameAdmission::Written
        );
        let mut changed = reply;
        changed.body = ResponseBody::Result(json!({"native":false}));
        assert!(matches!(
            begin_frame(
                &tx,
                "account",
                4,
                &changed.identity.operation_id(),
                &changed.frame()
            ),
            Err(CallbackError::Conflict)
        ));
    }

    #[test]
    fn late_written_callback_retires_body_after_generation_and_ack_replacement() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let (identity, input) = input();
        let reply = Reply::new(
            identity.clone(),
            ResponseBody::Result(json!({"original":true})),
        )
        .expect("reply");
        save_reply(&tx, &input, &reply).expect("response obligation");
        let operation = reply.identity.operation_id();
        assert_eq!(
            begin_frame(&tx, "account", 4, &operation, &reply.frame()).expect("submit"),
            FrameAdmission::Send
        );
        record_frame_outcome(&tx, &operation, DeliveryOutcome::Unknown).expect("lost outcome");
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,connection_generation,connection_id,journal_ack,recovery_required,streams) VALUES('account',8,8,'replacement',0,0,'null')",[]).expect("old ACK and attachment replaced");
        record_frame_outcome(&tx, &operation, DeliveryOutcome::Written)
            .expect("exact late written proof");
        assert!(response(&tx, &identity).expect("retired body").is_none());
        let owner = owner(&tx, &identity)
            .expect("owner")
            .expect("generation fence");
        assert_eq!(owner.status, ToolRequestStatus::Completed);
        let token = owner.payload.admission.unwrap();
        assert!(
            RequestResults::replay(
                &tx,
                token.id(),
                token.signature(),
                chrono::Utc::now().timestamp_millis()
            )
            .expect("exact final result")
            .is_some()
        );
        assert_eq!(
            begin_frame(&tx, "account", 4, &operation, &reply.frame()).expect("no resend"),
            FrameAdmission::Written
        );
    }

    #[test]
    fn unknown_callback_body_survives_expiry_and_generation_replacement() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let (identity, input) = input();
        let reply = Reply::new(
            identity.clone(),
            ResponseBody::Result(json!({"unresolved":true})),
        )
        .expect("reply");
        save_reply(&tx, &input, &reply).expect("response obligation");
        let operation = reply.identity.operation_id();
        assert_eq!(
            begin_frame(&tx, "account", 4, &operation, &reply.frame()).expect("submit"),
            FrameAdmission::Send
        );
        record_frame_outcome(&tx, &operation, DeliveryOutcome::Unknown).expect("hold uncertainty");
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,connection_generation,connection_id,journal_ack,recovery_required,streams) VALUES('account',8,8,'replacement',0,0,'null')",[]).expect("replacement generation");
        let owner = owner(&tx, &identity)
            .expect("owner")
            .expect("unknown source");
        let expiry = owner.payload.admission.unwrap().created()
            + studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS;
        RequestResults::gc(&tx, expiry).expect("cache collection");
        let retained = response(&tx, &identity)
            .expect("outbox")
            .expect("unknown survives");
        assert_eq!(retained.status, OutboxStatus::Unknown);
        assert_eq!(retained.next_at, None);
        assert_eq!(retained.payload.body, CallbackBody::from(&reply.body));
        assert_eq!(
            begin_frame(&tx, "account", 4, &operation, &reply.frame()).expect("no resend"),
            FrameAdmission::Unknown
        );
    }

    #[test]
    fn callback_pending_count_uses_status_index_and_excludes_completed_tasks() {
        #[derive(Serialize)]
        struct AcceptedAccount<'a> {
            account: &'a str,
        }
        #[derive(Serialize)]
        struct Obligation<'a> {
            accepted: AcceptedAccount<'a>,
        }
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        for (id, status) in [
            ("pending", ToolRequestStatus::Queued),
            ("completed", ToolRequestStatus::Completed),
        ] {
            tool_requests::put_callback(
                &tx,
                &CallbackTask {
                    id: id.into(),
                    origin_key: id.into(),
                    status,
                    updated: 1,
                    payload: Obligation {
                        accepted: AcceptedAccount { account: "account" },
                    },
                },
            )
            .expect("callback obligation");
        }
        assert_eq!(
            pending_callback_count(&tx, "account").expect("pending count"),
            1
        );
        let detail:String=tx.query_row("EXPLAIN QUERY PLAN SELECT count(*) FROM runtime_tool_requests WHERE kind=?1 AND status IN (?2,?3,?4) AND json_extract(record,'$.payload.accepted.account')=?5",params![studio_runtime_schema::persistence::ToolRequestKind::ProviderCallback,ToolRequestStatus::Queued,ToolRequestStatus::Running,ToolRequestStatus::Unknown,"account"],|row|row.get(3)).expect("query plan");
        assert!(
            detail.contains("runtime_tool_request_current_status"),
            "{detail}"
        );
    }

    #[test]
    fn callback_credential_refresh_reuses_final_pointer_after_response_loss() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let (identity, input) = input();
        let owner = admit_owner(&tx, &identity, &input.signature()).expect("callback owner");
        let source = owner.payload.admission.expect("source token");
        let intent = crate::account_worker::retained::ProviderCallIntent {
            root_request_id: owner.id.clone(),
            admitted_request: None,
            scope: crate::recovery_history::ConnectionScope {
                account: "account".into(),
                connection: "host-attachment".into(),
                generation: 9,
            },
            operation: format!("vm-codex-token-refresh:account:{}", owner.id),
            actor: None,
            transfer: None,
            deleted_actor_session: None,
            method: studio_runtime_providers::Method::AccountRead,
            params: json!({"refreshToken":true}),
        };
        let final_effect = reserve_credential_refresh(&tx, &owner.id, &source, intent.clone())
            .expect("reserve once");
        assert_ne!(final_effect, source);
        assert_eq!(
            callback_admission(&tx, &owner.id).expect("source unchanged"),
            Some(source.clone())
        );
        assert_eq!(
            owner_by_root(&tx, &owner.id)
                .expect("owner")
                .unwrap()
                .payload
                .credential_refresh_admission,
            Some(final_effect.clone())
        );
        tx.execute("DELETE FROM runtime_outbox", [])
            .expect("completed transport reply retired");
        assert_eq!(
            reserve_credential_refresh(&tx, &owner.id, &source, intent.clone())
                .expect("retry saved pointer"),
            final_effect
        );
        let count: i64 = tx
            .query_row("SELECT count(*) FROM runtime_outbox", [], |row| row.get(0))
            .expect("no new effect");
        assert_eq!(count, 0);
        let mut changed = intent;
        changed.params = json!({"refreshToken":false});
        assert!(matches!(
            reserve_credential_refresh(&tx, &owner.id, &source, changed),
            Err(CallbackError::Conflict)
        ));
        assert_eq!(
            owner_by_root(&tx, &owner.id)
                .expect("owner")
                .unwrap()
                .payload
                .credential_refresh_admission,
            Some(final_effect)
        );
    }

    #[test]
    fn retained_frame_uses_callback_outbox_and_only_exact_late_proof_settles_unknown() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let (identity, input) = input();
        let reply = Reply::new(identity.clone(), ResponseBody::Result(json!({"once":true})))
            .expect("reply");
        save_reply(&tx, &input, &reply).expect("admit response");
        let operation = reply.identity.operation_id();
        assert_eq!(
            begin_frame(&tx, "account", 4, &operation, &reply.frame()).expect("first transport"),
            FrameAdmission::Send
        );
        assert_eq!(
            begin_frame(&tx, "account", 4, &operation, &reply.frame())
                .expect("lost transport response"),
            FrameAdmission::Unknown
        );
        record_frame_outcome(&tx, &operation, DeliveryOutcome::Unknown).expect("hold uncertainty");
        assert!(matches!(
            saved_reply(&tx, &identity, &input),
            Err(CallbackError::Unknown)
        ));
        assert!(begin_frame(&tx, "account", 5, &operation, &reply.frame()).is_err());
        let mut late = reply.clone();
        late.identity.attempt = "other-attempt".into();
        assert!(
            record_frame_outcome(&tx, &late.identity.operation_id(), DeliveryOutcome::Written)
                .is_err()
        );
        record_frame_outcome(&tx, &operation, DeliveryOutcome::Written).expect("exact late proof");
        assert_eq!(
            begin_frame(&tx, "account", 4, &operation, &reply.frame()).expect("written proof"),
            FrameAdmission::Written
        );
        let count: i64 = tx
            .query_row("SELECT count(*) FROM runtime_outbox", [], |row| row.get(0))
            .expect("written response retired atomically");
        assert_eq!(count, 0);
    }
}
