//! Transaction adapter for the pure transfer reducer. No provider or file I/O
//! runs under the writer lock. Native integration calls reserve/receive with its
//! exact ticket; it may send only after reserve returns a committed decision.
use crate::account_transfer::{
    Action, Command, Decision, EXECUTOR_REQUIRED, Snapshot, Ticket, TransferError,
};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
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

/// The operation id of a lead summary that still claims an active transfer.
pub(crate) fn active_summary_id(agent: &Value) -> Option<&str> {
    let summary = agent
        .get("accountTransfer")
        .filter(|value| !value.is_null())?;
    if matches!(summary["status"].as_str(), Some("completed" | "cancelled")) {
        return None;
    }
    summary["id"].as_str()
}

/// A lead summary mirrors its transfer row. An active summary without the row
/// can never advance: a move to another server copies the agent record, not the
/// source server's transfer row. Remove that summary so it neither holds the
/// agent's turns nor blocks a new transfer. Returns the changed agent.
fn drop_orphan_summary_in(
    tx: &Transaction<'_>,
    agent_id: &str,
) -> Result<Option<Value>, TransferError> {
    if !Repository::available(tx)? {
        return Ok(None);
    }
    let Some(raw) = Repository::agent(tx, agent_id)? else {
        return Ok(None);
    };
    let mut value: Value =
        serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
    let Some(operation) = active_summary_id(&value) else {
        return Ok(None);
    };
    if Repository::get(tx, operation)?.is_some() {
        return Ok(None);
    }
    crate::account_transfer::remove(&mut value, &["accountTransfer"])?;
    let agent =
        serde_json::from_value(value.clone()).map_err(studio_runtime_store::StoreError::from)?;
    AgentRepository::put(tx, &agent)?;
    crate::agents_create::publish_agent(tx, &value)?;
    Ok(Some(value))
}

pub async fn drop_orphan_summary(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<Option<Value>, TransferError> {
    let agent_id = agent_id.to_owned();
    store
        .transaction_with_error(move |tx| drop_orphan_summary_in(tx, &agent_id))
        .await
}

/// Release a member that was busy at its transfer request once its turn has
/// ended (`Snapshot::release_waiting`). The caller has not claimed the agent's
/// queued input, so the destination account's worker claims it. Returns whether
/// the member was released.
pub async fn release_waiting(
    store: &RuntimeStore,
    agent_id: &str,
    now: f64,
) -> Result<bool, TransferError> {
    let agent_id = agent_id.to_owned();
    store
        .transaction_with_error(move |tx| release_waiting_in(tx, &agent_id, now))
        .await
}

/// Release in the terminal transaction, after source-account continuation policy.
pub(crate) fn release_waiting_in(
    tx: &Transaction<'_>,
    agent_id: &str,
    now: f64,
) -> Result<bool, TransferError> {
    if !Repository::available(tx)? {
        return Ok(false);
    }
    let Some(raw) = Repository::agent(tx, agent_id)? else {
        return Ok(false);
    };
    let agent: Value =
        serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
    let Some(operation) = agent
        .get("accountTransferId")
        .and_then(Value::as_str)
        .filter(|_| !studio_runtime_store::truthy(agent.get("lazyAccountTransfer")))
    else {
        return Ok(false);
    };
    if Repository::get(tx, operation)?.is_none() {
        return Ok(false);
    }
    let Some(decision) = load(tx, operation)?.release_waiting(agent_id)? else {
        return Ok(false);
    };
    persist(tx, decision, now)?;
    Ok(true)
}

/// A transfer may bypass only its own native-start admission marker, and only
/// while its exact member is durably in the Reading phase.
pub fn owns_reading_member(
    tx: &Transaction<'_>,
    transfer_id: &str,
    member_id: &str,
) -> Result<bool, TransferError> {
    let snapshot = load(tx, transfer_id)?;
    let operation = snapshot.operation();
    if operation["status"] != "pending" || operation["members"][member_id]["phase"] != "reading" {
        return Ok(false);
    }
    let agent = snapshot.agent(member_id)?;
    Ok(agent["accountTransferId"] == transfer_id
        && agent["lazyAccountTransfer"]["id"] == transfer_id)
}

fn snapshot_agents(
    snapshot: &Snapshot,
) -> Result<std::collections::BTreeMap<String, Value>, TransferError> {
    let operation = snapshot.operation();
    let mut ids = operation["members"]
        .as_object()
        .map(|members| members.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let lead = operation["leadId"]
        .as_str()
        .ok_or(TransferError::Refused("Invalid transfer lead"))?;
    if !ids.iter().any(|id| id == lead) {
        ids.push(lead.into());
    }
    ids.into_iter()
        .map(|id| {
            let value = snapshot.agent(&id)?.clone();
            Ok((id, value))
        })
        .collect()
}

/// The account worker calls this once during startup before admitting turns.
pub async fn recover(store: &RuntimeStore, now: f64) -> Result<usize, TransferError> {
    store
        .transaction_with_error(move |tx| {
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
        .await
}
pub async fn action(
    store: &RuntimeStore,
    id: &str,
    action: Action,
    now: f64,
) -> Result<Value, TransferError> {
    let id = id.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &id)?;
            persist(tx, snapshot.decide(Command::Action(action))?, now)
        })
        .await
}
/// No provider submission is repeated by this method. It only replays an exact
/// durable request; a genuinely new request needs the executor before any write.
pub async fn request(
    store: &RuntimeStore,
    id: &str,
    lead: &str,
    target: &str,
    scope: &str,
    target_provider: &str,
    now: f64,
) -> Result<Value, TransferError> {
    uuid::Uuid::parse_str(id)
        .map_err(|_| TransferError::Refused("Supply a transfer request id"))?;
    if !matches!(scope, "team" | "subagents") || !matches!(target_provider, "codex" | "claude") {
        return Err(TransferError::Refused(
            "Choose a valid transfer scope and destination provider",
        ));
    }
    let id = id.to_owned();
    let lead = lead.to_owned();
    let target = target.to_owned();
    let scope = scope.to_owned();
    target_provider
        .parse::<studio_runtime_providers::ProviderKind>()
        .map_err(|_| TransferError::Refused("Unknown destination provider"))?;
    let target_provider = target_provider.to_owned();
    store
        .transaction_with_error(move |tx| {
            if !Repository::available(tx)? {
                return Err(TransferError::Requires(EXECUTOR_REQUIRED));
            }
            drop_orphan_summary_in(tx, &lead)?;
            let actor = crate::event_write::checked_agent(tx, &lead)
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
                .is_some_and(|keys| keys.iter().any(|key| key.as_str() == Some(target.as_str())))
            {
                return Err(TransferError::Refused(
                    "This account is disabled for this chat",
                ));
            }
            for key in Repository::ids(tx)? {
                let snapshot = load(tx, &key)?;
                if snapshot.replay(&id, &lead, &target, &scope)? {
                    return snapshot.public_view();
                }
            }
            if let Some(active) = actor.extra.get("accountTransfer").filter(|value| {
                !matches!(value["status"].as_str(), Some("completed" | "cancelled"))
            }) {
                if active["targetAccountKey"] != target
                    || active
                        .get("scope")
                        .and_then(Value::as_str)
                        .unwrap_or("team")
                        != scope
                {
                    return Err(TransferError::Refused(
                        "Finish or cancel the current transfer first",
                    ));
                }
                let active_id = active["id"]
                    .as_str()
                    .ok_or(TransferError::Refused("Invalid current transfer"))?;
                let snapshot = load(tx, active_id)?;
                let mut operation = snapshot.operation().clone();
                let mut agents = snapshot_agents(&snapshot)?;
                let ids = {
                    let mut statement = tx
                        .prepare("SELECT id FROM runtime_agents ORDER BY rowid LIMIT 4097")
                        .map_err(studio_runtime_store::StoreError::from)?;
                    statement
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(studio_runtime_store::StoreError::from)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(studio_runtime_store::StoreError::from)?
                };
                if ids.len() > 4096 {
                    return Err(TransferError::Refused(
                        "Transfer member scan exceeds the supported limit",
                    ));
                }
                let target_provider = operation["targetProvider"]
                    .as_str()
                    .unwrap_or(&target_provider)
                    .to_owned();
                for member_id in ids {
                    if operation["members"].get(&member_id).is_some()
                        || (scope == "subagents" && member_id == lead)
                    {
                        continue;
                    }
                    let Some(raw) = Repository::agent(tx, &member_id)? else {
                        continue;
                    };
                    let mut value: Value = serde_json::from_str(&raw)
                        .map_err(studio_runtime_store::StoreError::from)?;
                    if studio_runtime_store::truthy(value.get("deletedAt"))
                        || (member_id != lead && value["rootId"].as_str() != Some(lead.as_str()))
                    {
                        continue;
                    }
                    operation["members"][&member_id] = crate::account_transfer::enroll(
                        &mut value,
                        active_id,
                        &target,
                        &target_provider,
                        member_id == lead,
                    )?;
                    agents.insert(member_id, value);
                }
                if agents.get(&lead).is_some_and(|lead_agent| {
                    lead_agent.get("accountKey").and_then(Value::as_str) == Some(target.as_str())
                }) {
                    let lead_agent = agents
                        .get_mut(&lead)
                        .ok_or(TransferError::Refused("Unknown transfer lead"))?;
                    let mut defaults = lead_agent
                        .get("workerDefaults")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    defaults.insert("accountKey".into(), json!(target));
                    lead_agent["workerDefaults"] = Value::Object(defaults);
                }
                operation["requests"][id.as_str()] =
                    json!({"leadId":lead,"targetAccountKey":target,"scope":scope});
                let mut decision = Snapshot::read(
                    &operation.to_string(),
                    agents
                        .iter()
                        .map(|(id, value)| (id.clone(), value.to_string())),
                )?
                .decide(Command::Restart)?;
                decision.operation = operation;
                decision.agents = agents;
                decision.changed = true;
                return persist(tx, decision, now);
            }

            let ids = {
                let mut statement = tx
                    .prepare("SELECT id FROM runtime_agents ORDER BY rowid LIMIT 4097")
                    .map_err(studio_runtime_store::StoreError::from)?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(studio_runtime_store::StoreError::from)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(studio_runtime_store::StoreError::from)?
            };
            if ids.len() > 4096 {
                return Err(TransferError::Refused(
                    "Transfer member scan exceeds the supported limit",
                ));
            }
            let mut agents = std::collections::BTreeMap::new();
            for id in ids {
                let Some(raw) = Repository::agent(tx, &id)? else {
                    continue;
                };
                let value: Value =
                    serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
                if studio_runtime_store::truthy(value.get("deletedAt"))
                    || (id != lead && value["rootId"].as_str() != Some(lead.as_str()))
                    || (scope == "subagents" && id == lead)
                {
                    continue;
                }
                agents.insert(id, value);
            }
            if !agents.contains_key(&lead) {
                return Err(TransferError::Refused("Unknown managed agent"));
            }
            if agents.values().any(|agent| {
                agent["environment"] == "linux"
                    && agent
                        .get("accountKey")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        != target
            }) {
                return Err(TransferError::Refused(
                    "Archive Linux VM workers before changing the team account",
                ));
            }
            let operation_id = id;
            let mut requests = serde_json::Map::new();
            requests.insert(
                operation_id.to_owned(),
                json!({"leadId":lead,"targetAccountKey":target,"scope":scope}),
            );
            let mut operation = json!({"id":operation_id,"leadId":lead,"targetAccountKey":target,
            "targetProvider":target_provider,"status":"pending","scope":scope,"created":now,
            "members":{},"requests":Value::Object(requests)});
            for (agent_id, value) in &mut agents {
                operation["members"][agent_id] = crate::account_transfer::enroll(
                    value,
                    &operation_id,
                    &target,
                    &target_provider,
                    agent_id.as_str() == lead.as_str(),
                )?;
            }
            if operation["members"].as_object().is_some_and(|members| {
                members
                    .values()
                    .all(|member| matches!(member["phase"].as_str(), Some("completed" | "left")))
            }) {
                operation["status"] = json!("completed");
            }
            let snapshot = Snapshot::read(
                &operation.to_string(),
                agents
                    .iter()
                    .map(|(id, value)| (id.clone(), value.to_string())),
            )?;
            let mut decision = snapshot.decide(Command::Restart)?;
            decision.operation = operation;
            decision.agents = agents;
            decision.changed = true;
            persist(tx, decision, now)
        })
        .await
}

pub async fn reserve(
    store: &RuntimeStore,
    ticket: &Ticket,
    method: &str,
    params: Value,
    scope: Value,
    now: f64,
) -> Result<(), TransferError> {
    let ticket = ticket.clone();
    let method = method.to_owned();
    store
        .transaction_with_error(move |tx| {
            check_durable_blockers(tx, &ticket.agent)?;
            let snapshot = load(tx, &ticket.operation)?;
            let decision =
                snapshot.provider_intent(&ticket.agent, &ticket.request, scope, &method, params)?;
            persist(tx, decision, now)?;
            Ok(())
        })
        .await
}
pub async fn receive(
    store: &RuntimeStore,
    ticket: &Ticket,
    result: Option<Value>,
    now: f64,
) -> Result<(), TransferError> {
    let ticket = ticket.clone();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &ticket.operation)?;
            persist(tx, snapshot.receive(&ticket, result)?, now)?;
            Ok(())
        })
        .await
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
    for (id, value) in &mut decision.agents {
        let prior = Repository::agent(tx, id)?
            .map(|raw| serde_json::from_str::<Value>(&raw))
            .transpose()
            .map_err(studio_runtime_store::StoreError::from)?;
        if let Some(prior) = &prior
            && prior
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != value
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
            && let Some(resume) = prior.get("usageResume")
            && matches!(
                resume.get("status").and_then(Value::as_str),
                Some("scheduled" | "started")
            )
        {
            let resume_id =
                resume
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(TransferError::Refused(
                        "Invalid account transfer resume identity",
                    ))?;
            let cancelled = studio_runtime_store::cancel_usage_resume(
                tx,
                id,
                resume_id,
                "The chat moved to another account.",
                now,
            )
            .map_err(|error| match error {
                studio_runtime_store::UsageRepositoryError::Store(error) => {
                    TransferError::Store(error)
                }
                _ => TransferError::Stored {
                    object: "usage resume",
                    pointer: "/".into(),
                },
            })?;
            value["usageResume"] = cancelled;
        }
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

/// Asynchronous effect port, implemented by the account worker's bounded native
/// request adapter. A failed wait is unknown, never evidence of non-execution.
pub type TransferFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub trait TransferTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        ticket: &'a Ticket,
        scope: &'a crate::account_session_runtime::ConnectionScope,
        method: &'a str,
        params: &'a Value,
    ) -> TransferFuture<'a, TransferDispatch>;
}
pub struct TransferDispatch {
    pub receipt: Value,
    pub result: Option<Value>,
    pub certainty: crate::account_session_runtime::RequestCertainty,
}
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    ReceiptSaved,
    Unknown,
}
/// The returned ticket reservation is committed before `send` is invoked.
/// Callers must not retry after any error; inspect the durable operation first.
pub async fn send_once(
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
    reserve(store, ticket, method, params.clone(), scope_value, now).await?;
    let dispatch = transport.send(ticket, &scope, method, &params).await;
    let known = dispatch.certainty == crate::account_session_runtime::RequestCertainty::Written
        && dispatch
            .result
            .as_ref()
            .and_then(|r| r.pointer("/thread/id"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
    provider_receipt(
        store,
        ProviderReceipt {
            operation: &ticket.operation,
            agent: &ticket.agent,
            provider_operation: &ticket.request,
            receipt: dispatch.receipt,
            result: dispatch.result,
            certainty: dispatch.certainty,
        },
        now,
    )
    .await?;
    Ok(if known {
        DispatchOutcome::ReceiptSaved
    } else {
        DispatchOutcome::Unknown
    })
}

/// Called after the account worker has checked its live preparation/catalog
/// reservations. Durable local blockers are checked again in this transaction.
pub async fn begin_read(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    now: f64,
) -> Result<(), TransferError> {
    let operation = operation.to_owned();
    let agent = agent.to_owned();
    store
        .transaction_with_error(move |tx| {
            check_durable_blockers(tx, &agent)?;
            persist(tx, load(tx, &operation)?.begin_read(&agent)?, now)?;
            Ok(())
        })
        .await
}

/// Persist a stable provider operation and its observed connection scope after
/// history/settings preparation and before calling the retained session.
pub struct ProviderIntent<'a> {
    pub operation: &'a str,
    pub agent: &'a str,
    pub provider_operation: &'a str,
    pub scope: Value,
    pub method: &'a str,
    pub params: Value,
}

pub async fn provider_intent(
    store: &RuntimeStore,
    intent: ProviderIntent<'_>,
    now: f64,
) -> Result<(), TransferError> {
    let operation = intent.operation.to_owned();
    let agent = intent.agent.to_owned();
    let provider_operation = intent.provider_operation.to_owned();
    let method = intent.method.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &operation)?;
            persist(
                tx,
                snapshot.provider_intent(
                    &agent,
                    &provider_operation,
                    intent.scope,
                    &method,
                    intent.params,
                )?,
                now,
            )?;
            Ok(())
        })
        .await
}

pub(crate) struct ProviderReadIntent<'a> {
    pub transfer: &'a str,
    pub agent: &'a str,
    pub operation: &'a str,
    pub account: &'a str,
    pub scope: Value,
    pub method: &'a str,
    pub params: Value,
}

#[cfg(test)]
mod background_read_compat_tests {
    use super::*;
    use studio_runtime_store::{AgentRecord, EventWriteRepository};

    const TRANSFER: &str = "88aa8d8c-3300-4da8-9c97-efc12a81bbd1";
    const READ: &str = "transfer:88aa8d8c-3300-4da8-9c97-efc12a81bbd1:member:background-before";
    fn scope() -> Value {
        json!({"account":"source","connection":"source-connection","generation":7})
    }

    async fn fixture(evidence: Option<&'static str>) -> (tempfile::TempDir, RuntimeStore, Value) {
        let root = tempfile::tempdir().expect("background read state");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        let legacy = json!({"operation":READ,"account":"source","scope":scope(),
            "method":"thread/backgroundTerminals/list","params":{"threadId":"source-thread"}});
        let old = legacy.clone();
        store.transaction(move |tx| {
            let agent = AgentRecord::from_value(json!({"id":"member","rootId":"member","epoch":3,
                "status":"completed","isLead":true,"autoWake":false,"accountKey":"target",
                "threadId":"source-thread","accountTransferId":TRANSFER}))?;
            AgentRepository::put(tx, &agent)?;
            let mut operation = json!({"id":TRANSFER,"leadId":"member","targetAccountKey":"target",
                "status":"pending","scope":"team","created":1.0,"members":{"member":{
                    "phase":"reading","sourceAccountKey":"source","sourceThreadId":"source-thread",
                    "providerReadIntents":{(READ):old}}},"requests":{}});
            if evidence == Some("transfer-unknown") {
                operation["members"]["member"]["providerReadReceipts"][READ] = json!({"certainty":"unknown"});
            }
            Repository::put(tx, TRANSFER, &operation.to_string())?;
            if let Some(phase) = evidence.filter(|phase| *phase != "transfer-unknown") {
                let key = format!("account-worker:retained:source:{}:{phase}", studio_runtime_store::sha256_hex(READ.as_bytes()));
                EventWriteRepository::replace_receipt(tx, &key, "account-worker", "{\"result\":\"Unknown\"}")?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.expect("legacy read intent");
        (root, store, legacy)
    }

    async fn corrected(
        store: &RuntimeStore,
        expected_scope: Value,
    ) -> Result<Option<Value>, TransferError> {
        provider_read_intent(
            store,
            ProviderReadIntent {
                transfer: TRANSFER,
                agent: "member",
                operation: READ,
                account: "source",
                scope: expected_scope,
                method: "thread/backgroundTerminals/list",
                params: json!({"threadId":"source-thread","limit":100}),
            },
            2.0,
        )
        .await
    }

    #[tokio::test]
    async fn legacy_background_read_without_transport_evidence_keeps_its_logical_identity() {
        let (_root, store, legacy) = fixture(None).await;
        assert!(
            corrected(&store, scope())
                .await
                .expect("correct the proven unsubmitted read")
                .is_none()
        );
        let operation = store
            .read_async("corrected read", |tx| {
                load(tx, TRANSFER).map(|snapshot| snapshot.operation().clone())
            })
            .await
            .expect("operation");
        assert_eq!(
            operation["members"]["member"]["providerReadIntents"][READ]["operation"],
            READ
        );
        assert_eq!(
            operation["members"]["member"]["providerReadIntents"][READ]["params"],
            json!({"threadId":"source-thread","limit":100})
        );
        assert_eq!(
            operation["members"]["member"]["providerReadIntentCorrections"][READ],
            legacy
        );
        assert!(
            corrected(&store, scope())
                .await
                .expect("resume before a native submission")
                .is_none()
        );
    }

    #[tokio::test]
    async fn legacy_background_read_reconnects_only_before_any_native_submission() {
        let (_root, store, legacy) = fixture(None).await;
        for (connection, generation) in
            [("new-source-connection", 8), ("next-source-connection", 9)]
        {
            let current =
                json!({"account":"source","connection":connection,"generation":generation});
            assert!(
                corrected(&store, current.clone())
                    .await
                    .expect("correct the unsubmitted legacy read for the current source session")
                    .is_none()
            );
            let operation = store
                .read_async("reconnected read", |tx| {
                    load(tx, TRANSFER).map(|snapshot| snapshot.operation().clone())
                })
                .await
                .expect("operation");
            let saved = &operation["members"]["member"]["providerReadIntents"][READ];
            assert_eq!(saved["operation"], READ);
            assert_eq!(saved["scope"], current);
            assert_eq!(
                operation["members"]["member"]["providerReadIntentCorrections"][READ],
                legacy
            );
        }
    }

    #[tokio::test]
    async fn legacy_background_read_never_discards_transport_evidence() {
        for evidence in [
            Some("submitted"),
            Some("written"),
            Some("result"),
            Some("transfer-unknown"),
        ] {
            let (_root, store, legacy) = fixture(evidence).await;
            let expected_scope =
                json!({"account":"source","connection":"new-connection","generation":8});
            assert!(
                corrected(&store, expected_scope).await.is_err(),
                "{evidence:?}"
            );
            let operation = store
                .read_async("unchanged read", |tx| {
                    load(tx, TRANSFER).map(|snapshot| snapshot.operation().clone())
                })
                .await
                .expect("operation");
            assert_eq!(
                operation["members"]["member"]["providerReadIntents"][READ],
                legacy
            );
            assert!(operation["members"]["member"]["providerReadIntentCorrections"].is_null());
        }
    }

    #[tokio::test]
    async fn legacy_background_read_refuses_invalid_source_scope_and_changed_thread() {
        for change in [
            "account",
            "empty-connection",
            "zero-generation",
            "negative-generation",
            "extra-scope",
            "old-account",
            "old-generation",
            "thread",
        ] {
            let (_root, store, _) = fixture(None).await;
            let mut current =
                json!({"account":"source","connection":"new-connection","generation":8});
            match change {
                "account" => current["account"] = json!("another-account"),
                "empty-connection" => current["connection"] = json!(""),
                "zero-generation" => current["generation"] = json!(0),
                "negative-generation" => current["generation"] = json!(-1),
                "extra-scope" => current["extra"] = json!(true),
                _ => {
                    store
                        .transaction(move |tx| {
                            let mut operation: Value = serde_json::from_str(
                                &Repository::get(tx, TRANSFER)?.expect("transfer"),
                            )?;
                            let saved =
                                &mut operation["members"]["member"]["providerReadIntents"][READ];
                            match change {
                                "old-account" => {
                                    saved["scope"]["account"] = json!("another-account")
                                }
                                "old-generation" => saved["scope"]["generation"] = json!(0),
                                _ => saved["params"]["threadId"] = json!("another-thread"),
                            }
                            Repository::put(tx, TRANSFER, &operation.to_string())
                        })
                        .await
                        .expect("invalid legacy identity");
                }
            }
            let before = store
                .read_async("before refusal", |tx| {
                    load(tx, TRANSFER).map(|s| s.operation().clone())
                })
                .await
                .expect("operation");
            assert!(corrected(&store, current).await.is_err(), "{change}");
            let after = store
                .read_async("after refusal", |tx| {
                    load(tx, TRANSFER).map(|s| s.operation().clone())
                })
                .await
                .expect("operation");
            assert_eq!(after, before, "{change}");
        }
    }

    #[tokio::test]
    async fn legacy_background_correction_refuses_other_unknown_fingerprints() {
        for changed in ["canonical-unknown", "mutation", "extra-field"] {
            let (_root, store, _) = fixture(None).await;
            store
                .transaction(move |tx| {
                    let mut operation = load(tx, TRANSFER)
                        .map_err(|error| {
                            studio_runtime_store::StoreError::SchemaMismatch(error.to_string())
                        })?
                        .operation()
                        .clone();
                    let saved = &mut operation["members"]["member"]["providerReadIntents"][READ];
                    match changed {
                        "canonical-unknown" => saved["params"]["limit"] = json!(100),
                        "mutation" => saved["method"] = json!("thread/unsubscribe"),
                        _ => saved["params"]["extra"] = json!(true),
                    }
                    Repository::put(tx, TRANSFER, &operation.to_string())
                })
                .await
                .expect("another saved fingerprint");
            let before = store
                .read_async("prior fingerprint", |tx| {
                    load(tx, TRANSFER).map(|snapshot| snapshot.operation().clone())
                })
                .await
                .expect("operation");
            let error = corrected(&store, scope())
                .await
                .expect_err("unknown or changed identity must stay held");
            if changed == "canonical-unknown" {
                assert!(error.to_string().contains("outcome is unknown"));
            }
            let after = store
                .read_async("held fingerprint", |tx| {
                    load(tx, TRANSFER).map(|snapshot| snapshot.operation().clone())
                })
                .await
                .expect("operation");
            assert_eq!(after, before, "{changed}");
        }
    }
}

pub(crate) async fn provider_read_intent(
    store: &RuntimeStore,
    intent: ProviderReadIntent<'_>,
    now: f64,
) -> Result<Option<Value>, TransferError> {
    let transfer = intent.transfer.to_owned();
    let agent = intent.agent.to_owned();
    let operation = intent.operation.to_owned();
    let account = intent.account.to_owned();
    let method = intent.method.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &transfer)?;
            let next = json!({"operation":operation,"account":account,"scope":intent.scope,
                "method":method,"params":intent.params});
            if let Some(reply) = snapshot.written_read_replay(&agent, &next)? {
                return Ok(Some(reply));
            }
            if let Some(correction) = snapshot.correct_unsubmitted_background_read(&agent, &operation, &next)? {
                let evidence = crate::account_worker::retained::has_transport_evidence(tx, &account, &operation)
                    .map_err(|error| match error {
                        crate::account_worker::WorkerError::Store(error) => TransferError::Store(error),
                        _ => TransferError::Refused("Cannot verify the legacy read transport evidence"),
                    })?;
                if evidence {
                    return Err(TransferError::Refused("The legacy background read has transport evidence; its intent is unchanged"));
                }
                persist(tx, correction, now)?;
                return Ok(None);
            }
            let prior = snapshot.operation()["members"][agent.as_str()]["providerReadReceipts"]
                [operation.as_str()]
            .as_object()
            .filter(|receipt| receipt.get("certainty") == Some(&Value::String("written".into())))
            .cloned()
            .map(Value::Object);
            let decision = snapshot.provider_read_intent(
                &agent,
                &operation,
                &account,
                intent.scope,
                &method,
                intent.params,
            )?;
            persist(tx, decision, now)?;
            Ok(prior)
        })
        .await
}

pub(crate) struct ProviderReadReceipt<'a> {
    pub transfer: &'a str,
    pub agent: &'a str,
    pub operation: &'a str,
    pub receipt: Value,
    pub result: Option<Value>,
    pub certainty: crate::account_session_runtime::RequestCertainty,
}

pub(crate) async fn provider_read_receipt(
    store: &RuntimeStore,
    receipt: ProviderReadReceipt<'_>,
    now: f64,
) -> Result<(), TransferError> {
    let transfer = receipt.transfer.to_owned();
    let agent = receipt.agent.to_owned();
    let operation = receipt.operation.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &transfer)?;
            persist(
                tx,
                snapshot.provider_read_receipt(
                    &agent,
                    &operation,
                    receipt.receipt,
                    receipt.result,
                    receipt.certainty,
                )?,
                now,
            )?;
            Ok(())
        })
        .await
}

/// Persist the provider's exact ticket and certainty before adopting any returned
/// thread. Unknown stays held; only a proven NotSent receipt can be retried.
pub struct ProviderReceipt<'a> {
    pub operation: &'a str,
    pub agent: &'a str,
    pub provider_operation: &'a str,
    pub receipt: Value,
    pub result: Option<Value>,
    pub certainty: crate::account_session_runtime::RequestCertainty,
}

pub async fn provider_receipt(
    store: &RuntimeStore,
    receipt: ProviderReceipt<'_>,
    now: f64,
) -> Result<(), TransferError> {
    let operation = receipt.operation.to_owned();
    let agent = receipt.agent.to_owned();
    let provider_operation = receipt.provider_operation.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &operation)?;
            persist(
                tx,
                snapshot.provider_receipt(
                    &agent,
                    &provider_operation,
                    receipt.receipt,
                    receipt.result,
                    receipt.certainty,
                )?,
                now,
            )?;
            Ok(())
        })
        .await
}

pub async fn prepared_member(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    target_settings: Value,
    portable_history: Option<Value>,
    now: f64,
) -> Result<(), TransferError> {
    let operation = operation.to_owned();
    let agent = agent.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &operation)?;
            persist(
                tx,
                snapshot.prepared_member(&agent, target_settings, portable_history)?,
                now,
            )?;
            Ok(())
        })
        .await
}

pub async fn adopt_receipt(
    store: &RuntimeStore,
    operation: &str,
    agent: &str,
    now: f64,
) -> Result<Value, TransferError> {
    let operation = operation.to_owned();
    let agent = agent.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &operation)?;
            persist(tx, snapshot.adopt_receipt(&agent)?, now)
        })
        .await
}

fn check_durable_blockers(tx: &Transaction<'_>, agent: &str) -> Result<(), TransferError> {
    for table in ["runtime_tasks", "runtime_requests"] {
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
    let mut monitors=tx.prepare_cached("SELECT id FROM runtime_monitors WHERE json_extract(record,'$.agent')=?1 AND json_extract(record,'$.status') IN ('running','starting','approval','pending','unknown')").map_err(studio_runtime_store::StoreError::from)?;
    let monitors = monitors
        .query_map([agent], |row| row.get::<_, String>(0))
        .map_err(studio_runtime_store::StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(studio_runtime_store::StoreError::from)?;
    for id in monitors {
        if !crate::account_worker::retained::monitor_survives_account_transfer(tx, &id, agent)
            .map_err(|error| match error {
                crate::account_worker::WorkerError::Store(error) => TransferError::Store(error),
                _ => {
                    TransferError::Refused("Source monitor process ownership could not be verified")
                }
            })?
        {
            return Err(TransferError::Refused(
                "Waiting for verified source monitor ownership. The original process was not stopped.",
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
    fn prepare<'a>(
        &'a self,
        input: &'a TransferPreparation,
    ) -> TransferFuture<'a, Result<PreparedTransfer, TransferError>>;
}

/// Execute one existing member through its durable receipt. Adoption remains a
/// separate transaction owned by the history/settings/resume-cancellation adapter.
/// Call `recover` once at startup, before running members or admitting turns.
pub async fn execute_member(
    store: &RuntimeStore,
    executor: &dyn TransferExecutor,
    operation: &str,
    agent: &str,
    now: f64,
) -> Result<DispatchOutcome, TransferError> {
    executor.available()?;
    let operation_id = operation.to_owned();
    let agent_id = agent.to_owned();
    let transaction_agent_id = agent_id.clone();
    let input = store
        .transaction_with_error(move |tx| {
            if let Err(error) = check_durable_blockers(tx, &transaction_agent_id) {
                if let TransferError::Refused(reason) = &error {
                    let snapshot = load(tx, &operation_id)?;
                    persist(
                        tx,
                        snapshot.waiting_for_blocker(&transaction_agent_id, reason)?,
                        now,
                    )?;
                }
                return Ok(Err(error));
            }
            let snapshot = load(tx, &operation_id)?;
            let decision = snapshot.begin_read(&transaction_agent_id)?;
            let input = TransferPreparation {
                operation: decision.operation.clone(),
                agent: snapshot.agent(&transaction_agent_id)?.clone(),
            };
            persist(tx, decision, now)?;
            Ok::<_, TransferError>(Ok(input))
        })
        .await??;
    let prepared = executor.prepare(&input).await.and_then(|prepared| {
        if prepared.ticket.operation != operation || prepared.ticket.agent != agent {
            return Err(TransferError::Refused(
                "The prepared transfer belongs to another member",
            ));
        }
        Ok(prepared)
    });
    let result = match prepared {
        Ok(prepared) => {
            send_once(
                store,
                executor,
                &prepared.ticket,
                prepared.scope,
                prepared.method.wire(),
                prepared.params,
                now,
            )
            .await
        }
        Err(error) => Err(error),
    };
    if result.is_err() {
        let operation_id = operation.to_owned();
        let failed_agent = agent_id.clone();
        store
            .transaction_with_error(move |tx| {
                let snapshot = load(tx, &operation_id)?;
                persist(
                    tx,
                    snapshot.preparation_failed(
                        &failed_agent,
                        &input.operation["members"][failed_agent.as_str()]["source"],
                    )?,
                    now,
                )?;
                Ok::<_, TransferError>(())
            })
            .await?;
    }
    result
}

#[cfg(test)]
mod request_tests {
    use super::request as request_async;
    use super::*;
    use serde_json::json;
    use std::future::Future;
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository};

    fn run_async<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime builds")
            .block_on(future)
    }

    fn request(
        store: &RuntimeStore,
        id: &str,
        lead: &str,
        target: &str,
        scope: &str,
        provider: &str,
        now: f64,
    ) -> Result<Value, TransferError> {
        run_async(request_async(store, id, lead, target, scope, provider, now))
    }

    fn store_with_agents() -> (tempfile::TempDir, RuntimeStore) {
        let root = tempfile::tempdir().expect("test fixture operation succeeds");
        let store = run_async(RuntimeStore::open(Some(root.path())))
            .expect("test fixture operation succeeds");
        run_async(store.transaction_with_error(|tx| {
            for value in [
                json!({"id":"lead","rootId":"lead","epoch":1,"status":"idle","isLead":true,
                    "accountKey":"source","provider":"codex","model":"gpt-6-astra","cwd":"/tmp/work"}),
                json!({"id":"child","rootId":"lead","epoch":2,"status":"idle","isLead":false,
                    "accountKey":"source","provider":"codex","model":"gpt-6-astra","cwd":"/tmp/work"}),
            ] {
                let agent = AgentRecord::from_value(value).expect("test fixture operation succeeds");
                AgentRepository::put(tx, &agent).expect("test fixture operation succeeds");
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })).expect("test fixture operation succeeds");
        (root, store)
    }

    #[test]
    fn a_running_retained_monitor_does_not_block_its_owners_account_transfer() {
        let (_root, store) = store_with_agents();
        run_async(store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_monitors(id,record) VALUES('watch',?1)",
                [json!({"id":"watch","agent":"lead","epoch":1,"status":"running","operation":{"agent":"lead","epoch":1,"accountKey":"source","connectionId":"original"}}).to_string()])?;
            let key=format!("retained:source:{}",studio_runtime_store::sha256_hex(b"monitor:watch"));
            let submitted=json!({"call":{"scope":{"account":"source","connection":"original","generation":4},"operation":"monitor:watch","actor":{"agent":"lead","epoch":1},"transfer":null,"deleted_actor_session":null,"method":"command/exec","params":{"processId":"watch","cwd":"/tmp/work","command":["true"]}},
                "request":{"id":key,"generation":4,"method":"command/exec","operation":key,"thread":null,"lightweight":true,"deadline":60000,"remoteId":null}});
            tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'test',?2)",(format!("account-worker:{key}:submitted"),submitted.to_string()))?;
            tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'test',?2)",(format!("account-worker:{key}:written"),submitted["request"].to_string()))?;
            Ok(())
        })).expect("persist the original written command");
        let transfer = "5a300f72-4ea6-4d30-8a0e-9b2c13c4a001";
        request(&store, transfer, "lead", "target", "team", "codex", 1.0)
            .expect("request the account transfer");
        run_async(begin_read(&store, transfer, "lead", 2.0))
            .expect("the model account can move without restarting its original process");
        let monitor = studio_runtime_store::MonitorsRepository::get(
            &store.test_reader().expect("reader"),
            "watch",
        )
        .expect("read monitor")
        .expect("monitor remains");
        assert_eq!(monitor.value()["status"], "running");
        assert_eq!(monitor.value()["operation"]["accountKey"], "source");
    }

    #[test]
    fn an_unproven_monitor_publishes_one_transfer_wait_without_repeated_writes() {
        struct NoNative;
        impl TransferTransport for NoNative {
            fn send<'a>(
                &'a self,
                _: &'a Ticket,
                _: &'a crate::account_session_runtime::ConnectionScope,
                _: &'a str,
                _: &'a Value,
            ) -> TransferFuture<'a, TransferDispatch> {
                Box::pin(async { panic!("an unproven monitor cannot dispatch a transfer") })
            }
        }
        impl TransferExecutor for NoNative {
            fn available(&self) -> Result<(), TransferError> {
                Ok(())
            }
            fn prepare<'a>(
                &'a self,
                _: &'a TransferPreparation,
            ) -> TransferFuture<'a, Result<PreparedTransfer, TransferError>> {
                Box::pin(async { panic!("an unproven monitor cannot prepare a transfer") })
            }
        }
        let (_root, store) = store_with_agents();
        run_async(store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_monitors VALUES('watch',?1)",[json!({"id":"watch","agent":"lead","epoch":1,"status":"running","operation":{"agent":"lead","epoch":1,"accountKey":"source","connectionId":"original"}}).to_string()])?;
            Ok(())
        })).expect("persist the unproven monitor");
        let transfer = "5a300f72-4ea6-4d30-8a0e-9b2c13c4a001";
        request(&store, transfer, "lead", "target", "team", "codex", 1.0)
            .expect("request the account transfer");
        let first = run_async(execute_member(&store, &NoNative, transfer, "lead", 2.0))
            .expect_err("unproven ownership blocks the transfer");
        assert!(
            first
                .to_string()
                .contains("verified source monitor ownership")
        );
        let before=run_async(store.transaction(|tx| {
            let raw=Repository::get(tx,transfer)?.expect("transfer remains");
            let operation:Value=serde_json::from_str(&raw)?;
            assert!(operation["members"]["lead"]["waiting"].as_str().expect("visible wait reason").contains("verified source monitor ownership"));
            assert_eq!(operation["members"]["lead"]["phase"],"lazy");
            tx.execute_batch("CREATE TRIGGER no_repeated_transfer_write BEFORE UPDATE ON runtime_account_transfers BEGIN SELECT RAISE(ABORT,'repeated transfer write'); END; CREATE TRIGGER no_repeated_actor_write BEFORE UPDATE ON runtime_agents BEGIN SELECT RAISE(ABORT,'repeated actor write'); END;")?;
            Ok(raw)
        })).expect("save the wait and install no-write guards");
        let second = run_async(execute_member(&store, &NoNative, transfer, "lead", 3.0))
            .expect_err("the same blocker remains read-only");
        assert_eq!(first.to_string(), second.to_string());
        let after = Repository::get(&store.test_reader().expect("reader"), transfer)
            .expect("read unchanged transfer")
            .expect("transfer remains");
        assert_eq!(
            before, after,
            "the same blocker changes no revision or timestamp"
        );
    }

    #[test]
    fn new_request_adopts_lazy_members_and_exact_replay_is_read_only() {
        let (_root, store) = store_with_agents();
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c4226d";
        let first = request(&store, id, "lead", "target", "team", "codex", 10.0)
            .expect("test fixture operation succeeds");
        assert_eq!(first["id"], id);
        assert_eq!(first["targetAccountKey"], "target");
        assert_eq!(first["members"]["child"]["phase"], "lazy");
        let child = AgentRepository::get(
            &store
                .test_reader()
                .expect("test fixture operation succeeds"),
            &AgentId::from("child"),
        )
        .expect("test fixture operation succeeds")
        .expect("test fixture operation succeeds")
        .value();
        assert_eq!(child["accountKey"], "target");
        assert_eq!(child["lazyAccountTransfer"]["sourceAccountKey"], "source");
        let replay = request(&store, id, "lead", "target", "team", "codex", 11.0)
            .expect("test fixture operation succeeds");
        assert_eq!(replay, first);
    }

    #[test]
    fn request_cancels_pending_usage_resume_in_the_transfer_transaction() {
        let (_root, store) = store_with_agents();
        let resume = json!({"id":"resume","status":"scheduled","accountKey":"source",
            "threadId":"source-thread","epoch":1,"turnId":"turn","cause":"rate_limit",
            "failedAt":1.0,"dueAt":200.0,"plannedAt":null,"resetAt":null,"reason":null,
            "taskClaims":[]});
        run_async(store.transaction_with_error(move |tx| {
            let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead exists");
            lead.extra.insert("usageResume".into(), resume.clone());
            lead.extra.insert("usageResumeEnabled".into(), json!(true));
            AgentRepository::put(tx, &lead)?;
            tx.execute(
                "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES(?1,?2,?3)",
                ("resume", "lead", resume.to_string()),
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }))
        .expect("fixture writes atomically");

        request(
            &store,
            "5a300f72-4ea6-4d30-8a0e-9b2c13c42279",
            "lead",
            "target",
            "team",
            "codex",
            10.0,
        )
        .expect("transfer request succeeds");

        let db = store.test_reader().expect("database is readable");
        let agent = AgentRepository::get(&db, &AgentId::from("lead"))
            .expect("lead read succeeds")
            .expect("lead exists")
            .value();
        assert_eq!(agent["usageResume"]["status"], "cancelled");
        assert_eq!(
            agent["usageResume"]["reason"],
            "The chat moved to another account."
        );
        assert_eq!(agent["usageResumeEnabled"], true);
        let saved: String = db
            .query_row(
                "SELECT record FROM runtime_usage_resumes WHERE id='resume'",
                [],
                |row| row.get(0),
            )
            .expect("resume row is readable");
        assert_eq!(
            serde_json::from_str::<Value>(&saved).expect("saved resume is valid JSON")["status"],
            "cancelled"
        );
    }

    #[test]
    fn new_request_adopts_descendants_created_during_active_transfer() {
        let (_root, store) = store_with_agents();
        request(
            &store,
            "5a300f72-4ea6-4d30-8a0e-9b2c13c42271",
            "lead",
            "target",
            "team",
            "codex",
            10.0,
        )
        .expect("test fixture operation succeeds");
        run_async(store.transaction_with_error(|tx| {
            let late = AgentRecord::from_value(json!({"id":"late","rootId":"lead","epoch":3,
                "status":"idle","isLead":false,"accountKey":"source","provider":"codex",
                "model":"gpt-6-astra","cwd":"/tmp/work"}))
            .expect("test fixture operation succeeds");
            AgentRepository::put(tx, &late)
        }))
        .expect("test fixture operation succeeds");
        let result = request(
            &store,
            "5a300f72-4ea6-4d30-8a0e-9b2c13c42272",
            "lead",
            "target",
            "team",
            "codex",
            11.0,
        )
        .expect("test fixture operation succeeds");
        assert_eq!(result["members"]["late"]["phase"], "lazy");
        let late = AgentRepository::get(
            &store
                .test_reader()
                .expect("test fixture operation succeeds"),
            &AgentId::from("late"),
        )
        .expect("test fixture operation succeeds")
        .expect("test fixture operation succeeds")
        .value();
        assert_eq!(late["accountKey"], "target");
        assert_eq!(late["lazyAccountTransfer"]["sourceAccountKey"], "source");
        run_async(store.transaction_with_error(|tx| {
            let stored = load(tx, "5a300f72-4ea6-4d30-8a0e-9b2c13c42271")
                .expect("test fixture operation succeeds");
            assert_eq!(
                stored.operation()["requests"]["5a300f72-4ea6-4d30-8a0e-9b2c13c42272"]["leadId"],
                "lead"
            );
            Ok::<_, studio_runtime_store::StoreError>(())
        }))
        .expect("test fixture operation succeeds");
    }

    fn set_lead_busy(store: &RuntimeStore, busy: bool) {
        run_async(store.transaction_with_error(move |tx| {
            let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead exists");
            lead.status = if busy {
                studio_runtime_store::AgentStatus::Running
            } else {
                studio_runtime_store::AgentStatus::Idle
            };
            lead.extra.insert("inFlight".into(), json!(busy));
            AgentRepository::put(tx, &lead)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }))
        .expect("lead state is written");
    }

    fn stored(store: &RuntimeStore, id: &str) -> Value {
        AgentRepository::get(
            &store.test_reader().expect("database is readable"),
            &AgentId::from(id),
        )
        .expect("agent read succeeds")
        .expect("agent exists")
        .value()
    }

    #[test]
    fn busy_member_says_why_it_waits_and_moves_once_its_turn_ended() {
        let (_root, store) = store_with_agents();
        set_lead_busy(&store, true);
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c4a101";
        let first = request(&store, id, "lead", "target", "team", "codex", 10.0)
            .expect("transfer request succeeds");
        assert_eq!(first["members"]["lead"]["phase"], "waiting");
        assert_eq!(
            first["members"]["lead"]["waiting"],
            crate::account_transfer::WAITING_FOR_NEXT_TURN
        );
        assert_eq!(first["members"]["child"]["phase"], "lazy");
        assert!(
            !run_async(release_waiting(&store, "lead", 11.0)).expect("release reads"),
            "a member stays while its turn runs"
        );
        assert_eq!(stored(&store, "lead")["accountKey"], "source");

        set_lead_busy(&store, false);
        assert!(run_async(release_waiting(&store, "lead", 12.0)).expect("release writes"));
        let lead = stored(&store, "lead");
        assert_eq!(lead["accountKey"], "target");
        assert_eq!(lead["accountTransferId"], id);
        assert_eq!(lead["lazyAccountTransfer"]["sourceAccountKey"], "source");
        assert_eq!(lead["accountTransfer"]["waitingCount"], 0);
        assert_eq!(lead["accountTransfer"]["nativeHistoryPending"], 2);
        let member = run_async(store.transaction_with_error(move |tx| {
            Ok::<_, TransferError>(load(tx, id)?.operation()["members"]["lead"].clone())
        }))
        .expect("transfer reads");
        assert_eq!(member["phase"], "lazy");
        assert_eq!(member["lazy"], true);
        assert!(
            !run_async(release_waiting(&store, "lead", 13.0)).expect("release reads"),
            "a released member is not released again"
        );
    }

    #[test]
    fn invalid_scope_and_excluded_account_refuse_without_writes() {
        let (_root, store) = store_with_agents();
        let before = AgentRepository::get(
            &store
                .test_reader()
                .expect("test fixture operation succeeds"),
            &AgentId::from("lead"),
        )
        .expect("test fixture operation succeeds")
        .expect("test fixture operation succeeds")
        .value();
        assert!(
            request(
                &store,
                "5a300f72-4ea6-4d30-8a0e-9b2c13c4226e",
                "lead",
                "target",
                "bad",
                "codex",
                10.0
            )
            .is_err()
        );
        run_async(store.transaction_with_error(|tx| {
            let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?
                .expect("test fixture operation succeeds");
            lead.extra
                .insert("excludedAccountKeys".into(), json!(["blocked"]));
            AgentRepository::put(tx, &lead)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }))
        .expect("test fixture operation succeeds");
        let before_blocked = AgentRepository::get(
            &store
                .test_reader()
                .expect("test fixture operation succeeds"),
            &AgentId::from("lead"),
        )
        .expect("test fixture operation succeeds")
        .expect("test fixture operation succeeds")
        .value();
        assert!(
            request(
                &store,
                "5a300f72-4ea6-4d30-8a0e-9b2c13c4226f",
                "lead",
                "blocked",
                "team",
                "codex",
                10.0
            )
            .is_err()
        );
        let after = AgentRepository::get(
            &store
                .test_reader()
                .expect("test fixture operation succeeds"),
            &AgentId::from("lead"),
        )
        .expect("test fixture operation succeeds")
        .expect("test fixture operation succeeds")
        .value();
        assert_eq!(after, before_blocked);
        assert_eq!(before["accountKey"], "source");
    }
}
