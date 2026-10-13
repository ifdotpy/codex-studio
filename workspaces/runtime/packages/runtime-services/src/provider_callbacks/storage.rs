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
        || saved["identity"] != serde_json::to_value(identity)?
    {
        return Err(CallbackError::Conflict);
    }
    match saved["delivery"].as_str() {
        Some("prepared" | "written") => {}
        Some("submitted" | "unknown") => return Err(CallbackError::Unknown),
        _ => return Err(CallbackError::Stored(identity.key())),
    }
    let body = if let Some(result) = saved["body"].get("result") {
        ResponseBody::Result(result.clone())
    } else if let Some(error) = saved["body"].get("error") {
        ResponseBody::Error(error.clone())
    } else {
        return Err(CallbackError::Stored(identity.key()));
    };
    Ok(Some(Reply::new(identity.clone(), body)))
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
/// Only a typed transport NotSent permits another submission of the same reply.
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
        if saved["delivery"] == "written" {
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
            super::cards::save(tx, &card)?;
            if next == "written" {
                let mut actor = records::get(tx, "runtime_agents", &identity.context.agent)?
                    .ok_or(CallbackError::Stale)?;
                if actor["epoch"].as_u64() == Some(identity.context.epoch)
                    && actor["autoWake"] == true
                {
                    actor["status"] = json!("running");
                    records::validate("runtime_agents", &identity.context.agent, actor.clone())?;
                    crate::agents_create::publish_agent(tx, &actor)?;
                }
            }
        }
        Ok(true)
    })
}
