//! End the provider work of a turn that its owner stopped while no account
//! worker was attached. A lost Studio connection leaves the agent interrupted
//! with a disconnect receipt, while the provider process keeps its generation,
//! the turn, and the turn's commands running. A stop in that window has no
//! connection to send `turn/interrupt` on, so it commits only the next epoch.
//! Each new account worker checks once, on its own connection, for such a
//! turn of its provider generation, and ends it as the stop would have.
use super::retained::{RequestActor, RequestResult, RetainedSessionPort, SessionRequest};
use crate::recovery_history::ConnectionScope;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_providers::{Method, ProviderKind};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore};

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
    signature: String,
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
        let interrupt = request(
            session,
            scope,
            &turn,
            format!("{INTERRUPT_OPERATION_PREFIX}{}", turn.signature),
            Method::TurnInterrupt,
            json!({"threadId":turn.thread,"turnId":turn.turn}),
        )
        .await;
        // An interrupt ends the turn, not the commands it left running.
        if interrupt != Outcome::NotSent
            && let Some(method) = turn.terminals
        {
            let cleaned = request(
                session,
                scope,
                &turn,
                format!("{TERMINALS_OPERATION_PREFIX}{}", turn.signature),
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
        if crate::scheduler::lifecycle_transaction_with(&store, move |tx| {
            record(tx, &recorded, generation, outcome)
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
/// ledger replay a lost reply instead of sending the request again.
async fn request(
    session: &RetainedSessionPort,
    scope: &ConnectionScope,
    turn: &StoppedTurn,
    operation: String,
    method: Method,
    params: Value,
) -> Outcome {
    let receipt = session
        .request(SessionRequest {
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
        })
        .await;
    match receipt {
        Ok(receipt) => match receipt.result {
            RequestResult::Reply(_) => Outcome::Accepted,
            RequestResult::Rejected(_) => Outcome::Rejected,
            RequestResult::NotSent => Outcome::NotSent,
            RequestResult::Unknown => Outcome::Unknown,
        },
        Err(super::retained::PortError::Unknown) => Outcome::Unknown,
        Err(_) => Outcome::NotSent,
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
        .and_then(ProviderKind::stop_terminals_method);
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
        signature,
    })
}

fn stopped_turns(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    generation: u64,
) -> Result<Vec<StoppedTurn>, studio_runtime_store::StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT id FROM runtime_agents WHERE account_key=?1 AND has_disconnect_recovery=1 AND COALESCE(in_flight,0)=0 AND auto_wake=0",
    )?;
    let ids = statement
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut turns = Vec::new();
    for id in ids {
        if let Some(agent) = AgentRepository::get(tx, &AgentId::from(id.as_str()))?
            && let Some(turn) = stopped_turn(&agent, generation)
        {
            turns.push(turn);
        }
    }
    Ok(turns)
}

/// Record the interrupt outcome on the receipt it ended, once, while the agent
/// still names that stopped turn.
fn record(
    tx: &studio_runtime_store::Transaction<'_>,
    turn: &StoppedTurn,
    generation: u64,
    outcome: &str,
) -> Result<bool, studio_runtime_store::StoreError> {
    let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(turn.agent.as_str()))? else {
        return Ok(false);
    };
    if stopped_turn(&agent, generation).is_none_or(|current| current.signature != turn.signature) {
        return Ok(false);
    }
    let Some(receipt) = agent.extra.get_mut("disconnectRecovery") else {
        return Ok(false);
    };
    receipt["stopInterrupt"] = json!({
        "operation":format!("{INTERRUPT_OPERATION_PREFIX}{}", turn.signature),
        "generation":generation,
        "outcome":outcome,
    });
    AgentRepository::put(tx, &agent)?;
    Ok(true)
}
