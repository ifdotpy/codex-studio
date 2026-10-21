//! Receipt settlement is in the same transaction as event delivery and agent changes.
use super::*;
use super::{pipeline::apply_patch, snapshot::*};
use crate::{thread_prepare as thread, turn_prepare as turn};
use rusqlite::OptionalExtension;
use studio_runtime_providers::RequestTicket;
use studio_runtime_store::Transaction;

/// The check that refused a provider thread reply. Its text is stored in the
/// agent error, so it names the failed check and never a stored secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThreadRejection {
    /// The pending request has no thread preparation attempt.
    NoAttempt,
    /// The reply is not the saved thread submission (ID, connection
    /// generation, method or operation).
    SubmissionReceipt,
    /// The reply is stale for the prepared attempt; the reason names the field.
    Stale(thread::StaleReason),
    /// The reply names no thread ID.
    NoThreadId,
    /// The reply names another thread than the prepared one.
    OtherThread { reply: String, prepared: String },
}

impl std::fmt::Display for ThreadRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAttempt => f.write_str("no thread preparation is pending for this request"),
            Self::SubmissionReceipt => f.write_str("the reply is not the saved thread request"),
            Self::Stale(reason) => write!(f, "stale {reason:?}"),
            Self::NoThreadId => f.write_str("the reply has no thread ID"),
            Self::OtherThread { reply, prepared } => {
                write!(
                    f,
                    "the reply thread {reply} is not the prepared thread {prepared}"
                )
            }
        }
    }
}

pub(crate) fn accept_thread(
    tx: &Transaction<'_>,
    root: &std::path::Path,
    resources: &PreparationResources,
    context: &Pending,
    ticket: &RequestTicket,
    value: &Value,
) -> Result<Result<(Effect, thread::LoadedIdentity), ThreadRejection>, WorkerError> {
    let Some(attempt) = &context.thread else {
        return Ok(Err(ThreadRejection::NoAttempt));
    };
    let saved = super::super::store::receipt(tx, &context.operation, "submitted")?
        .ok_or_else(|| invalid("thread submission receipt"))?;
    if saved["request"]["id"] != json!(ticket.id)
        || saved["request"]["generation"] != ticket.generation
        || saved["request"]["method"] != ticket.method.as_str()
        || saved["request"]["operation"] != context.operation
    {
        return Ok(Err(ThreadRejection::SubmissionReceipt));
    }
    let mut record = agent(tx, &context.intent.agent_id)?.value();
    let progress_directory = match &attempt.snapshot.workspace {
        thread::WorkspaceReadiness::Ready { progress_directory } => progress_directory.clone(),
        _ => None,
    };
    let filesystem = ThreadFilesystemInputs { progress_directory };
    let mut current = thread_snapshot(
        tx,
        resources,
        &record,
        &ticket.generation.to_string(),
        root,
        &filesystem,
        None,
    )?;
    align_receipt_cwd(&mut current, &attempt.snapshot);
    let receipt = thread::ProviderReceipt {
        identity: thread::ReceiptIdentity {
            operation_id: format!(
                "prepare:{}:{}",
                context.intent.agent_id, context.intent.attempt_id
            ),
            transport_ticket: super::super::store::id_string(&ticket.id),
            agent_id: context.intent.agent_id.clone(),
            epoch: u64::try_from(context.intent.epoch).map_err(|_| invalid("epoch"))?,
            account_key: context.intent.account_key.clone(),
            connection_id: ticket.generation.to_string(),
        },
        thread_id: value["thread"]["id"].as_str().map(str::to_owned),
        fields: value
            .as_object()
            .ok_or_else(|| invalid("thread receipt"))?
            .clone(),
        reattached: false,
    };
    let (patch, loaded) =
        match thread::reduce_receipt(attempt, &current, Some(&receipt), None, json!(now())) {
            thread::ReceiptOutcome::Accepted { patch, loaded } => (patch, loaded),
            thread::ReceiptOutcome::Stale(reason) => {
                return Ok(Err(ThreadRejection::Stale(reason)));
            }
            // With a receipt, an unknown outcome means that the reply names no
            // thread or another thread than the prepared one.
            thread::ReceiptOutcome::Unknown => {
                return Ok(Err(
                    match (receipt.thread_id, &attempt.snapshot.actor.thread_id) {
                        (Some(reply), Some(prepared)) if !reply.is_empty() => {
                            ThreadRejection::OtherThread {
                                reply,
                                prepared: prepared.clone(),
                            }
                        }
                        _ => ThreadRejection::NoThreadId,
                    },
                ));
            }
        };
    apply_patch(&mut record, &patch)?;
    studio_runtime_store::ExecutionHistoryRepository::save_receipt(
        tx,
        &context.intent.attempt_id,
        "threadPrepared",
        json!({"agent":context.intent.agent_id,"epoch":context.intent.epoch,"account":context.intent.account_key,"previousThread":context.intent.thread_id,"thread":loaded.thread_id}),
        json!(true),
    )?;
    let recovered = record.get("startAttempt").is_none_or(Value::is_null)
        && record["startOutcomeHold"]["reason"] == super::super::store::RECEIPT_PENDING_HOLD
        && record["startOutcomeHold"]["operationId"] == context.operation;
    if recovered {
        record["autoWake"] = json!(true);
        record["inFlight"] = json!(false);
        record["status"] = json!("queued");
        record["nativeFailureHold"] = json!(false);
        record["error"] = Value::Null;
        record
            .as_object_mut()
            .ok_or_else(|| invalid("agent"))?
            .remove("startOutcomeHold");
    }
    AgentRepository::put(tx, &decode(record, &context.intent.agent_id)?)?;
    let mut intent = context.intent.clone();
    intent.thread_id = Some(loaded.thread_id.clone());
    let effect = if recovered {
        Effect::Wake(intent.agent_id, intent.epoch)
    } else {
        Effect::Submit(intent)
    };
    Ok(Ok((effect, loaded)))
}
pub(crate) fn accept_turn(
    tx: &Transaction<'_>,
    context: &Pending,
    ticket: &RequestTicket,
    turn_id: &str,
) -> Result<(), WorkerError> {
    let Some(prepared) = &context.compiled else {
        return Ok(());
    };
    let saved = super::super::store::receipt(tx, &context.operation, "submitted")?
        .ok_or_else(|| invalid("turn submission receipt"))?;
    if saved["request"]["id"] != json!(ticket.id)
        || saved["request"]["generation"] != ticket.generation
        || ticket.method != Method::TurnStart
    {
        return Ok(());
    }
    // Delivery must already have been authenticated against the durable attempt by the scheduler.
    let events = &prepared.event_ids;
    let all_delivered=events.iter().try_fold(true,|ok,id|->Result<bool,WorkerError>{Ok(ok && tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE id=?1 AND agent=?2 AND status='delivered' AND turn_id=?3)",rusqlite::params![id,context.intent.agent_id,turn_id],|r|r.get::<_,bool>(0))? )})?;
    if !all_delivered {
        return Ok(());
    }
    ensure_context_tables(tx)?;
    let current=tx.query_row("SELECT record FROM runtime_context_manifests WHERE agent=?1 AND thread_id=?2 AND compactions=?3",rusqlite::params![context.intent.agent_id,prepared.epoch.0.as_deref().unwrap_or(""),i64::try_from(prepared.epoch.1).map_err(|_|invalid("manifest compactions"))?],|r|r.get::<_,String>(0)).optional()?.map(|s|decode(serde_json::from_str(&s).map_err(|_|invalid("manifest"))?,"manifest")).transpose()?;
    let transition = turn::accept(
        prepared,
        &turn::NativeReceipt {
            operation_id: context.operation.clone(),
            actor_id: context.intent.agent_id.clone(),
            epoch: turn::Epoch(context.intent.thread_id.clone(), prepared.epoch.1),
            event_ids: context.intent.events.iter().map(|e| e.id.clone()).collect(),
            outcome: turn::ReceiptOutcome::Accepted {
                turn_id: turn_id.into(),
            },
        },
        current.as_ref(),
    );
    if let Some(manifest) = transition.manifest_upsert {
        write_manifest(tx, &context.intent.agent_id, &events[0], &manifest)?;
    }
    Ok(())
}
fn write_manifest(
    tx: &Transaction<'_>,
    agent: &str,
    event: &str,
    manifest: &turn::ContextManifest,
) -> Result<(), WorkerError> {
    ensure_context_tables(tx)?;
    tx.execute("INSERT INTO runtime_context_manifests(agent,thread_id,compactions,sequence,event_id,record) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(agent,thread_id,compactions) DO UPDATE SET sequence=excluded.sequence,event_id=excluded.event_id,record=excluded.record WHERE excluded.sequence>=runtime_context_manifests.sequence",rusqlite::params![agent,manifest.epoch.0.as_deref().unwrap_or(""),i64::try_from(manifest.epoch.1).map_err(|_|invalid("manifest compactions"))?,i64::try_from(manifest.sequence).map_err(|_|invalid("manifest sequence"))?,event,json!(manifest).to_string()])?;
    if let Some(version) = manifest.versions.get("worktreeReminder") {
        tx.execute("INSERT OR IGNORE INTO runtime_context_reminders(agent,version,event_id) VALUES(?1,?2,?3)",rusqlite::params![agent,version,event])?;
    }
    Ok(())
}
/// Call after exact positive event delivery has been authenticated by the repair owner.
/// The event-status check and manifest writes stay in that owner's transaction.
pub fn remember_delivered_manifests(
    tx: &Transaction<'_>,
    agent: &str,
    events: &[String],
) -> Result<(), WorkerError> {
    for event in events {
        let raw=tx.query_row("SELECT m.record FROM runtime_events e JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?1 AND e.agent=?2 AND e.status='delivered'",rusqlite::params![event,agent],|r|r.get::<_,String>(0)).optional()?;
        if let Some(raw) = raw {
            let metadata: Value =
                serde_json::from_str(&raw).map_err(|_| invalid("delivered metadata"))?;
            if let Some(value) = metadata.get("contextManifest") {
                let manifest = decode(value.clone(), "delivered manifest")?;
                write_manifest(tx, agent, event, &manifest)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ThreadRejection;

    /// The stored error names the failed check of each rejection.
    #[test]
    fn each_thread_rejection_names_its_check() {
        for (rejection, text) in [
            (
                ThreadRejection::NoAttempt,
                "no thread preparation is pending for this request",
            ),
            (
                ThreadRejection::SubmissionReceipt,
                "the reply is not the saved thread request",
            ),
            (
                ThreadRejection::Stale(crate::thread_prepare::StaleReason::ReceiptIdentity),
                "stale ReceiptIdentity",
            ),
            (
                ThreadRejection::Stale(crate::thread_prepare::StaleReason::Cwd),
                "stale Cwd",
            ),
            (ThreadRejection::NoThreadId, "the reply has no thread ID"),
        ] {
            assert_eq!(rejection.to_string(), text);
        }
    }
}
