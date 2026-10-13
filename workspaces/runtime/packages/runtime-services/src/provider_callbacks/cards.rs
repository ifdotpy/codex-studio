use super::{model::*, storage};
use rusqlite::Transaction;
use serde_json::{Value, json};
use studio_runtime_providers::{RequestId, ResponseBody};
use studio_runtime_store::{QuestionsWriteRepository, RuntimeStore, system_reads as records};

pub(super) fn interactive(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
    )
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
        crate::tool_requests::reconcile_effect(tx, "requests", card, &actor)?;
    }
    let deleted = card["status"] != "pending";
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
    let key = format!("{}:card", identity.key());
    if let Some(saved) = storage::read(tx, &key)? {
        if saved["signature"] != input.signature()
            || saved["identity"] != serde_json::to_value(&identity)?
        {
            return Err(CallbackError::Conflict);
        }
        let id = saved["card"]
            .as_str()
            .ok_or_else(|| CallbackError::Stored(key.clone()))?;
        let card = records::get(tx, "runtime_requests", id)?
            .ok_or_else(|| CallbackError::Stored(key.clone()))?;
        return match card["status"].as_str() {
            Some("pending") => Ok(CallbackDecision::Pending(id.into())),
            _ => Err(CallbackError::Invalid("Request is no longer pending")),
        };
    }
    let pending:i64=tx.query_row("SELECT count(*) FROM runtime_requests WHERE json_extract(record,'$.accountKey')=?1 AND json_extract(record,'$.status') IN ('pending','answering')",[&context.account],|row|row.get(0))?;
    if pending >= MAX_PENDING as i64 {
        return Err(CallbackError::Invalid("Too many pending provider requests"));
    }
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, key.as_bytes()).to_string();
    let mut card = json!({"id":id,"rpcId":input.id,"method":input.method,"params":input.params,"agent":context.agent,"accountKey":context.account,"connectionId":context.connection,"epoch":context.epoch,"supervisor":{"generation":context.generation},"status":"pending","createdAt":now});
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
    storage::save(
        tx,
        &key,
        &json!({"identity":identity,"signature":input.signature(),"card":id}),
    )?;
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
    let choice = || -> Result<&str, CallbackError> {
        let decision = match data.get("decision") {
            None => "decline",
            Some(Value::String(value)) => value.as_str(),
            Some(_) => return Err(CallbackError::Invalid("Choose accept, decline or cancel")),
        };
        if matches!(decision, "accept" | "decline" | "cancel") {
            Ok(decision)
        } else {
            Err(CallbackError::Invalid("Choose accept, decline or cancel"))
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
            json!({"permissions":if data["decision"]=="accept" {params.get("permissions").cloned().unwrap_or(json!({}))}else{json!({})},"scope":"turn"})
        }
        "mcpServer/elicitation/request" => {
            json!({"action":choice()?,"content":if choice()?=="accept" {data.get("content").cloned().unwrap_or(Value::Null)} else {Value::Null}})
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
pub fn answer(
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
    store.transaction_with(|tx| answer_in(tx, context, id, data, now))
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
        || card["connectionId"] != context.connection
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
        if card["status"] != "answered" {
            return Err(CallbackError::Unknown);
        }
        return storage::saved_reply(tx, &identity, &input)?.ok_or(CallbackError::Unknown);
    }
    if card["status"] != "pending" {
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
    card["status"] = json!("answering");
    let reply = Reply::new(identity, ResponseBody::Result(result))?;
    storage::save_reply(tx, &input, &reply)?;
    let key = reply.identity.key();
    let mut outbox = storage::read(tx, &key)?.ok_or(CallbackError::Unknown)?;
    outbox["card"] = json!(id);
    outbox["unanswered"] = unanswered;
    if let Some(admission) = storage::read(tx, &format!("{key}:card"))? {
        if let Some(cursor) = admission.get("journalCursor") {
            outbox["journalCursor"] = cursor.clone();
        }
    } else {
        // Python already admitted this card without the Rust worker journal.
        // Keep a replay tombstone, but its Written payload needs no Rust ACK.
        outbox["legacyAdmission"] = json!(true);
    }
    storage::update(tx, &key, &outbox)?;
    save(tx, &card)?;
    Ok(reply)
}

/// Disconnect/stop/restart never infers approval or rebinds a replaced bridge.
pub fn expire(
    store: &RuntimeStore,
    account: &str,
    connection: &str,
) -> Result<usize, CallbackError> {
    store.transaction_with(|tx| {
        let mut count = 0;
        for mut card in records::all(tx, "runtime_requests")? {
            if card["accountKey"] != account || card["connectionId"] != connection {
                continue;
            }
            let next = match card["status"].as_str() {
                Some("pending") => "expired",
                Some("answering") => "uncertain",
                _ => continue,
            };
            card["status"] = json!(next);
            tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.delivery','unknown') WHERE json_extract(result,'$.card')=?1 AND json_extract(result,'$.delivery') IN ('prepared','submitted')", [card["id"].as_str().ok_or(CallbackError::Invalid("Invalid callback card identity"))?])?;
            save(tx, &card)?;
            count += 1;
        }
        Ok(count)
    })
}
pub fn resolve(
    store: &RuntimeStore,
    context: &CallbackContext,
    rpc_id: &RequestId,
    now: f64,
) -> Result<usize, CallbackError> {
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    store.transaction_with(|tx| {
        let identity = ReplyIdentity::new(context, rpc_id);
        let resolution_key = format!("{}:resolved", identity.key());
        let mut card_signature = None;
        for card in records::all(tx, "runtime_requests")? {
            if card["agent"] == context.agent
                && card["accountKey"] == context.account
                && card["connectionId"] == context.connection
                && card["epoch"].as_u64() == Some(context.epoch)
                && card
                    .pointer("/supervisor/generation")
                    .and_then(Value::as_u64)
                    == Some(context.generation)
                && card["rpcId"] == serde_json::to_value(rpc_id)?
            {
                let card_id = card["id"]
                    .as_str()
                    .ok_or_else(|| CallbackError::Stored("runtime_requests/id".into()))?;
                let input = Input {
                    id: serde_json::from_value(card["rpcId"].clone())
                        .map_err(|_| CallbackError::Stored(format!("runtime_requests/{card_id}")))?,
                    method: card["method"]
                        .as_str()
                        .ok_or_else(|| CallbackError::Stored(format!("runtime_requests/{card_id}")))?
                        .into(),
                    params: card
                        .get("params")
                        .filter(|params| params.is_object())
                        .cloned()
                        .ok_or_else(|| CallbackError::Stored(format!("runtime_requests/{card_id}")))?,
                };
                input.check_context(context)?;
                let signature = input.signature();
                if card_signature
                    .as_ref()
                    .is_some_and(|previous| previous != &signature)
                {
                    return Err(CallbackError::Conflict);
                }
                card_signature = Some(signature);
            }
        }
        let receipt_signature = storage::read(tx, &identity.key())?
            .or(storage::read(tx, &format!("{}:card", identity.key()))?)
            .and_then(|receipt| receipt["signature"].as_str().map(str::to_owned));
        if card_signature
            .as_ref()
            .zip(receipt_signature.as_ref())
            .is_some_and(|(card, receipt)| card != receipt)
        {
            return Err(CallbackError::Conflict);
        }
        let known_signature = card_signature.or(receipt_signature);
        if let Some(previous) = storage::read(tx, &resolution_key)? {
            if previous["identity"] != serde_json::to_value(&identity)? {
                return Err(CallbackError::Stale);
            }
            if let Some(signature) = known_signature {
                if previous
                    .get("signature")
                    .is_some_and(|saved| saved != &signature)
                {
                    return Err(CallbackError::Conflict);
                }
                if previous.get("signature").is_none() {
                    let mut proof = previous;
                    proof["signature"] = json!(signature);
                    storage::update(tx, &resolution_key, &proof)?;
                }
            }
        } else {
            let mut proof = json!({"identity":identity,"resolvedAt":now});
            if let Some(signature) = known_signature {
                proof["signature"] = json!(signature);
            }
            storage::save(tx, &resolution_key, &proof)?;
        }
        let mut count = 0;
        for mut card in records::all(tx, "runtime_requests")? {
            if card["agent"] == context.agent
                && card["accountKey"] == context.account
                && card["connectionId"] == context.connection
                && card["epoch"].as_u64() == Some(context.epoch)
                && card
                    .pointer("/supervisor/generation")
                    .and_then(Value::as_u64)
                    == Some(context.generation)
                && card["rpcId"] == serde_json::to_value(rpc_id)?
                && matches!(card["status"].as_str(), Some("pending" | "answering" | "uncertain" | "blocked"))
            {
                card["status"] = json!("resolved");
                card["resolvedAt"] = json!(now);
                // Retire prepared/submitted replies before any delayed worker can
                // submit or settle them; provider resolution is not approval.
                tx.execute("UPDATE runtime_operation_receipts SET result=json_remove(json_set(result,'$.delivery','resolved'),'$.body','$.unanswered') WHERE json_extract(result,'$.card')=?1 AND json_extract(result,'$.delivery')!='written'", [card["id"].as_str().ok_or(CallbackError::Stored("runtime_requests/id".into()))?])?;
                save(tx, &card)?;
                count += 1;
            }
        }
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_requests WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.status')='pending')", [&context.agent], |r| r.get(0))?;
        if !pending {
            let mut actor = records::get(tx, "runtime_agents", &context.agent)?.ok_or(CallbackError::Stale)?;
            if actor["epoch"].as_u64() == Some(context.epoch) && actor["status"] == "approval" && studio_runtime_store::truthy(actor.get("inFlight")) && actor["autoWake"] == true {
                actor["status"] = json!("running");
                storage::save_actor(tx, &actor)?;
            }
        }
        Ok(count)
    })
}

fn answer_signature(data: &Value) -> String {
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
        agent,
        account: card["accountKey"].as_str().unwrap_or("default").into(),
        connection: required(&card["connectionId"])?,
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

pub fn prepare_user_answer(
    store: &RuntimeStore,
    id: &str,
    data: &Value,
    now: f64,
    delete: bool,
) -> Result<Option<Reply>, CallbackError> {
    if !data.is_object() || data.to_string().len() > MAX_BYTES || !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback answer"));
    }
    store.transaction_with(|tx| prepare_answer_in(tx, id, data, now, delete))
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
        if card["status"] == "answered" {
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
    let context = card_context(tx, &card)?;
    let reply = answer_in(tx, &context, id, data, now)?;
    if delete {
        let key = reply.identity.key();
        let mut outbox = storage::read(tx, &key)?.ok_or(CallbackError::Unknown)?;
        outbox["deleteRequestedAt"] = json!(now);
        storage::update(tx, &key, &outbox)?;
    }
    Ok(Some(reply))
}

pub fn prepare_delete(
    store: &RuntimeStore,
    id: &str,
    now: f64,
) -> Result<Option<Reply>, CallbackError> {
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    store.transaction_with(|tx| {
        let mut card = records::get(tx, "runtime_requests", id)?
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
            _ => CallbackError::Invalid("Unable to delete question"),
        })?;
        if !allowed {
            return Ok(None);
        }
        let method = card["method"].as_str().unwrap_or_default();
        if card["status"] == "pending" && method != "agent/asyncQuestion" {
            let data = if method == "mcpServer/elicitation/request" {
                json!({"decision":"cancel"})
            } else {
                json!({"answers":{}})
            };
            return prepare_answer_in(tx, id, &data, now, true);
        }
        crate::questions_write::mark_state_only_deleted(&mut card, now);
        save(tx, &card)?;
        Ok(None)
    })
}
