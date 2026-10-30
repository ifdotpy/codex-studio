//! Native provider requests (Codex and Claude) as typed approvals.
//!
//! A card is a row of the `approvals` table. The row holds the typed request
//! and the answer. The outbox holds the provider reply. The state moves with
//! the delivery of that reply and never by inference.
use super::{model::*, storage};
use rusqlite::{Connection, Transaction};
use serde_json::{Map, Value, json};
use studio_runtime_providers::{ProviderKind, RequestId, ResponseBody};
use studio_runtime_schema::approvals::{
    AgentId, Answer, Approval, ApprovalId, ApprovalState, CloseReason, CommandId, Decision,
    Delivery, Detail, ElicitationAction, Hidden, Principal, ProviderObject, Question,
    QuestionAnswer, QuestionOption, Request, RpcId, Supervisor, Timestamp, TurnId,
};
use studio_runtime_schema::persistence::ExecutionEffectKind;
use studio_runtime_store::{RuntimeStore, approvals, system_reads as records};

/// The native provider requests that wait for the user as a request card. The
/// web client renders each of them (`components/questions/Requests.tsx`).
pub const NATIVE_REQUEST_METHODS: [&str; 5] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
];

/// Seconds a Claude approval waits in full access mode before Studio declines
/// it. Nobody watches an agent that runs without approvals, so an unanswered
/// request must not block the turn and every later message. Claude Code
/// resolves a permission dialog parked at a remote client to its no-action
/// default after 5 minutes, 10 at most, so Studio declines at that maximum.
/// The server owns this value; the Claude bridge has no timer.
pub const CLAUDE_FULL_ACCESS_APPROVAL_SECONDS: f64 = 600.0;

/// The Claude tool permission request, the only kind with that timer.
const CLAUDE_TOOL_METHOD: &str = NATIVE_REQUEST_METHODS[0];

/// Prefix of the id of a row that records an automatic answer.
const POLICY_ID_PREFIX: &str = "policy:";

pub fn interactive(method: &str) -> bool {
    NATIVE_REQUEST_METHODS.contains(&method)
}

/// Unix seconds now, for callers that run outside a request clock.
pub(super) fn clock() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// The provider method a native request kind answers.
fn method_of(request: &Request) -> Option<&'static str> {
    Some(match request {
        Request::Exec { .. } => NATIVE_REQUEST_METHODS[0],
        Request::Patch { .. } => NATIVE_REQUEST_METHODS[1],
        Request::Permissions { .. } => NATIVE_REQUEST_METHODS[2],
        Request::Question { .. } => NATIVE_REQUEST_METHODS[3],
        Request::Elicitation { .. } => NATIVE_REQUEST_METHODS[4],
        Request::ToolUse { .. }
        | Request::AgentQuestion { .. }
        | Request::MonitorStart { .. }
        | Request::BrowserOpen { .. } => return None,
    })
}

fn request_id(rpc: &RpcId) -> Result<RequestId, CallbackError> {
    let value = match rpc {
        RpcId::Number { value } => json!(value),
        RpcId::Text { value } => json!(value),
    };
    Ok(serde_json::from_value(value)?)
}

fn rpc_id(id: &RequestId) -> Result<RpcId, CallbackError> {
    match serde_json::to_value(id)? {
        Value::Number(number) => Ok(RpcId::Number {
            value: number
                .as_i64()
                .ok_or(CallbackError::Invalid("Invalid request id"))?,
        }),
        Value::String(value) => Ok(RpcId::Text { value }),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => {
            Err(CallbackError::Invalid("Invalid request id"))
        }
    }
}

fn get_row(tx: &Connection, id: &str) -> Result<Option<Approval>, CallbackError> {
    Ok(approvals::get(tx, &ApprovalId(id.to_owned()))?)
}

fn delivery(row: &Approval) -> Result<&Delivery, CallbackError> {
    row.delivery
        .as_ref()
        .ok_or_else(|| CallbackError::Stored(format!("approvals/{}/delivery", row.id.as_str())))
}

fn generation(row: &Approval) -> Option<u64> {
    row.delivery
        .as_ref()
        .and_then(|delivery| delivery.supervisor.as_ref())
        .map(|supervisor| supervisor.generation)
}

fn thread(row: &Approval) -> Option<&str> {
    row.detail.params.0.get("threadId").and_then(Value::as_str)
}

/// The legacy execution-effect status that follows an approval.
pub(super) fn effect_status(row: &Approval) -> &'static str {
    if row.hidden.is_some() {
        return "deleted";
    }
    match &row.state {
        ApprovalState::Pending => "pending",
        ApprovalState::Answering { .. } => "answering",
        ApprovalState::Answered { .. } => "answered",
        ApprovalState::Unknown { .. } => "uncertain",
        ApprovalState::Closed {
            reason: CloseReason::ProviderResolved,
            ..
        } => "resolved",
        ApprovalState::Closed { .. } => "expired",
    }
}

/// Follow the approval in the execution effects and publish it.
pub(super) fn publish(tx: &Transaction<'_>, row: &Approval) -> Result<(), CallbackError> {
    if let Some(actor) = records::get(tx, "runtime_agents", row.agent.as_str())? {
        let effect = json!({
            "id": row.id.as_str(),
            "turnId": row.turn.as_ref().map(TurnId::as_str),
            "status": effect_status(row),
        });
        crate::tool_requests::reconcile_effect(tx, ExecutionEffectKind::Requests, &effect, &actor)?;
    }
    crate::approval_sync::publish(tx, row)?;
    Ok(())
}

/// Write the changed approval, then publish it.
pub(super) fn save(tx: &Transaction<'_>, row: &Approval) -> Result<(), CallbackError> {
    approvals::put(tx, row)?;
    publish(tx, row)
}

/// Read, change, write and publish one approval.
fn update(
    tx: &Transaction<'_>,
    id: &str,
    change: impl FnOnce(&mut Approval),
) -> Result<Approval, CallbackError> {
    let mut row = get_row(tx, id)?.ok_or(CallbackError::Stale)?;
    change(&mut row);
    save(tx, &row)?;
    Ok(row)
}

/// How the account worker settles one native provider request.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeAdmission {
    /// Answer now, without the user: a refusal, or a decline for a deleted agent.
    Answer(ResponseBody),
    /// The pending card owns the answer. The user answers it over HTTP.
    Card(String),
    /// A card or a resolution of this exact request already settled it.
    Settled,
}

/// Admit one native request of the account session as a pending card, in one
/// transaction. Repeating it for the same request (a worker restart before its
/// record retired) finds the first card and admits nothing new. The user's
/// answer is sent only by the card's HTTP path, never from here, so a replay
/// cannot send it a second time.
pub async fn admit_native(
    store: std::sync::Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &studio_runtime_providers::Frame,
    now: f64,
) -> Result<NativeAdmission, CallbackError> {
    let request = Input::from_frame(frame)?
        .filter(|request| interactive(&request.method))
        .ok_or(CallbackError::Invalid("Expected a native provider request"))?;
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    let context = context.clone();
    store
        .transaction_with(move |tx| {
            let identity = ReplyIdentity::new(&context, &request.id);
            if let Some(decision) = storage::saved_decision(tx, &identity, &request)? {
                return Ok(match decision {
                    CallbackDecision::Pending(id) => {
                        update(tx, &id, |row| {
                            if let Some(delivery) = row.delivery.as_mut() {
                                delivery.connection = context.connection.clone();
                            }
                        })?;
                        NativeAdmission::Card(id)
                    }
                    CallbackDecision::Answer(_)
                    | CallbackDecision::Settled(_)
                    | CallbackDecision::Unsupported => NativeAdmission::Settled,
                });
            }
            let snapshot = storage::snapshot(tx, &context)?;
            if let Some(body) = immediate(&snapshot, &request)? {
                return Ok(NativeAdmission::Answer(body));
            }
            // Read the mode once; Ask can race an approval already admitted.
            if snapshot.actor()["provider"] != ProviderKind::Claude.key()
                && snapshot.actor()["yoloMode"] == true
                && full_access_covers(&request.method)
            {
                return record_policy_answer(tx, &context, &identity, &request, now)
                    .map(NativeAdmission::Answer);
            }
            let CallbackDecision::Pending(id) = admit(tx, &context, &snapshot, &request, now)?
            else {
                return Err(CallbackError::Unknown);
            };
            Ok(NativeAdmission::Card(id))
        })
        .await
}

/// Tool approvals share the answer conversion of explicit user answers.
/// Questions and elicitation always continue to require an answer from the user.
fn full_access_covers(method: &str) -> bool {
    NATIVE_REQUEST_METHODS[..3].contains(&method)
}

/// A policy answer (full access) is a row created as answered by the policy.
/// The row id comes from the request identity, so a repeat finds the row and
/// writes nothing. The provider reply is built from the same typed answer.
fn record_policy_answer(
    tx: &Transaction<'_>,
    context: &CallbackContext,
    identity: &ReplyIdentity,
    input: &Input,
    now: f64,
) -> Result<ResponseBody, CallbackError> {
    let id = format!("{POLICY_ID_PREFIX}{}", identity.key());
    let new = new_approval(tx, &id, context, input, None)?;
    let (answer, result) = provider_answer(
        &new.request,
        &new.detail.params,
        &json!({"decision":"accept"}),
    )?;
    if get_row(tx, &id)?.is_none() {
        approvals::create_policy_answered(tx, new, answer, Timestamp(now))?;
    }
    Ok(ResponseBody::Result(result))
}

/// The row a provider request creates. Typed request fields leave the params;
/// the rest stays as the provider sent it.
fn new_approval(
    tx: &Transaction<'_>,
    id: &str,
    context: &CallbackContext,
    input: &Input,
    fire_at: Option<Timestamp>,
) -> Result<approvals::NewApproval, CallbackError> {
    let preview = item_preview(
        tx,
        &context.agent,
        input.params.as_object().unwrap_or(&Map::new()),
    )?;
    let (request, params) = split_request(&input.method, &input.params, preview.as_ref())?;
    Ok(approvals::NewApproval {
        id: ApprovalId(id.to_owned()),
        agent: AgentId(context.agent.clone()),
        turn: Some(TurnId(context.turn.clone())),
        epoch: Some(context.epoch),
        request,
        delivery: Some(Delivery {
            rpc_id: rpc_id(&input.id)?,
            account: context.account.clone(),
            connection: context.connection.clone(),
            supervisor: Some(Supervisor {
                generation: context.generation,
                state_dir: None,
                handle: None,
            }),
        }),
        detail: Detail {
            params,
            preview: preview.map(ProviderObject),
            answer_error: None,
            host_close_reason: None,
        },
        fire_at,
    })
}

/// The typed request of a provider request and the params that stay with it.
/// Typed fields and the turn (a column of the row) leave the provider params.
pub fn split_request(
    method: &str,
    params: &Value,
    preview: Option<&Map<String, Value>>,
) -> Result<(Request, ProviderObject), CallbackError> {
    let mut rest = params.as_object().cloned().unwrap_or_default();
    let request = typed_request(method, &mut rest, preview)?;
    take_text(&mut rest, "turnId");
    Ok((request, ProviderObject(rest)))
}

/// The item the request is about, as the transcript stored it.
fn item_preview(
    tx: &Transaction<'_>,
    agent: &str,
    params: &Map<String, Value>,
) -> Result<Option<Map<String, Value>>, CallbackError> {
    let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let item = records::get(tx, "runtime_items", &format!("{agent}:{item_id}"))?;
    Ok(item
        .as_ref()
        .and_then(|item| item["text"].as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|preview| preview.as_object().cloned()))
}

fn take_text(params: &mut Map<String, Value>, key: &str) -> Option<String> {
    match params.remove(key) {
        Some(Value::String(text)) => Some(text),
        Some(other) => {
            params.insert(key.to_owned(), other);
            None
        }
        None => None,
    }
}

fn questions(params: &mut Map<String, Value>) -> Vec<Question> {
    let Some(Value::Array(list)) = params.remove("questions") else {
        return Vec::new();
    };
    list.iter()
        .enumerate()
        .map(|(index, question)| {
            let text = |key: &str| question.get(key).and_then(Value::as_str).map(str::to_owned);
            Question {
                id: text("id").unwrap_or_else(|| index.to_string()),
                question: text("question").unwrap_or_default(),
                header: text("header"),
                options: question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        let text =
                            |key: &str| option.get(key).and_then(Value::as_str).map(str::to_owned);
                        Some(QuestionOption {
                            label: text("label")?,
                            description: text("description"),
                            preview: text("preview"),
                        })
                    })
                    .collect(),
                is_secret: studio_runtime_store::truthy(question.get("isSecret")),
                multi_select: question.get("multiSelect").and_then(Value::as_bool),
                is_other: question.get("isOther").and_then(Value::as_bool),
            }
        })
        .collect()
}

/// The typed request of a provider method. Typed fields leave `params`.
fn typed_request(
    method: &str,
    params: &mut Map<String, Value>,
    preview: Option<&Map<String, Value>>,
) -> Result<Request, CallbackError> {
    let preview_text = |key: &str| {
        preview
            .and_then(|preview| preview.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Ok(match method {
        "item/commandExecution/requestApproval" => Request::Exec {
            command: take_text(params, "command")
                .or_else(|| preview_text("command"))
                .unwrap_or_default(),
            cwd: take_text(params, "cwd")
                .or_else(|| preview_text("cwd"))
                .unwrap_or_default(),
            reason: take_text(params, "reason"),
        },
        "item/fileChange/requestApproval" => Request::Patch {
            item: take_text(params, "itemId").unwrap_or_default(),
            files: preview
                .and_then(|preview| preview.get("changes"))
                .and_then(Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(|change| {
                            change
                                .get("path")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            reason: take_text(params, "reason"),
        },
        "item/permissions/requestApproval" => Request::Permissions {
            requested: params
                .get("permissions")
                .and_then(Value::as_object)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default(),
        },
        "mcpServer/elicitation/request" => Request::Elicitation {
            schema: params.remove("requestedSchema").unwrap_or(Value::Null),
        },
        "item/tool/requestUserInput" => Request::Question {
            questions: questions(params),
        },
        _ => return Err(CallbackError::Invalid("Unsupported request type")),
    })
}

/// Expire the pending native cards of `account` that another provider
/// generation sent. That process ended, so no answer can reach the sender and
/// the card must not wait for the user. Run in the worker's startup recovery
/// transaction, after the generation is known.
pub fn expire_ended_generations(
    tx: &Transaction<'_>,
    account: &str,
    generation: u64,
) -> Result<usize, CallbackError> {
    let mut count = 0;
    for card in approvals::open_for_account(tx, account)? {
        if method_of(&card.request).is_none() || self::generation(&card) == Some(generation) {
            continue;
        }
        // One card that cannot be saved must not stop the worker start: its
        // writes roll back and it keeps its state.
        tx.execute_batch("SAVEPOINT native_card_expiry")?;
        match expire_card(tx, &card, clock()) {
            Ok(expired) => {
                tx.execute_batch("RELEASE native_card_expiry")?;
                count += usize::from(expired);
            }
            Err(error) => {
                tx.execute_batch("ROLLBACK TO native_card_expiry; RELEASE native_card_expiry")?;
                eprintln!(
                    "native request card kept its state; account={account} card={} generation={generation}: {error}",
                    card.id.as_str()
                );
            }
        }
    }
    Ok(count)
}

/// The provider session of the card is gone. A pending card closes; an answer
/// whose delivery started becomes unknown.
fn expire_card(tx: &Transaction<'_>, card: &Approval, now: f64) -> Result<bool, CallbackError> {
    let now = Timestamp(now);
    match card.state {
        ApprovalState::Pending => {
            approvals::close(tx, &card.id, CloseReason::SessionLost, now)?;
        }
        ApprovalState::Answering { .. } => {
            approvals::delivery_lost(tx, &card.id, now)?;
        }
        ApprovalState::Answered { .. }
        | ApprovalState::Unknown { .. }
        | ApprovalState::Closed { .. } => return Ok(false),
    }
    storage::hold_card(tx, card.id.as_str())?;
    let after = get_row(tx, card.id.as_str())?.ok_or(CallbackError::Stale)?;
    publish(tx, &after)?;
    Ok(true)
}

pub(super) fn immediate(
    snapshot: &CallbackSnapshot,
    input: &Input,
) -> Result<Option<ResponseBody>, CallbackError> {
    input.check_context(snapshot.context())?;
    if studio_runtime_store::truthy(snapshot.actor().get("deletedAt")) {
        return Ok(Some(match input.method.as_str() {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                ResponseBody::Result(json!({"decision":"decline"}))
            }
            "item/permissions/requestApproval" => {
                ResponseBody::Result(json!({"permissions":{},"scope":"turn"}))
            }
            "mcpServer/elicitation/request" => {
                ResponseBody::Result(json!({"action":"decline","content":null}))
            }
            _ => ResponseBody::Error(
                json!({"code":-32600,"message":"This agent is unavailable and cannot handle this request."}),
            ),
        }));
    }
    if input.method == "item/tool/requestUserInput" && snapshot.actor()["isLead"] != true {
        return Ok(Some(ResponseBody::Error(
            json!({"code":-32600,"message":"Only the orchestrator can ask the user. Send your question with orchestration_message target=lead; the orchestrator decides whether to contact the user."}),
        )));
    }
    snapshot.active()?;
    Ok(None)
}

pub(super) fn admit(
    tx: &Transaction<'_>,
    context: &CallbackContext,
    snapshot: &CallbackSnapshot,
    input: &Input,
    now: f64,
) -> Result<CallbackDecision, CallbackError> {
    let identity = ReplyIdentity::new(context, &input.id);
    if let Some(decision) = storage::saved_decision(tx, &identity, input)? {
        return Ok(decision);
    }
    let mut owner = storage::admit_owner(tx, &identity, &input.signature())?;
    if approvals::open_for_account(tx, &context.account)?.len() >= MAX_PENDING {
        return Err(CallbackError::Invalid("Too many pending provider requests"));
    }
    let id = owner.id.clone();
    // A Claude agent in full access mode asks only what Claude Code insists on.
    // Nobody watches for it, so the server declines it after a time.
    let fire_at = (snapshot.actor()["provider"] == ProviderKind::Claude.key()
        && snapshot.actor()["yoloMode"] == true
        && input.method == CLAUDE_TOOL_METHOD)
        .then_some(Timestamp(now + CLAUDE_FULL_ACCESS_APPROVAL_SECONDS));
    let new = new_approval(tx, &id, context, input, fire_at)?;
    let card = approvals::create(tx, new, Timestamp(now))?;
    publish(tx, &card)?;
    let mut actor = snapshot.actor().clone();
    actor["status"] = json!("approval");
    storage::save_actor(tx, &actor)?;
    owner.payload.card = Some(id.clone());
    storage::put_owner(tx, &owner)?;
    Ok(CallbackDecision::Pending(id))
}

/// Only the authenticated user HTTP adapter may supply User. No dynamic tool
/// routes here; model callers cannot grant native permissions.
#[derive(Clone, Copy)]
pub enum AnswerAuthority {
    User,
    Agent,
}

const REDACTED: &str = "[redacted]";

fn decision_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Accept => "accept",
        Decision::AcceptForSession => "acceptForSession",
        Decision::Decline => "decline",
        Decision::Cancel => "cancel",
    }
}

fn action_name(action: ElicitationAction) -> &'static str {
    match action {
        ElicitationAction::Accept => "accept",
        ElicitationAction::Decline => "decline",
        ElicitationAction::Cancel => "cancel",
    }
}

fn secret_field(field: &Value) -> bool {
    studio_runtime_store::truthy(field.get("isSecret"))
        || studio_runtime_store::truthy(field.get("writeOnly"))
        || field["format"] == "password"
}

/// The typed answer to store and the provider response to send, for the data
/// of one user answer. A secret value is stored redacted and sent as given.
pub fn provider_answer(
    request: &Request,
    params: &ProviderObject,
    data: &Value,
) -> Result<(Answer, Value), CallbackError> {
    let decision = || -> Result<Option<Decision>, CallbackError> {
        match data.get("decision").map(Value::as_str) {
            None => Ok(None),
            Some(Some("accept")) => Ok(Some(Decision::Accept)),
            Some(Some("decline")) => Ok(Some(Decision::Decline)),
            Some(Some("cancel")) => Ok(Some(Decision::Cancel)),
            Some(_) => Err(CallbackError::Invalid("Choose accept, decline or cancel")),
        }
    };
    match request {
        Request::Exec { .. } | Request::Patch { .. } => {
            let decision =
                decision()?.ok_or(CallbackError::Invalid("Choose accept, decline or cancel"))?;
            Ok((
                Answer::Decision { decision },
                json!({"decision":decision_name(decision)}),
            ))
        }
        Request::Permissions { .. } => {
            let decision = decision()?.unwrap_or(Decision::Decline);
            let granted = if decision == Decision::Accept {
                params.0.get("permissions").cloned().unwrap_or(json!({}))
            } else {
                json!({})
            };
            Ok((
                Answer::Decision { decision },
                json!({"permissions":granted,"scope":"turn"}),
            ))
        }
        Request::Elicitation { schema } => {
            let action = match decision()?.unwrap_or(Decision::Decline) {
                Decision::Accept | Decision::AcceptForSession => ElicitationAction::Accept,
                Decision::Decline => ElicitationAction::Decline,
                Decision::Cancel => ElicitationAction::Cancel,
            };
            let accepted = action == ElicitationAction::Accept;
            let content = if accepted {
                data.get("content").cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            let mut stored = content.clone();
            if let (Some(object), Some(properties)) = (
                stored.as_object_mut(),
                schema.get("properties").and_then(Value::as_object),
            ) {
                for (name, field) in properties {
                    if secret_field(field) && object.contains_key(name) {
                        object.insert(name.clone(), json!(REDACTED));
                    }
                }
            }
            Ok((
                Answer::Elicitation {
                    action,
                    content: (!stored.is_null()).then_some(stored),
                },
                json!({"action":action_name(action),"content":content}),
            ))
        }
        Request::Question { questions } => {
            if !data["answers"].is_object() {
                return Err(CallbackError::Invalid("Supply question answers"));
            }
            let answers = questions
                .iter()
                .map(|question| QuestionAnswer {
                    id: question.id.clone(),
                    answer: if question.is_secret {
                        vec![REDACTED.to_owned()]
                    } else {
                        data["answers"][question.id.as_str()]
                            .get("answers")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|text| text.as_str().map(str::to_owned))
                            .collect()
                    },
                })
                .collect();
            Ok((
                Answer::Answers { answers },
                json!({"answers":data["answers"]}),
            ))
        }
        Request::ToolUse { .. }
        | Request::AgentQuestion { .. }
        | Request::MonitorStart { .. }
        | Request::BrowserOpen { .. } => Err(CallbackError::Invalid("Unsupported request type")),
    }
}

pub async fn answer(
    store: &RuntimeStore,
    context: &CallbackContext,
    id: &str,
    authority: AnswerAuthority,
    data: &Value,
    now: f64,
) -> Result<Reply, CallbackError> {
    if !matches!(authority, AnswerAuthority::User) {
        return Err(CallbackError::Invalid(
            "Only the user can answer native requests",
        ));
    }
    if !data.is_object() || data.to_string().len() > MAX_BYTES || !now.is_finite() {
        return Err(CallbackError::Invalid("Invalid callback answer"));
    }
    let context = context.clone();
    let id = id.to_owned();
    let data = data.clone();
    store
        .transaction_with(move |tx| answer_in(tx, &context, &id, &data, now))
        .await
}

/// The answer a state holds, if it holds one.
fn held_answer(state: &ApprovalState) -> Option<&Answer> {
    match state {
        ApprovalState::Answering { answer, .. }
        | ApprovalState::Answered { answer, .. }
        | ApprovalState::Unknown { answer, .. } => Some(answer),
        ApprovalState::Pending | ApprovalState::Closed { .. } => None,
    }
}

fn answer_in(
    tx: &Transaction<'_>,
    context: &CallbackContext,
    id: &str,
    data: &Value,
    _now: f64,
) -> Result<Reply, CallbackError> {
    let snapshot = storage::snapshot(tx, context)?;
    snapshot.active()?;
    let card = get_row(tx, id)?.ok_or(CallbackError::Invalid("Unknown request"))?;
    let rpc = delivery(&card)?;
    if card.agent.as_str() != context.agent
        || card.epoch != Some(context.epoch)
        || rpc.account != context.account
        || generation(&card) != Some(context.generation)
    {
        return Err(CallbackError::Stale);
    }
    if matches!(card.request, Request::Question { .. }) && snapshot.actor()["isLead"] != true {
        return Err(CallbackError::Invalid(
            "Only the orchestrator can ask the user",
        ));
    }
    let (answer, result) = provider_answer(&card.request, &card.detail.params, data)?;
    let identity = ReplyIdentity::new(context, &request_id(&rpc.rpc_id)?);
    let owner = storage::owner(tx, &identity)?.ok_or(CallbackError::Stale)?;
    if owner.payload.card.as_deref() != Some(id) {
        return Err(CallbackError::Stale);
    }
    let signature = owner
        .payload
        .signature
        .clone()
        .ok_or_else(|| CallbackError::Stored(format!("approvals/{id}/signature")))?;
    if let Some(given) = held_answer(&card.state) {
        if *given != answer {
            return Err(CallbackError::Conflict);
        }
        if !matches!(card.state, ApprovalState::Answered { .. }) {
            return Err(CallbackError::Unknown);
        }
        return storage::saved_reply_signed(tx, &identity, &signature)?
            .ok_or(CallbackError::Unknown);
    }
    if !matches!(card.state, ApprovalState::Pending) {
        return Err(CallbackError::Invalid("Request is no longer pending"));
    }
    let reply = Reply::new(identity, ResponseBody::Result(result))?;
    approvals::answer(tx, &card.id, answer, CommandId(reply.identity.key()))?;
    storage::save_reply_signed(tx, &signature, &reply)?;
    let mut outbox = storage::response(tx, &reply.identity)?.ok_or(CallbackError::Unknown)?;
    outbox.payload.card = Some(id.into());
    // The expiry timer ends with the pending state. A reply refused before it
    // left puts the card back, timer included.
    outbox.payload.reopen = Some(storage::Reopen {
        fire_at: card.fire_at,
    });
    storage::put_response(tx, &outbox)?;
    let answering = get_row(tx, id)?.ok_or(CallbackError::Stale)?;
    publish(tx, &answering)?;
    Ok(reply)
}

/// The reply was refused before it left: the approval waits for its answer
/// again, with the expiry timer it had.
pub(super) fn reopen(
    tx: &Transaction<'_>,
    id: &str,
    fire_at: Option<Timestamp>,
) -> Result<(), CallbackError> {
    let card = get_row(tx, id)?.ok_or(CallbackError::Stale)?;
    if matches!(card.state, ApprovalState::Answering { .. }) {
        let row = approvals::reopen(tx, &card.id, fire_at)?;
        publish(tx, &row)?;
    }
    Ok(())
}

/// Disconnect/stop/restart never infers approval or rebinds a replaced bridge.
pub async fn expire(
    store: &RuntimeStore,
    account: &str,
    connection: &str,
    now: f64,
) -> Result<usize, CallbackError> {
    let account = account.to_owned();
    let connection = connection.to_owned();
    store
        .transaction_with(move |tx| {
            let mut count = 0;
            for card in approvals::open_for_account(tx, &account)? {
                if card
                    .delivery
                    .as_ref()
                    .is_none_or(|delivery| delivery.connection != connection)
                {
                    continue;
                }
                if expire_card(tx, &card, now)? {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await
}

/// Decline every approval whose expiry timer fired (Claude full access mode).
/// The decline leaves through the outbox like any reply, so a restart before
/// the delivery keeps it. The caller delivers the returned replies.
pub async fn expire_due(store: &RuntimeStore, now: f64) -> Result<Vec<Reply>, CallbackError> {
    let due = store
        .read_async::<_, CallbackError>("runtime-services/provider_callbacks/expiry", move |db| {
            Ok(approvals::any_due(db, Timestamp(now))?)
        })
        .await?;
    if !due {
        return Ok(Vec::new());
    }
    store
        .transaction_with(move |tx| {
            let mut replies = Vec::new();
            for expiry in approvals::expire_due(tx, Timestamp(now))? {
                let card = get_row(tx, expiry.id.as_str())?.ok_or(CallbackError::Stale)?;
                publish(tx, &card)?;
                if let Some(epoch) = card.epoch {
                    resume_agent(tx, card.agent.as_str(), epoch)?;
                }
                match decline_reply(tx, &card, &expiry.effect) {
                    Ok(reply) => replies.push(reply),
                    // The agent or its session is gone: no provider waits for it.
                    Err(error @ (CallbackError::Stale | CallbackError::Dependency(_))) => {
                        eprintln!(
                            "expired approval has no provider to decline; card={}: {error}",
                            card.id.as_str()
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(replies)
        })
        .await
}

/// The outbox reply that declines an expired approval. The reply does not
/// belong to the card: its state is closed and stays closed.
fn decline_reply(
    tx: &Transaction<'_>,
    card: &Approval,
    effect: &Answer,
) -> Result<Reply, CallbackError> {
    let (_, result) = match effect {
        Answer::Decision { decision } => provider_answer(
            &card.request,
            &card.detail.params,
            &json!({"decision":decision_name(*decision)}),
        )?,
        Answer::Answers { .. } | Answer::Elicitation { .. } => {
            return Err(CallbackError::Invalid("Unsupported request type"));
        }
    };
    let context = provider_context(tx, card)?;
    let identity = ReplyIdentity::new(&context, &request_id(&delivery(card)?.rpc_id)?);
    let owner = storage::owner(tx, &identity)?.ok_or(CallbackError::Stale)?;
    let signature = owner.payload.signature.clone().ok_or_else(|| {
        CallbackError::Stored(format!("approvals/{}/signature", card.id.as_str()))
    })?;
    let reply = Reply::new(identity, ResponseBody::Result(result))?;
    storage::save_reply_signed(tx, &signature, &reply)?;
    let mut outbox = storage::response(tx, &reply.identity)?.ok_or(CallbackError::Unknown)?;
    outbox.payload.card = None;
    storage::put_response(tx, &outbox)?;
    Ok(reply)
}

pub async fn resolve(
    store: &RuntimeStore,
    context: &CallbackContext,
    rpc_id: &RequestId,
    now: f64,
) -> Result<usize, CallbackError> {
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    let context = context.clone();
    let rpc_id = rpc_id.clone();
    store
        .transaction_with(move |tx| {
            let rpc = self::rpc_id(&rpc_id)?;
            let identity = ReplyIdentity::new(&context, &rpc_id);
            // The owner exists whenever a card does, and keeps its own signature.
            storage::resolve_owner(tx, &identity, None)?;
            let mut count = 0;
            for card in approvals::open_for_agent(tx, &AgentId(context.agent.clone()))? {
                if card.epoch == Some(context.epoch)
                    && generation(&card) == Some(context.generation)
                    && card.delivery.as_ref().is_some_and(|delivery| {
                        delivery.account == context.account && delivery.rpc_id == rpc
                    })
                {
                    // Retire prepared/submitted replies before any delayed worker can
                    // submit or settle them; provider resolution is not approval.
                    // A pending card closes. An answer whose delivery started is not
                    // known to have arrived, so it becomes unknown.
                    let at = Timestamp(now);
                    match card.state {
                        ApprovalState::Pending => {
                            approvals::close(tx, &card.id, CloseReason::ProviderResolved, at)?;
                        }
                        ApprovalState::Answering { .. } => {
                            approvals::delivery_lost(tx, &card.id, at)?;
                        }
                        ApprovalState::Answered { .. }
                        | ApprovalState::Unknown { .. }
                        | ApprovalState::Closed { .. } => continue,
                    }
                    let after = get_row(tx, card.id.as_str())?.ok_or(CallbackError::Stale)?;
                    publish(tx, &after)?;
                    count += 1;
                }
            }
            resume_agent(tx, &context.agent, context.epoch)?;
            Ok(count)
        })
        .await
}

/// The last pending approval of an agent is gone: an agent that waited for it
/// runs again.
fn resume_agent(tx: &Transaction<'_>, agent: &str, epoch: u64) -> Result<(), CallbackError> {
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM approvals WHERE agent=?1 AND state='pending')",
        [agent],
        |r| r.get(0),
    )?;
    if !pending {
        let mut actor = records::get(tx, "runtime_agents", agent)?.ok_or(CallbackError::Stale)?;
        if actor["epoch"].as_u64() == Some(epoch)
            && actor["status"] == "approval"
            && studio_runtime_store::truthy(actor.get("inFlight"))
            && actor["autoWake"] == true
        {
            actor["status"] = json!("running");
            storage::save_actor(tx, &actor)?;
        }
    }
    Ok(())
}

pub(crate) fn answer_signature(data: &Value) -> String {
    let relevant: serde_json::Map<String, Value> = ["answers", "content", "decision"]
        .into_iter()
        .filter_map(|key| data.get(key).map(|v| (key.to_owned(), v.clone())))
        .collect();
    studio_runtime_store::sha256_hex(
        studio_runtime_store::python_json_utf8(&Value::Object(relevant)).as_bytes(),
    )
}

/// Context comes exclusively from the validated stored card and actor, never
/// from HTTP fields. Recheck the actor and card inside the answer transaction.
fn card_context(tx: &Transaction<'_>, card: &Approval) -> Result<CallbackContext, CallbackError> {
    let stored =
        |name: &str| CallbackError::Stored(format!("approvals/{}/{name}", card.id.as_str()));
    let rpc = delivery(card)?;
    let actor =
        records::get(tx, "runtime_agents", card.agent.as_str())?.ok_or(CallbackError::Stale)?;
    Ok(CallbackContext {
        agent: card.agent.as_str().to_owned(),
        account: rpc.account.clone(),
        stream_id: if actor["executionMode"] == "vm" {
            crate::account_worker::vm_journal_scope(&rpc.account, card.agent.as_str())
        } else {
            rpc.account.clone()
        },
        connection: actor
            .pointer("/startAttempt/connectionId")
            .and_then(Value::as_str)
            .filter(|connection| !connection.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| rpc.connection.clone()),
        generation: generation(card).ok_or(CallbackError::Stale)?,
        epoch: card.epoch.ok_or(CallbackError::Stale)?,
        thread: thread(card)
            .filter(|thread| !thread.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| stored("threadId"))?,
        turn: card
            .turn
            .as_ref()
            .map(|turn| turn.as_str().to_owned())
            .filter(|turn| !turn.is_empty())
            .or_else(|| {
                actor["turnId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .ok_or_else(|| stored("turn"))?,
    })
}

fn provider_context(
    tx: &Transaction<'_>,
    card: &Approval,
) -> Result<CallbackContext, CallbackError> {
    card_context(tx, card).map_err(|error| match error {
        CallbackError::Stored(_) | CallbackError::Stale => {
            CallbackError::Dependency("provider-session/question-answer")
        }
        error @ CallbackError::Expired
        | error @ CallbackError::Invalid(_)
        | error @ CallbackError::InvalidResultSchema(_)
        | error @ CallbackError::Dependency(_)
        | error @ CallbackError::Conflict
        | error @ CallbackError::Unknown
        | error @ CallbackError::Store(_) => error,
    })
}

/// Read the exact session identity needed by a pending native delete before
/// the durable answer intent is prepared. The lifecycle transaction rechecks
/// the card after this query; delivery still owns races after preflight.
pub async fn pending_native_delete_context(
    store: &RuntimeStore,
    id: &str,
) -> Result<Option<CallbackContext>, CallbackError> {
    let id = id.to_owned();
    store
        .read_async("runtime-services/provider_callbacks/cards", move |tx| {
            let card = get_row(tx, &id)?.ok_or(CallbackError::Invalid("Unknown question"))?;
            if matches!(card.state, ApprovalState::Pending)
                && !matches!(card.request, Request::AgentQuestion { .. })
            {
                provider_context(tx, &card).map(Some)
            } else {
                Ok(None)
            }
        })
        .await
}

pub async fn prepare_user_answer(
    store: &RuntimeStore,
    id: &str,
    data: &Value,
    now: f64,
    delete: bool,
) -> Result<Option<Reply>, CallbackError> {
    if !data.is_object() || data.to_string().len() > MAX_BYTES || !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback answer"));
    }
    let id = id.to_owned();
    let data = data.clone();
    store
        .transaction_with(move |tx| prepare_answer_in(tx, &id, &data, now, delete))
        .await
}
fn prepare_answer_in(
    tx: &Transaction<'_>,
    id: &str,
    data: &Value,
    now: f64,
    delete: bool,
) -> Result<Option<Reply>, CallbackError> {
    let card = get_row(tx, id)?.ok_or(CallbackError::Invalid("Unknown request"))?;
    if method_of(&card.request).is_none() {
        return Err(match card.request {
            Request::AgentQuestion { .. } => {
                CallbackError::Dependency("question-answer/async-question")
            }
            Request::MonitorStart { .. } => {
                CallbackError::Dependency("question-answer/monitor-approval")
            }
            Request::BrowserOpen { .. } => {
                CallbackError::Dependency("question-answer/browser-open")
            }
            Request::Exec { .. }
            | Request::Patch { .. }
            | Request::Permissions { .. }
            | Request::Elicitation { .. }
            | Request::Question { .. }
            | Request::ToolUse { .. } => CallbackError::Invalid(
                "Unsupported request type; stop the agent and use a supported Codex client",
            ),
        });
    }
    if let Some(given) = held_answer(&card.state) {
        let (answer, _) = provider_answer(&card.request, &card.detail.params, data)?;
        if *given != answer {
            return Err(CallbackError::Conflict);
        }
        if matches!(card.state, ApprovalState::Answered { .. }) {
            return Ok(None);
        }
        return Err(CallbackError::Unknown);
    }
    let context = provider_context(tx, &card)?;
    let reply = answer_in(tx, &context, id, data, now)?;
    if delete {
        let mut outbox = storage::response(tx, &reply.identity)?.ok_or(CallbackError::Unknown)?;
        outbox.payload.delete_requested_at = Some(now);
        storage::put_response(tx, &outbox)?;
    }
    Ok(Some(reply))
}

/// A form question or an agent question can be deleted. A delivery in flight
/// must settle first.
fn delete_allowed(row: &Approval, agent_deleted: bool) -> Result<bool, CallbackError> {
    let question = match &row.request {
        Request::AgentQuestion { .. } | Request::Question { .. } => true,
        Request::Elicitation { .. } => {
            row.detail.params.0.get("mode").and_then(Value::as_str) == Some("form")
        }
        Request::Exec { .. }
        | Request::Patch { .. }
        | Request::Permissions { .. }
        | Request::ToolUse { .. }
        | Request::MonitorStart { .. }
        | Request::BrowserOpen { .. } => false,
    };
    if !question {
        return Err(CallbackError::Invalid("Only a question can be deleted"));
    }
    if agent_deleted {
        return Err(CallbackError::Invalid("This agent was deleted"));
    }
    if row.hidden.is_some() {
        return Ok(false);
    }
    if matches!(
        row.state,
        ApprovalState::Answering { .. } | ApprovalState::Unknown { .. }
    ) {
        return Err(CallbackError::Invalid(
            "Question delivery is uncertain; wait for its receipt",
        ));
    }
    Ok(true)
}

/// History stays: a deleted card is hidden and keeps its state.
fn hide(row: &mut Approval, at: f64) {
    row.hidden = Some(Hidden {
        at: Timestamp(at),
        by: Principal::Owner,
    });
    if let Some(deferral) = row.deferral.as_mut() {
        deferral.active = false;
    }
}

pub async fn prepare_delete(
    store: &RuntimeStore,
    id: &str,
    now: f64,
) -> Result<Option<Reply>, CallbackError> {
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    let id = id.to_owned();
    store
        .transaction_with(move |tx| {
<<<<<<< head
            let mut card = get_row(tx, &id)?.ok_or(CallbackError::Invalid("Unknown question"))?;
            let actor = records::get(tx, "runtime_agents", card.agent.as_str())?
                .ok_or(CallbackError::Stale)?;
            if !delete_allowed(&card, studio_runtime_store::truthy(actor.get("deletedAt")))? {
||||||| base
            let mut card = records::get(tx, "runtime_requests", &id)?
                .ok_or(CallbackError::Invalid("Unknown question"))?;
            let actor = records::get(
                tx,
                "runtime_agents",
                card["agent"].as_str().ok_or(CallbackError::Stale)?,
            )?
            .ok_or(CallbackError::Stale)?;
            let allowed = crate::questions_write::state_only_delete_allowed(
                &card,
                studio_runtime_store::truthy(actor.get("deletedAt")),
                false,
            )
            .map_err(|error| match error {
                crate::questions_write::QuestionsWriteError::InvalidRequest(message) => {
                    CallbackError::Invalid(message)
                }
                crate::questions_write::QuestionsWriteError::QuestionAnswerUnavailable => {
                    CallbackError::Dependency("provider-session/question-answer")
                }
                crate::questions_write::QuestionsWriteError::Conflict(_)
                | crate::questions_write::QuestionsWriteError::InvalidStoredObject { .. }
                | crate::questions_write::QuestionsWriteError::Event(_)
                | crate::questions_write::QuestionsWriteError::Store(_)
                | crate::questions_write::QuestionsWriteError::Json(_)
                | crate::questions_write::QuestionsWriteError::Clock
                | crate::questions_write::QuestionsWriteError::Coordination(_)
                | crate::questions_write::QuestionsWriteError::Projects(_) => {
                    CallbackError::Invalid("Unable to delete question")
                }
            })?;
            if !allowed {
=======
            let mut card = records::get(tx, "runtime_requests", &id)?
                .ok_or(CallbackError::Invalid("Unknown question"))?;
            let actor = records::get(
                tx,
                "runtime_agents",
                card["agent"].as_str().ok_or(CallbackError::Stale)?,
            )?
            .ok_or(CallbackError::Stale)?;
            let allowed = crate::questions_write::state_only_delete_allowed(
                &card,
                studio_runtime_store::truthy(actor.get("deletedAt")),
                false,
            )
            .map_err(|error| match error {
                crate::questions_write::QuestionsWriteError::InvalidRequest(message) => {
                    CallbackError::Invalid(message)
                }
                crate::questions_write::QuestionsWriteError::QuestionAnswerUnavailable => {
                    CallbackError::Dependency("provider-session/question-answer")
                }
                crate::questions_write::QuestionsWriteError::Conflict(_)
                | crate::questions_write::QuestionsWriteError::InvalidStoredObject { .. }
                | crate::questions_write::QuestionsWriteError::Event(_)
                | crate::questions_write::QuestionsWriteError::Store(_)
                | crate::questions_write::QuestionsWriteError::Approval(_)
                | crate::questions_write::QuestionsWriteError::Json(_)
                | crate::questions_write::QuestionsWriteError::Clock
                | crate::questions_write::QuestionsWriteError::Coordination(_)
                | crate::questions_write::QuestionsWriteError::Projects(_) => {
                    CallbackError::Invalid("Unable to delete question")
                }
            })?;
            if !allowed {
>>>>>>> origin/feat/approval-entity
                return Ok(None);
            }
            if matches!(card.state, ApprovalState::Pending)
                && !matches!(card.request, Request::AgentQuestion { .. })
            {
                // A native answer is only possible for a card tied to a live
                // provider request. Missing callback identity is a named refusal,
                // checked before prepare_answer_in writes durable intent.
                provider_context(tx, &card)?;
                let data = if matches!(card.request, Request::Elicitation { .. }) {
                    json!({"decision":"cancel"})
                } else {
                    json!({"answers":{}})
                };
                return prepare_answer_in(tx, &id, &data, now, true);
            }
            hide(&mut card, now);
            save(tx, &card)?;
            Ok(None)
        })
        .await
}
