//! Transaction adapter for the pure transfer reducer. No provider or file I/O
//! runs under the writer lock. Native integration calls reserve/receive with its
//! exact ticket; it may send only after reserve returns a committed decision.
pub use crate::account_transfer::TransferDispatch;
use crate::account_transfer::{
    Action, Command, Decision, EXECUTOR_REQUIRED, Snapshot, Ticket, TransferError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use studio_runtime_store::{
    AccountTransferRepository as Repository, AgentId, AgentRepository, RuntimeStore, Transaction,
};

pub fn load(tx: &Transaction<'_>, id: &str) -> Result<Snapshot, TransferError> {
    if !Repository::available(tx)? {
        return Err(TransferError::Requires(
            "store-bootstrap/account-transfer-schema",
        ));
    }
    let (op, raw) = Repository::get_with_raw(tx, id)?
        .ok_or(TransferError::Refused("Unknown account transfer"))?;
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
    let agents = ids.into_iter().map(|id| {
        let raw =
            Repository::agent(tx, &id)?.ok_or(TransferError::Refused("Unknown transfer member"))?;
        Ok((id, raw))
    });
    Snapshot::read_fallible(&raw, agents)
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
            for id in Repository::recovery_ids(tx)? {
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
    if !matches!(scope, "team" | "subagents") {
        return Err(TransferError::Refused(
            "Choose a valid transfer scope and destination provider",
        ));
    }
    let id = id.to_owned();
    let lead = lead.to_owned();
    let target = target.to_owned();
    let scope = scope.to_owned();
    let target_provider = target_provider
        .parse::<studio_runtime_providers::ProviderKind>()
        .map_err(|_| {
            TransferError::Refused("Choose a valid transfer scope and destination provider")
        })?;
    let target_provider = target_provider.key().to_owned();
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
            if let Some(key) = Repository::request_operation(tx, &id)? {
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
                let target_provider = operation["targetProvider"]
                    .as_str()
                    .unwrap_or(&target_provider)
                    .to_owned();
                for member in Repository::team_agents(tx, &AgentId::from(lead.as_str()))? {
                    let member_id = member.id.0.clone();
                    if operation["members"].get(&member_id).is_some()
                        || (scope == "subagents" && member_id == lead)
                    {
                        continue;
                    }
                    let mut value = member.into_value();
                    operation["members"][&member_id] = crate::account_transfer::enroll(
                        &mut value,
                        active_id,
                        &target,
                        &target_provider,
                        member_id == lead,
                    )?;
                    agents.insert(member_id, value);
                }
                if scope == "subagents"
                    || agents.get(&lead).is_some_and(|lead_agent| {
                        lead_agent.get("accountKey").and_then(Value::as_str)
                            == Some(target.as_str())
                    })
                {
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

            let mut agents: std::collections::BTreeMap<_, _> =
                Repository::team_agents(tx, &AgentId::from(lead.as_str()))?
                    .into_iter()
                    .map(|agent| (agent.id.0.clone(), agent.into_value()))
                    .collect();
            if !agents.contains_key(&lead) {
                return Err(TransferError::Refused("Unknown managed agent"));
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
                if scope == "subagents" && agent_id == &lead {
                    let mut defaults = value
                        .get("workerDefaults")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    defaults.insert("accountKey".into(), json!(target));
                    value["workerDefaults"] = Value::Object(defaults);
                    continue;
                }
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
            let mut decision =
                snapshot.provider_intent(&ticket.agent, &ticket.request, scope, &method, params)?;
            admit_mutation(&snapshot, &mut decision, &ticket.agent)?;
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
                studio_runtime_store::UsageRepositoryError::MissingFamily
                | studio_runtime_store::UsageRepositoryError::EventDriverRequired
                | studio_runtime_store::UsageRepositoryError::UnknownAgent
                | studio_runtime_store::UsageRepositoryError::UnknownContinuation
                | studio_runtime_store::UsageRepositoryError::InvalidStoredObject { .. }
                | studio_runtime_store::UsageRepositoryError::Sqlite(_) => TransferError::Stored {
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
    ) -> TransferFuture<'a, Result<TransferDispatch, TransferError>>;
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
    let dispatch = transport.send(ticket, &scope, method, &params).await?;
    let known = dispatch.has_thread();
    provider_receipt(
        store,
        ProviderReceipt {
            operation: &ticket.operation,
            agent: &ticket.agent,
            provider_operation: &ticket.request,
            receipt: dispatch,
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
            let mut decision = snapshot.provider_intent(
                &agent,
                &provider_operation,
                intent.scope,
                &method,
                intent.params,
            )?;
            admit_mutation(&snapshot, &mut decision, &agent)?;
            persist(tx, decision, now)?;
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

/// The root is created only with a new transfer obligation. Imported obligations
/// carry their final provider token from the migration.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderAdmission {
    pub root_request_id: String,
    pub admitted_request: Option<studio_runtime_store::request_results::AdmittedRequest>,
}

fn admit_mutation(
    snapshot: &Snapshot,
    decision: &mut Decision,
    agent: &str,
) -> Result<(), TransferError> {
    if snapshot.operation()["members"][agent]
        .get("providerIntent")
        .is_none()
    {
        decision.operation["members"][agent]["providerAdmission"] = json!(ProviderAdmission {
            root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
            admitted_request: None,
        });
    } else if snapshot.operation()["members"][agent]
        .get("providerAdmission")
        .is_none()
    {
        return Err(TransferError::Refused(
            "The transfer provider admission is missing",
        ));
    }
    Ok(())
}

pub(crate) async fn provider_admission(
    store: &RuntimeStore,
    transfer: &str,
    agent: &str,
    operation: &str,
) -> Result<ProviderAdmission, TransferError> {
    let transfer = transfer.to_owned();
    let agent = agent.to_owned();
    let operation = operation.to_owned();
    store
        .read_async("account-transfer/provider-admission", move |tx| {
            let snapshot = load(tx, &transfer)?;
            let member = &snapshot.operation()["members"][&agent];
            let admission = if member["providerIntent"]["operation"] == operation {
                &member["providerAdmission"]
            } else {
                &member["providerReadAdmissions"][&operation]
            };
            serde_json::from_value(admission.clone())
                .map_err(|_| TransferError::Refused("The transfer provider admission is missing"))
        })
        .await
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
            let prior = snapshot.operation()["members"][agent.as_str()]["providerReadReceipts"]
                [operation.as_str()]
            .as_object()
            .filter(|receipt| receipt.get("certainty") == Some(&Value::String("written".into())))
            .cloned()
            .map(Value::Object);
            let mut decision = snapshot.provider_read_intent(
                &agent,
                &operation,
                &account,
                intent.scope,
                &method,
                intent.params,
            )?;
            if method == "thread/unsubscribe" {
                if snapshot.operation()["members"][&agent]["providerReadIntents"]
                    .get(&operation)
                    .is_none()
                {
                    decision.operation["members"][&agent]["providerReadAdmissions"][&operation] =
                        json!(ProviderAdmission {
                            root_request_id:
                                studio_runtime_schema::persistence::RequestIdentity::issue_root(),
                            admitted_request: None,
                        });
                } else if snapshot.operation()["members"][&agent]["providerReadAdmissions"]
                    .get(&operation)
                    .is_none()
                {
                    return Err(TransferError::Refused(
                        "The transfer provider admission is missing",
                    ));
                }
            }
            persist(tx, decision, now)?;
            Ok(prior)
        })
        .await
}

pub(crate) struct ProviderReadReceipt<'a> {
    pub transfer: &'a str,
    pub agent: &'a str,
    pub operation: &'a str,
    pub receipt: crate::account_session_runtime::SessionReceipt,
}

pub(crate) async fn provider_read_receipt(
    store: &RuntimeStore,
    receipt: ProviderReadReceipt<'_>,
    now: f64,
) -> Result<crate::account_session_runtime::SessionReceipt, TransferError> {
    let transfer = receipt.transfer.to_owned();
    let agent = receipt.agent.to_owned();
    let operation = receipt.operation.to_owned();
    store
        .transaction_with_error(move |tx| {
            let snapshot = load(tx, &transfer)?;
            persist(
                tx,
                snapshot.provider_read_receipt(&agent, &operation, &receipt.receipt)?,
                now,
            )?;
            Ok(receipt.receipt)
        })
        .await
}

/// Persist the provider's exact ticket and certainty before adopting any returned
/// thread. Unknown stays held; only a proven NotSent receipt can be retried.
pub struct ProviderReceipt<'a> {
    pub operation: &'a str,
    pub agent: &'a str,
    pub provider_operation: &'a str,
    pub receipt: TransferDispatch,
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
                snapshot.provider_receipt(&agent, &provider_operation, &receipt.receipt)?,
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
                crate::account_worker::WorkerError::Recovery(_)
                | crate::account_worker::WorkerError::Scheduler(_)
                | crate::account_worker::WorkerError::Session(_)
                | crate::account_worker::WorkerError::Notification(_)
                | crate::account_worker::WorkerError::PreparationRefused(_)
                | crate::account_worker::WorkerError::StaleVmActor
                | crate::account_worker::WorkerError::PreparationPending(_)
                | crate::account_worker::WorkerError::PreparationAwaitsAgent(_)
                | crate::account_worker::WorkerError::CleanupUnconfirmed(_)
                | crate::account_worker::WorkerError::AlreadyRunning
                | crate::account_worker::WorkerError::Closed
                | crate::account_worker::WorkerError::ShutdownTimeout
                | crate::account_worker::WorkerError::TaskFailed
                | crate::account_worker::WorkerError::Capacity
                | crate::account_worker::WorkerError::CallbackUnknown => {
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
        let cwd = store.root().to_path_buf();
        run_async(store.transaction_with_error(move |tx| {
            for value in [
                json!({"id":"lead","rootId":"lead","epoch":1,"status":"idle","isLead":true,
                    "accountKey":"source","provider":"codex","model":"gpt-6-astra","cwd":cwd}),
                json!({"id":"child","rootId":"lead","epoch":2,"status":"idle","isLead":false,
                    "accountKey":"source","provider":"codex","model":"gpt-6-astra","cwd":cwd}),
            ] {
                let agent =
                    AgentRecord::from_value(value).expect("test fixture operation succeeds");
                AgentRepository::put(tx, &agent).expect("test fixture operation succeeds");
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        }))
        .expect("test fixture operation succeeds");
        (root, store)
    }

    fn add_agents(store: &RuntimeStore, root: &str, count: usize) {
        let root = root.to_owned();
        let cwd = store.root().to_path_buf();
        run_async(store.transaction(move |tx| {
            let template = AgentRecord::from_value(json!({
                "id":"template", "rootId":root, "epoch":1, "status":"idle",
                "isLead":false, "accountKey":"source", "provider":"codex",
                "model":"gpt-6-astra", "cwd":cwd
            }))?.value().to_string();
            tx.execute(
                "WITH RECURSIVE members(n) AS (SELECT 1 WHERE ?1>0 UNION ALL SELECT n+1 FROM members WHERE n<?1)
                 INSERT INTO runtime_agents(id,record)
                 SELECT 'fixture-'||n,json_set(?2,'$.id','fixture-'||n) FROM members",
                rusqlite::params![i64::try_from(count).expect("fixture count fits SQLite"), template],
            )?;
            Ok(())
        })).expect("seed scoped fixture agents");
    }

    #[test]
    fn new_request_ignores_more_than_4096_unrelated_agents() {
        let (_root, store) = store_with_agents();
        add_agents(&store, "another-team", 4097);
        let unrelated = stored(&store, "fixture-4097");
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c42280";
        let result = request(&store, id, "lead", "target", "team", "codex", 10.0)
            .expect("unrelated database size cannot prevent a team transfer");
        assert_eq!(result["members"].as_object().expect("members").len(), 2);
        assert_eq!(stored(&store, "fixture-4097"), unrelated);
        assert_eq!(
            request(&store, id, "lead", "target", "team", "codex", 11.0).expect("exact replay"),
            result
        );
        assert!(request(&store, id, "lead", "changed", "team", "codex", 12.0).is_err());
    }

    #[test]
    fn active_request_ignores_more_than_4096_unrelated_agents() {
        let (_root, store) = store_with_agents();
        let original = "5a300f72-4ea6-4d30-8a0e-9b2c13c42281";
        request(&store, original, "lead", "target", "team", "codex", 10.0)
            .expect("initial request");
        add_agents(&store, "another-team", 4097);
        let cwd = store.root().to_path_buf();
        run_async(store.transaction(move |tx| {
            let late =
                AgentRecord::from_value(json!({"id":"late","rootId":"lead","parentId":"child",
                "epoch":3,"status":"idle","isLead":false,"accountKey":"source","provider":"codex",
                "model":"gpt-6-astra","cwd":cwd}))?;
            AgentRepository::put(tx, &late)
        }))
        .expect("late descendant");
        let alias = "5a300f72-4ea6-4d30-8a0e-9b2c13c42282";
        let result = request(&store, alias, "lead", "target", "team", "codex", 11.0)
            .expect("active transfer selects only its own team");
        assert_eq!(result["id"], original);
        assert_eq!(result["members"]["late"]["phase"], "lazy");
        assert_eq!(result["members"].as_object().expect("members").len(), 3);
        assert_eq!(
            request(&store, alias, "lead", "target", "team", "codex", 12.0).expect("alias replay"),
            result
        );
        assert!(request(&store, alias, "lead", "target", "subagents", "codex", 13.0).is_err());
    }

    #[test]
    fn subagents_request_keeps_lead_as_summary_owner_without_enrolling_it() {
        let (_root, store) = store_with_agents();
        add_agents(&store, "another-team", 4097);
        let before = stored(&store, "lead");
        let result = request(
            &store,
            "5a300f72-4ea6-4d30-8a0e-9b2c13c42283",
            "lead",
            "target",
            "subagents",
            "codex",
            10.0,
        )
        .expect("subagents request retains the lead snapshot");
        assert_eq!(result["members"].as_object().expect("members").len(), 1);
        assert_eq!(result["members"]["child"]["phase"], "lazy");
        let lead = stored(&store, "lead");
        assert_eq!(lead["accountKey"], before["accountKey"]);
        assert_eq!(lead["provider"], before["provider"]);
        assert_eq!(lead["threadId"], before["threadId"]);
        assert_eq!(lead["workerDefaults"]["accountKey"], "target");
        assert!(lead.get("accountTransferId").is_none());
        assert_eq!(lead["accountTransfer"]["id"], result["id"]);
        assert_eq!(stored(&store, "child")["accountKey"], "target");
        run_async(store.transaction(|tx| {
            let late = AgentRecord::from_value(json!({"id":"late", "rootId":"lead", "epoch":3,
                "status":"idle", "provider":"codex", "accountKey":"source"}))?;
            AgentRepository::put(tx, &late)
        }))
        .expect("late subagent");
        let adopted = request(
            &store,
            "5a300f72-4ea6-4d30-8a0e-9b2c13c42287",
            "lead",
            "target",
            "subagents",
            "codex",
            11.0,
        )
        .expect("active subagents request adopts late workers");
        assert_eq!(adopted["id"], result["id"]);
        assert_eq!(adopted["members"].as_object().expect("members").len(), 2);
        assert_eq!(adopted["members"]["late"]["phase"], "lazy");
        assert_eq!(stored(&store, "lead")["accountKey"], "source");
    }

    #[test]
    fn scoped_membership_keeps_nested_workers_and_excludes_deleted_or_other_roots() {
        let (_root, store) = store_with_agents();
        run_async(store.transaction(|tx| {
            let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
            lead.root_id = AgentId::from("legacy-root");
            AgentRepository::put(tx, &lead)?;
            let mut child = AgentRepository::get(tx, &AgentId::from("child"))?.expect("child");
            child.extra.insert("deletedAt".into(), json!(0));
            AgentRepository::put(tx, &child)?;
            for value in [
                json!({"id":"nested", "rootId":"lead", "parentId":"child", "provider":"codex"}),
                json!({"id":"other-root", "rootId":"other", "parentId":"lead", "provider":"codex", "environment":"linux"}),
                json!({"id":"deleted", "rootId":"lead", "provider":"codex", "deletedAt":1.0, "environment":"linux"}),
                json!({"id":"other-provider", "rootId":"lead", "provider":"claude"}),
            ] {
                let mut value = value;
                value["epoch"] = json!(3);
                value["status"] = json!("idle");
                value["accountKey"] = json!("source");
                AgentRepository::put(tx, &AgentRecord::from_value(value)?)?;
            }
            Ok(())
        })).expect("membership fixture");
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c42288";
        let result =
            request(&store, id, "lead", "target", "team", "codex", 10.0).expect("scoped transfer");
        assert_eq!(
            result["members"]
                .as_object()
                .expect("members")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["child", "lead", "nested", "other-provider"]
        );
        assert_eq!(result["members"]["other-provider"]["phase"], "left");
        for member in ["child", "lead", "nested"] {
            assert_eq!(stored(&store, member)["accountKey"], "target");
        }
        for member in ["other-root", "deleted", "other-provider"] {
            assert_eq!(stored(&store, member)["accountKey"], "source");
        }
    }

    #[test]
    fn vm_member_guard_applies_only_to_enrolled_members() {
        for scope in ["team", "subagents"] {
            let (_root, store) = store_with_agents();
            run_async(store.transaction(|tx| {
                let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
                lead.extra.insert("environment".into(), json!("linux"));
                AgentRepository::put(tx, &lead)
            }))
            .expect("VM lead fixture");
            let before = stored(&store, "lead");
            let result = request(
                &store,
                "5a300f72-4ea6-4d30-8a0e-9b2c13c42289",
                "lead",
                "target",
                scope,
                "codex",
                10.0,
            );
            if scope == "team" {
                assert_eq!(
                    result.expect_err("VM transfer guard").to_string(),
                    "Archive VM workers before changing the team account"
                );
                assert_eq!(stored(&store, "lead"), before);
                assert_eq!(stored(&store, "child")["accountKey"], "source");
            } else {
                assert_eq!(
                    result.expect("VM lead stays outside subagent enrollment")["members"]["child"]
                        ["phase"],
                    "lazy"
                );
                assert_eq!(stored(&store, "lead")["accountKey"], "source");
            }
        }
    }

    #[test]
    fn recovery_does_not_hide_unknown_or_missing_operation_status() {
        for status in [json!("future-status"), Value::Null] {
            let (_root, store) = store_with_agents();
            run_async(store.transaction(move |tx| {
                let record = json!({"id":"invalid", "leadId":"lead", "targetAccountKey":"target",
                    "status":status, "created":1.0, "members":{}})
                .to_string();
                tx.execute(
                    "INSERT INTO runtime_account_transfers(id,record) VALUES('invalid',?1)",
                    [record],
                )?;
                Ok(())
            }))
            .expect("invalid operation fixture");
            let before = stored(&store, "lead");
            assert!(
                run_async(recover(&store, 10.0)).is_err(),
                "invalid status must reach the validator"
            );
            assert_eq!(stored(&store, "lead"), before);
        }
    }

    #[test]
    fn unrelated_transfer_history_does_not_block_requests_or_pending_recovery() {
        let (_root, store) = store_with_agents();
        run_async(store.transaction(|tx| {
            let template = json!({"id":"history", "leadId":"lead", "targetAccountKey":"target",
                "status":"completed", "scope":"team", "created":1.0, "members":{}, "requests":{}}).to_string();
            tx.execute(
                "WITH RECURSIVE history(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM history WHERE n<4097)
                 INSERT INTO runtime_account_transfers(id,record)
                 SELECT 'history-'||n,json_set(?1,'$.id','history-'||n) FROM history",
                [template],
            )?;
            Ok(())
        })).expect("seed completed transfer history");
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c42285";
        let result = request(&store, id, "lead", "target", "team", "codex", 10.0)
            .expect("history size cannot prevent a new transfer");
        assert_eq!(
            request(&store, id, "lead", "target", "team", "codex", 11.0).expect("exact replay"),
            result
        );
        assert_eq!(
            run_async(recover(&store, 12.0)).expect("pending recovery ignores completed history"),
            0
        );
    }

    #[test]
    fn request_identity_is_global_even_between_leads() {
        let (_root, store) = store_with_agents();
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c42286";
        let original =
            request(&store, id, "lead", "target", "team", "codex", 10.0).expect("original request");
        run_async(store.transaction(|tx| {
            let other = AgentRecord::from_value(json!({"id":"other", "rootId":"other", "epoch":1,
                "status":"idle", "isLead":true, "provider":"codex", "accountKey":"source"}))?;
            AgentRepository::put(tx, &other)
        }))
        .expect("other lead");
        let before = stored(&store, "other");
        let error = request(&store, id, "other", "target", "team", "codex", 11.0)
            .expect_err("request identity cannot cross leads");
        assert_eq!(error.to_string(), "This transfer id has different content");
        assert_eq!(stored(&store, "other"), before);
        assert_eq!(
            request(&store, id, "lead", "target", "team", "codex", 12.0).expect("original replay"),
            original
        );
    }

    #[test]
    fn own_team_transfer_boundary_is_enforced_without_partial_enrollment() {
        let (_root, store) = store_with_agents();
        let max = studio_runtime_schema::agent_limits::MAX_TRANSFER_AGENT_RECORDS;
        add_agents(&store, "lead", max - 1);
        let before = stored(&store, "lead");
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c42284";
        for scope in ["team", "subagents"] {
            assert!(request(&store, id, "lead", "target", scope, "codex", 10.0).is_err());
        }
        assert_eq!(stored(&store, "lead"), before);
        assert_eq!(stored(&store, "child")["accountKey"], "source");
        assert!(
            Repository::get(&store.test_reader().expect("reader"), id)
                .expect("transfer lookup")
                .is_none()
        );
        run_async(store.transaction(|tx| {
            let mut deleted =
                AgentRepository::get(tx, &AgentId::from("fixture-1"))?.expect("fixture member");
            deleted.extra.insert("deletedAt".into(), json!(1.0));
            AgentRepository::put(tx, &deleted)
        }))
        .expect("deleted own-team records do not consume the snapshot boundary");
        let result = request(&store, id, "lead", "target", "team", "codex", 11.0)
            .expect("exact team boundary succeeds");
        assert_eq!(result["members"].as_object().expect("members").len(), max);
        run_async(store.transaction(|tx| {
            let mut restored =
                AgentRepository::get(tx, &AgentId::from("fixture-1"))?.expect("fixture member");
            restored.extra.insert("deletedAt".into(), json!(0));
            AgentRepository::put(tx, &restored)
        }))
        .expect("restore a member beyond the active transfer boundary");
        let before = Repository::get(&store.test_reader().expect("reader"), id).expect("operation");
        let lead = stored(&store, "lead");
        assert!(
            request(
                &store,
                "5a300f72-4ea6-4d30-8a0e-9b2c13c42290",
                "lead",
                "target",
                "team",
                "codex",
                12.0
            )
            .is_err()
        );
        assert_eq!(
            Repository::get(&store.test_reader().expect("reader"), id).expect("operation"),
            before
        );
        assert_eq!(stored(&store, "lead"), lead);
        assert_eq!(stored(&store, "fixture-1")["accountKey"], "source");
    }

    #[test]
    fn active_alias_refuses_a_late_vm_member_before_any_enrollment_write() {
        let (_root, store) = store_with_agents();
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c42291";
        request(&store, id, "lead", "target", "team", "codex", 10.0).expect("initial transfer");
        run_async(
            store.transaction(|tx| {
                let vm = AgentRecord::from_value(
                    json!({"id":"late-vm", "rootId":"lead", "epoch":3,
                "status":"idle", "provider":"codex", "accountKey":"source", "environment":"linux"}),
                )?;
                AgentRepository::put(tx, &vm)
            }),
        )
        .expect("late VM fixture");
        let before = Repository::get(&store.test_reader().expect("reader"), id).expect("operation");
        let vm = stored(&store, "late-vm");
        let error = request(
            &store,
            "5a300f72-4ea6-4d30-8a0e-9b2c13c42292",
            "lead",
            "target",
            "team",
            "codex",
            11.0,
        )
        .expect_err("active enrollment uses the same VM guard as a fresh request");
        assert_eq!(
            error.to_string(),
            "Archive VM workers before changing the team account"
        );
        assert_eq!(stored(&store, "late-vm"), vm);
        assert_eq!(
            Repository::get(&store.test_reader().expect("reader"), id).expect("operation"),
            before
        );
    }

    #[test]
    fn initial_and_active_enrollment_exclude_other_providers_before_the_vm_guard() {
        for active in [false, true] {
            for provider in ["codex", "claude"] {
                for vm in [false, true] {
                    let (_root, store) = store_with_agents();
                    let original = "5a300f72-4ea6-4d30-8a0e-9b2c13c42294";
                    if active {
                        request(&store, original, "lead", "target", "team", "codex", 10.0)
                            .expect("active transfer");
                    }
                    run_async(store.transaction(move |tx| {
                        let mut candidate = json!({"id":"candidate", "rootId":"lead", "epoch":3,
                            "status":"idle", "provider":provider, "accountKey":"source"});
                        if vm {
                            candidate["environment"] = json!("linux");
                        }
                        AgentRepository::put(tx, &AgentRecord::from_value(candidate)?)
                    }))
                    .expect("candidate");
                    let candidate = stored(&store, "candidate");
                    let lead = stored(&store, "lead");
                    let before = Repository::get(&store.test_reader().expect("reader"), original)
                        .expect("prior operation");
                    let alias = "5a300f72-4ea6-4d30-8a0e-9b2c13c42295";
                    let result = request(&store, alias, "lead", "target", "team", "codex", 11.0);
                    if provider == "codex" && vm {
                        assert_eq!(
                            result
                                .expect_err("same-provider VM must be refused")
                                .to_string(),
                            "Archive VM workers before changing the team account"
                        );
                        assert_eq!(stored(&store, "candidate"), candidate);
                        assert_eq!(stored(&store, "lead"), lead);
                        assert_eq!(
                            Repository::get(&store.test_reader().expect("reader"), original)
                                .expect("prior operation"),
                            before
                        );
                        assert!(
                            Repository::get(&store.test_reader().expect("reader"), alias)
                                .expect("alias operation")
                                .is_none()
                        );
                    } else {
                        let result =
                            result.expect("other-provider exclusion precedes VM restriction");
                        if provider == "claude" {
                            assert_eq!(result["members"]["candidate"]["phase"], "left");
                            assert_eq!(stored(&store, "candidate"), candidate);
                        } else {
                            assert_eq!(result["members"]["candidate"]["phase"], "lazy");
                            assert_eq!(stored(&store, "candidate")["accountKey"], "target");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn exact_replay_stops_reading_members_at_the_snapshot_byte_boundary() {
        let (_root, store) = store_with_agents();
        let id = "5a300f72-4ea6-4d30-8a0e-9b2c13c42293";
        run_async(store.transaction(move |tx| {
            let mut operation = json!({"id":id, "leadId":"lead", "targetAccountKey":"target",
                "status":"pending", "scope":"team", "created":1.0, "members":{}, "requests":{}});
            let member = json!({"phase":"waiting", "sourceAccountKey":"source", "lazy":false});
            operation["members"]["lead"] = member.clone();
            // Each valid row fits the per-record limit; their combined size
            // crosses the snapshot bound before the final oversized sentinel.
            let padding_bytes = studio_runtime_store::MAX_TRANSFER_RECORD_BYTES - 1024 * 1024;
            let template = json!({"id":"padding", "rootId":"lead", "epoch":1, "status":"idle",
                "provider":"codex", "fixturePadding":"x".repeat(padding_bytes)}).to_string();
            let count = Repository::MAX_SNAPSHOT_BYTES / padding_bytes + 1;
            for index in 0..count {
                let member_id = format!("padding-{index:02}");
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,json_set(?2,'$.id',?1))",
                    rusqlite::params![member_id, template])?;
                operation["members"][&member_id] = member.clone();
            }
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('z-sentinel',json_set(?1,'$.id','z-sentinel','$.fixturePadding',?2))",
                rusqlite::params![template, "x".repeat(studio_runtime_store::MAX_TRANSFER_RECORD_BYTES)])?;
            operation["members"]["z-sentinel"] = member;
            Repository::put(tx, id, &operation.to_string())
        })).expect("bounded snapshot fixture");
        let before = stored(&store, "lead");
        let error = request(&store, id, "lead", "target", "team", "codex", 10.0)
            .expect_err("exact replay rejects aggregate overflow before reading later members");
        assert!(
            matches!(error, TransferError::Stored { object: "transfer snapshot", ref pointer } if pointer == "/resourceLimit"),
            "snapshot bound must win before the oversized unread sentinel: {error}"
        );
        assert_eq!(stored(&store, "lead"), before);
    }

    #[test]
    fn a_running_retained_monitor_does_not_block_its_owners_account_transfer() {
        let (root, store) = store_with_agents();
        let cwd = root.path().to_owned();
        run_async(store.transaction(move |tx| {
            tx.execute("INSERT INTO runtime_monitors(id,record) VALUES('watch',?1)",
                [json!({"id":"watch","agent":"lead","epoch":1,"status":"running","operation":{"agent":"lead","epoch":1,"accountKey":"source","connectionId":"original"}}).to_string()])?;
            let key=format!("retained:source:{}",studio_runtime_store::sha256_hex(b"monitor:watch"));
            let submitted=json!({"call":{"scope":{"account":"source","connection":"original","generation":4},"operation":"monitor:watch","actor":{"agent":"lead","epoch":1},"transfer":null,"deleted_actor_session":null,"method":"command/exec","params":{"processId":"watch","cwd":cwd,"command":["true"]}},
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
            ) -> TransferFuture<'a, Result<TransferDispatch, TransferError>> {
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
            let operation=Repository::get(tx,transfer)?.expect("transfer remains");
            assert!(operation["members"]["lead"]["waiting"].as_str().expect("visible wait reason").contains("verified source monitor ownership"));
            assert_eq!(operation["members"]["lead"]["phase"],"lazy");
            tx.execute_batch("CREATE TRIGGER no_repeated_transfer_write BEFORE UPDATE ON runtime_account_transfers BEGIN SELECT RAISE(ABORT,'repeated transfer write'); END; CREATE TRIGGER no_repeated_actor_write BEFORE UPDATE ON runtime_agents BEGIN SELECT RAISE(ABORT,'repeated actor write'); END;")?;
            Ok(operation)
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
        let child = stored(&store, "child");
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
        let cwd = store.root().to_path_buf();
        run_async(store.transaction_with_error(move |tx| {
            let late = AgentRecord::from_value(json!({"id":"late","rootId":"lead","epoch":3,
                "status":"idle","isLead":false,"accountKey":"source","provider":"codex",
                "model":"gpt-6-astra","cwd":cwd}))
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
        let late = stored(&store, "late");
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
        let snapshot = || {
            let db = store.test_reader().expect("reader");
            let agents = ["lead", "child"].map(|id| stored(&store, id));
            let transfers = db
                .prepare("SELECT id, record FROM runtime_account_transfers ORDER BY id")
                .expect("prepare transfers")
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .expect("query transfers")
                .collect::<Result<Vec<_>, _>>()
                .expect("decode transfers");
            (agents, transfers)
        };
        let before = snapshot();
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
        assert_eq!(snapshot(), before, "invalid scope wrote durable state");
        run_async(store.transaction_with_error(|tx| {
            let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?
                .expect("test fixture operation succeeds");
            lead.extra
                .insert("excludedAccountKeys".into(), json!(["blocked"]));
            AgentRepository::put(tx, &lead)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }))
        .expect("test fixture operation succeeds");
        let before_blocked = snapshot();
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
        assert_eq!(
            snapshot(),
            before_blocked,
            "excluded account wrote durable state"
        );
    }
}

#[cfg(test)]
mod provider_admission_tests {
    use super::*;

    fn snapshot(intent: bool, admission: Option<Value>) -> Snapshot {
        let mut operation = json!({"id":"transfer","leadId":"agent","targetAccountKey":"target",
            "status":"pending","created":1.0,"scope":"team",
            "members":{"agent":{"phase":"reading","sourceAccountKey":"source","sourceThreadId":"thread"}}});
        if intent {
            operation["members"]["agent"]["providerIntent"] = json!({"operation":"start"});
        }
        if let Some(admission) = admission {
            operation["members"]["agent"]["providerAdmission"] = admission;
        }
        Snapshot::read(&operation.to_string(),[("agent".into(), json!({"provider":"codex","id":"agent",
            "rootId":"agent","epoch":0,"status":"idle","accountKey":"target","accountTransferId":"transfer"}).to_string())])
            .expect("transfer owner")
    }

    #[test]
    fn provider_root_is_persisted_once_and_missing_retry_identity_is_refused() {
        let first = snapshot(false, None);
        let mut decision = first.decide(Command::Restart).expect("decision");
        admit_mutation(&first, &mut decision, "agent").expect("new obligation");
        let saved = decision.operation["members"]["agent"]["providerAdmission"].clone();
        let admission: ProviderAdmission =
            serde_json::from_value(saved.clone()).expect("admission");
        studio_runtime_schema::persistence::RequestIdentity::new(
            "test",
            &[],
            &admission.root_request_id,
            &[],
        )
        .expect("v7 root");
        let retry = snapshot(true, Some(saved.clone()));
        let mut decision = retry.decide(Command::Restart).expect("retry decision");
        admit_mutation(&retry, &mut decision, "agent").expect("saved identity");
        assert_eq!(
            decision.operation["members"]["agent"]["providerAdmission"],
            saved
        );
        let missing = snapshot(true, None);
        let mut decision = missing.decide(Command::Restart).expect("held decision");
        assert!(admit_mutation(&missing, &mut decision, "agent").is_err());
        assert!(decision.operation["members"]["agent"]["providerAdmission"].is_null());
    }
}
