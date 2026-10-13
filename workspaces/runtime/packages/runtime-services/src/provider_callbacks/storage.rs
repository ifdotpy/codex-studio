//! Existing operation receipts hold protected response payloads. They are not
//! request-card history and are never copied to sync projections.
use super::model::*;
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use studio_runtime_providers::ResponseBody;
use studio_runtime_store::{EventWriteRepository, RuntimeStore, system_reads as records};

pub(super) fn snapshot(
    tx: &Transaction<'_>,
    context: &CallbackContext,
) -> Result<CallbackSnapshot, CallbackError> {
    let raw: String = tx.query_row(
        "SELECT record FROM runtime_agents WHERE id=?1",
        [&context.agent],
        |row| row.get(0),
    )?;
    CallbackSnapshot::from_record(&raw, context)
}
pub(super) fn save_actor(tx: &Transaction<'_>, actor: &Value) -> Result<(), CallbackError> {
    let id = actor["id"]
        .as_str()
        .ok_or(CallbackError::Stored("runtime_agents/id".into()))?;
    records::validate("runtime_agents", id, actor.clone())?;
    let changed = tx.execute(
        "UPDATE runtime_agents SET record=?2 WHERE id=?1",
        params![id, actor.to_string()],
    )?;
    if changed != 1 {
        return Err(CallbackError::Stale);
    }
    crate::agents_create::publish_agent(tx, actor)?;
    Ok(())
}
pub(super) fn read(tx: &Transaction<'_>, key: &str) -> Result<Option<Value>, CallbackError> {
    EventWriteRepository::receipt(tx, key)?
        .map(|row| serde_json::from_str(&row.result).map_err(|_| CallbackError::Stored(key.into())))
        .transpose()
}
pub(super) fn save(tx: &Transaction<'_>, key: &str, value: &Value) -> Result<(), CallbackError> {
    if value.to_string().len() > MAX_BYTES {
        return Err(CallbackError::Invalid(
            "Callback result exceeds durable reply bound",
        ));
    }
    if let Some(old) = read(tx, key)? {
        if old != *value {
            return Err(CallbackError::Conflict);
        }
    } else {
        EventWriteRepository::put_receipt(tx, key, "provider-callback", &value.to_string())?;
    }
    Ok(())
}
pub(super) fn update(tx: &Transaction<'_>, key: &str, value: &Value) -> Result<(), CallbackError> {
    if value.to_string().len() > MAX_BYTES {
        return Err(CallbackError::Invalid(
            "Callback result exceeds durable reply bound",
        ));
    }
    let changed = tx.execute(
        "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
        params![value.to_string(), key],
    )?;
    if changed != 1 {
        return Err(CallbackError::Stored(key.into()));
    }
    Ok(())
}
pub(super) fn save_reply(
    tx: &Transaction<'_>,
    input: &Input,
    reply: &Reply,
) -> Result<(), CallbackError> {
    input.check_context(&reply.identity.context)?;
    let outstanding: i64 = tx.query_row("SELECT count(*) FROM runtime_operation_receipts WHERE signature='provider-callback' AND json_extract(result,'$.identity.context.account')=?1 AND json_extract(result,'$.delivery') IN ('prepared','submitted','unknown')", [&reply.identity.context.account], |r| r.get(0))?;
    if outstanding >= MAX_PENDING as i64 {
        return Err(CallbackError::Invalid(
            "Too many unresolved provider replies",
        ));
    }
    let body = match &reply.body {
        ResponseBody::Result(value) => json!({"result":value}),
        ResponseBody::Error(value) => json!({"error":value}),
    };
    save(
        tx,
        &reply.identity.key(),
        &json!({"identity":reply.identity,"signature":input.signature(),"body":body,"delivery":"prepared"}),
    )
}
pub(super) fn saved_reply(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    input: &Input,
) -> Result<Option<Reply>, CallbackError> {
    input.check_context(&identity.context)?;
    let Some(saved) = read(tx, &identity.key())? else {
        return Ok(None);
    };
    if saved["signature"] != input.signature()
        || saved["identity"]["context"] != serde_json::to_value(&identity.context)?
        || saved["identity"]["rpc_id"] != serde_json::to_value(&identity.rpc_id)?
    {
        return Err(CallbackError::Conflict);
    }
    match saved["delivery"].as_str() {
        Some("prepared" | "written") => {}
        Some("submitted" | "unknown") => return Err(CallbackError::Unknown),
        _ => return Err(CallbackError::Stored(identity.key())),
    }
    Ok(Some(Reply {
        identity: serde_json::from_value(saved["identity"].clone())?,
        body: response_body(&saved, &identity.key())?,
    }))
}
fn response_body(saved: &Value, key: &str) -> Result<ResponseBody, CallbackError> {
    if let Some(result) = saved["body"].get("result") {
        Ok(ResponseBody::Result(result.clone()))
    } else if let Some(error) = saved["body"].get("error") {
        Ok(ResponseBody::Error(error.clone()))
    } else {
        Err(CallbackError::Stored(key.into()))
    }
}

/// The retained worker calls this under CallbackRuntime's lifecycle gate, just
/// before transport admission. No I/O occurs in this read.
pub fn submission_ready(store: &RuntimeStore, reply: &Reply) -> Result<bool, CallbackError> {
    store.transaction_with(|tx| {
        let identity = &reply.identity;
        let saved = read(tx, &identity.key())?.ok_or(CallbackError::Unknown)?;
        if saved["identity"] != serde_json::to_value(identity)? {
            return Err(CallbackError::Stale);
        }
        if read(tx, &format!("{}:resolved", identity.key()))?.is_some()
            || matches!(
                saved["delivery"].as_str(),
                Some("resolved" | "written" | "retired")
            )
        {
            return Ok(false);
        }
        if saved["delivery"] != "submitted" {
            return Err(CallbackError::Unknown);
        }
        if response_body(&saved, &identity.key())? != reply.body {
            return Err(CallbackError::Conflict);
        }
        if saved.get("card").is_some() || saved.get("toolRequest").is_some() {
            let actor = snapshot(tx, &identity.context)?;
            actor.active()?;
            if !matches!(
                actor.actor()["status"].as_str(),
                Some("approval" | "running")
            ) {
                return Err(CallbackError::Stale);
            }
            if let Some(id) = saved["card"].as_str() {
                let card = records::get(tx, "runtime_requests", id)?.ok_or(CallbackError::Stale)?;
                if card["status"] != "answering"
                    || studio_runtime_store::truthy(card.get("deletedAt"))
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    })
}
#[derive(Clone, Copy, Debug)]
pub enum DeliveryOutcome {
    Submitted,
    Written,
    NotSent,
    Unknown,
}
/// Returns false for a previously written answer: ACK its journal cursor without
/// sending again. Submitted must commit before the caller writes any bytes.
/// A typed transport NotSent permits a replacement attempt, never reuse of the old reply.
pub fn delivery(
    store: &RuntimeStore,
    identity: &ReplyIdentity,
    outcome: DeliveryOutcome,
) -> Result<bool, CallbackError> {
    store.transaction_with(|tx| {
        let key = identity.key();
        let mut saved = read(tx, &key)?.ok_or(CallbackError::Invalid("Unknown callback reply"))?;
        if saved["identity"] != serde_json::to_value(identity)? {
            return Err(CallbackError::Stale);
        }
        if matches!(saved["delivery"].as_str(), Some("written" | "retired")) {
            return Ok(false);
        }
        if matches!(outcome, DeliveryOutcome::Submitted) && !identity.context.agent.is_empty() {
            let actor = snapshot(tx, &identity.context)?;
            if saved.get("card").is_some() || saved.get("toolRequest").is_some() {
                actor.active()?;
            }
        }
        let next = match (saved["delivery"].as_str(), outcome) {
            (Some("prepared"), DeliveryOutcome::Submitted) => "submitted",
            (Some("submitted"), DeliveryOutcome::Written) => "written",
            (Some("submitted"), DeliveryOutcome::NotSent) => "prepared",
            (Some("submitted"), DeliveryOutcome::Unknown) => "unknown",
            _ => return Err(CallbackError::Unknown),
        };
        saved["delivery"] = json!(next);
        if next == "prepared" && saved.get("card").is_none() {
            // A delayed receipt from the rejected attempt must not settle its replacement.
            let body = response_body(&saved, &key)?;
            saved["identity"] = serde_json::to_value(Reply::new(identity.clone(), body)?.identity)?;
        }
        update(tx, &key, &saved)?;
        if let Some(card_id) = saved["card"].as_str() {
            if next == "prepared" {
                super::cards::save(tx, &saved["unanswered"])?;
                tx.execute("DELETE FROM runtime_operation_receipts WHERE id=?1", [&key])?;
                return Ok(true);
            }
            let mut card = records::get(tx, "runtime_requests", card_id)?
                .ok_or(CallbackError::Invalid("Unknown callback card"))?;
            card["status"] = json!(match next {
                "written" => "answered",
                "unknown" => "uncertain",
                _ => "answering",
            });
            if next == "unknown" {
                card["answerError"] = json!("Answer delivery could not be confirmed. Inspect the agent conversation before taking further action.");
            }
            if next == "written"
                && let Some(at) = saved.get("deleteRequestedAt")
            {
                card["status"] = json!("deleted");
                card["deletedAt"] = at.clone();
                card["deletedBy"] = json!("user");
                card["deferred"] = json!(false);
            }
            super::cards::save(tx, &card)?;
            if next == "written" {
                let mut actor = records::get(tx, "runtime_agents", &identity.context.agent)?
                    .ok_or(CallbackError::Stale)?;
                if CallbackSnapshot::from_record(&actor.to_string(), &identity.context)
                    .and_then(|snapshot| snapshot.active()).is_ok()
                    && matches!(actor["status"].as_str(), Some("approval" | "running"))
                {
                    actor["status"] = json!("running");
                    save_actor(tx, &actor)?;
                }
            }
        }
        if next == "written" && saved["legacyAdmission"] == true {
            // Legacy admission has no worker cursor. Written and payload
            // retirement commit together, including a crash before HTTP replay.
            saved.as_object_mut().ok_or_else(|| CallbackError::Stored(key.clone()))?.remove("body");
            saved.as_object_mut().ok_or_else(|| CallbackError::Stored(key.clone()))?.remove("unanswered");
            saved["delivery"] = json!("retired");
            update(tx, &key, &saved)?;
        }
        Ok(true)
    })
}

/// Bind the exact prepared attempt (or durable pending admission) to its source
/// cursor. Do this before settling a Pending journal entry or sending an Answer.
pub fn bind_journal(
    store: &RuntimeStore,
    decision: &CallbackDecision,
    context: &CallbackContext,
    rpc_id: &studio_runtime_providers::RequestId,
    cursor: u64,
) -> Result<(), CallbackError> {
    if cursor == 0 {
        return Err(CallbackError::Invalid("Invalid callback journal cursor"));
    }
    let identity = match decision {
        CallbackDecision::Answer(reply) => &reply.identity,
        CallbackDecision::Settled(identity) => identity,
        CallbackDecision::Pending(_) => &ReplyIdentity::new(context, rpc_id),
        CallbackDecision::Unsupported => return Ok(()),
    };
    if identity.context != *context || identity.rpc_id != *rpc_id {
        return Err(CallbackError::Stale);
    }
    store.transaction_with(|tx| {
        let pending =
            matches!(decision, CallbackDecision::Pending(_)) || identity.attempt.is_empty();
        let key = if matches!(decision, CallbackDecision::Settled(_)) && identity.attempt.is_empty()
        {
            format!("{}:resolved", identity.key())
        } else if pending {
            format!("{}:card", identity.key())
        } else {
            identity.key()
        };
        let mut saved = read(tx, &key)?.ok_or(CallbackError::Unknown)?;
        if saved["identity"] != serde_json::to_value(identity)? {
            return Err(CallbackError::Stale);
        }
        // A replay at a later cursor keeps its first durable admission binding.
        if let CallbackDecision::Pending(card) = decision
            && saved["card"] != *card
        {
            return Err(CallbackError::Stale);
        }
        if saved.get("journalCursor").is_none() {
            saved["journalCursor"] = json!(cursor);
            update(tx, &key, &saved)?;
        }
        // HTTP may commit an answer between admission and the worker binding.
        // Carry the first cursor onto that exact answer without rebinding it.
        if pending && let Some(mut reply) = read(tx, &identity.key())? {
            if reply["identity"]["context"] != saved["identity"]["context"]
                || reply["identity"]["rpc_id"] != saved["identity"]["rpc_id"]
            {
                return Err(CallbackError::Stale);
            }
            if reply.get("journalCursor").is_none() {
                reply["journalCursor"] = saved["journalCursor"].clone();
                update(tx, &identity.key(), &reply)?;
            }
        }
        Ok(())
    })
}

/// Run in the worker's checkpoint transaction AFTER advancing its durable ACK.
/// Only payloads are removed. Exact identity/signature tombstones suppress RPC
/// replays even at later cursors; Unknown proof is never garbage-collected.
pub fn retire_checkpointed(
    tx: &Transaction<'_>,
    account: &str,
    generation: u64,
) -> Result<usize, CallbackError> {
    let checkpoint = read(
        tx,
        &format!("account-worker:cursor:{account}:{generation}:ack"),
    )?
    .map(|v| {
        v.as_u64()
            .ok_or_else(|| CallbackError::Stored("callback checkpoint".into()))
    })
    .transpose()?
    .unwrap_or(0);
    let mut stmt = tx.prepare("SELECT id FROM runtime_operation_receipts WHERE signature='provider-callback' AND json_extract(result,'$.identity.context.account')=?1 AND json_extract(result,'$.identity.context.generation')=?2 AND json_extract(result,'$.delivery')='written' AND (json_extract(result,'$.journalCursor')<=?3 OR json_extract(result,'$.legacyAdmission')=1) LIMIT 128")?;
    let ids = stmt
        .query_map(
            params![
                account,
                i64::try_from(generation).map_err(|_| CallbackError::Stale)?,
                i64::try_from(checkpoint).map_err(|_| CallbackError::Stale)?
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for id in &ids {
        let mut saved = read(tx, id)?.ok_or(CallbackError::Unknown)?;
        let identity: ReplyIdentity = serde_json::from_value(saved["identity"].clone())
            .map_err(|_| CallbackError::Stored(id.clone()))?;
        if identity.key() != *id
            || identity.attempt.is_empty()
            || (saved["legacyAdmission"] != true
                && !saved["journalCursor"]
                    .as_u64()
                    .is_some_and(|cursor| cursor > 0 && cursor <= checkpoint))
        {
            return Err(CallbackError::Stored(id.clone()));
        }
        let object = saved
            .as_object_mut()
            .ok_or_else(|| CallbackError::Stored(id.clone()))?;
        object.remove("body");
        object.remove("unanswered");
        object.insert("delivery".into(), json!("retired"));
        update(tx, id, &saved)?;
    }
    Ok(ids.len())
}

pub(super) fn saved_decision(
    tx: &Transaction<'_>,
    identity: &ReplyIdentity,
    input: &Input,
) -> Result<Option<CallbackDecision>, CallbackError> {
    let resolution_key = format!("{}:resolved", identity.key());
    if let Some(mut proof) = read(tx, &resolution_key)? {
        input.check_context(&identity.context)?;
        if proof["identity"] != serde_json::to_value(identity)? {
            return Err(CallbackError::Stale);
        }
        if let Some(signature) = proof.get("signature") {
            if signature != &input.signature() {
                return Err(CallbackError::Conflict);
            }
        } else {
            proof["signature"] = json!(input.signature());
            update(tx, &resolution_key, &proof)?;
        }
        return Ok(Some(CallbackDecision::Settled(identity.clone())));
    }
    if let Some(saved) = read(tx, &identity.key())?
        && matches!(saved["delivery"].as_str(), Some("retired" | "resolved"))
    {
        if saved["signature"] != input.signature()
            || saved["identity"]["context"] != serde_json::to_value(&identity.context)?
            || saved["identity"]["rpc_id"] != serde_json::to_value(&identity.rpc_id)?
        {
            return Err(CallbackError::Conflict);
        }
        input.check_context(&identity.context)?;
        return Ok(Some(CallbackDecision::Settled(serde_json::from_value(
            saved["identity"].clone(),
        )?)));
    }
    if read(tx, &identity.key())?.is_none()
        && let Some(admission) = read(tx, &format!("{}:card", identity.key()))?
        && let Some(card_id) = admission["card"].as_str()
        && let Some(card) = records::get(tx, "runtime_requests", card_id)?
        && card["status"] == "resolved"
    {
        input.check_context(&identity.context)?;
        if admission["signature"] != input.signature()
            || admission["identity"] != serde_json::to_value(identity)?
        {
            return Err(CallbackError::Conflict);
        }
        return Ok(Some(CallbackDecision::Settled(identity.clone())));
    }
    saved_reply(tx, identity, input).map(|v| v.map(CallbackDecision::Answer))
}

/// On a surviving session, drain only Prepared replies. Submitted/Unknown rows
/// never enter this queue. Connection loss uses recover_connection instead.
pub fn prepared_replies(
    store: &RuntimeStore,
    context: &CallbackContext,
) -> Result<Vec<Reply>, CallbackError> {
    store.transaction_with(|tx| {
        snapshot(tx, context)?.active()?;
        let mut stmt = tx.prepare("SELECT id,result FROM runtime_operation_receipts WHERE signature='provider-callback' AND json_extract(result,'$.delivery')='prepared' AND json_extract(result,'$.identity.context.agent')=?1 LIMIT 129")?;
        let rows = stmt.query_map([&context.agent], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
        if rows.len() > MAX_PENDING { return Err(CallbackError::Invalid("Too many prepared provider replies")); }
        let mut replies = Vec::new();
        for (id,raw) in rows {
            let saved: Value = serde_json::from_str(&raw).map_err(|_| CallbackError::Stored(id.clone()))?;
            let identity: ReplyIdentity = serde_json::from_value(saved["identity"].clone()).map_err(|_| CallbackError::Stored(id.clone()))?;
            if identity.context != *context { continue; }
            if identity.key() != id || identity.attempt.is_empty() { return Err(CallbackError::Stored(id)); }
            let body = response_body(&saved, &id)?;
            replies.push(Reply { identity, body });
        }
        Ok(replies)
    })
}
