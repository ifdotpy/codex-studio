//! Transaction adapter for the pure transfer reducer. No provider or file I/O
//! runs under the writer lock. Native integration calls reserve/receive with its
//! exact ticket; it may send only after reserve returns a committed decision.
use crate::account_transfer::{
    Action, Command, Decision, EXECUTOR_REQUIRED, Snapshot, Ticket, TransferError,
};
use serde_json::{Value, json};
use studio_runtime_store::{
    AccountTransferRepository as Repository, AgentRepository, RuntimeStore, Transaction,
};

pub fn load(tx: &Transaction<'_>, id: &str) -> Result<Snapshot, TransferError> {
    if !Repository::available(tx)? {
        return Err(TransferError::Requires(
            "store-bootstrap/account-transfer-schema",
        ));
    }
    let raw = Repository::get(tx, id)?.ok_or(TransferError::Refused("Unknown account transfer"))?;
    let op: Value = serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
    let mut ids = op["members"]
        .as_object()
        .map(|m| m.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let lead = op["leadId"]
        .as_str()
        .ok_or(TransferError::Refused("Invalid transfer lead"))?;
    if !ids.iter().any(|id| id == lead) {
        ids.push(lead.into());
    }
    let agents = ids
        .into_iter()
        .map(|id| {
            let raw = Repository::agent(tx, &id)?
                .ok_or(TransferError::Refused("Unknown transfer member"))?;
            Ok((id, raw))
        })
        .collect::<Result<Vec<_>, TransferError>>()?;
    Snapshot::read(&raw, agents)
}

/// The account worker calls this once during startup before admitting turns.
pub fn recover(store: &RuntimeStore, now: f64) -> Result<usize, TransferError> {
    store.transaction_with_error(|tx| {
        if !Repository::available(tx)? {
            return Ok(0);
        }
        let mut changed = 0;
        for id in Repository::ids(tx)? {
            let snapshot = load(tx, &id)?;
            let decision = snapshot.decide(Command::Restart)?;
            changed += usize::from(decision.changed);
            persist(tx, decision, now)?;
        }
        Ok(changed)
    })
}
pub fn action(
    store: &RuntimeStore,
    id: &str,
    action: Action,
    now: f64,
) -> Result<Value, TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, id)?;
        persist(tx, snapshot.decide(Command::Action(action))?, now)
    })
}
/// No provider submission is repeated by this method. It only replays an exact
/// durable request; a genuinely new request needs the executor before any write.
pub fn request(
    store: &RuntimeStore,
    id: &str,
    lead: &str,
    target: &str,
    scope: &str,
) -> Result<Value, TransferError> {
    uuid::Uuid::parse_str(id)
        .map_err(|_| TransferError::Refused("Supply a transfer request id"))?;
    store.transaction_with_error(|tx| {
        if !Repository::available(tx)? {
            return Err(TransferError::Requires(EXECUTOR_REQUIRED));
        }
        let actor = crate::event_write::checked_agent(tx, lead)
            .map_err(|_| TransferError::Refused("Unknown managed agent"))?;
        if actor.extra.get("isLead") != Some(&Value::Bool(true)) {
            return Err(TransferError::Refused(
                "Choose the orchestrator to transfer its team",
            ));
        }
        if actor
            .extra
            .get("excludedAccountKeys")
            .and_then(Value::as_array)
            .is_some_and(|keys| keys.iter().any(|key| key == target))
        {
            return Err(TransferError::Refused(
                "This account is disabled for this chat",
            ));
        }
        for key in Repository::ids(tx)? {
            let snapshot = load(tx, &key)?;
            if snapshot.replay(id, lead, target, scope)? {
                return snapshot.public_view();
            }
        }
        Err(TransferError::Requires(EXECUTOR_REQUIRED))
    })
}

pub fn reserve(
    store: &RuntimeStore,
    ticket: &Ticket,
    method: &str,
    params: Value,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, &ticket.operation)?;
        let mut decision = snapshot.submit(ticket, method, params)?;
        decision.operation["members"][&ticket.agent]["submittedAt"] = json!(now);
        persist(tx, decision, now)?;
        Ok(())
    })
}
pub fn receive(
    store: &RuntimeStore,
    ticket: &Ticket,
    result: Option<Value>,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, &ticket.operation)?;
        persist(tx, snapshot.receive(ticket, result)?, now)?;
        Ok(())
    })
}

fn persist(tx: &Transaction<'_>, mut decision: Decision, now: f64) -> Result<Value, TransferError> {
    if !decision.changed {
        return decision.public_view();
    }
    let lead = decision.operation["leadId"]
        .as_str()
        .ok_or(TransferError::Refused("Invalid transfer lead"))?;
    let old = decision
        .agents
        .get(lead)
        .and_then(|a| a.pointer("/accountTransfer/id"))
        .and_then(Value::as_str);
    let previous_created = old
        .map(|id| Repository::get(tx, id))
        .transpose()?
        .flatten()
        .map(|raw| serde_json::from_str::<Value>(&raw))
        .transpose()
        .map_err(studio_runtime_store::StoreError::from)?
        .and_then(|op| op["created"].as_f64());
    decision.stamp(now, previous_created)?;
    let raw = decision.encoded()?;
    let id = decision.operation["id"]
        .as_str()
        .ok_or(TransferError::Refused("Invalid transfer identity"))?;
    Repository::put(tx, id, &raw)?;
    for (id, value) in &decision.agents {
        let prior = Repository::agent(tx, id)?
            .map(|raw| serde_json::from_str::<Value>(&raw))
            .transpose()
            .map_err(studio_runtime_store::StoreError::from)?;
        if prior.as_ref() == Some(value) {
            continue;
        }
        let agent = serde_json::from_value(value.clone())
            .map_err(studio_runtime_store::StoreError::from)?;
        AgentRepository::put(tx, &agent)?;
        crate::agents_create::publish_agent(tx, value)?;
    }
    for followup in &decision.followups {
        let value = decision
            .agents
            .get(&followup.agent)
            .ok_or(TransferError::Refused("Unknown transfer member"))?;
        let agent = serde_json::from_value(value.clone())
            .map_err(studio_runtime_store::StoreError::from)?;
        crate::event_write::enqueue_event(
            tx,
            &agent,
            followup.id.clone(),
            "followup",
            followup.text.into(),
            now,
        )
        .map_err(|_| TransferError::Refused("Cannot enqueue transfer continuation"))?;
    }
    decision.public_view()
}

/// Synchronous effect port, implemented by the account worker's bounded native
/// request adapter. A failed wait is unknown, never evidence of non-execution.
pub trait TransferTransport {
    fn send(&self, ticket: &Ticket, method: &str, params: &Value) -> Option<Value>;
}
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    ReceiptSaved,
    Unknown,
}
/// The returned ticket reservation is committed before `send` is invoked.
/// Callers must not retry after any error; inspect the durable operation first.
pub fn send_once(
    store: &RuntimeStore,
    transport: &dyn TransferTransport,
    ticket: &Ticket,
    method: &str,
    params: Value,
    now: f64,
) -> Result<DispatchOutcome, TransferError> {
    reserve(store, ticket, method, params.clone(), now)?;
    let result = transport.send(ticket, method, &params);
    let known = result
        .as_ref()
        .and_then(|r| r.pointer("/thread/id"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    receive(store, ticket, result, now)?;
    Ok(if known {
        DispatchOutcome::ReceiptSaved
    } else {
        DispatchOutcome::Unknown
    })
}

/// Called after the account worker has checked its live preparation/catalog
/// reservations. Durable local blockers are checked again in this transaction.
pub fn begin_read(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        for table in ["runtime_tasks", "runtime_monitors", "runtime_requests"] {
            let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.status') IN ('running','starting','approval','pending','unknown'))");
            let busy: bool = tx.query_row(&sql, [agent], |r| r.get(0)).map_err(studio_runtime_store::StoreError::from)?;
            if busy { return Err(TransferError::Refused("Waiting for background work or a tool response")); }
        }
        let uncertain: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status IN ('reserved','dispatching','uncertain'))",[agent],|r| r.get(0)).map_err(studio_runtime_store::StoreError::from)?;
        if uncertain { return Err(TransferError::Refused("Waiting for confirmed message delivery")); }
        persist(tx, load(tx, operation)?.begin_read(agent)?, now)?;
        Ok(())
    })
}
