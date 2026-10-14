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
        if let Some(active) = actor.extra.get("accountTransfer").filter(|value| {
            !matches!(value["status"].as_str(), Some("completed" | "cancelled"))
        }) {
            if active["targetAccountKey"] != target || active.get("scope").and_then(Value::as_str).unwrap_or("team") != scope {
                return Err(TransferError::Refused("Finish or cancel the current transfer first"));
            }
            let active_id = active["id"].as_str().ok_or(TransferError::Refused("Invalid current transfer"))?;
            let snapshot = load(tx, active_id)?;
            let mut operation = snapshot.operation().clone();
            let mut agents = snapshot_agents(&snapshot)?;
            let ids = {
                let mut statement = tx.prepare("SELECT id FROM runtime_agents ORDER BY rowid LIMIT 4097")
                    .map_err(studio_runtime_store::StoreError::from)?;
                statement.query_map([], |row| row.get::<_, String>(0))
                    .map_err(studio_runtime_store::StoreError::from)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(studio_runtime_store::StoreError::from)?
            };
            if ids.len() > 4096 {
                return Err(TransferError::Refused("Transfer member scan exceeds the supported limit"));
            }
            let target_provider = operation["targetProvider"].as_str().unwrap_or(target_provider).to_owned();
            for member_id in ids {
                if operation["members"].get(&member_id).is_some()
                    || (scope == "subagents" && member_id == lead)
                {
                    continue;
                }
                let Some(raw) = Repository::agent(tx, &member_id)? else { continue };
                let mut value: Value = serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
                if studio_runtime_store::truthy(value.get("deletedAt"))
                    || (member_id != lead && value["rootId"].as_str() != Some(lead))
                {
                    continue;
                }
                let source_account = value.get("accountKey").and_then(Value::as_str).unwrap_or("default").to_owned();
                let provider = value.get("provider").and_then(Value::as_str).unwrap_or("codex").to_owned();
                let source_thread = value.get("threadId").cloned().filter(|thread| !thread.is_null());
                if member_id != lead && provider != target_provider {
                    operation["members"][&member_id] = json!({"phase":"left","sourceAccountKey":source_account,
                        "sourceThreadId":source_thread,"provider":provider,"name":value.get("name"),
                        "reason":format!("Uses {provider}; the destination account uses {target_provider}")});
                    agents.insert(member_id, value);
                    continue;
                }
                let done = source_account == target;
                let active_member = value.get("inFlight").is_some_and(|flag| studio_runtime_store::truthy(Some(flag)))
                    || matches!(value["status"].as_str(), Some("running" | "starting" | "approval"));
                let lazy = !done && !active_member;
                let pending = value.get("pendingSettings").filter(|settings| !settings.is_null());
                let pending_mismatch = pending.is_some()
                    && value.get("pendingSettingsAccountKey").and_then(Value::as_str).unwrap_or(&source_account) != source_account;
                let blocked = lazy && pending_mismatch;
                operation["members"][&member_id] = json!({
                    "phase":if done {"completed"} else if blocked {"blocked"} else if active_member {"waiting"} else {"lazy"},
                    "sourceAccountKey":source_account,"sourceThreadId":source_thread,"provider":provider,
                    "name":value.get("name"),"lazy":lazy,
                    "pendingSettings":pending.cloned().unwrap_or(Value::Null),
                    "sourcePendingSettings":pending.cloned().unwrap_or(Value::Null),
                    "sourceClaudeOptions":value.get("claudeOptions"),
                    "error":if blocked {json!("Queued settings belong to another account. Save them again before transfer")} else {Value::Null}
                });
                if !done && !blocked {
                    value["accountTransferId"] = json!(active_id);
                    if lazy {
                        let source_state = ["provider","model","effort","nativeEffort","fastMode","daybreakEnabled",
                            "cyberAccessProgram","workerDefaults","pendingSettings","pendingSettingsAccountKey",
                            "claudeOptions","executionSettingsAccountKey","status","error","nativeFailureHold"]
                            .into_iter().map(|key| (key.to_owned(), value.get(key).cloned().unwrap_or(Value::Null)))
                            .collect::<serde_json::Map<_,_>>();
                        value["lazyAccountTransfer"] = json!({"id":active_id,"sourceAccountKey":source_account,
                            "sourceThreadId":source_thread,"sourceState":source_state});
                        value["accountKey"] = json!(target);
                        if provider != target_provider {
                            value["provider"] = json!(target_provider);
                            value["model"] = json!(if target_provider == "claude" {"default"} else {crate::agents_create::DEFAULT_LEAD_MODEL});
                            value["effort"] = Value::Null;
                            value["nativeEffort"] = json!("medium");
                            value["fastMode"] = json!(false);
                            value["daybreakEnabled"] = json!(false);
                            value["cyberAccessProgram"] = json!("standard");
                            if let Some(object) = value.as_object_mut() {
                                for key in ["claudeOptions","pendingSettings","pendingSettingsAccountKey"] {
                                    drop(object.remove(key));
                                }
                            }
                        }
                        let mut defaults = value.get("workerDefaults").and_then(Value::as_object).cloned().unwrap_or_default();
                        if provider != target_provider {
                            defaults.insert("model".into(), Value::Null);
                            defaults.insert("effort".into(), json!("medium"));
                            defaults.insert("fastMode".into(), json!(false));
                            defaults.insert("daybreakEnabled".into(), json!(false));
                            defaults.insert("cyberAccessProgram".into(), json!("standard"));
                        }
                        defaults.insert("accountKey".into(), json!(target));
                        value["workerDefaults"] = Value::Object(defaults);
                        value["pendingSettingsAccountKey"] = if value.get("pendingSettings").is_some_and(|v| !v.is_null()) {json!(target)} else {Value::Null};
                        if value.get("pendingSettings").is_none_or(Value::is_null) { value["executionSettingsAccountKey"] = json!(source_account); }
                    }
                }
                agents.insert(member_id, value);
            }
            if agents.get(lead).is_some_and(|lead_agent| lead_agent.get("accountKey").and_then(Value::as_str) == Some(target)) {
                let lead_agent = agents.get_mut(lead).ok_or(TransferError::Refused("Unknown transfer lead"))?;
                let mut defaults = lead_agent.get("workerDefaults").and_then(Value::as_object).cloned().unwrap_or_default();
                defaults.insert("accountKey".into(), json!(target));
                lead_agent["workerDefaults"] = Value::Object(defaults);
            }
            operation["requests"][id] = json!({"leadId":lead,"targetAccountKey":target,"scope":scope});
            let mut decision = Snapshot::read(
                &operation.to_string(),
                agents.iter().map(|(id,value)|(id.clone(),value.to_string())),
            )?.decide(Command::Restart)?;
            decision.operation = operation;
            decision.agents = agents;
            decision.changed = true;
            return persist(tx, decision, now);
        }

        let ids = {
            let mut statement = tx.prepare("SELECT id FROM runtime_agents ORDER BY rowid LIMIT 4097")
                .map_err(studio_runtime_store::StoreError::from)?;
            statement.query_map([], |row| row.get::<_, String>(0))
                .map_err(studio_runtime_store::StoreError::from)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(studio_runtime_store::StoreError::from)?
        };
        if ids.len() > 4096 {
            return Err(TransferError::Refused("Transfer member scan exceeds the supported limit"));
        }
        let mut agents = std::collections::BTreeMap::new();
        for id in ids {
            let Some(raw) = Repository::agent(tx, &id)? else { continue };
            let value: Value = serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
            if studio_runtime_store::truthy(value.get("deletedAt"))
                || (id != lead && value["rootId"].as_str() != Some(lead))
                || (scope == "subagents" && id == lead)
            {
                continue;
            }
            agents.insert(id, value);
        }
        if !agents.contains_key(lead) {
            return Err(TransferError::Refused("Unknown managed agent"));
        }
        if agents.values().any(|agent| {
            agent["environment"] == "linux"
                && agent.get("accountKey").and_then(Value::as_str).unwrap_or("default") != target
        }) {
            return Err(TransferError::Refused("Archive Linux VM workers before changing the team account"));
        }
        let operation_id = id;
        let mut requests = serde_json::Map::new();
        requests.insert(operation_id.to_owned(), json!({"leadId":lead,"targetAccountKey":target,"scope":scope}));
        let mut operation = json!({"id":operation_id,"leadId":lead,"targetAccountKey":target,
            "targetProvider":target_provider,"status":"pending","scope":scope,"created":now,
            "members":{},"requests":Value::Object(requests)});
        for (agent_id, value) in &mut agents {
            let source_account = value.get("accountKey").and_then(Value::as_str).unwrap_or("default").to_owned();
            let provider = value.get("provider").and_then(Value::as_str).unwrap_or("codex").to_owned();
            let source_thread = value.get("threadId").cloned().filter(|thread| !thread.is_null());
            if agent_id != lead && provider != target_provider {
                operation["members"][agent_id] = json!({"phase":"left","sourceAccountKey":source_account,
                    "sourceThreadId":source_thread,"provider":provider,"name":value.get("name"),
                    "reason":format!("Uses {provider}; the destination account uses {target_provider}")});
                continue;
            }
            let done = source_account == target;
            let active = value.get("inFlight").is_some_and(|flag| studio_runtime_store::truthy(Some(flag)))
                || matches!(value["status"].as_str(), Some("running" | "starting" | "approval"));
            let lazy = !done && !active;
            let pending = value.get("pendingSettings").filter(|settings| !settings.is_null());
            let pending_mismatch = pending.is_some()
                && value.get("pendingSettingsAccountKey").and_then(Value::as_str)
                    .unwrap_or(&source_account) != source_account;
            let blocked = lazy && pending_mismatch;
            operation["members"][agent_id] = json!({
                "phase": if done {"completed"} else if blocked {"blocked"} else if active {"waiting"} else {"lazy"},
                "sourceAccountKey":source_account,"sourceThreadId":source_thread,"provider":provider,
                "name":value.get("name"),"lazy":lazy,
                "pendingSettings":pending.cloned().unwrap_or(Value::Null),
                "sourcePendingSettings":pending.cloned().unwrap_or(Value::Null),
                "sourceClaudeOptions":value.get("claudeOptions"),
                "error":if blocked {json!("Queued settings belong to another account. Save them again before transfer")} else {Value::Null}
            });
            if !done && !blocked {
                value["accountTransferId"] = json!(operation_id);
                if lazy {
                    let source_state = ["provider","model","effort","nativeEffort","fastMode","daybreakEnabled",
                        "cyberAccessProgram","workerDefaults","pendingSettings","pendingSettingsAccountKey",
                        "claudeOptions","executionSettingsAccountKey","status","error","nativeFailureHold"]
                        .into_iter().map(|key| (key.to_owned(), value.get(key).cloned().unwrap_or(Value::Null)))
                        .collect::<serde_json::Map<_,_>>();
                    value["lazyAccountTransfer"] = json!({"id":operation_id,"sourceAccountKey":source_account,
                        "sourceThreadId":source_thread,"sourceState":source_state});
                    value["accountKey"] = json!(target);
                    if provider != target_provider {
                        value["provider"] = json!(target_provider);
                        value["model"] = json!(if target_provider == "claude" {"default"} else {crate::agents_create::DEFAULT_LEAD_MODEL});
                        value["effort"] = Value::Null;
                        value["nativeEffort"] = json!("medium");
                        value["fastMode"] = json!(false);
                        value["daybreakEnabled"] = json!(false);
                        value["cyberAccessProgram"] = json!("standard");
                        value.as_object_mut().expect("agent record object").remove("claudeOptions");
                        value.as_object_mut().expect("agent record object").remove("pendingSettings");
                        value.as_object_mut().expect("agent record object").remove("pendingSettingsAccountKey");
                    }
                    let mut defaults = value.get("workerDefaults").and_then(Value::as_object).cloned().unwrap_or_default();
                    if provider != target_provider {
                        defaults.insert("model".into(), Value::Null);
                        defaults.insert("effort".into(), json!("medium"));
                        defaults.insert("fastMode".into(), json!(false));
                        defaults.insert("daybreakEnabled".into(), json!(false));
                        defaults.insert("cyberAccessProgram".into(), json!("standard"));
                    }
                    defaults.insert("accountKey".into(), json!(target));
                    value["workerDefaults"] = Value::Object(defaults);
                    value["pendingSettingsAccountKey"] = if value.get("pendingSettings").is_some_and(|v| !v.is_null()) {json!(target)} else {Value::Null};
                    if value.get("pendingSettings").is_none_or(Value::is_null) {
                        value["executionSettingsAccountKey"] = json!(source_account);
                    }
                }
            }
        }
        if operation["members"].as_object().is_some_and(|members| members.values().all(|member| matches!(member["phase"].as_str(),Some("completed"|"left")))) {
            operation["status"] = json!("completed");
        }
        let snapshot = Snapshot::read(&operation.to_string(), agents.iter().map(|(id,value)|(id.clone(),value.to_string())))?;
        let mut decision = snapshot.decide(Command::Restart)?;
        decision.operation = operation;
        decision.agents = agents;
        decision.changed = true;
        persist(tx, decision, now)
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
        ProviderReceipt {
            operation: &ticket.operation,
            agent: &ticket.agent,
            provider_operation: &ticket.request,
            receipt: dispatch.receipt,
            result: dispatch.result,
            certainty: dispatch.certainty,
        },
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
pub struct ProviderIntent<'a> {
    pub operation: &'a str,
    pub agent: &'a str,
    pub provider_operation: &'a str,
    pub scope: Value,
    pub method: &'a str,
    pub params: Value,
}

pub fn provider_intent(
    store: &RuntimeStore,
    intent: ProviderIntent<'_>,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, intent.operation)?;
        persist(
            tx,
            snapshot.provider_intent(
                intent.agent,
                intent.provider_operation,
                intent.scope,
                intent.method,
                intent.params,
            )?,
            now,
        )?;
        Ok(())
    })
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

pub(crate) fn provider_read_intent(
    store: &RuntimeStore,
    intent: ProviderReadIntent<'_>,
    now: f64,
) -> Result<Option<Value>, TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, intent.transfer)?;
        let prior = snapshot.operation()["members"][intent.agent]["providerReadReceipts"]
            [intent.operation]
            .as_object()
            .filter(|receipt| receipt.get("certainty") == Some(&Value::String("written".into())))
            .cloned()
            .map(Value::Object);
        let decision = snapshot.provider_read_intent(
            intent.agent,
            intent.operation,
            intent.account,
            intent.scope,
            intent.method,
            intent.params,
        )?;
        persist(tx, decision, now)?;
        Ok(prior)
    })
}

pub(crate) struct ProviderReadReceipt<'a> {
    pub transfer: &'a str,
    pub agent: &'a str,
    pub operation: &'a str,
    pub receipt: Value,
    pub result: Option<Value>,
    pub certainty: &'a str,
}

pub(crate) fn provider_read_receipt(
    store: &RuntimeStore,
    receipt: ProviderReadReceipt<'_>,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, receipt.transfer)?;
        persist(
            tx,
            snapshot.provider_read_receipt(
                receipt.agent,
                receipt.operation,
                receipt.receipt,
                receipt.result,
                receipt.certainty,
            )?,
            now,
        )?;
        Ok(())
    })
}

/// Persist the provider's exact ticket and certainty before adopting any returned
/// thread. Unknown stays held; only a proven NotSent receipt can be retried.
pub(crate) struct ProviderReceipt<'a> {
    pub operation: &'a str,
    pub agent: &'a str,
    pub provider_operation: &'a str,
    pub receipt: Value,
    pub result: Option<Value>,
    pub certainty: &'a str,
}

pub(crate) fn provider_receipt(
    store: &RuntimeStore,
    receipt: ProviderReceipt<'_>,
    now: f64,
) -> Result<(), TransferError> {
    store.transaction_with_error(|tx| {
        let snapshot = load(tx, receipt.operation)?;
        persist(
            tx,
            snapshot.provider_receipt(
                receipt.agent,
                receipt.provider_operation,
                receipt.receipt,
                receipt.result,
                receipt.certainty,
            )?,
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

#[cfg(test)]
mod request_tests {
    use super::*;
    use serde_json::json;
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository};

    fn store_with_agents() -> (tempfile::TempDir, RuntimeStore) {
        let root = tempfile::tempdir().expect("test fixture operation succeeds");
        let store = RuntimeStore::open(Some(root.path())).expect("test fixture operation succeeds");
        store.transaction_with_error(|tx| {
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
        }).expect("test fixture operation succeeds");
        (root, store)
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
            &store.read().expect("test fixture operation succeeds"),
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
        store
            .transaction_with_error(|tx| {
                let late = AgentRecord::from_value(json!({"id":"late","rootId":"lead","epoch":3,
                "status":"idle","isLead":false,"accountKey":"source","provider":"codex",
                "model":"gpt-6-astra","cwd":"/tmp/work"}))
                .expect("test fixture operation succeeds");
                AgentRepository::put(tx, &late)
            })
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
            &store.read().expect("test fixture operation succeeds"),
            &AgentId::from("late"),
        )
        .expect("test fixture operation succeeds")
        .expect("test fixture operation succeeds")
        .value();
        assert_eq!(late["accountKey"], "target");
        assert_eq!(late["lazyAccountTransfer"]["sourceAccountKey"], "source");
        store.transaction_with_error(|tx| {
            let stored = load(tx, "5a300f72-4ea6-4d30-8a0e-9b2c13c42271").expect("test fixture operation succeeds");
            assert_eq!(stored.operation()["requests"]["5a300f72-4ea6-4d30-8a0e-9b2c13c42272"]["leadId"], "lead");
            Ok::<_, studio_runtime_store::StoreError>(())
        }).expect("test fixture operation succeeds");
    }

    #[test]
    fn invalid_scope_and_excluded_account_refuse_without_writes() {
        let (_root, store) = store_with_agents();
        let before = AgentRepository::get(
            &store.read().expect("test fixture operation succeeds"),
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
        store
            .transaction_with_error(|tx| {
                let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?
                    .expect("test fixture operation succeeds");
                lead.extra
                    .insert("excludedAccountKeys".into(), json!(["blocked"]));
                AgentRepository::put(tx, &lead)?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .expect("test fixture operation succeeds");
        let before_blocked = AgentRepository::get(
            &store.read().expect("test fixture operation succeeds"),
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
            &store.read().expect("test fixture operation succeeds"),
            &AgentId::from("lead"),
        )
        .expect("test fixture operation succeeds")
        .expect("test fixture operation succeeds")
        .value();
        assert_eq!(after, before_blocked);
        assert_eq!(before["accountKey"], "source");
    }
}
