use super::{model::*, storage};
use rusqlite::Transaction;
use serde_json::{Value, json};
use studio_runtime_providers::ProviderKind;
use studio_runtime_providers::{RequestId, ResponseBody};
use studio_runtime_schema::persistence::RequestCardStatus;
use studio_runtime_schema::question_answers::{
    QuestionApproval, QuestionCardAnswer, QuestionDecision,
};
use studio_runtime_store::{QuestionsWriteRepository, RuntimeStore, system_reads as records};

/// The native provider requests that wait for the user as a request card. The
/// web client renders each of them (`components/questions/Requests.tsx`).
pub const NATIVE_REQUEST_METHODS: [&str; 5] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
];

pub(super) fn status(card: &Value) -> Result<RequestCardStatus, CallbackError> {
    serde_json::from_value(card["status"].clone())
        .map_err(|_| CallbackError::Stored("runtime_requests/status".into()))
}

pub fn interactive(method: &str) -> bool {
    NATIVE_REQUEST_METHODS.contains(&method)
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
/// cannot send it a second time. `cursor` is the request's journal cursor: it
/// lets the written answer's payload retire with the worker checkpoint.
pub async fn admit_native(
    store: std::sync::Arc<RuntimeStore>,
    context: &CallbackContext,
    frame: &studio_runtime_providers::Frame,
    cursor: u64,
    now: f64,
) -> Result<NativeAdmission, CallbackError> {
    let request = Input::from_frame(frame)?
        .filter(|request| interactive(&request.method))
        .ok_or(CallbackError::Invalid("Expected a native provider request"))?;
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    if cursor == 0 {
        return Err(CallbackError::Invalid("Invalid callback journal cursor"));
    }
    let context = context.clone();
    store
        .transaction_with(move |tx| {
            let identity = ReplyIdentity::new(&context, &request.id);
            if let Some(decision) = storage::saved_decision(tx, &identity, &request)? {
                return Ok(match decision {
                    CallbackDecision::Pending(id) => {
                        let mut card = records::get(tx, "runtime_requests", &id)?
                            .ok_or(CallbackError::Stale)?;
                        card["connectionId"] = json!(context.connection);
                        save(tx, &card)?;
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
                && let Some(result) = full_access_answer(&request.method, &request.params)?
            {
                return Ok(NativeAdmission::Answer(ResponseBody::Result(result)));
            }
            let CallbackDecision::Pending(id) = admit(tx, &context, &snapshot, &request, now)?
            else {
                return Err(CallbackError::Unknown);
            };
            let mut owner = storage::owner(tx, &identity)?.ok_or(CallbackError::Unknown)?;
            owner.payload.journal_cursor = Some(cursor);
            storage::put_owner(tx, &owner)?;
            Ok(NativeAdmission::Card(id))
        })
        .await
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
    let rows = {
        let mut statement = tx.prepare_cached(
            "SELECT id,record FROM runtime_requests WHERE json_valid(record) \
             AND json_extract(record,'$.status') IN (?6,?7) \
             AND json_extract(record,'$.method') IN (?1,?2,?3,?4,?5)",
        )?;
        statement
            .query_map(
                rusqlite::params![
                    NATIVE_REQUEST_METHODS[0],
                    NATIVE_REQUEST_METHODS[1],
                    NATIVE_REQUEST_METHODS[2],
                    NATIVE_REQUEST_METHODS[3],
                    NATIVE_REQUEST_METHODS[4],
                    RequestCardStatus::Pending,
                    RequestCardStatus::Answering
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut count = 0;
    for (id, raw) in rows {
        // A row this build cannot read keeps its state; it was never admitted here.
        let Ok(mut card) = records::decode("runtime_requests", &id, &raw) else {
            continue;
        };
        if card
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != account
            || card
                .pointer("/supervisor/generation")
                .and_then(Value::as_u64)
                == Some(generation)
        {
            continue;
        }
        // One card that cannot be saved must not stop the worker start: its
        // writes roll back and it keeps its state.
        tx.execute_batch("SAVEPOINT native_card_expiry")?;
        match expire_card(tx, &mut card) {
            Ok(expired) => {
                tx.execute_batch("RELEASE native_card_expiry")?;
                count += usize::from(expired);
            }
            Err(error) => {
                tx.execute_batch("ROLLBACK TO native_card_expiry; RELEASE native_card_expiry")?;
                eprintln!(
                    "native request card kept its state; account={account} card={id} generation={generation}: {error}"
                );
            }
        }
    }
    Ok(count)
}

/// A pending card expires; an answer whose delivery started becomes uncertain.
fn expire_card(tx: &Transaction<'_>, card: &mut Value) -> Result<bool, CallbackError> {
    let next = match status(card)? {
        RequestCardStatus::Pending => RequestCardStatus::Expired,
        RequestCardStatus::Answering => RequestCardStatus::Uncertain,
        RequestCardStatus::Answered
        | RequestCardStatus::Uncertain
        | RequestCardStatus::Blocked
        | RequestCardStatus::Resolved
        | RequestCardStatus::Expired
        | RequestCardStatus::Deleted => return Ok(false),
    };
    card["status"] = json!(next);
    storage::hold_card(tx, card["id"].as_str().ok_or(CallbackError::Stale)?)?;
    save(tx, card)?;
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
                ResponseBody::Result(json!({"decision":QuestionDecision::Decline}))
            }
            "item/permissions/requestApproval" => {
                ResponseBody::Result(json!({"permissions":{},"scope":"turn"}))
            }
            "mcpServer/elicitation/request" => {
                ResponseBody::Result(json!({"action":QuestionDecision::Decline,"content":null}))
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

/// Tool approvals share the same answer conversion as explicit user answers.
/// Questions and elicitation always continue to require an answer from the user.
fn full_access_answer(method: &str, params: &Value) -> Result<Option<Value>, CallbackError> {
    let data = QuestionApproval {
        decision: QuestionDecision::Accept,
    };
    let answer = match method {
        "item/commandExecution/requestApproval" => QuestionCardAnswer::Command(data),
        "item/fileChange/requestApproval" => QuestionCardAnswer::File(data),
        "item/permissions/requestApproval" => QuestionCardAnswer::Permissions(data),
        _ => return Ok(None),
    };
    let answer = serde_json::to_value(answer)?;
    answer_value(method, params, &answer["data"]).map(|(result, _)| Some(result))
}

pub(super) fn save(tx: &Transaction<'_>, card: &Value) -> Result<(), CallbackError> {
    let id = card["id"]
        .as_str()
        .ok_or(CallbackError::Invalid("Invalid callback card identity"))?;
    records::validate("runtime_requests", id, card.clone())?;
    QuestionsWriteRepository::put_request(tx, id, &card.to_string())?;
    let actor = card["agent"]
        .as_str()
        .map(|id| records::get(tx, "runtime_agents", id))
        .transpose()?
        .flatten();
    if let Some(actor) = actor {
        crate::tool_requests::reconcile_effect(
            tx,
            studio_runtime_schema::persistence::ExecutionEffectKind::Requests,
            card,
            &actor,
        )?;
    }
    let deleted = status(card)? != RequestCardStatus::Pending;
    let value = if deleted {
        json!({})
    } else {
        crate::sync_views::project_persisted_record("request", card)
            .ok_or_else(|| CallbackError::Stored(format!("runtime_requests/{id}")))?
    };
    studio_runtime_store::SyncDocumentsRepository::put_entity(tx, "request", id, value, deleted)?;
    Ok(())
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
    let pending:i64=tx.query_row("SELECT count(*) FROM runtime_requests WHERE json_extract(record,'$.accountKey')=?1 AND json_extract(record,'$.status') IN (?2,?3)",rusqlite::params![context.account,RequestCardStatus::Pending,RequestCardStatus::Answering],|row|row.get(0))?;
    if pending >= MAX_PENDING as i64 {
        return Err(CallbackError::Invalid("Too many pending provider requests"));
    }
    let id = owner.id.clone();
    let mut card = json!({"id":id,"rpcId":input.id,"method":input.method,"params":input.params,"agent":context.agent,"accountKey":context.account,"connectionId":context.connection,"epoch":context.epoch,"supervisor":{"generation":context.generation},"status":RequestCardStatus::Pending,"createdAt":now});
    if let Some(item_id) = input.params["itemId"].as_str() {
        let key = format!("{}:{item_id}", context.agent);
        if let Some(item) = records::get(tx, "runtime_items", &key)?
            && let Some(preview) = item["text"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
        {
            card["preview"] = preview;
        }
    }
    save(tx, &card)?;
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
/// Pure provider response and redacted history, shared by every native card.
pub fn answer_value(
    method: &str,
    params: &Value,
    data: &Value,
) -> Result<(Value, Value), CallbackError> {
    let choice = || -> Result<QuestionDecision, CallbackError> {
        match data.get("decision") {
            None => Ok(QuestionDecision::Decline),
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|_| CallbackError::Invalid("Choose accept, decline or cancel")),
        }
    };
    let result = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            if data.get("decision").is_none() {
                return Err(CallbackError::Invalid("Choose accept, decline or cancel"));
            }
            json!({"decision":choice()?})
        }
        "item/tool/requestUserInput" => {
            if !data["answers"].is_object() {
                return Err(CallbackError::Invalid("Supply question answers"));
            }
            json!({"answers":data["answers"]})
        }
        "item/permissions/requestApproval" => {
            json!({"permissions":if matches!(serde_json::from_value(data["decision"].clone()), Ok(QuestionDecision::Accept)) {params.get("permissions").cloned().unwrap_or(json!({}))}else{json!({})},"scope":"turn"})
        }
        "mcpServer/elicitation/request" => {
            let decision = choice()?;
            json!({"action":decision,"content":if matches!(decision, QuestionDecision::Accept) {data.get("content").cloned().unwrap_or(Value::Null)} else {Value::Null}})
        }
        _ => return Err(CallbackError::Invalid("Unsupported request type")),
    };
    let fields = if method == "mcpServer/elicitation/request" {
        params
            .pointer("/requestedSchema/properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(id, field)| {
                json!({
                    "id":id,
                    "question":field.get("title").unwrap_or(&json!(id)),
                    "isSecret":studio_runtime_store::truthy(field.get("isSecret"))
                        || studio_runtime_store::truthy(field.get("writeOnly"))
                        || field["format"] == "password"
                })
            })
            .collect::<Vec<_>>()
    } else {
        params["questions"].as_array().cloned().unwrap_or_default()
    };
    let history = fields.iter().map(|field| {
        let id = field["id"].as_str().ok_or(CallbackError::Invalid("Invalid question field"))?;
        let secret = studio_runtime_store::truthy(field.get("isSecret"));
        let answer = if secret {
            json!("[redacted]")
        } else if data.get("answers").is_some() {
            data["answers"][id].get("answers").cloned().unwrap_or(json!([]))
        } else {
            data["content"][id].clone()
        };
        Ok(json!({"id":id,"question":field.get("question").unwrap_or(&json!(id)),"isSecret":secret,"answer":answer}))
    }).collect::<Result<Vec<_>, CallbackError>>()?;
    Ok((result, json!(history)))
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
fn answer_in(
    tx: &Transaction<'_>,
    context: &CallbackContext,
    id: &str,
    data: &Value,
    now: f64,
) -> Result<Reply, CallbackError> {
    let snapshot = storage::snapshot(tx, context)?;
    snapshot.active()?;
    let mut card = records::get(tx, "runtime_requests", id)?
        .ok_or(CallbackError::Invalid("Unknown request"))?;
    if card["agent"] != context.agent
        || card["epoch"].as_u64() != Some(context.epoch)
        || card["accountKey"] != context.account
        || card
            .pointer("/supervisor/generation")
            .and_then(Value::as_u64)
            != Some(context.generation)
    {
        return Err(CallbackError::Stale);
    }
    let method = card["method"]
        .as_str()
        .ok_or_else(|| CallbackError::Stored(id.into()))?
        .to_owned();
    if method == "item/tool/requestUserInput" && snapshot.actor()["isLead"] != true {
        return Err(CallbackError::Invalid(
            "Only the orchestrator can ask the user",
        ));
    }
    let signature = answer_signature(data);
    let input = Input {
        id: serde_json::from_value::<RequestId>(card["rpcId"].clone())?,
        method,
        params: card["params"].clone(),
    };
    let identity = ReplyIdentity::new(context, &input.id);
    if let Some(previous) = card.get("answerSignature") {
        if previous != &signature {
            return Err(CallbackError::Conflict);
        }
        if status(&card)? != RequestCardStatus::Answered {
            return Err(CallbackError::Unknown);
        }
        return storage::saved_reply(tx, &identity, &input)?.ok_or(CallbackError::Unknown);
    }
    if status(&card)? != RequestCardStatus::Pending {
        return Err(CallbackError::Invalid("Request is no longer pending"));
    }
    let unanswered = card.clone();
    let (result, history) = answer_value(&input.method, &input.params, data)?;
    card["answerSignature"] = json!(signature);
    card["answeredBy"] = json!("user");
    card["answeredAt"] = json!(now);
    card["decision"] = data.get("decision").cloned().unwrap_or(json!("answer"));
    card["deferred"] = json!(false);
    card["answerHistory"] = history;
    card["status"] = json!(RequestCardStatus::Answering);
    let reply = Reply::new(identity, ResponseBody::Result(result))?;
    storage::save_reply(tx, &input, &reply)?;
    let mut outbox = storage::response(tx, &reply.identity)?.ok_or(CallbackError::Unknown)?;
    outbox.payload.card = Some(id.into());
    outbox.payload.unanswered = Some(unanswered);
    storage::put_response(tx, &outbox)?;
    save(tx, &card)?;
    Ok(reply)
}

/// Disconnect/stop/restart never infers approval or rebinds a replaced bridge.
pub async fn expire(
    store: &RuntimeStore,
    account: &str,
    connection: &str,
) -> Result<usize, CallbackError> {
    let account = account.to_owned();
    let connection = connection.to_owned();
    store
        .transaction_with(move |tx| {
            let mut count = 0;
            for mut card in records::all(tx, "runtime_requests")? {
                if card["accountKey"] != account || card["connectionId"] != connection {
                    continue;
                }
                if expire_card(tx, &mut card)? {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await
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
    store.transaction_with(move |tx| {
        let rpc_value = serde_json::to_value(&rpc_id)?;
        let identity = ReplyIdentity::new(&context, &rpc_id);
        let matching = matching_rpc_cards(tx, &context, &rpc_id)?;
        let signature = matching.first().map(|card| Input {
            id: rpc_id.clone(), method: card["method"].as_str().unwrap_or_default().into(), params: card["params"].clone(),
        }.signature());
        storage::resolve_owner(tx, &identity, signature.as_deref())?;
        let mut count = 0;
        for mut card in records::all(tx, "runtime_requests")? {
            if card["agent"] == context.agent
                && card["accountKey"] == context.account
                && card["epoch"].as_u64() == Some(context.epoch)
                && card
                    .pointer("/supervisor/generation")
                    .and_then(Value::as_u64)
                    == Some(context.generation)
                && card["rpcId"] == rpc_value
                && matches!(status(&card)?, RequestCardStatus::Pending | RequestCardStatus::Answering | RequestCardStatus::Uncertain | RequestCardStatus::Blocked)
            {
                card["status"] = json!(RequestCardStatus::Resolved);
                card["resolvedAt"] = json!(now);
                // Retire prepared/submitted replies before any delayed worker can
                // submit or settle them; provider resolution is not approval.
                save(tx, &card)?;
                count += 1;
            }
        }
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.status')=?2)", rusqlite::params![context.agent,RequestCardStatus::Pending], |r| r.get(0))?;
        if !pending {
            let mut actor = records::get(tx, "runtime_agents", &context.agent)?.ok_or(CallbackError::Stale)?;
            if actor["epoch"].as_u64() == Some(context.epoch) && actor["status"] == "approval" && studio_runtime_store::truthy(actor.get("inFlight")) && actor["autoWake"] == true {
                actor["status"] = json!("running");
                storage::save_actor(tx, &actor)?;
            }
        }
        Ok(count)
    }).await
}

fn matching_rpc_cards(
    tx: &Transaction<'_>,
    context: &CallbackContext,
    rpc_id: &RequestId,
) -> Result<Vec<Value>, CallbackError> {
    let rpc_id = serde_json::to_string(rpc_id)?;
    let mut statement = tx.prepare_cached(
        "SELECT record FROM runtime_requests \
         WHERE json_extract(record,'$.agent')=?1 \
           AND json_extract(record,'$.accountKey')=?2 \
           AND json_extract(record,'$.epoch')=?3 \
           AND json_extract(record,'$.supervisor.generation')=?4 \
           AND json_extract(record,'$.rpcId')=json_extract(?5,'$')",
    )?;
    let rows = statement.query_map(
        rusqlite::params![
            context.agent,
            context.account,
            i64::try_from(context.epoch).map_err(|_| CallbackError::Stale)?,
            i64::try_from(context.generation).map_err(|_| CallbackError::Stale)?,
            rpc_id,
        ],
        |row| row.get::<_, String>(0),
    )?;
    rows.map(|row| {
        let raw = row?;
        serde_json::from_str(&raw)
            .map_err(|_| CallbackError::Stored("runtime_requests/matching-rpc".into()))
    })
    .collect()
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
fn card_context(tx: &Transaction<'_>, card: &Value) -> Result<CallbackContext, CallbackError> {
    let required = |v: &Value| {
        v.as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| CallbackError::Stored("runtime_requests/context".into()))
    };
    let agent = required(&card["agent"])?;
    let actor = records::get(tx, "runtime_agents", &agent)?.ok_or(CallbackError::Stale)?;
    Ok(CallbackContext {
        agent: agent.clone(),
        account: card["accountKey"].as_str().unwrap_or("default").into(),
        stream_id: if actor["executionMode"] == "vm" {
            crate::account_worker::vm_journal_scope(
                card["accountKey"].as_str().unwrap_or("default"),
                &agent,
            )
        } else {
            card["accountKey"].as_str().unwrap_or("default").into()
        },
        connection: actor
            .pointer("/startAttempt/connectionId")
            .and_then(Value::as_str)
            .filter(|connection| !connection.is_empty())
            .map(str::to_owned)
            .map(Ok)
            .unwrap_or_else(|| required(&card["connectionId"]))?,
        generation: card
            .pointer("/supervisor/generation")
            .and_then(Value::as_u64)
            .ok_or(CallbackError::Stale)?,
        epoch: card["epoch"].as_u64().ok_or(CallbackError::Stale)?,
        thread: required(&card["params"]["threadId"])?,
        turn: card["params"]["turnId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .map(Ok)
            .unwrap_or_else(|| required(&actor["turnId"]))?,
    })
}

fn provider_context(tx: &Transaction<'_>, card: &Value) -> Result<CallbackContext, CallbackError> {
    card_context(tx, card).map_err(|error| match error {
        CallbackError::Stored(_) | CallbackError::Stale => {
            CallbackError::Dependency("provider-session/question-answer")
        }
        error @ CallbackError::Invalid(_)
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
            let card = records::get(tx, "runtime_requests", &id)?
                .ok_or(CallbackError::Invalid("Unknown question"))?;
            let method = card["method"].as_str().unwrap_or_default();
            if status(&card)? == RequestCardStatus::Pending && method != "agent/asyncQuestion" {
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
    let card = records::get(tx, "runtime_requests", id)?
        .ok_or(CallbackError::Invalid("Unknown request"))?;
    if let Some(signature) = card.get("answerSignature") {
        if signature != &answer_signature(data) {
            return Err(CallbackError::Conflict);
        }
        if status(&card)? == RequestCardStatus::Answered {
            return Ok(None);
        }
        return Err(CallbackError::Unknown);
    }
    if !interactive(card["method"].as_str().unwrap_or_default()) {
        return Err(match card["method"].as_str() {
            Some("agent/asyncQuestion") => {
                CallbackError::Dependency("question-answer/async-question")
            }
            Some("monitor/approve") => {
                CallbackError::Dependency("question-answer/monitor-approval")
            }
            Some("browser/open") => CallbackError::Dependency("question-answer/browser-open"),
            _ => CallbackError::Invalid(
                "Unsupported request type; stop the agent and use a supported Codex client",
            ),
        });
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
                return Ok(None);
            }
            let method = card["method"].as_str().unwrap_or_default();
            if status(&card)? == RequestCardStatus::Pending && method != "agent/asyncQuestion" {
                // A native answer is only possible for a card tied to a live
                // provider request. Missing callback identity is a named refusal,
                // checked before prepare_answer_in writes durable intent.
                provider_context(tx, &card)?;
                let data = if method == "mcpServer/elicitation/request" {
                    json!({"decision":QuestionDecision::Cancel})
                } else {
                    json!({"answers":{}})
                };
                return prepare_answer_in(tx, &id, &data, now, true);
            }
            crate::questions_write::mark_state_only_deleted(&mut card, now);
            save(tx, &card)?;
            Ok(None)
        })
        .await
}
