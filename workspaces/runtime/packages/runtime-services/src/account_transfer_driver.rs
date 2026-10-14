//! Transaction adapter for the pure transfer reducer. No provider or file I/O
//! runs under the writer lock. Native integration calls reserve/receive with its
//! exact ticket; it may send only after reserve returns a committed decision.
use crate::account_transfer::{
    Action, Command, Decision, EXECUTOR_REQUIRED, Snapshot, Ticket, TransferError,
};
use serde_json::Value;
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
    scope: Value,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        check_durable_blockers(tx, &ticket.agent)?;
        let snapshot = load(tx, &ticket.operation)?;
        let decision =
            snapshot.provider_intent(&ticket.agent, &ticket.request, scope, method, params)?;
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
    fn send(
        &self,
        ticket: &Ticket,
        scope: &crate::account_session_runtime::ConnectionScope,
        method: &str,
        params: &Value,
    ) -> TransferDispatch;
}
pub struct TransferDispatch {
    pub receipt: Value,
    pub result: Option<Value>,
    pub certainty: &'static str,
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
    scope: crate::account_session_runtime::ConnectionScope,
    method: &str,
    params: Value,
    now: f64,
) -> Result<DispatchOutcome, TransferError> {
    let scope_value = serde_json::to_value(&scope)
        .map_err(|_| TransferError::Refused("Invalid provider scope"))?;
    reserve(store, ticket, method, params.clone(), scope_value, now)?;
    let dispatch = transport.send(ticket, &scope, method, &params);
    let known = dispatch.certainty == "written"
        && dispatch
            .result
            .as_ref()
            .and_then(|r| r.pointer("/thread/id"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
    provider_receipt(
        store,
        &ticket.operation,
        &ticket.agent,
        &ticket.request,
        dispatch.receipt,
        dispatch.result,
        dispatch.certainty,
        now,
    )?;
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
        check_durable_blockers(tx, agent)?;
        persist(tx, load(tx, operation)?.begin_read(agent)?, now)?;
        Ok(())
    })
}

/// Persist a stable provider operation and its observed connection scope after
/// history/settings preparation and before calling the retained session.
pub fn provider_intent(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    provider_operation: &str,
    scope: Value,
    method: &str,
    params: Value,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, operation)?;
        persist(
            tx,
            snapshot.provider_intent(agent, provider_operation, scope, method, params)?,
            now,
        )?;
        Ok(())
    })
}

pub fn provider_read_intent(
    store: &RuntimeStore,
    transfer: &str,
    agent: &str,
    operation: &str,
    account: &str,
    scope: Value,
    method: &str,
    params: Value,
    now: f64,
) -> Result<Option<Value>, TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, transfer)?;
        let prior = snapshot.operation()["members"][agent]["providerReadReceipts"][operation]
            .as_object()
            .filter(|receipt| receipt.get("certainty") == Some(&Value::String("written".into())))
            .cloned()
            .map(Value::Object);
        let decision =
            snapshot.provider_read_intent(agent, operation, account, scope, method, params)?;
        persist(tx, decision, now)?;
        Ok(prior)
    })
}

pub fn provider_read_receipt(
    store: &RuntimeStore,
    transfer: &str,
    agent: &str,
    operation: &str,
    receipt: Value,
    result: Option<Value>,
    certainty: &str,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, transfer)?;
        persist(
            tx,
            snapshot.provider_read_receipt(agent, operation, receipt, result, certainty)?,
            now,
        )?;
        Ok(())
    })
}

/// Persist the provider's exact ticket and certainty before adopting any returned
/// thread. Unknown stays held; only a proven NotSent receipt can be retried.
pub fn provider_receipt(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    provider_operation: &str,
    receipt: Value,
    result: Option<Value>,
    certainty: &str,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, operation)?;
        persist(
            tx,
            snapshot.provider_receipt(agent, provider_operation, receipt, result, certainty)?,
            now,
        )?;
        Ok(())
    })
}

pub fn prepared_member(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    target_settings: Value,
    portable_history: Option<Value>,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, operation)?;
        persist(
            tx,
            snapshot.prepared_member(agent, target_settings, portable_history)?,
            now,
        )?;
        Ok(())
    })
}

pub fn adopt_receipt(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    now: f64,
) -> Result<Value, TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, operation)?;
        persist(tx, snapshot.adopt_receipt(agent)?, now)
    })
}

fn check_durable_blockers(tx: &Transaction<'_>, agent: &str) -> Result<(), TransferError> {
    for table in ["runtime_tasks", "runtime_monitors", "runtime_requests"] {
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM {table} WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.status') IN ('running','starting','approval','pending','unknown'))"
        );
        let busy: bool = tx
            .query_row(&sql, [agent], |r| r.get(0))
            .map_err(studio_runtime_store::StoreError::from)?;
        if busy {
            return Err(TransferError::Refused(
                "Waiting for background work or a tool response",
            ));
        }
    }
    let uncertain: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status IN ('reserved','dispatching','uncertain'))",[agent],|r| r.get(0)).map_err(studio_runtime_store::StoreError::from)?;
    if uncertain {
        return Err(TransferError::Refused(
            "Waiting for confirmed message delivery",
        ));
    }
    Ok(())
}

/// Captured after the durable reading intent. History and destination settings
/// are prepared outside the writer transaction from these checked records.
pub struct TransferPreparation {
    operation: Value,
    agent: Value,
}
impl TransferPreparation {
    pub fn operation(&self) -> &Value {
        &self.operation
    }
    pub fn agent(&self) -> &Value {
        &self.agent
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferMethod {
    Start,
    Fork,
}
impl TransferMethod {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Start => "thread/start",
            Self::Fork => "thread/fork",
        }
    }
}
pub struct PreparedTransfer {
    pub ticket: Ticket,
    pub scope: crate::account_session_runtime::ConnectionScope,
    pub method: TransferMethod,
    pub params: Value,
}

/// P07 supplies history/settings preparation; P01 supplies the retained transport.
/// `available` must refuse before any write if either dependency is absent.
/// Preparation must be bounded and safe to repeat for the same operation/member;
/// it must not send a native mutation. The transport must use the supplied ticket.
pub trait TransferExecutor: TransferTransport {
    fn available(&self) -> Result<(), TransferError>;
    fn prepare(&self, input: &TransferPreparation) -> Result<PreparedTransfer, TransferError>;
}

/// Execute one existing member through its durable receipt. Adoption remains a
/// separate transaction owned by the history/settings/resume-cancellation adapter.
/// Call `recover` once at startup, before running members or admitting turns.
pub fn execute_member(
    store: &RuntimeStore,
    executor: &dyn TransferExecutor,
    operation: &str,
    agent: &str,
    now: f64,
) -> Result<DispatchOutcome, TransferError> {
    executor.available()?;
    let input = store.transaction_with_error(|tx| {
        check_durable_blockers(tx, agent)?;
        let snapshot = load(tx, operation)?;
        let decision = snapshot.begin_read(agent)?;
        let input = TransferPreparation {
            operation: decision.operation.clone(),
            agent: snapshot.agent(agent)?.clone(),
        };
        persist(tx, decision, now)?;
        Ok::<_, TransferError>(input)
    })?;
    let prepared = executor.prepare(&input).and_then(|prepared| {
        if prepared.ticket.operation != operation || prepared.ticket.agent != agent {
            return Err(TransferError::Refused(
                "The prepared transfer belongs to another member",
            ));
        }
        Ok(prepared)
    });
    let result = prepared.and_then(|prepared| {
        send_once(
            store,
            executor,
            &prepared.ticket,
            prepared.scope,
            prepared.method.wire(),
            prepared.params,
            now,
        )
    });
    if result.is_err() {
        store.transaction_with_error(|tx| {
            let snapshot = load(tx, operation)?;
            persist(
                tx,
                snapshot.preparation_failed(agent, &input.operation["members"][agent]["source"])?,
                now,
            )?;
            Ok::<_, TransferError>(())
        })?;
    }
    result
}
