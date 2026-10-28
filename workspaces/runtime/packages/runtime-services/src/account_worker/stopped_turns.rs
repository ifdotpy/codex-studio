//! End the provider work of a turn that its owner stopped while no account
//! worker was attached. The provider process keeps its generation, the turn,
//! and the turn's commands running in two cases:
//! - A lost Studio connection leaves the agent interrupted with a disconnect
//!   receipt. A stop in that window has no connection to send
//!   `turn/interrupt` on, so it commits only the next epoch.
//! - The process supervisor retains the provider when its account worker ends,
//!   so the turn stays running without a disconnect receipt. A stop in that
//!   window stages its interrupt, but no worker can send it, so the stop's
//!   receipt stays unsent or unknown.
//!
//! Each new account worker checks once, on its own connection, for such a
//! turn of its provider generation, and ends it as the stop would have.
use super::retained::{RequestActor, RequestResult, RetainedSessionPort, SessionRequest};
use crate::recovery_history::ConnectionScope;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_providers::{Method, ProviderKind};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, RuntimeStore, SchedulerRepository,
};

/// Operation-id prefix of the interrupt of a stopped disconnected turn.
const INTERRUPT_OPERATION_PREFIX: &str = "stopped-turn-interrupt:";
/// Operation-id prefix of the command cleanup of a stopped disconnected turn.
const TERMINALS_OPERATION_PREFIX: &str = "stopped-turn-terminals:";
/// Unit: wall time for one request of a stopped turn.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A stopped turn that the provider process of this generation can still run.
#[derive(Clone)]
struct StoppedTurn {
    agent: String,
    /// The epoch that ran the turn; the stop committed the next one.
    epoch: i64,
    thread: String,
    turn: String,
    terminals: Option<Method>,
    owner: Owner,
}

/// The receipt that owns the requests which end a stopped turn.
#[derive(Clone, PartialEq, Eq)]
enum Owner {
    /// A disconnect receipt: the stop sent nothing, so this module names the
    /// requests with its own operation ids and records them on the receipt.
    Disconnect { signature: String },
    /// The stop's own interrupt, staged but not settled. Its exact ticket and
    /// cleanup operation make the retained ledger send each request at most
    /// once, also when an earlier worker wrote it and lost the reply.
    Stop { ticket: String, terminals: String },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAdmissions {
    admission: Option<Box<serde_json::value::RawValue>>,
    terminals_admission: Option<Box<serde_json::value::RawValue>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredStop {
    start_attempt: StoredStopAttempt,
}

#[derive(serde::Deserialize)]
struct StoredStopAttempt {
    interrupt: StoredAdmissions,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredDisconnect {
    disconnect_recovery: StoredDisconnectRecovery,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredDisconnectRecovery {
    stop_interrupt: StoredAdmissions,
}

fn stored_admission(
    raw: &str,
    owner: &Owner,
    field: &str,
    agent: &str,
) -> Result<studio_runtime_store::request_results::AdmittedRequest, studio_runtime_store::StoreError>
{
    let invalid = || studio_runtime_store::StoreError::InvalidStoredObject {
        table: "runtime_agents",
        id: agent.to_owned(),
    };
    let admissions = match owner {
        Owner::Disconnect { .. } => {
            serde_json::from_str::<StoredDisconnect>(raw)
                .map_err(|_| invalid())?
                .disconnect_recovery
                .stop_interrupt
        }
        Owner::Stop { .. } => {
            serde_json::from_str::<StoredStop>(raw)
                .map_err(|_| invalid())?
                .start_attempt
                .interrupt
        }
    };
    let saved = if field == "admission" {
        admissions.admission
    } else {
        admissions.terminals_admission
    }
    .ok_or_else(invalid)?;
    serde_json::from_str(saved.get()).map_err(|_| invalid())
}

#[cfg(test)]
mod stored_admission_tests {
    use super::*;

    #[test]
    fn reads_exact_saved_tokens_and_rejects_invalid_evidence() {
        let stop = Owner::Stop {
            ticket: "interrupt".into(),
            terminals: "terminals".into(),
        };
        let disconnect = Owner::Disconnect {
            signature: "scope".into(),
        };
        let stop_record = r#"{"startAttempt":{"interrupt":{"rootRequestId":"root","admission":{"id":"interrupt-id","signature":"interrupt-sig","created":123},"terminalsAdmission":{"id":"cleanup-id","signature":"cleanup-sig","created":124}}}}"#;
        let disconnect_record = r#"{"disconnectRecovery":{"stopInterrupt":{"admission":{"id":"interrupt-id","signature":"interrupt-sig","created":123},"terminalsAdmission":{"id":"cleanup-id","signature":"cleanup-sig","created":124}}}}"#;
        for (raw, owner) in [(stop_record, &stop), (disconnect_record, &disconnect)] {
            let admission = stored_admission(raw, owner, "admission", "agent").expect("admission");
            assert_eq!(
                (admission.id(), admission.signature(), admission.created()),
                ("interrupt-id", "interrupt-sig", 123)
            );
            let cleanup =
                stored_admission(raw, owner, "terminalsAdmission", "agent").expect("cleanup");
            assert_eq!(
                (cleanup.id(), cleanup.signature(), cleanup.created()),
                ("cleanup-id", "cleanup-sig", 124)
            );
            let malformed = raw.replace("\"created\":123", "\"created\":123,\"unexpected\":true");
            assert!(
                matches!(stored_admission(&malformed, owner, "admission", "agent"), Err(studio_runtime_store::StoreError::InvalidStoredObject { table: "runtime_agents", id }) if id == "agent")
            );
        }
    }
}

impl StoppedTurn {
    fn interrupt_operation(&self) -> String {
        match &self.owner {
            Owner::Disconnect { signature } => format!("{INTERRUPT_OPERATION_PREFIX}{signature}"),
            Owner::Stop { ticket, .. } => ticket.clone(),
        }
    }
    fn terminals_operation(&self) -> String {
        match &self.owner {
            Owner::Disconnect { signature } => format!("{TERMINALS_OPERATION_PREFIX}{signature}"),
            Owner::Stop { terminals, .. } => terminals.clone(),
        }
    }
}

/// End every such turn of `account` once. A request that fails leaves its
/// receipt for the next worker; it never stops the worker or another agent.
pub(super) async fn end_account(
    store: Arc<RuntimeStore>,
    account: &str,
    scope: &ConnectionScope,
    session: &RetainedSessionPort,
) -> usize {
    let read_account = account.to_owned();
    let generation = scope.generation;
    let Ok(turns) = store
        .read_async("runtime-services/stopped_turns", move |tx| {
            stopped_turns(tx, &read_account, generation)
        })
        .await
    else {
        return 0;
    };
    let mut ended = 0;
    for turn in turns {
        if !session.is_live() {
            break;
        }
        let (interrupt, response) = request(
            &store,
            session,
            scope,
            &turn,
            turn.interrupt_operation(),
            Method::TurnInterrupt,
            json!({"threadId":turn.thread,"turnId":turn.turn}),
        )
        .await;
        // An interrupt ends the turn, not the commands it left running.
        if interrupt != Outcome::NotSent
            && let Some(method) = turn.terminals
        {
            let (cleaned, _) = request(
                &store,
                session,
                scope,
                &turn,
                turn.terminals_operation(),
                method,
                json!({"threadId":turn.thread}),
            )
            .await;
            if cleaned != Outcome::Accepted {
                eprintln!(
                    "stopped agent {} command cleanup on thread {} was not confirmed: {cleaned:?}",
                    turn.agent, turn.thread
                );
            }
        }
        if interrupt == Outcome::NotSent {
            continue;
        }
        let recorded = turn.clone();
        let outcome = interrupt.as_str();
        let recorded_account = account.to_owned();
        if crate::scheduler::lifecycle_transaction_with(&store, move |tx| {
            record(
                tx,
                &recorded,
                &recorded_account,
                generation,
                outcome,
                &response,
            )
        })
        .await
        .is_ok_and(|changed| changed)
        {
            ended += 1;
        }
    }
    ended
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Accepted,
    Rejected,
    NotSent,
    Unknown,
}
impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::NotSent => "unsent",
            Self::Unknown => "unknown",
        }
    }
}

/// Send one request as the prior-epoch actor that the retained worker
/// authorizes for a stopped turn. Its exact operation id makes the retained
/// ledger replay a lost reply instead of sending the request again. Returns
/// the provider's interrupt receipt with the outcome, or null.
async fn request(
    store: &RuntimeStore,
    session: &RetainedSessionPort,
    scope: &ConnectionScope,
    turn: &StoppedTurn,
    operation: String,
    method: Method,
    params: Value,
) -> (Outcome, Value) {
    let mut request = SessionRequest {
        root_request_id: String::new(),
        admitted_request: None,
        scope: scope.clone(),
        expected_scope: Some(scope.clone()),
        operation,
        actor: Some(RequestActor {
            agent: turn.agent.clone(),
            epoch: turn.epoch,
        }),
        transfer: None,
        deleted_actor_session: None,
        method,
        params,
        deadline: std::time::Instant::now() + REQUEST_TIMEOUT,
    };
    let actor = turn.clone();
    let mut intent = super::retained::ProviderCallIntent::from(&request);
    let reserved = store
        .transaction_with(move |tx| {
            let (mut agent, raw) = studio_runtime_store::Table::<AgentRecord>::new(tx)
                .get_with_raw(&AgentId::from(actor.agent.as_str()))?
                .ok_or(super::WorkerError::CallbackUnknown)?;
            if !owner_matches(&agent, &actor, intent.scope.generation) {
                return Err(super::WorkerError::CallbackUnknown);
            }
            let attempt_id = match &actor.owner {
                Owner::Disconnect { .. } => agent
                    .extra
                    .get("disconnectRecovery")
                    .and_then(|r| r.pointer("/startAttempt/id")),
                Owner::Stop { .. } => agent.extra.get("startAttempt").and_then(|a| a.get("id")),
            }
            .and_then(Value::as_str)
            .ok_or(super::WorkerError::CallbackUnknown)?
            .to_owned();
            let (execution_root, source) = super::store::execution_admission(tx, &attempt_id)?;
            let field = if intent.operation == actor.interrupt_operation() {
                "admission"
            } else {
                "terminalsAdmission"
            };
            let pointer = match &actor.owner {
                Owner::Disconnect { .. } => &mut agent
                    .extra
                    .get_mut("disconnectRecovery")
                    .ok_or(super::WorkerError::CallbackUnknown)?["stopInterrupt"],
                Owner::Stop { .. } => &mut agent
                    .extra
                    .get_mut("startAttempt")
                    .ok_or(super::WorkerError::CallbackUnknown)?["interrupt"],
            };
            let root = match &actor.owner {
                Owner::Disconnect { .. } => execution_root,
                Owner::Stop { .. } => pointer
                    .get("rootRequestId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| studio_runtime_store::StoreError::InvalidStoredObject {
                        table: "runtime_agents",
                        id: actor.agent.clone(),
                    })?
                    .to_owned(),
            };
            let admitted = if pointer.get(field).is_some() {
                stored_admission(&raw, &actor.owner, field, &actor.agent)?
            } else {
                intent.root_request_id = root.clone();
                let key = studio_runtime_schema::provider_call_contract::transport_key(
                    &intent.scope.account,
                    &intent.operation,
                );
                let existing = studio_runtime_store::outbox::Outbox::get::<
                    super::store::ProviderCall,
                >(tx, &key)
                .map_err(studio_runtime_store::StoreError::from)?;
                let admitted = if let Some(row) = existing {
                    match row.payload {
                        super::store::ProviderCall::Retained(call)
                            if call.call.root_request_id == root =>
                        {
                            call.admitted
                        }
                        super::store::ProviderCall::Retained(_) => {
                            return Err(super::WorkerError::CallbackUnknown);
                        }
                        super::store::ProviderCall::Account(_) => {
                            return Err(super::WorkerError::CallbackUnknown);
                        }
                    }
                } else {
                    super::retained::reserve_provider_call(tx, &source, intent)?
                };
                if pointer.is_null() {
                    *pointer = json!({});
                }
                pointer[field] = studio_runtime_store::projection_value(&admitted);
                admitted
            };
            AgentRepository::put(tx, &agent)?;
            Ok((root, admitted))
        })
        .await;
    let Ok((root_request_id, admitted_request)) = reserved else {
        return (Outcome::Unknown, Value::Null);
    };
    request.root_request_id = root_request_id;
    request.admitted_request = Some(admitted_request);
    let receipt = session.request(request).await;
    match receipt {
        Ok(receipt) => match receipt.result {
            RequestResult::Reply(reply) => (Outcome::Accepted, reply),
            RequestResult::Rejected(error) => {
                (Outcome::Rejected, error["data"]["interruptReceipt"].clone())
            }
            RequestResult::NotSent => (Outcome::NotSent, Value::Null),
            RequestResult::Unknown => (Outcome::Unknown, Value::Null),
        },
        Err(super::retained::PortError::NotSent) => (Outcome::NotSent, Value::Null),
        Err(_) => (Outcome::Unknown, Value::Null),
    }
}

/// The disconnect receipt of a turn that a stop ended without a provider
/// request, on this provider generation, and not yet ended by a worker.
fn stopped_turn(agent: &AgentRecord, generation: u64) -> Option<StoppedTurn> {
    let extra = &agent.extra;
    let receipt = extra.get("disconnectRecovery")?;
    let turn = receipt["turnId"].as_str().filter(|turn| !turn.is_empty())?;
    let thread = receipt["threadId"]
        .as_str()
        .filter(|thread| !thread.is_empty())?;
    let epoch = receipt["epoch"].as_i64()?;
    // An interrupt that the stop itself staged owns this turn: a lost reply is
    // never answered by a second interrupt. Only a proven unsent one is not.
    let staged = |attempt: &Value| {
        let interrupt = &attempt["interrupt"];
        interrupt["threadId"].as_str() == Some(thread)
            && interrupt["turnId"].as_str() == Some(turn)
            && interrupt["outcome"].as_str() != Some("unsent")
    };
    let ended = matches!(
        receipt["stopInterrupt"]["outcome"].as_str(),
        Some("accepted" | "rejected" | "unknown")
    ) || staged(&receipt["startAttempt"])
        || extra.get("startAttempt").is_some_and(staged);
    let stopped = extra.get("autoWake") == Some(&json!(false))
        && !studio_runtime_store::truthy(extra.get("inFlight"))
        && epoch.checked_add(1) == Some(agent.epoch);
    if ended
        || !stopped
        || receipt["startAttempt"]["connectionGeneration"].as_u64() != Some(generation)
        || receipt["startAttempt"]["turnId"].as_str() != Some(turn)
        // A delete tombstone clears the turn id; the epoch still names the stop.
        || extra
            .get("turnId")
            .is_some_and(|current| !current.is_null() && current.as_str() != Some(turn))
        || extra.get("threadId").and_then(Value::as_str) != Some(thread)
        || extra.get("lastCompletedTurn").and_then(Value::as_str) == Some(turn)
    {
        return None;
    }
    let terminals = ProviderKind::of_field(extra.get("provider"))
        .ok()
        .and_then(studio_runtime_providers::stop_terminals_method);
    let signature = studio_runtime_store::sha256_hex(
        json!([agent.id.0, epoch, generation, thread, turn])
            .to_string()
            .as_bytes(),
    );
    Some(StoppedTurn {
        agent: agent.id.0.clone(),
        epoch,
        thread: thread.to_owned(),
        turn: turn.to_owned(),
        terminals,
        owner: Owner::Disconnect { signature },
    })
}

/// The stop's own interrupt of a turn on this provider generation that no
/// worker settled: the stop found no live connection for it. A disconnect
/// receipt that already ended this turn owns it instead.
fn unsettled_stop(agent: &AgentRecord, generation: u64) -> Option<StoppedTurn> {
    let extra = &agent.extra;
    let attempt = extra.get("startAttempt")?;
    let interrupt = &attempt["interrupt"];
    let ticket = interrupt["requestTicket"].as_str()?;
    let terminals_operation = crate::lifecycle_effects::stop_terminals_operation(ticket)?;
    let thread = interrupt["threadId"]
        .as_str()
        .filter(|thread| !thread.is_empty())?;
    let turn = interrupt["turnId"]
        .as_str()
        .filter(|turn| !turn.is_empty())?;
    let epoch = agent.epoch.checked_sub(1)?;
    let receipt = extra.get("disconnectRecovery").unwrap_or(&Value::Null);
    let ended_by_receipt = receipt["turnId"].as_str() == Some(turn)
        && receipt["stopInterrupt"].get("outcome").is_some();
    if ended_by_receipt
        || !matches!(interrupt["outcome"].as_str(), Some("unknown" | "unsent"))
        || interrupt["generation"].as_u64() != Some(generation)
        || attempt["connectionGeneration"].as_u64() != Some(generation)
        || attempt["epoch"].as_i64() != Some(epoch)
        || extra.get("autoWake") != Some(&json!(false))
        // A delete tombstone clears the turn id; the epoch still names the stop.
        || extra
            .get("turnId")
            .is_some_and(|current| !current.is_null() && current.as_str() != Some(turn))
        || extra.get("threadId").and_then(Value::as_str) != Some(thread)
        || extra.get("lastCompletedTurn").and_then(Value::as_str) == Some(turn)
    {
        return None;
    }
    let terminals = ProviderKind::of_field(extra.get("provider"))
        .ok()
        .and_then(studio_runtime_providers::stop_terminals_method);
    Some(StoppedTurn {
        agent: agent.id.0.clone(),
        epoch,
        thread: thread.to_owned(),
        turn: turn.to_owned(),
        terminals,
        owner: Owner::Stop {
            ticket: ticket.to_owned(),
            terminals: terminals_operation,
        },
    })
}

/// The stopped turn of `agent` on this generation, if any.
fn ended_turn(agent: &AgentRecord, generation: u64) -> Option<StoppedTurn> {
    stopped_turn(agent, generation).or_else(|| unsettled_stop(agent, generation))
}

fn stopped_turns(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    generation: u64,
) -> Result<Vec<StoppedTurn>, studio_runtime_store::StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT id FROM runtime_agents WHERE account_key=?1 AND auto_wake=0 AND ((has_disconnect_recovery=1 AND COALESCE(in_flight,0)=0) OR json_extract(record,'$.startAttempt.interrupt.outcome') IN ('unknown','unsent'))",
    )?;
    let ids = statement
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut turns = Vec::new();
    for id in ids {
        if let Some(agent) = AgentRepository::get(tx, &AgentId::from(id.as_str()))?
            && let Some(turn) = ended_turn(&agent, generation)
        {
            turns.push(turn);
        }
    }
    Ok(turns)
}

/// Record the interrupt outcome on the receipt that owns it, once, while the
/// agent still names that stopped turn.
fn record(
    tx: &studio_runtime_store::Transaction<'_>,
    turn: &StoppedTurn,
    account: &str,
    generation: u64,
    outcome: &str,
    response: &Value,
) -> Result<bool, studio_runtime_store::StoreError> {
    let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(turn.agent.as_str()))? else {
        return Ok(false);
    };
    if agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        != account
        || !owner_matches(&agent, turn, generation)
    {
        return Ok(false);
    }
    if let Owner::Stop { ticket, .. } = &turn.owner {
        // The stop's own receipt settles as the stop would have settled it.
        SchedulerRepository::settle_interrupt_inputs(
            tx,
            &turn.agent,
            account,
            generation,
            &turn.thread,
            response,
        )?;
        SchedulerRepository::settle_interrupt_result(tx, ticket, generation, outcome, None)?;
        return Ok(true);
    }
    let Some(receipt) = agent.extra.get_mut("disconnectRecovery") else {
        return Ok(false);
    };
    if receipt["stopInterrupt"].is_null() {
        receipt["stopInterrupt"] = json!({});
    }
    receipt["stopInterrupt"]["operation"] = json!(turn.interrupt_operation());
    receipt["stopInterrupt"]["generation"] = json!(generation);
    receipt["stopInterrupt"]["outcome"] = json!(outcome);
    AgentRepository::put(tx, &agent)?;
    Ok(true)
}

fn owner_matches(agent: &AgentRecord, turn: &StoppedTurn, generation: u64) -> bool {
    if turn.epoch.checked_add(1) != Some(agent.epoch) {
        return false;
    }
    let value = match turn.owner {
        Owner::Disconnect { .. } => agent.extra.get("disconnectRecovery"),
        Owner::Stop { .. } => agent.extra.get("startAttempt"),
    }
    .unwrap_or(&Value::Null);
    match &turn.owner {
        Owner::Disconnect { signature } => {
            value["epoch"].as_i64() == Some(turn.epoch)
                && value["threadId"] == turn.thread
                && value["turnId"] == turn.turn
                && value["startAttempt"]["connectionGeneration"].as_u64() == Some(generation)
                && *signature
                    == studio_runtime_store::sha256_hex(
                        json!([agent.id.0, turn.epoch, generation, turn.thread, turn.turn])
                            .to_string()
                            .as_bytes(),
                    )
        }
        Owner::Stop { ticket, .. } => {
            value["epoch"].as_i64() == Some(turn.epoch)
                && value["connectionGeneration"].as_u64() == Some(generation)
                && value["interrupt"]["requestTicket"] == *ticket
                && value["interrupt"]["threadId"] == turn.thread
                && value["interrupt"]["turnId"] == turn.turn
        }
    }
}

/// Persist a stopped turn's exact late completion before its transport row retires.
pub(in crate::account_worker) fn settle_stopped_turn(
    tx: &studio_runtime_store::Transaction<'_>,
    call: &super::retained::ProviderCallIntent,
    result: &RequestResult,
) -> studio_runtime_store::StoreResult<bool> {
    if call.method != Method::TurnInterrupt {
        return Ok(true);
    }
    let Some(actor) = &call.actor else {
        return Ok(true);
    };
    let Some(agent) = AgentRepository::get(tx, &AgentId::from(actor.agent.as_str()))? else {
        return Ok(true);
    };
    let (Some(thread), Some(turn)) = (
        call.params["threadId"].as_str(),
        call.params["turnId"].as_str(),
    ) else {
        return Ok(false);
    };
    let owner = if let Some(signature) = call.operation.strip_prefix(INTERRUPT_OPERATION_PREFIX) {
        Owner::Disconnect {
            signature: signature.to_owned(),
        }
    } else if agent
        .extra
        .get("startAttempt")
        .is_some_and(|attempt| attempt["interrupt"]["requestTicket"] == call.operation)
    {
        Owner::Stop {
            ticket: call.operation.clone(),
            terminals: crate::lifecycle_effects::stop_terminals_operation(&call.operation)
                .unwrap_or_default(),
        }
    } else {
        return Ok(true);
    };
    let turn = StoppedTurn {
        agent: actor.agent.clone(),
        epoch: actor.epoch,
        thread: thread.to_owned(),
        turn: turn.to_owned(),
        terminals: None,
        owner,
    };
    let (outcome, response) = match result {
        RequestResult::Reply(reply) => (Outcome::Accepted, reply),
        RequestResult::Rejected(error) => (Outcome::Rejected, &error["data"]["interruptReceipt"]),
        RequestResult::Unknown => return Ok(false),
        RequestResult::NotSent => return Ok(true),
    };
    record(
        tx,
        &turn,
        &call.scope.account,
        call.scope.generation,
        outcome.as_str(),
        response,
    )
}
