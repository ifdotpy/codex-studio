//! Stored room membership, team checks and idempotent Canvas chat writes.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use studio_runtime_schema::delivery::DeliveryMoment;
use studio_runtime_store::{
    CoordinationRepository, QueueEventId, QueueEventRepository, QueueWriteRepository, StoreError,
    Transaction,
};
use thiserror::Error;

pub fn message_text_within_limit(text: &str) -> bool {
    text.chars().count() <= studio_runtime_schema::shared_values::MAX_MANAGED_MESSAGE_CODEPOINTS
}

#[derive(Debug, Error)]
pub enum CoordinationError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("Not implemented: requires {0}")]
    RequiresDependency(&'static str),
    #[error("invalid stored object {object}/{id}")]
    InvalidStored { object: &'static str, id: String },
    #[error("{0}")]
    AdmissionDenied(String),
    #[error(transparent)]
    BudgetRuntime(#[from] crate::budget_runtime::BudgetRuntimeError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn history(
    connection: &studio_runtime_store::Connection,
    room: &str,
) -> Result<Value, CoordinationError> {
    let rows = CoordinationRepository::messages(connection, room)?;
    rows.into_iter()
        .map(|row| validated_message(row).map(message_json))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

pub fn create_chat(
    transaction: &Transaction<'_>,
    id: &str,
    name: &str,
    members: &[String],
) -> Result<Value, CoordinationError> {
    let name = name.trim();
    if !(1..=100).contains(&name.chars().count()) {
        return Err(CoordinationError::InvalidRequest(
            "Use a chat name with 1 to 100 characters.",
        ));
    }
    let mut members = members.to_vec();
    members.sort();
    members.dedup();
    let directory = teams(transaction)?;
    let selected = members
        .iter()
        .map(|member| {
            required_team(
                &directory,
                member,
                "The selected agents are no longer available.",
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    require_single_team(selected.iter().copied())?;
    check_historical_team(transaction, id, &members, &directory)?;
    let by_agent = directory.team_by_id.contains_key(id);
    if by_agent {
        return Err(CoordinationError::InvalidRequest(
            "This identity belongs to an agent.",
        ));
    }

    if let Some(existing) = CoordinationRepository::group(transaction, id)? {
        let original_members = validate_group(&existing)?;
        if existing.name != name || original_members != members {
            return Err(CoordinationError::Conflict(
                "The request identity already has different content.",
            ));
        }
        return Ok(json!({"id":id}));
    }

    let group = studio_runtime_store::ChatGroup {
        id: id.to_owned(),
        name: name.to_owned(),
        members: members.clone(),
        original_members: json!(members).to_string(),
    };
    CoordinationRepository::put_group(transaction, &group)?;
    for member in members {
        let edge_id = CoordinationRepository::edge_id(&member, id);
        CoordinationRepository::put_edge(transaction, &edge_id, &member, id)?;
        CoordinationRepository::edge_projection(transaction, &edge_id, &member, id, false)?;
    }
    CoordinationRepository::chat_projection(transaction, id, false)?;
    Ok(json!({"id":id}))
}

pub fn connect_chat(
    transaction: &Transaction<'_>,
    source: &str,
    target: &str,
    connected: bool,
) -> Result<Value, CoordinationError> {
    let group = CoordinationRepository::group(transaction, target)?.ok_or(
        CoordinationError::InvalidRequest("The target must be a chat."),
    )?;
    validate_group(&group)?;
    let directory = teams(transaction)?;
    required_team(&directory, source, "The source must be a current agent.")?;
    if connected {
        let mut members = group.members.clone();
        members.push(source.to_owned());
        check_historical_team(transaction, target, &members, &directory)?;
    }
    let edge_id = CoordinationRepository::edge_id(source, target);
    if connected {
        CoordinationRepository::put_edge(transaction, &edge_id, source, target)?;
    } else {
        CoordinationRepository::remove_edge(transaction, &edge_id)?;
    }
    CoordinationRepository::edge_projection(transaction, &edge_id, source, target, !connected)?;
    Ok(json!({"id":edge_id,"connected":connected}))
}

pub struct PostMessage<'a> {
    pub room: &'a str,
    pub id: &'a str,
    pub text: &'a str,
}

pub struct PostManagedMessage<'a> {
    pub room: &'a str,
    pub id: &'a str,
    pub text: &'a str,
    pub delivery: DeliveryMoment,
    pub assets: &'a [Value],
}

/// Queue promotion changes effective delivery, never the accepted request identity.
fn matches_requested_delivery(
    metadata: &Value,
    delivery: DeliveryMoment,
) -> Result<bool, CoordinationError> {
    if metadata.get("requestedDelivery").is_none()
        && crate::event_write::truthy(metadata.get("sendNow"))
    {
        // Historical promoted events did not retain their original timing.
        return Ok(true);
    }
    let requested = metadata
        .get("requestedDelivery")
        .or_else(|| metadata.get("delivery"));
    let stored = DeliveryMoment::for_event("user", "", Some(&json!({"delivery":requested})))
        .map_err(|_| CoordinationError::InvalidRequest("Invalid stored delivery moment"))?;
    Ok(stored == delivery)
}

/// Return the saved receipt only when the stable message identity already
/// names the exact durable message accepted for a paired anchor input.
pub(crate) fn remote_message_receipt(
    connection: &Transaction<'_>,
    room: &str,
    id: &str,
    text: &str,
    delivery: DeliveryMoment,
) -> Result<Option<Value>, CoordinationError> {
    let text = crate::event_write::python_strip(text);
    let Some(event) = QueueEventRepository::get(connection, &QueueEventId::from(id))? else {
        return Ok(None);
    };
    let metadata = QueueWriteRepository::metadata(connection, id)?
        .unwrap_or_else(|| json!({"assets": [], "delivery": DeliveryMoment::CurrentTurn}));
    let assets = metadata
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid_stored("runtime_event_meta", id))?;
    if event.agent.0 != room
        || event.kind != "user"
        || event.text != text
        || !assets.is_empty()
        || !matches_requested_delivery(&metadata, delivery)?
    {
        return Ok(None);
    }

    let signature = crate::event_write::python_json(&json!({
        "agent": room,
        "text": text,
        "assets": [],
    }));
    crate::event_write::read_receipt(
        connection,
        Some(id),
        &signature,
        "runtime_events",
        id,
        |value| valid_message_receipt(value, id),
    )
    .map_err(event_write_error)
}

#[derive(Debug, PartialEq)]
pub struct PostMessageResult {
    pub receipt: Value,
    pub event_inserted: bool,
}

impl PostMessageResult {
    /// Call only after the accepting transaction commits. Replays carry no new
    /// work and must not signal the consumer again.
    pub fn notify_committed(&self, wake: impl FnOnce()) {
        if self.event_inserted {
            wake();
        }
    }
}

/// The caller owns the transaction and publishes only after commit.
pub fn post_message_with_admission(
    transaction: &Transaction<'_>,
    input: PostManagedMessage<'_>,
    admission: &dyn crate::message_admission::MessageAdmission,
) -> Result<PostMessageResult, CoordinationError> {
    match post_message(
        transaction,
        PostMessage {
            room: input.room,
            id: input.id,
            text: input.text,
        },
    ) {
        Ok(receipt) => Ok(PostMessageResult {
            receipt,
            event_inserted: false,
        }),
        Err(CoordinationError::RequiresDependency("event-driver/message-send")) => {
            post_managed_message(transaction, input, admission)
        }
        Err(error) => Err(error),
    }
}

fn post_managed_message(
    transaction: &Transaction<'_>,
    input: PostManagedMessage<'_>,
    admission: &dyn crate::message_admission::MessageAdmission,
) -> Result<PostMessageResult, CoordinationError> {
    let text = crate::event_write::python_strip(input.text);
    if !message_text_within_limit(input.text) || (text.is_empty() && input.assets.is_empty()) {
        return Err(CoordinationError::InvalidRequest(
            "Message must have text or attachments, at most 32000 characters",
        ));
    }
    let mut agent =
        crate::event_write::checked_agent(transaction, input.room).map_err(event_write_error)?;
    if safety_retry_active(&agent) {
        return Err(CoordinationError::InvalidRequest(
            "Wait for the model change before sending another message",
        ));
    }
    crate::message_admission::attachments(input.assets)?;
    let signature = crate::event_write::python_json(&json!({
        "agent": input.room,
        "text": text,
        "assets": input.assets,
    }));
    if let Some(event) = QueueEventRepository::get(transaction, &QueueEventId::from(input.id))? {
        let metadata = QueueWriteRepository::metadata(transaction, input.id)?
            .unwrap_or_else(|| json!({"assets": [], "delivery": DeliveryMoment::CurrentTurn}));
        let old_assets = metadata
            .get("assets")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| invalid_stored("runtime_event_meta", input.id))?;
        if event.agent.0 != input.room
            || event.kind != "user"
            || event.text != text
            || old_assets != input.assets
            || !matches_requested_delivery(&metadata, input.delivery)?
        {
            return Err(CoordinationError::Conflict(
                "This message id has different content",
            ));
        }
        if event.status == "stored_only"
            && crate::event_write::truthy(agent.extra.get("remoteAnchor"))
            && let Some(receipt) = crate::event_write::read_receipt(
                transaction,
                Some(input.id),
                &signature,
                "runtime_events",
                input.id,
                |value| valid_message_receipt(value, input.id),
            )
            .map_err(event_write_error)?
        {
            return Ok(PostMessageResult {
                receipt,
                event_inserted: false,
            });
        }
        return Ok(PostMessageResult {
            receipt: studio_runtime_store::system_reads::validate(
                "runtime_events",
                input.id,
                // Python keeps accepted input pending until actual submission;
                // Rust's reservation is only an internal claim phase.
                json!({
                    "id": input.id,
                    "status": if event.status == "reserved" { "pending" } else { &event.status },
                    "error": event.error,
                }),
            )?,
            event_inserted: false,
        });
    }

    // An orphan receipt cannot prove that this event is still durable.
    let receipt_id = input.id.to_owned();
    if let Some(receipt) = crate::event_write::read_receipt(
        transaction,
        Some(&receipt_id),
        &signature,
        "runtime_events",
        input.id,
        |value| valid_message_receipt(value, input.id),
    )
    .map_err(event_write_error)?
    {
        if crate::event_write::truthy(agent.extra.get("remoteWorker"))
            || crate::event_write::truthy(agent.extra.get("remoteAnchor"))
        {
            return Ok(PostMessageResult {
                receipt,
                event_inserted: false,
            });
        }
        return Err(invalid_stored("runtime_events", input.id));
    }
    if crate::legacy_image_workspace::is_legacy(&agent.value()) {
        return Err(CoordinationError::InvalidRequest(
            "This legacy workspace is finished. Start a new chat to send a message.",
        ));
    }
    let root = if agent.root_id == agent.id {
        agent.clone()
    } else {
        crate::event_write::checked_agent(transaction, &agent.root_id.0)
            .map_err(event_write_error)?
    };
    require_not_excluded(&root, &agent)?;
    if agent.root_id != agent.id {
        crate::event_write::assert_delegation(&root).map_err(event_write_error)?;
    }
    assert_native_thread_open(&agent)?;
    let blockers = admission.workspace(transaction, &agent)?;
    admission.budget(transaction, &agent, &root)?;

    // A manual send wakes a stopped agent in the same transaction as the event.
    agent.extra.insert("autoWake".to_owned(), Value::Bool(true));
    agent.extra.insert("error".to_owned(), Value::Null);
    agent.extra.insert("complaintMisses".to_owned(), json!(0));
    agent.extra.remove("nativeFailureHold");
    capacity_reset(transaction, &mut agent)?;
    studio_runtime_store::AgentRepository::put(transaction, &agent)?;
    let projection =
        serde_json::to_value(&agent).map_err(studio_runtime_store::StoreError::from)?;
    crate::agents_create::publish_agent(transaction, &projection)?;

    let created = crate::event_write::unix_time().map_err(event_write_error)?;
    QueueWriteRepository::put_metadata(
        transaction,
        input.id,
        &json!({"assets": input.assets, "acceptedAt": created, "delivery": input.delivery, "requestedDelivery": input.delivery}),
    )?;
    let inserted = crate::event_write::enqueue_event(
        transaction,
        &agent,
        input.id.to_owned(),
        "user",
        text.to_owned(),
        created,
    )
    .map_err(event_write_error)?;
    let mut result = json!({"id": input.id, "status": "queued"});
    if !blockers.is_empty() {
        result["waitingFor"] = json!(blockers);
    }
    crate::event_write::save_receipt(transaction, Some(input.id), &signature, &result)
        .map_err(event_write_error)?;

    if CoordinationRepository::message(transaction, input.id)?.is_none() {
        CoordinationRepository::put_message(
            transaction,
            &studio_runtime_store::CanvasMessage {
                id: input.id.to_owned(),
                room: input.room.to_owned(),
                author: "user".to_owned(),
                text: text.to_owned(),
                at: created,
                deliveries: json!({(input.room): "queued"}).to_string(),
            },
        )?;
    }
    Ok(PostMessageResult {
        receipt: result,
        event_inserted: inserted,
    })
}

/// Queue model-authored follow-up input for a descendant. This is the durable
/// event path used by Runtime.send; the caller resolves descendant ancestry
/// from the same transaction before invoking it.
pub struct AgentFollowup<'a> {
    pub sender_id: &'a str,
    pub sender_epoch: i64,
    pub recipient_id: &'a str,
    pub id: &'a str,
    pub text: &'a str,
    pub delivery: DeliveryMoment,
    pub now: f64,
}

pub fn send_followup(
    transaction: &Transaction<'_>,
    input: AgentFollowup<'_>,
    admission: &dyn crate::message_admission::MessageAdmission,
) -> Result<PostMessageResult, CoordinationError> {
    send_followup_inner(
        transaction,
        Some((input.sender_id, input.sender_epoch)),
        FollowupInput {
            recipient_id: input.recipient_id,
            id: input.id,
            text: input.text,
            delivery: input.delivery,
            now: input.now,
            radio_answer_turn_id: None,
        },
        admission,
    )
}

/// An actorless runtime follow-up, matching `Runtime.send(..., sender=None)`.
pub struct SystemFollowup<'a> {
    pub recipient_id: &'a str,
    pub id: &'a str,
    pub text: &'a str,
    pub delivery: DeliveryMoment,
    pub now: f64,
}

pub fn send_system_followup(
    transaction: &Transaction<'_>,
    input: SystemFollowup<'_>,
    admission: &dyn crate::message_admission::MessageAdmission,
) -> Result<PostMessageResult, CoordinationError> {
    send_system_followup_with_radio_answer(transaction, input, None, admission)
}

/// Queue an async-question answer and bind it to the exact active radio turn,
/// as Runtime.send(..., radio_question=...) does.
pub fn send_system_followup_with_radio_answer(
    transaction: &Transaction<'_>,
    input: SystemFollowup<'_>,
    radio_answer_turn_id: Option<&str>,
    admission: &dyn crate::message_admission::MessageAdmission,
) -> Result<PostMessageResult, CoordinationError> {
    send_followup_inner(
        transaction,
        None,
        FollowupInput {
            recipient_id: input.recipient_id,
            id: input.id,
            text: input.text,
            delivery: input.delivery,
            now: input.now,
            radio_answer_turn_id,
        },
        admission,
    )
}

struct FollowupInput<'a> {
    recipient_id: &'a str,
    id: &'a str,
    text: &'a str,
    delivery: DeliveryMoment,
    now: f64,
    radio_answer_turn_id: Option<&'a str>,
}

fn send_followup_inner(
    transaction: &Transaction<'_>,
    sender_identity: Option<(&str, i64)>,
    input: FollowupInput<'_>,
    admission: &dyn crate::message_admission::MessageAdmission,
) -> Result<PostMessageResult, CoordinationError> {
    let FollowupInput {
        recipient_id,
        id,
        text,
        delivery,
        now,
        radio_answer_turn_id,
    } = input;
    let text = crate::event_write::python_strip(text);
    if text.is_empty() || !message_text_within_limit(text) {
        return Err(CoordinationError::InvalidRequest(
            "Message must have text or attachments, at most 32000 characters",
        ));
    }
    let mut recipient =
        crate::event_write::checked_agent(transaction, recipient_id).map_err(event_write_error)?;
    if let Some((sender_id, sender_epoch)) = sender_identity {
        let sender =
            crate::event_write::checked_agent(transaction, sender_id).map_err(event_write_error)?;
        if sender.epoch != sender_epoch || !crate::event_write::truthy(sender.extra.get("autoWake"))
        {
            return Err(CoordinationError::InvalidRequest("Sender was stopped"));
        }
        if recipient.root_id != sender.root_id {
            return Err(CoordinationError::InvalidRequest(
                "This record belongs to another team",
            ));
        }
    } else if !crate::event_write::truthy(recipient.extra.get("autoWake")) {
        return Err(CoordinationError::InvalidRequest(
            "Agent is stopped; no message was queued",
        ));
    }
    if safety_retry_active(&recipient) {
        return Err(CoordinationError::InvalidRequest(
            "Wait for the model change before sending another message",
        ));
    }
    let root = if recipient.root_id == recipient.id {
        recipient.clone()
    } else {
        crate::event_write::checked_agent(transaction, &recipient.root_id.0)
            .map_err(event_write_error)?
    };
    require_not_excluded(&root, &recipient)?;
    if recipient.root_id != recipient.id {
        crate::event_write::assert_delegation(&root).map_err(event_write_error)?;
    }
    assert_native_thread_open(&recipient)?;
    let blockers = admission.workspace(transaction, &recipient)?;
    admission.budget(transaction, &recipient, &root)?;

    if let Some(event) = QueueEventRepository::get(transaction, &QueueEventId::from(id))? {
        let metadata =
            QueueWriteRepository::metadata(transaction, id)?.unwrap_or_else(|| json!({}));
        if event.agent.0 != recipient_id
            || event.kind != "followup"
            || event.text != text
            || !matches_requested_delivery(&metadata, delivery)?
        {
            return Err(CoordinationError::Conflict(
                "This message id has different content",
            ));
        }
        return Ok(PostMessageResult {
            receipt: studio_runtime_store::system_reads::validate(
                "runtime_events",
                id,
                json!({"id":id,"status":event.status,"error":event.error}),
            )?,
            event_inserted: false,
        });
    }
    if sender_identity.is_some() {
        recipient.extra.insert("autoWake".into(), Value::Bool(true));
        recipient.extra.insert("error".into(), Value::Null);
        recipient.extra.insert("complaintMisses".into(), json!(0));
        recipient.extra.remove("nativeFailureHold");
        capacity_reset(transaction, &mut recipient)?;
        studio_runtime_store::AgentRepository::put(transaction, &recipient)?;
        let projection = serde_json::to_value(&recipient).map_err(StoreError::from)?;
        crate::agents_create::publish_agent(transaction, &projection)?;
    }
    let mut metadata =
        json!({"assets":[],"acceptedAt":now,"delivery":delivery,"requestedDelivery":delivery});
    if let Some(turn_id) = radio_answer_turn_id.filter(|value| !value.is_empty()) {
        metadata["radioAnswerTurnId"] = Value::String(turn_id.to_owned());
    }
    if let Some((sender_id, _)) = sender_identity {
        metadata["senderId"] = Value::String(sender_id.to_owned());
    }
    QueueWriteRepository::put_metadata(transaction, id, &metadata)?;
    let inserted = crate::event_write::enqueue_event(
        transaction,
        &recipient,
        id.to_owned(),
        "followup",
        text.to_owned(),
        now,
    )
    .map_err(event_write_error)?;
    let mut receipt = json!({"id":id,"status":"queued"});
    if !blockers.is_empty() {
        receipt["waitingFor"] = json!(blockers);
    }
    Ok(PostMessageResult {
        receipt,
        event_inserted: inserted,
    })
}

fn valid_message_receipt(value: &Value, id: &str) -> bool {
    value.get("id").and_then(Value::as_str) == Some(id)
        && value.get("status").and_then(Value::as_str).is_some()
        && value
            .get("error")
            .is_none_or(|error| error.is_null() || error.is_string())
}

fn safety_retry_active(agent: &studio_runtime_store::AgentRecord) -> bool {
    const ACTIVE: &[&str] = &[
        "turns",
        "items",
        "interrupt",
        "verify_turns",
        "verify_items",
        "fork",
        "start",
        "unknown",
    ];
    agent
        .extra
        .get("nativeSafetyRetry")
        .and_then(Value::as_object)
        .is_some_and(|retry| {
            retry
                .get("stage")
                .and_then(Value::as_str)
                .is_some_and(|stage| ACTIVE.contains(&stage))
                && retry.get("epoch").and_then(Value::as_i64) == Some(agent.epoch)
                && retry.get("accountKey").and_then(Value::as_str)
                    == Some(
                        agent
                            .extra
                            .get("accountKey")
                            .and_then(Value::as_str)
                            .unwrap_or("default"),
                    )
        })
}

fn require_not_excluded(
    root: &studio_runtime_store::AgentRecord,
    agent: &studio_runtime_store::AgentRecord,
) -> Result<(), CoordinationError> {
    let account = agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    if root
        .extra
        .get("excludedAccountKeys")
        .and_then(Value::as_array)
        .is_some_and(|keys| keys.iter().any(|key| key.as_str() == Some(account)))
    {
        return Err(CoordinationError::InvalidRequest(
            "This account is disabled for this chat",
        ));
    }
    Ok(())
}

fn assert_native_thread_open(
    agent: &studio_runtime_store::AgentRecord,
) -> Result<(), CoordinationError> {
    let thread = agent.extra.get("threadId").and_then(Value::as_str);
    let block = agent
        .extra
        .get("nativeThreadBlock")
        .and_then(Value::as_object);
    let blocked = block
        .filter(|block| {
            block
                .get("threadId")
                .and_then(Value::as_str)
                .is_some_and(|id| Some(id) == thread)
        })
        .is_some()
        || (thread.is_some()
            && agent
                .extra
                .get("error")
                .and_then(Value::as_object)
                .is_some_and(|error| {
                    error.get("codexErrorInfo").and_then(Value::as_str)
                        == Some("misalignmentPolicyViolation")
                }))
        || (thread.is_some()
            && agent
                .extra
                .get("nativeTurnError")
                .and_then(Value::as_object)
                .is_some_and(|previous| {
                    previous.get("turnId") == agent.extra.get("turnId")
                        && previous
                            .get("error")
                            .and_then(Value::as_object)
                            .is_some_and(|error| {
                                error.get("codexErrorInfo").and_then(Value::as_str)
                                    == Some("misalignmentPolicyViolation")
                            })
                }));
    if blocked {
        return Err(CoordinationError::InvalidRequest(
            "This chat is stopped as a precaution. Start or resume another chat.",
        ));
    }
    Ok(())
}

fn capacity_reset(
    transaction: &Transaction<'_>,
    agent: &mut studio_runtime_store::AgentRecord,
) -> Result<(), CoordinationError> {
    let retry = agent.extra.get("capacityRetry").cloned();
    if let Some(retry) = retry {
        let id = retry
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_stored("runtime_agents", &agent.id.0))?
            .to_owned();
        match studio_runtime_store::EventWriteRepository::finish_capacity_retry(
            transaction,
            &agent.id.0,
            &id,
        )? {
            Some(true) => {
                // Python capacity_save refreshes this clock when manual input
                // replaces the retry, even if no provider turn was submitted.
                transaction.execute(
                    "UPDATE runtime_capacity_retries SET record=json_set(record,'$.updatedAt',?1) WHERE id=?2 AND agent=?3",
                    rusqlite::params![crate::event_write::unix_time().map_err(event_write_error)?, id, agent.id.0],
                )?;
            }
            Some(false) => return Err(invalid_stored("runtime_capacity_retries", &id)),
            None => return Err(invalid_stored("runtime_capacity_retries", &id)),
        }
        if agent
            .extra
            .get("startAttempt")
            .and_then(Value::as_object)
            .is_some_and(|attempt| {
                attempt.get("capacityRetryId").and_then(Value::as_str) == Some(&id)
                    && attempt.get("submitted") == Some(&Value::Bool(false))
            })
        {
            agent.extra.remove("startAttempt");
            agent.extra.insert("inFlight".into(), Value::Bool(false));
            agent.status = studio_runtime_store::AgentStatus::Failed;
        }
    }
    agent.extra.remove("capacityRetry");
    agent.extra.remove("capacityRetryCount");
    Ok(())
}

pub fn post_message(
    transaction: &Transaction<'_>,
    input: PostMessage<'_>,
) -> Result<Value, CoordinationError> {
    if CoordinationRepository::federated_room_exists(transaction, input.room)? {
        return Err(CoordinationError::RequiresDependency(
            "federation-driver/rooms",
        ));
    }
    if CoordinationRepository::agent_exists(transaction, input.room)? {
        return Err(CoordinationError::RequiresDependency(
            "event-driver/message-send",
        ));
    }
    let group = CoordinationRepository::group(transaction, input.room)?;
    let Some(group) = group else {
        let directory = teams(transaction)?;
        if directory.suppressed_aliases.contains(input.room) {
            return Err(CoordinationError::InvalidRequest(
                "This chat is no longer available.",
            ));
        }
        if directory.unresolved_thread_agents.contains(input.room) {
            return Err(CoordinationError::RequiresDependency(
                "agent-discovery/launcher-status",
            ));
        }
        if directory.team_by_id.contains_key(input.room) {
            return Err(CoordinationError::RequiresDependency(
                "event-driver/message-send",
            ));
        }
        return Err(CoordinationError::RequiresDependency(
            "agent-discovery/launcher-status",
        ));
    };
    let text = input.text.trim();
    if !(1..=12_000).contains(&text.chars().count()) {
        return Err(CoordinationError::InvalidRequest(
            "Use a message with 1 to 12000 characters.",
        ));
    }
    let members = group.members.clone();
    validate_group(&group)?;
    let directory = teams(transaction)?;
    check_historical_team(transaction, input.room, &members, &directory)?;

    if let Some(previous) =
        CoordinationRepository::message(transaction, input.id).map_err(|_| {
            CoordinationError::InvalidStored {
                object: "messages",
                id: input.id.to_owned(),
            }
        })?
    {
        let previous = validated_message(previous)?;
        if previous.room != input.room || previous.author != "user" || previous.text != text {
            return Err(CoordinationError::Conflict(
                "The request identity already has different content.",
            ));
        }
        return Ok(message_receipt(previous, false));
    }
    let deliveries = members
        .into_iter()
        .filter(|member| member != "user")
        .map(|member| (member, Value::String("pending".to_owned())))
        .collect::<BTreeMap<_, _>>();
    let message = studio_runtime_store::CanvasMessage {
        id: input.id.to_owned(),
        room: input.room.to_owned(),
        author: "user".to_owned(),
        text: text.to_owned(),
        at: crate::event_write::unix_time().map_err(event_write_error)?,
        deliveries: serde_json::to_string(&Value::Object(deliveries.into_iter().collect()))
            .map_err(|_| CoordinationError::InvalidRequest("Invalid message delivery state"))?,
    };
    CoordinationRepository::put_message(transaction, &message)?;
    CoordinationRepository::chat_projection(transaction, input.room, false)?;
    Ok(message_receipt(validated_message(message)?, true))
}

struct AgentDirectory {
    team_by_id: BTreeMap<String, String>,
    suppressed_aliases: BTreeSet<String>,
    unresolved_thread_agents: BTreeSet<String>,
    invalid_agents: BTreeSet<String>,
}

fn teams(transaction: &Transaction<'_>) -> Result<AgentDirectory, CoordinationError> {
    let mut values = BTreeMap::<String, Value>::new();
    let mut graph_agents = BTreeSet::new();
    let mut managed_threads = BTreeSet::new();
    let mut invalid_agents = BTreeSet::new();
    for (id, mut value, is_graph) in CoordinationRepository::agents(transaction)? {
        if is_graph {
            graph_agents.insert(id.clone());
            value["source"] = Value::String("registered".to_owned());
        } else if let Some(thread_id) = value.get("threadId").and_then(Value::as_str) {
            managed_threads.insert(thread_id.to_owned());
        }
        values.insert(id, value);
    }
    let mut suppressed_aliases = BTreeSet::new();
    let mut unresolved_thread_agents = BTreeSet::new();
    for id in &graph_agents {
        let Some(record) = values.get_mut(id) else {
            continue;
        };
        record["source"] = Value::String("registered".to_owned());
        if let Some(thread_id) = record.get("threadId").and_then(Value::as_str) {
            if managed_threads.contains(thread_id) {
                suppressed_aliases.insert(id.clone());
            } else {
                unresolved_thread_agents.insert(id.clone());
            }
        }
    }
    let mut result = BTreeMap::new();
    'agents: for (id, value) in &values {
        if suppressed_aliases.contains(id) || unresolved_thread_agents.contains(id) {
            continue;
        }
        if super::workspace_views::deleted_agent(value) {
            continue;
        }
        let team = if let Some(root) = value
            .get("rootId")
            .and_then(Value::as_str)
            .filter(|root| !root.is_empty())
        {
            format!("managed:{root}")
        } else if value.get("source").and_then(Value::as_str) == Some("app-server")
            && let (Some(wave), Some(run_id)) = (
                value.get("wave").and_then(Value::as_str),
                value.get("runId").and_then(Value::as_str),
            )
        {
            format!("wave:{wave}:{run_id}")
        } else if value.get("source").and_then(Value::as_str) == Some("registered") {
            let mut current = id.as_str();
            let mut seen = BTreeSet::new();
            loop {
                if !seen.insert(current.to_owned()) {
                    invalid_agents.insert(id.clone());
                    continue 'agents;
                }
                let Some(node) = values.get(current) else {
                    unresolved_thread_agents.insert(id.clone());
                    continue 'agents;
                };
                if let Some(root) = node
                    .get("rootId")
                    .and_then(Value::as_str)
                    .filter(|root| !root.is_empty())
                {
                    break format!("managed:{root}");
                }
                if node.get("source").and_then(Value::as_str) == Some("app-server")
                    && let (Some(wave), Some(run_id)) = (
                        node.get("wave").and_then(Value::as_str),
                        node.get("runId").and_then(Value::as_str),
                    )
                {
                    break format!("wave:{wave}:{run_id}");
                }
                if node.get("source").and_then(Value::as_str) == Some("registered") {
                    match node
                        .get("parentId")
                        .and_then(Value::as_str)
                        .filter(|parent| !parent.is_empty())
                    {
                        Some(parent) => current = parent,
                        None => break format!("registered:{current}"),
                    }
                } else {
                    unresolved_thread_agents.insert(id.clone());
                    continue 'agents;
                }
            }
        } else {
            unresolved_thread_agents.insert(id.clone());
            continue 'agents;
        };
        if !suppressed_aliases.contains(id) && !unresolved_thread_agents.contains(id) {
            result.insert(id.clone(), team);
        }
    }
    Ok(AgentDirectory {
        team_by_id: result,
        suppressed_aliases,
        unresolved_thread_agents,
        invalid_agents,
    })
}

fn check_historical_team(
    transaction: &Transaction<'_>,
    room: &str,
    members: &[String],
    directory: &AgentDirectory,
) -> Result<(), CoordinationError> {
    let mut participants = members.iter().cloned().collect::<BTreeSet<_>>();
    if let Some(group) = CoordinationRepository::group(transaction, room)? {
        validate_group(&group)?;
        participants.extend(group.members);
    }
    for row in CoordinationRepository::participants(transaction, room)? {
        let deliveries = parse_deliveries(&row.deliveries, &row.id)?;
        if row.author != "user" {
            participants.insert(row.author);
        }
        participants.extend(
            deliveries
                .as_object()
                .expect("validated deliveries")
                .keys()
                .cloned(),
        );
    }
    let mut found = BTreeSet::new();
    for member in participants {
        found.insert(required_team(
            directory,
            &member,
            "The agent team cannot be verified. Use the current team's runtime tools.",
        )?);
    }
    require_single_team(found.into_iter())
}

fn required_team<'a>(
    directory: &'a AgentDirectory,
    id: &str,
    unavailable: &'static str,
) -> Result<&'a str, CoordinationError> {
    if directory.invalid_agents.contains(id) {
        return Err(invalid_agent(id));
    }
    if directory.suppressed_aliases.contains(id) {
        return Err(CoordinationError::InvalidRequest(unavailable));
    }
    if directory.unresolved_thread_agents.contains(id) {
        return Err(CoordinationError::RequiresDependency(
            "agent-discovery/launcher-status",
        ));
    }
    directory
        .team_by_id
        .get(id)
        .map(String::as_str)
        .ok_or(CoordinationError::RequiresDependency(
            "agent-discovery/launcher-status",
        ))
}

fn require_single_team<'a>(
    mut teams: impl Iterator<Item = &'a str>,
) -> Result<(), CoordinationError> {
    let Some(first) = teams.next() else {
        return Ok(());
    };
    if teams.any(|team| team != first) {
        return Err(CoordinationError::InvalidRequest(
            "Communication is limited to one team. This chat contains another team.",
        ));
    }
    Ok(())
}

fn validate_group(
    group: &studio_runtime_store::ChatGroup,
) -> Result<Vec<String>, CoordinationError> {
    let members: Vec<String> = serde_json::from_str(&group.original_members).map_err(|_| {
        CoordinationError::InvalidStored {
            object: "groups",
            id: group.id.clone(),
        }
    })?;
    if group.id.is_empty()
        || group.name.trim().is_empty()
        || group.name.chars().count() > 100
        || members.iter().any(String::is_empty)
        || members.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(CoordinationError::InvalidStored {
            object: "groups",
            id: group.id.clone(),
        });
    }
    Ok(members)
}

struct ValidatedMessage {
    id: String,
    room: String,
    author: String,
    text: String,
    at: f64,
    deliveries: Value,
}

fn validated_message(
    message: studio_runtime_store::CanvasMessage,
) -> Result<ValidatedMessage, CoordinationError> {
    let deliveries = parse_deliveries(&message.deliveries, &message.id)?;
    if message.id.is_empty()
        || message.room.is_empty()
        || message.author.is_empty()
        || !message_text_within_limit(&message.text)
        || !message.at.is_finite()
    {
        return Err(CoordinationError::InvalidStored {
            object: "messages",
            id: message.id.clone(),
        });
    }
    Ok(ValidatedMessage {
        id: message.id,
        room: message.room,
        author: message.author,
        text: message.text,
        at: message.at,
        deliveries,
    })
}

fn parse_deliveries(raw: &str, id: &str) -> Result<Value, CoordinationError> {
    let deliveries: Value =
        serde_json::from_str(raw).map_err(|_| CoordinationError::InvalidStored {
            object: "messages",
            id: id.to_owned(),
        })?;
    if !deliveries
        .as_object()
        .is_some_and(|items| items.values().all(Value::is_string))
    {
        return Err(CoordinationError::InvalidStored {
            object: "messages",
            id: id.to_owned(),
        });
    }
    Ok(deliveries)
}

fn message_json(message: ValidatedMessage) -> Value {
    json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries})
}

fn message_receipt(message: ValidatedMessage, newly_queued: bool) -> Value {
    let deliveries = message.deliveries.as_object();
    let status = if deliveries.is_none_or(|items| items.is_empty()) {
        "delivered"
    } else {
        let values = deliveries
            .expect("deliveries was checked")
            .values()
            .collect::<Vec<_>>();
        let accepted = if newly_queued {
            &["queued", "pending", "delivered", "accepted", "sent"][..]
        } else {
            &["queued", "delivered", "accepted", "sent"][..]
        };
        if values.iter().all(|value| {
            value
                .as_str()
                .is_some_and(|value| accepted.contains(&value))
        }) {
            if values
                .iter()
                .any(|value| *value == "queued" || (newly_queued && *value == "pending"))
            {
                "queued"
            } else {
                "delivered"
            }
        } else {
            let failed = values
                .iter()
                .filter_map(|value| value.as_str())
                .filter(|value| value.starts_with("failed:") || value.starts_with("cancelled:"))
                .collect::<Vec<_>>();
            if failed.len() == values.len() {
                let mut unique = Vec::new();
                for value in &failed {
                    if !unique.contains(value) {
                        unique.push(*value);
                    }
                }
                return json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries,"status":"failed","error":unique.join("; ")});
            }
            return json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries,"status":"uncertain","error":"Delivery is unconfirmed for one or more recipients. Check the saved deliveries before sending again."});
        }
    };
    json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries,"status":status})
}

fn invalid_agent(id: &str) -> CoordinationError {
    CoordinationError::InvalidStored {
        object: "agents",
        id: id.to_owned(),
    }
}

fn invalid_stored(object: &'static str, id: &str) -> CoordinationError {
    CoordinationError::InvalidStored {
        object,
        id: id.to_owned(),
    }
}

pub(crate) fn event_write_error(error: crate::event_write::EventWriteError) -> CoordinationError {
    use crate::event_write::EventWriteError;
    match error {
        EventWriteError::InvalidRequest(message) | EventWriteError::Conflict(message) => {
            CoordinationError::InvalidRequest(message)
        }
        EventWriteError::InvalidResultSchema(_) => {
            CoordinationError::InvalidRequest("Invalid result_schema")
        }
        EventWriteError::RemoteEventsUnavailable => {
            CoordinationError::RequiresDependency("event-driver/remote-events")
        }
        EventWriteError::WorkerAssignmentUnavailable => {
            CoordinationError::RequiresDependency("worker-driver/work-assignment")
        }
        EventWriteError::InvalidStoredObject { object, id } => {
            CoordinationError::InvalidStored { object, id }
        }
        EventWriteError::Store(StoreError::InvalidStoredRecord { object, id }) => {
            CoordinationError::InvalidStored { object, id }
        }
        EventWriteError::Store(error) => CoordinationError::Store(error),
        EventWriteError::Clock => {
            CoordinationError::InvalidRequest("system clock is before the Unix epoch")
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use studio_runtime_schema::delivery::DeliveryMoment;
    use studio_runtime_store::Connection;

    use super::{
        CoordinationError, PostManagedMessage, PostMessage, connect_chat, create_chat,
        message_text_within_limit, post_message, post_message_with_admission, require_single_team,
    };

    #[test]
    fn message_limit_counts_unicode_characters_for_paired_inputs() {
        assert!(message_text_within_limit(&"🙂".repeat(10_000)));
        assert!(!message_text_within_limit(&"🙂".repeat(32_001)));
    }

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        connection.execute_batch(
            &studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE groups(id TEXT PRIMARY KEY,name TEXT NOT NULL,members TEXT NOT NULL);
             CREATE TABLE graph_edges(id TEXT PRIMARY KEY,source TEXT NOT NULL,target TEXT NOT NULL,kind TEXT NOT NULL,UNIQUE(source,target,kind));
             CREATE TABLE messages(id TEXT PRIMARY KEY,room TEXT NOT NULL,author TEXT NOT NULL,text TEXT NOT NULL,at REAL NOT NULL,deliveries TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
             CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE graph_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE sync_entities(collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY AUTOINCREMENT,scope TEXT NOT NULL,id TEXT NOT NULL,payload TEXT NOT NULL,deleted INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY,scope TEXT NOT NULL,hash TEXT NOT NULL,deleted INTEGER NOT NULL,updated REAL NOT NULL);")
        ).expect("coordination schema creates");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        for (id, root) in [("lead", "lead"), ("worker", "lead"), ("other", "other")] {
            let record = json!({"id":id,"rootId":root,"epoch":0,"status":"idle"}).to_string();
            connection
                .execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    [id, record.as_str()],
                )
                .expect("agent inserts");
        }
        connection
    }

    fn database_at(path: &std::path::Path) -> Connection {
        let connection = Connection::open(path).expect("durable test database opens");
        connection.execute_batch(
            &studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE IF NOT EXISTS groups(id TEXT PRIMARY KEY,name TEXT NOT NULL,members TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS graph_edges(id TEXT PRIMARY KEY,source TEXT NOT NULL,target TEXT NOT NULL,kind TEXT NOT NULL,UNIQUE(source,target,kind));
             CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY,room TEXT NOT NULL,author TEXT NOT NULL,text TEXT NOT NULL,at REAL NOT NULL,deliveries TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE IF NOT EXISTS runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS graph_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sync_entities(collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(collection,id));
             CREATE TABLE IF NOT EXISTS sync_documents(seq INTEGER PRIMARY KEY,scope TEXT NOT NULL,id TEXT NOT NULL,payload TEXT NOT NULL,deleted INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS sync_versions(seq INTEGER PRIMARY KEY,scope TEXT NOT NULL,hash TEXT NOT NULL,deleted INTEGER NOT NULL,updated REAL NOT NULL);")
        ).expect("durable test schema creates");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        for (id, root) in [("lead", "lead"), ("worker", "lead")] {
            let record = json!({"id":id,"rootId":root,"epoch":0,"status":if id == "lead" {"running"} else {"idle"},"autoWake":id == "lead","inFlight":id == "lead"}).to_string();
            connection
                .execute(
                    "INSERT OR IGNORE INTO runtime_agents(id,record) VALUES(?1,?2)",
                    [id, record.as_str()],
                )
                .expect("durable test agent inserts");
        }
        connection
    }

    fn count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count reads")
    }

    #[test]
    fn mixed_teams_are_refused() {
        assert!(require_single_team(["team-a", "team-b"].into_iter()).is_err());
        assert!(require_single_team(["team-a", "team-a"].into_iter()).is_ok());
    }

    #[test]
    fn refused_chat_validation_does_not_advance_sqlite_sequence() {
        let mut connection = database();
        let mut lead = studio_runtime_store::AgentRepository::get(
            &connection,
            &studio_runtime_store::AgentId::from("lead"),
        )
        .expect("lead lookup succeeds")
        .expect("lead exists");
        lead.extra.insert("threadId".into(), json!("thread-1"));
        studio_runtime_store::AgentRepository::put(&connection, &lead)
            .expect("managed thread persists");
        connection
            .execute(
                "INSERT INTO graph_agents(id,record) VALUES('alias',?1)",
                [json!({"id":"alias","threadId":"thread-1"}).to_string()],
            )
            .expect("registered alias inserts");
        let before: i64 = connection
            .query_row(
                "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='sync_documents'),0)",
                [],
                |row| row.get(0),
            )
            .expect("sequence reads");
        let transaction = connection.transaction().expect("transaction starts");
        let result = create_chat(&transaction, "chat", "Team", &["alias".into()]);
        assert!(matches!(result, Err(CoordinationError::InvalidRequest(_))));
        drop(transaction);
        let after: i64 = connection
            .query_row(
                "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='sync_documents'),0)",
                [],
                |row| row.get(0),
            )
            .expect("sequence reads after refusal");
        assert_eq!(after, before);
    }

    #[test]
    // Python creates these exact row values at codex_runtime.py:5014-5015.
    fn managed_message_preserves_python_profile_fields_on_agent_write() {
        let mut connection = database();
        let mut lead = studio_runtime_store::AgentRepository::get(
            &connection,
            &studio_runtime_store::AgentId::from("lead"),
        )
        .expect("lead lookup succeeds")
        .expect("lead exists");
        lead.extra.insert("autoWake".into(), json!(true));
        lead.extra.insert("profileId".into(), json!("profile-1"));
        lead.extra.insert(
            "profileInstructions".into(),
            json!("Use the project profile."),
        );
        studio_runtime_store::AgentRepository::put(&connection, &lead)
            .expect("profile fields persist");

        let transaction = connection.transaction().expect("transaction starts");
        let result = post_message_with_admission(
            &transaction,
            PostManagedMessage {
                room: "lead",
                id: "profile-message",
                text: "hello",
                delivery: DeliveryMoment::CurrentTurn,
                assets: &[],
            },
            &AllowAdmission,
        )
        .expect("managed message queues");
        assert!(result.event_inserted);
        transaction.commit().expect("message transaction commits");

        let saved = studio_runtime_store::AgentRepository::get(
            &connection,
            &studio_runtime_store::AgentId::from("lead"),
        )
        .expect("lead lookup succeeds after write")
        .expect("lead remains");
        assert_eq!(saved.extra.get("profileId"), Some(&json!("profile-1")));
        assert_eq!(
            saved.extra.get("profileInstructions"),
            Some(&json!("Use the project profile."))
        );
    }

    #[test]
    fn managed_message_retry_preserves_original_timing_after_promotion() {
        let mut connection = database();
        let transaction = connection.transaction().expect("transaction");
        let send = |delivery| {
            post_message_with_admission(
                &transaction,
                PostManagedMessage {
                    room: "lead",
                    id: "promoted-message",
                    text: "hello",
                    delivery,
                    assets: &[],
                },
                &AllowAdmission,
            )
        };
        assert!(
            send(DeliveryMoment::NextTurn)
                .expect("first send")
                .event_inserted
        );
        let mut metadata =
            studio_runtime_store::QueueWriteRepository::metadata(&transaction, "promoted-message")
                .expect("metadata")
                .expect("accepted metadata");
        assert_eq!(
            metadata["requestedDelivery"],
            json!(DeliveryMoment::NextTurn)
        );
        // An old queued message has only the legacy effective timing.
        metadata
            .as_object_mut()
            .expect("metadata object")
            .remove("requestedDelivery");
        metadata["delivery"] = json!("after_turn");
        studio_runtime_store::QueueWriteRepository::put_metadata(
            &transaction,
            "promoted-message",
            &metadata,
        )
        .expect("legacy metadata");
        crate::queue_write::action(
            &transaction,
            "lead",
            &json!({"action":"send_now", "message_id":"promoted-message", "request_id":"promote"}),
            "promotion-fixture",
            |tx, _| {
                let event = studio_runtime_store::QueueEventRepository::get(
                    tx,
                    &studio_runtime_store::QueueEventId::from("promoted-message"),
                )?
                .expect("queued event");
                assert_eq!(event.status, "pending");
                Ok(crate::queue_write::QueueSnapshot {
                    items: vec![json!({"id":event.id.0,"kind":event.kind,"text":event.text,"created":event.created})],
                    revision: event.created.to_string(),
                    capabilities: json!({}),
                })
            },
        )
        .expect("queue promotion");
        let metadata =
            studio_runtime_store::QueueWriteRepository::metadata(&transaction, "promoted-message")
                .expect("promoted metadata")
                .expect("metadata exists");
        assert_eq!(
            metadata["requestedDelivery"],
            json!(DeliveryMoment::NextTurn)
        );
        assert_eq!(metadata["delivery"], json!(DeliveryMoment::CurrentTurn));
        assert!(
            !send(DeliveryMoment::NextTurn)
                .expect("exact retry")
                .event_inserted
        );
        assert!(matches!(
            send(DeliveryMoment::CurrentTurn),
            Err(CoordinationError::Conflict(_))
        ));
        assert_eq!(
            studio_runtime_store::QueueWriteRepository::metadata(&transaction, "promoted-message")
                .expect("metadata after retry"),
            Some(metadata)
        );
    }

    #[test]
    fn review_regression_legacy_chat_refuses_new_message_without_writes() {
        let mut connection = database();
        let mut lead = studio_runtime_store::AgentRepository::get(
            &connection,
            &studio_runtime_store::AgentId::from("lead"),
        )
        .expect("fixture operation succeeds")
        .expect("fixture operation succeeds");
        lead.status = studio_runtime_store::AgentStatus::Completed;
        lead.extra.insert("workspaceMode".into(), json!("image"));
        lead.extra
            .insert("legacyWorkspaceFinished".into(), json!(true));
        lead.extra.insert("autoWake".into(), json!(false));
        studio_runtime_store::AgentRepository::put(&connection, &lead)
            .expect("fixture operation succeeds");
        let tx = connection
            .transaction()
            .expect("fixture operation succeeds");
        let result = post_message_with_admission(
            &tx,
            PostManagedMessage {
                room: "lead",
                id: "legacy-new-message",
                text: "Please continue",
                delivery: DeliveryMoment::CurrentTurn,
                assets: &[],
            },
            &AllowAdmission,
        );
        assert!(
            matches!(result, Err(CoordinationError::InvalidRequest(message)) if message.contains("legacy workspace")),
            "{result:?}"
        );
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM runtime_events WHERE id='legacy-new-message'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("fixture operation succeeds"),
            0
        );
        let saved = studio_runtime_store::AgentRepository::get(
            &tx,
            &studio_runtime_store::AgentId::from("lead"),
        )
        .expect("fixture operation succeeds")
        .expect("fixture operation succeeds");
        assert_eq!(saved.extra["autoWake"], false);
    }

    struct AllowAdmission;

    impl crate::message_admission::MessageAdmission for AllowAdmission {
        fn workspace(
            &self,
            _tx: &studio_runtime_store::Transaction<'_>,
            _agent: &studio_runtime_store::AgentRecord,
        ) -> Result<Vec<serde_json::Value>, CoordinationError> {
            Ok(Vec::new())
        }

        fn budget(
            &self,
            _tx: &studio_runtime_store::Transaction<'_>,
            _agent: &studio_runtime_store::AgentRecord,
            _root: &studio_runtime_store::AgentRecord,
        ) -> Result<(), CoordinationError> {
            Ok(())
        }
    }

    #[test]
    fn actorless_followup_matches_python_runtime_send_row_and_replays_once() {
        use super::{SystemFollowup, send_system_followup};

        let mut connection = database();
        let mut lead = studio_runtime_store::AgentRepository::get(
            &connection,
            &studio_runtime_store::AgentId::from("lead"),
        )
        .expect("lead lookup succeeds")
        .expect("lead exists");
        lead.status = studio_runtime_store::AgentStatus::Running;
        lead.extra.insert("autoWake".into(), json!(true));
        lead.extra.insert("inFlight".into(), json!(true));
        studio_runtime_store::AgentRepository::put(&connection, &lead)
            .expect("recipient becomes an active lead");
        let send = |connection: &mut Connection, text: &str| {
            let tx = connection.transaction().expect("transaction starts");
            let result = send_system_followup(
                &tx,
                SystemFollowup {
                    recipient_id: "lead",
                    id: "mac-conflict:state:abcd",
                    text,
                    delivery: DeliveryMoment::NextTurn,
                    now: 123.5,
                },
                &AllowAdmission,
            );
            if result.is_ok() {
                tx.commit().expect("system follow-up commits");
            }
            result
        };
        send(&mut connection, " Mac conflict ").expect("Python-compatible actorless input queues");
        send(&mut connection, " Mac conflict ").expect("same identity replay returns receipt");
        let event: (String, String, String, String, i64) = connection
            .query_row(
                "SELECT id,agent,kind,text,epoch FROM runtime_events",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("one follow-up row exists");
        assert_eq!(
            event,
            (
                "mac-conflict:state:abcd".to_owned(),
                "lead".to_owned(),
                "followup".to_owned(),
                "Mac conflict".to_owned(),
                0,
            )
        );
        let metadata: String = connection
            .query_row(
                "SELECT record FROM runtime_event_meta WHERE id='mac-conflict:state:abcd'",
                [],
                |row| row.get(0),
            )
            .expect("event metadata exists");
        let metadata: serde_json::Value =
            serde_json::from_str(&metadata).expect("metadata is valid JSON");
        assert_eq!(metadata["delivery"], json!(DeliveryMoment::NextTurn));
        assert_eq!(metadata["assets"], json!([]));
        assert!(metadata.get("senderId").is_none());
        assert_eq!(count(&connection, "runtime_events"), 1);
    }

    #[cfg(unix)]
    #[test]
    fn mac_handoff_survives_p03_precommit_and_postcommit_restarts() {
        use super::{SystemFollowup, send_system_followup};
        use crate::linux_vm_guest::{GuestError, GuestRpc};
        use serde_json::Value;
        use std::time::Duration;

        struct Guest(std::sync::Mutex<usize>);
        impl GuestRpc for Guest {
            fn stream(
                &self,
                method: &str,
                _params: &Value,
                _request_id: Option<&str>,
                _timeout: Duration,
                _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
            ) -> Result<Value, GuestError> {
                if method == "sync.mac.handoff" {
                    *self.0.lock().expect("guest count lock") += 1;
                }
                Ok(match method {
                    "sync.mac.head" => json!({"mainStateId":"main-1"}),
                    "sync.mac.read" => {
                        json!({"kind":"file","mode":420,"data":"Z3Vlc3QgdmVyc2lvbg==","eof":true})
                    }
                    "sync.mac.handoff" => {
                        json!({"line":"mac-conflict","stateId":"handoff-1","mainStateId":"main-1"})
                    }
                    _ => json!({"ok":true}),
                })
            }
        }

        let root = tempfile::tempdir().expect("temporary state");
        let state_dir = root.path().join("state");
        let project = state_dir.join("layr-projects/p1");
        let folder = state_dir.join("mac-folder");
        std::fs::create_dir_all(project.join("sync")).expect("project fixture");
        std::fs::create_dir_all(&folder).expect("Mac folder fixture");
        std::fs::write(
            project.join("project.json"),
            json!({"projectId":"p1","source":folder}).to_string(),
        )
        .expect("project record");
        std::fs::write(folder.join("file.txt"), b"mac version").expect("Mac version");
        std::fs::write(
            project.join("sync/held.json"),
            json!({"file.txt":{"h":"old","why":"conflict"}}).to_string(),
        )
        .expect("held conflict");
        std::fs::write(project.join("sync/status.json"), json!({"state":"conflict","conflicts":["file.txt"],"choices":[],"withAgent":[],"deletes":0}).to_string()).expect("sync status");
        let guest = Guest(std::sync::Mutex::new(0));
        let first = crate::mac_sync::resolve(&state_dir, "p1", "file.txt", "agent", &guest)
            .expect("guest handoff succeeds");
        let identity = first.handoff_id.expect("handoff identity");
        let text = first.handoff_text.expect("handoff text");
        assert_eq!(*guest.0.lock().expect("guest count lock"), 1);

        let database_path = root.path().join("runtime.sqlite");
        {
            let mut connection = database_at(&database_path);
            // Fail point before the P03 transaction: dropping it simulates process loss.
            let tx = connection.transaction().expect("P03 transaction starts");
            send_system_followup(
                &tx,
                SystemFollowup {
                    recipient_id: "lead",
                    id: &identity,
                    text: &text,
                    delivery: DeliveryMoment::NextTurn,
                    now: 123.0,
                },
                &AllowAdmission,
            )
            .expect("P03 row stages before commit");
            drop(tx);
            drop(connection);
        }
        {
            let mut connection = database_at(&database_path);
            assert_eq!(count(&connection, "runtime_events"), 0);
            let restarted_guest = Guest(std::sync::Mutex::new(0));
            let replay =
                crate::mac_sync::resolve(&state_dir, "p1", "file.txt", "agent", &restarted_guest)
                    .expect("restart resolves saved guest receipt");
            assert_eq!(replay.handoff_id.as_deref(), Some(identity.as_str()));
            assert_eq!(*restarted_guest.0.lock().expect("guest count lock"), 0);
            let tx = connection
                .transaction()
                .expect("P03 transaction starts after restart");
            send_system_followup(
                &tx,
                SystemFollowup {
                    recipient_id: "lead",
                    id: &identity,
                    text: &text,
                    delivery: DeliveryMoment::NextTurn,
                    now: 124.0,
                },
                &AllowAdmission,
            )
            .expect("P03 row commits after restart");
            tx.commit().expect("P03 transaction commits");
        }
        {
            let mut connection = database_at(&database_path);
            // Fail point after the P03 commit: replay after restart returns the same receipt.
            let restarted_guest = Guest(std::sync::Mutex::new(0));
            let replay =
                crate::mac_sync::resolve(&state_dir, "p1", "file.txt", "agent", &restarted_guest)
                    .expect("postcommit restart resolves");
            assert_eq!(replay.handoff_id.as_deref(), Some(identity.as_str()));
            let tx = connection
                .transaction()
                .expect("P03 replay transaction starts");
            let receipt = send_system_followup(
                &tx,
                SystemFollowup {
                    recipient_id: "lead",
                    id: &identity,
                    text: &text,
                    delivery: DeliveryMoment::NextTurn,
                    now: 125.0,
                },
                &AllowAdmission,
            )
            .expect("same-identity replay gets receipt");
            assert!(!receipt.event_inserted);
            tx.commit().expect("idempotent replay transaction commits");
            assert_eq!(count(&connection, "runtime_events"), 1);
            assert_eq!(*restarted_guest.0.lock().expect("guest count lock"), 0);
        }
    }

    #[test]
    fn chat_write_is_idempotent_and_conflicts_leave_no_rows() {
        let mut connection = database();
        let id = "12345678-1234-1234-1234-123456789abc";
        let create = |connection: &mut Connection, name: &str| {
            let transaction = connection.transaction().expect("transaction starts");
            let result = create_chat(
                &transaction,
                id,
                name,
                &["lead".to_owned(), "worker".to_owned()],
            );
            if result.is_ok() {
                transaction.commit().expect("chat commits");
            }
            result
        };
        assert_eq!(
            create(&mut connection, "Team").expect("chat creates"),
            json!({"id":id})
        );
        assert_eq!(
            create(&mut connection, "Team").expect("chat replays"),
            json!({"id":id})
        );
        assert!(matches!(
            create(&mut connection, "Changed"),
            Err(CoordinationError::Conflict(_))
        ));
        assert_eq!(count(&connection, "groups"), 1);
        assert_eq!(count(&connection, "graph_edges"), 2);
        assert_eq!(count(&connection, "sync_entities"), 3);
    }

    #[test]
    fn cross_team_connection_refusal_writes_nothing() {
        let mut connection = database();
        let id = "12345678-1234-1234-1234-123456789abc";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(&transaction, id, "Team", &["lead".to_owned()])
            .expect("same team room creates");
        transaction.commit().expect("chat commits");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            connect_chat(&transaction, "other", id, true),
            Err(CoordinationError::InvalidRequest(_))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "graph_edges"), 1);
        assert_eq!(count(&connection, "sync_entities"), 2);
    }

    #[test]
    fn message_replay_is_idempotent_and_changed_content_conflicts() {
        let mut connection = database();
        let chat_id = "12345678-1234-1234-1234-123456789abc";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            chat_id,
            "Team",
            &["lead".to_owned(), "worker".to_owned()],
        )
        .expect("chat creates");
        transaction.commit().expect("chat commits");
        let message_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let post = |connection: &mut Connection, text: &str| {
            let transaction = connection.transaction().expect("transaction starts");
            let result = post_message(
                &transaction,
                PostMessage {
                    room: chat_id,
                    id: message_id,
                    text,
                },
            );
            if result.is_ok() {
                transaction.commit().expect("message commits");
            }
            result
        };
        let first = post(&mut connection, "Hello").expect("message writes");
        assert_eq!(first["status"], "queued");
        assert_eq!(first["deliveries"]["worker"], "pending");
        let replay = post(&mut connection, "Hello").expect("exact replay returns receipt");
        assert_eq!(replay["deliveries"], first["deliveries"]);
        assert_eq!(replay["status"], "uncertain");
        assert!(replay.get("error").is_some());
        assert!(matches!(
            post(&mut connection, "Changed"),
            Err(CoordinationError::Conflict(_))
        ));
        assert_eq!(count(&connection, "messages"), 1);
        assert_eq!(count(&connection, "sync_entities"), 3);
    }

    #[test]
    fn direct_agent_send_requires_the_event_consumer() {
        let connection = database();
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        let result = post_message(
            &transaction,
            PostMessage {
                room: "worker",
                id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                text: "Hello",
            },
        );
        assert!(matches!(
            result,
            Err(CoordinationError::RequiresDependency(
                "event-driver/message-send"
            ))
        ));
        assert_eq!(count(&connection, "messages"), 0);
    }

    #[test]
    fn direct_dispatch_and_federation_refuse_before_identity_or_text_rules() {
        let connection = database();
        connection
            .execute_batch(
                "CREATE TABLE runtime_federation_rooms(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_federation_rooms VALUES('federated:peer:room','{}');",
            )
            .expect("federation fixture creates");
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        assert!(matches!(
            post_message(
                &transaction,
                PostMessage {
                    room: "worker",
                    id: "invalid",
                    text: ""
                }
            ),
            Err(CoordinationError::RequiresDependency(
                "event-driver/message-send"
            ))
        ));
        assert!(matches!(
            post_message(
                &transaction,
                PostMessage {
                    room: "federated:peer:room",
                    id: "invalid",
                    text: ""
                }
            ),
            Err(CoordinationError::RequiresDependency(
                "federation-driver/rooms"
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "messages"), 0);
    }

    #[test]
    fn historical_room_participants_prevent_cross_team_takeover() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO messages VALUES(?1,?2,'other','old',1.0,'{}')",
                [
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                ],
            )
            .expect("historical message inserts");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            create_chat(
                &transaction,
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "Team",
                &["lead".to_owned()]
            ),
            Err(CoordinationError::InvalidRequest(
                "Communication is limited to one team. This chat contains another team."
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), 0);
        assert_eq!(count(&connection, "graph_edges"), 0);
        assert_eq!(count(&connection, "sync_entities"), 0);
    }

    #[test]
    fn registered_descendants_inherit_managed_team_and_managed_aliases_are_suppressed() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='lead'",
                [json!({"id":"lead","rootId":"lead","epoch":0,"status":"idle","threadId":"managed-thread"}).to_string()],
            )
            .expect("managed thread fixture updates");
        connection
            .execute(
                "INSERT INTO graph_agents VALUES('child',?1),('alias',?2)",
                [
                    json!({"id":"child","parentId":"lead"}).to_string(),
                    json!({"id":"alias","threadId":"managed-thread"}).to_string(),
                ],
            )
            .expect("registered fixtures insert");
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "Team",
            &["lead".to_owned(), "child".to_owned()],
        )
        .expect("child inherits parent team");
        transaction.commit().expect("chat commits");
        let before = count(&connection, "groups");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            create_chat(
                &transaction,
                "cccccccc-cccc-cccc-cccc-cccccccccccc",
                "Alias",
                &["alias".to_owned()]
            ),
            Err(CoordinationError::InvalidRequest(
                "The selected agents are no longer available."
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), before);
    }

    #[test]
    fn status_file_agents_and_aliases_require_launcher_discovery_without_writes() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO graph_agents VALUES('launcher-alias',?1)",
                [
                    json!({"id":"launcher-alias","threadId":"thread-only-in-status-file"})
                        .to_string(),
                ],
            )
            .expect("alias fixture inserts");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            create_chat(
                &transaction,
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "Launcher",
                &["launcher-alias".to_owned()]
            ),
            Err(CoordinationError::RequiresDependency(
                "agent-discovery/launcher-status"
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), 0);
        assert_eq!(count(&connection, "graph_edges"), 0);
        assert_eq!(count(&connection, "sync_entities"), 0);
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "Known",
            &[],
        )
        .expect("chat creates");
        transaction.commit().expect("chat commits");
        let before = count(&connection, "sync_entities");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            connect_chat(
                &transaction,
                "status-only-agent",
                "cccccccc-cccc-cccc-cccc-cccccccccccc",
                false
            ),
            Err(CoordinationError::RequiresDependency(
                "agent-discovery/launcher-status"
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "sync_entities"), before);
    }

    #[test]
    fn group_post_excludes_author_from_pending_recipients_and_bounds_chat_tail() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES('user',?1)",
                [json!({"id":"user","rootId":"lead","epoch":0,"status":"idle"}).to_string()],
            )
            .expect("user agent fixture inserts");
        let room = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            room,
            "Team",
            &["lead".to_owned(), "user".to_owned()],
        )
        .expect("chat creates");
        transaction.commit().expect("chat commits");
        let transaction = connection.transaction().expect("transaction starts");
        let receipt = post_message(
            &transaction,
            PostMessage {
                room,
                id: "cccccccc-cccc-cccc-cccc-cccccccccccc",
                text: &"x".repeat(2_500),
            },
        )
        .expect("message writes");
        assert_eq!(receipt["status"], "queued");
        assert_eq!(receipt["deliveries"]["lead"], "pending");
        assert!(receipt["deliveries"].get("user").is_none());
        transaction.commit().expect("message commits");
        let payload: String = connection
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat projection reads");
        let payload: serde_json::Value = serde_json::from_str(&payload).expect("projection parses");
        assert_eq!(
            payload["value"]["tail"]
                .as_str()
                .expect("tail is text")
                .chars()
                .count(),
            2_000
        );
    }

    #[test]
    fn replay_preserves_python_delivery_receipts() {
        let mut connection = database();
        let room = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(&transaction, room, "Team", &[]).expect("chat creates");
        transaction.commit().expect("chat commits");
        for (id, delivery, status, error) in [
            (
                "10101010-1010-1010-1010-101010101010",
                "delivered",
                "delivered",
                None,
            ),
            (
                "11111111-1111-1111-1111-111111111111",
                "accepted",
                "delivered",
                None,
            ),
            (
                "22222222-2222-2222-2222-222222222222",
                "sent",
                "delivered",
                None,
            ),
            (
                "33333333-3333-3333-3333-333333333333",
                "failed: mailbox closed",
                "failed",
                Some("failed: mailbox closed"),
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO messages VALUES(?1,?2,'user','saved',1.0,?3)",
                    [id, room, &json!({"worker":delivery}).to_string()],
                )
                .expect("Python message inserts");
            let transaction = connection
                .unchecked_transaction()
                .expect("transaction starts");
            let receipt = post_message(
                &transaction,
                PostMessage {
                    room,
                    id,
                    text: "saved",
                },
            )
            .expect("replay returns saved receipt");
            assert_eq!(receipt["status"], status);
            assert_eq!(
                receipt.get("error").and_then(serde_json::Value::as_str),
                error
            );
            assert_eq!(receipt["deliveries"]["worker"], delivery);
        }
    }

    #[test]
    fn late_chat_projection_failure_rolls_back_group_edges_and_sync_rows() {
        let mut connection = database();
        connection.execute_batch(
            "CREATE TRIGGER fail_chat_projection BEFORE INSERT ON sync_entities
             WHEN NEW.collection='chat' BEGIN SELECT RAISE(ABORT,'injected chat projection failure'); END;",
        ).expect("failure trigger creates");
        let transaction = connection.transaction().expect("transaction starts");
        let result = create_chat(
            &transaction,
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "Team",
            &["lead".to_owned()],
        );
        assert!(matches!(result, Err(CoordinationError::Store(_))));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), 0);
        assert_eq!(count(&connection, "graph_edges"), 0);
        assert_eq!(count(&connection, "sync_entities"), 0);
        assert_eq!(count(&connection, "sync_documents"), 0);
        assert_eq!(count(&connection, "sync_versions"), 0);
    }

    #[test]
    fn connection_changes_publish_only_edge_projection() {
        let mut connection = database();
        let room = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(&transaction, room, "Team", &["lead".to_owned()]).expect("chat creates");
        transaction.commit().expect("chat commits");
        let chat_sequence: i64 = connection
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat sequence reads");
        let transaction = connection.transaction().expect("transaction starts");
        connect_chat(&transaction, "worker", room, true).expect("edge connects");
        transaction.commit().expect("edge commits");
        let after_connect: i64 = connection
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat sequence reads");
        assert_eq!(chat_sequence, after_connect);
        let transaction = connection.transaction().expect("transaction starts");
        connect_chat(&transaction, "worker", room, false).expect("edge disconnects");
        transaction.commit().expect("edge removal commits");
        let after_disconnect: i64 = connection
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat sequence reads");
        assert_eq!(chat_sequence, after_disconnect);
    }
}
