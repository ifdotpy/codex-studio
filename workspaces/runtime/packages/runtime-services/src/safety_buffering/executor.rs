use super::{
    SafetyBufferingError as Error, SafetyDecision, SafetyRetryStage as Stage, SafetySnapshot,
    decide_action, public_receipt,
};
use crate::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ProviderRequest, ProviderResponse,
        RequestCertainty, SessionReceipt,
    },
    account_worker::{
        preparation::{PreparationResources, SafetyRetryPreparation, prepare_safety_retry},
        retained::RequestActor,
    },
    agent_lifecycle_runtime::{LifecycleEffect, LifecycleEffectKind, LifecyclePorts},
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use studio_runtime_providers::{Method, RequestId};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, RuntimeStore, SchedulerRepository, StoreError,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const NOT_SENT_RETRIES: usize = 1;

/// Execute the complete retry action against the agent's retained Codex session.
/// Every request is journaled in runtime_safety_retries before hand-off.
pub fn execute_action(
    store: &RuntimeStore,
    provider: &dyn AccountSessionPort,
    resources: &Arc<PreparationResources>,
    lifecycle: Option<&dyn LifecyclePorts>,
    agent_id: &str,
    actor: RequestActor,
    turn_id: &str,
    choice: &str,
) -> Result<Value, Error> {
    if actor.agent != agent_id || turn_id.is_empty() {
        return Err(Error::Inactive);
    }
    if !matches!(choice, "wait" | "retry" | "cancel") {
        return Err(Error::InvalidChoice);
    }
    let account = store.transaction_with(|tx| {
        let agent = read_agent(tx, agent_id)?;
        if agent.get("epoch").and_then(Value::as_i64) != Some(actor.epoch) {
            return Err(Error::StaleRetry);
        }
        Ok(agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_owned())
    })?;
    let scope = provider.current_scope(&account);
    if scope.as_ref().is_some_and(|scope| scope.account != account) {
        return Err(Error::StaleRetry);
    }
    let identity = format!("{agent_id}:{turn_id}");
    let now = unix_seconds();
    let fresh_attempt = uuid::Uuid::new_v4().to_string();
    let decision = store.transaction_with(|tx| {
        let mut agent = read_agent(tx, agent_id)?;
        if agent.get("epoch").and_then(Value::as_i64) != Some(actor.epoch)
            || agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != account
        {
            return Err(Error::StaleRetry);
        }
        let existing = read_retry(tx, &identity)?;
        if let Some(mut operation) = existing {
            let stage = Stage::parse(operation.get("stage").ok_or(Error::InvalidStoredState)?)?;
            if operation.get("accountKey").and_then(Value::as_str) != Some(&account)
                || operation.get("epoch").and_then(Value::as_i64) != Some(actor.epoch)
                || operation.get("agent").and_then(Value::as_str) != Some(agent_id)
                || operation.get("turnId").and_then(Value::as_str) != Some(turn_id)
                || operation.get("id").and_then(Value::as_str) != Some(&identity)
            {
                return Err(Error::StaleRetry);
            }
            if choice == "cancel" && super::ACTIVE_STAGES.contains(&stage) {
                if operation.get("rpcMethod").and_then(Value::as_str) == Some("turn/start") {
                    return Err(Error::StartAlreadySubmitted);
                }
                operation["stage"] = json!(Stage::Cancelled.as_str());
                operation["dispatch"] = json!("cancelled");
                operation["error"] = Value::Null;
                if let Some(buffering) = agent
                    .get_mut("nativeSafetyBuffering")
                    .and_then(Value::as_object_mut)
                {
                    buffering.insert("dismissed".into(), Value::Bool(true));
                }
                if agent.get("inFlight").and_then(Value::as_bool) != Some(true) {
                    agent["status"] = json!("interrupted");
                }
                save_operation(tx, &mut agent, &operation)?;
                return Ok(ActionPlan::Return(public_receipt(&operation)?));
            }
            let public = public_receipt(&operation)?;
            if choice == "retry"
                && !matches!(
                    stage,
                    Stage::Unknown | Stage::Running | Stage::Failed | Stage::Cancelled
                )
                && operation.get("dispatch").and_then(Value::as_str) != Some("unknown")
            {
                return Ok(ActionPlan::Drive(operation));
            }
            return Ok(ActionPlan::Return(public));
        }
        if choice == "cancel" {
            return Err(Error::NoRetryToCancel);
        }
        let scope = scope
            .as_ref()
            .ok_or(Error::Requires("provider-session/native-request"))?;
        let buffering = agent
            .get("nativeSafetyBuffering")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let active_scope = buffering.get("connectionId").and_then(Value::as_str)
            == Some(scope.connection.as_str());
        let thread_open = !crate::continuation::native_thread_blocked(&agent);
        let snapshot = SafetySnapshot::from_agent(&agent, active_scope, thread_open)?;
        match decide_action(
            &snapshot,
            choice,
            turn_id,
            None,
            &identity,
            &fresh_attempt,
            now,
        )? {
            SafetyDecision::Replay { receipt } => Ok(ActionPlan::Return(receipt)),
            SafetyDecision::Cancelled { .. } => Err(Error::InvalidStoredState),
            SafetyDecision::Waiting { .. } => {
                if let Some(buffering) = agent
                    .get_mut("nativeSafetyBuffering")
                    .and_then(Value::as_object_mut)
                {
                    buffering.insert("dismissed".into(), Value::Bool(true));
                }
                put_agent(tx, &agent)?;
                Ok(ActionPlan::Return(json!({"stage":"waiting"})))
            }
            SafetyDecision::BeginRetry { receipt } => {
                let mut operation = receipt;
                operation["scope"] =
                    serde_json::to_value(scope).map_err(|_| Error::InvalidStoredState)?;
                operation["responses"] = json!({});
                operation["receipts"] = json!({});
                operation["dispatch"] = json!("ready");
                save_operation(tx, &mut agent, &operation)?;
                Ok(ActionPlan::Drive(operation))
            }
        }
    })?;
    match decision {
        ActionPlan::Return(receipt) => Ok(receipt),
        ActionPlan::Drive(operation) => drive(store, provider, resources, lifecycle, operation),
    }
}

enum ActionPlan {
    Return(Value),
    Drive(Value),
}

fn drive(
    store: &RuntimeStore,
    provider: &dyn AccountSessionPort,
    resources: &Arc<PreparationResources>,
    lifecycle: Option<&dyn LifecyclePorts>,
    mut operation: Value,
) -> Result<Value, Error> {
    loop {
        let stage = Stage::parse(operation.get("stage").ok_or(Error::InvalidStoredState)?)?;
        if matches!(
            stage,
            Stage::Running | Stage::Failed | Stage::Cancelled | Stage::Unknown
        ) {
            return public_receipt(&operation);
        }
        let account = required_str(&operation, "accountKey")?.to_owned();
        let scope = provider
            .current_scope(&account)
            .ok_or(Error::Requires("provider-session/native-request"))?;
        let saved_scope: crate::account_session_runtime::ConnectionScope = serde_json::from_value(
            operation
                .get("scope")
                .cloned()
                .ok_or(Error::InvalidStoredState)?,
        )
        .map_err(|_| Error::InvalidStoredState)?;
        if scope != saved_scope {
            return Err(Error::StaleRetry);
        }
        let operation_id = format!("{}:{}", required_str(&operation, "id")?, stage.as_str());
        if let Some(response) =
            recorded_written_response(&operation, stage, &operation_id, &saved_scope)?
        {
            match advance_stage(store, &mut operation, stage, response, lifecycle) {
                Ok(true) => continue,
                Ok(false) => return public_receipt(&operation),
                Err(error @ (Error::ProviderRejected | Error::Inactive)) => {
                    operation["stage"] = json!(Stage::Failed.as_str());
                    operation["dispatch"] = json!("rejected");
                    operation["error"] = json!("Safety retry stage was refused");
                    persist_operation(store, &mut operation)?;
                    return Err(error);
                }
                Err(Error::ProviderUnknown) => {
                    operation["stage"] = json!(Stage::Unknown.as_str());
                    operation["dispatch"] = json!("unknown");
                    operation["error"] =
                        json!("Provider response did not identify the resulting thread or turn");
                    persist_operation(store, &mut operation)?;
                    return public_receipt(&operation);
                }
                Err(error) => return Err(error),
            }
        }
        let (method, params) =
            request_for_stage(store, resources, &operation, stage, scope.generation)?;
        if !persist_intent(store, &mut operation, stage, &operation_id, method, &params)? {
            return public_receipt(&operation);
        }
        let request = ProviderRequest {
            account: account.clone(),
            operation: operation_id.clone(),
            actor: Some(RequestActor {
                agent: required_str(&operation, "agent")?.to_owned(),
                epoch: operation
                    .get("epoch")
                    .and_then(Value::as_i64)
                    .ok_or(Error::InvalidStoredState)?,
            }),
            deleted_actor_session: None,
            expected_scope: Some(scope),
            method,
            params: params.clone(),
            deadline: Instant::now() + REQUEST_TIMEOUT,
        };
        let mut sent_retry = 0;
        let receipt = loop {
            match provider.provider_request(request.clone()) {
                Ok(receipt) => {
                    validate_receipt(&receipt, &operation_id, &saved_scope)?;
                    if !persist_stage_receipt(store, &mut operation, stage, &receipt)? {
                        return public_receipt(&operation);
                    }
                    if receipt.certainty == RequestCertainty::NotSent
                        && sent_retry < NOT_SENT_RETRIES
                    {
                        sent_retry += 1;
                        operation["dispatch"] = json!("intent");
                        persist_operation(store, &mut operation)?;
                        if Stage::parse(operation.get("stage").ok_or(Error::InvalidStoredState)?)?
                            != stage
                        {
                            return public_receipt(&operation);
                        }
                        continue;
                    }
                    break receipt;
                }
                Err(AccountSessionError::Requires(name)) => return Err(Error::Requires(name)),
                Err(AccountSessionError::Unknown) => {
                    if !persist_unknown_receipt(
                        store,
                        &mut operation,
                        stage,
                        &operation_id,
                        "unknown",
                    )? {
                        return public_receipt(&operation);
                    }
                    operation["stage"] = json!(Stage::Unknown.as_str());
                    operation["dispatch"] = json!("unknown");
                    operation["error"] = json!("Provider request outcome is unknown");
                    persist_operation(store, &mut operation)?;
                    return public_receipt(&operation);
                }
                Err(AccountSessionError::Refused(_)) => {
                    if !persist_unknown_receipt(
                        store,
                        &mut operation,
                        stage,
                        &operation_id,
                        "refused",
                    )? {
                        return public_receipt(&operation);
                    }
                    if stage == Stage::Start
                        && !settle_start_rejected(
                            store,
                            &mut operation,
                            None,
                            "Provider refused turn/start",
                        )?
                    {
                        return public_receipt(&operation);
                    }
                    operation["stage"] = json!(Stage::Failed.as_str());
                    operation["dispatch"] = json!("rejected");
                    operation["error"] = json!("Provider request refused");
                    persist_operation(store, &mut operation)?;
                    return Err(Error::ProviderRejected);
                }
                Err(_) => {
                    if !persist_unknown_receipt(
                        store,
                        &mut operation,
                        stage,
                        &operation_id,
                        "unknown",
                    )? {
                        return public_receipt(&operation);
                    }
                    operation["stage"] = json!(Stage::Unknown.as_str());
                    operation["dispatch"] = json!("unknown");
                    operation["error"] = json!("Provider request outcome is unknown");
                    persist_operation(store, &mut operation)?;
                    return public_receipt(&operation);
                }
            }
        };
        if receipt.certainty == RequestCertainty::NotSent {
            operation["dispatch"] = json!("ready");
            persist_operation(store, &mut operation)?;
            return public_receipt(&operation);
        }
        if receipt.certainty == RequestCertainty::Unknown {
            operation["stage"] = json!(Stage::Unknown.as_str());
            operation["dispatch"] = json!("unknown");
            operation["error"] = json!("Provider request outcome is unknown");
            persist_operation(store, &mut operation)?;
            return public_receipt(&operation);
        }
        let body = match receipt.result {
            Some(ProviderResponse::Reply(value)) => value,
            Some(ProviderResponse::Rejected(_)) => {
                if stage == Stage::Start
                    && !settle_start_rejected(
                        store,
                        &mut operation,
                        receipt.ticket.as_ref(),
                        "Provider rejected turn/start",
                    )?
                {
                    return public_receipt(&operation);
                }
                operation["stage"] = json!(Stage::Failed.as_str());
                operation["dispatch"] = json!("rejected");
                operation["error"] = json!("Provider rejected the safety retry request");
                persist_operation(store, &mut operation)?;
                return Err(Error::ProviderRejected);
            }
            None => {
                operation["stage"] = json!(Stage::Unknown.as_str());
                operation["dispatch"] = json!("unknown");
                operation["error"] = json!("Provider receipt has no result");
                persist_operation(store, &mut operation)?;
                return public_receipt(&operation);
            }
        };
        operation
            .get_mut("responses")
            .and_then(Value::as_object_mut)
            .ok_or(Error::InvalidStoredState)?
            .insert(stage.as_str().into(), body.clone());
        operation["dispatch"] = json!("received");
        match advance_stage(store, &mut operation, stage, body, lifecycle) {
            Ok(true) => (),
            Ok(false) => return public_receipt(&operation),
            Err(error @ (Error::ProviderRejected | Error::Inactive)) => {
                operation["stage"] = json!(Stage::Failed.as_str());
                operation["dispatch"] = json!("rejected");
                operation["error"] = json!("Safety retry stage was refused");
                persist_operation(store, &mut operation)?;
                return Err(error);
            }
            Err(Error::ProviderUnknown) => {
                operation["stage"] = json!(Stage::Unknown.as_str());
                operation["dispatch"] = json!("unknown");
                operation["error"] =
                    json!("Provider response did not identify the resulting thread or turn");
                persist_operation(store, &mut operation)?;
                return public_receipt(&operation);
            }
            Err(error) => return Err(error),
        }
        if Stage::parse(operation.get("stage").ok_or(Error::InvalidStoredState)?)? == Stage::Running
        {
            return public_receipt(&operation);
        }
    }
}

fn request_for_stage(
    store: &RuntimeStore,
    resources: &Arc<PreparationResources>,
    operation: &Value,
    stage: Stage,
    generation: u64,
) -> Result<(Method, Value), Error> {
    let thread = required_str(operation, "threadId")?;
    let method = match stage {
        Stage::Turns | Stage::VerifyTurns => Method::ThreadTurnsList,
        Stage::Items | Stage::VerifyItems => Method::ThreadItemsList,
        Stage::Interrupt => Method::TurnInterrupt,
        Stage::Fork => Method::ThreadFork,
        Stage::Start => Method::TurnStart,
        _ => return Err(Error::InvalidStoredState),
    };
    let params = match stage {
        Stage::Turns | Stage::VerifyTurns => {
            json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"})
        }
        Stage::Items | Stage::VerifyItems => {
            json!({"threadId":thread,"turnId":required_str(operation,"turnId")?,"limit":1000,"sortDirection":"asc"})
        }
        Stage::Interrupt => json!({"threadId":thread,"turnId":required_str(operation,"turnId")?}),
        Stage::Fork | Stage::Start => {
            let mut prepared = prepare_safety_retry(
                store,
                resources,
                &SafetyRetryPreparation {
                    agent_id: required_str(operation, "agent")?.to_owned(),
                    attempt_id: required_str(operation, "attemptId")?.to_owned(),
                    generation,
                    source_thread_id: if stage == Stage::Fork {
                        required_str(operation, "sourceThreadId")?.to_owned()
                    } else {
                        required_str(operation, "newThreadId")?.to_owned()
                    },
                    turn_id: required_str(operation, "turnId")?.to_owned(),
                    model: required_str(operation, "model")?.to_owned(),
                    input: operation
                        .get("input")
                        .cloned()
                        .ok_or(Error::InvalidStoredState)?,
                },
            )
            .map_err(|error| Error::Preparation(error.to_string()))?;
            if stage == Stage::Fork {
                prepared.fork_params
            } else {
                prepared
                    .with_thread_id(required_str(operation, "newThreadId")?)
                    .map_err(|error| Error::Preparation(error.to_string()))?;
                prepared.turn_params
            }
        }
        _ => return Err(Error::InvalidStoredState),
    };
    Ok((method, params))
}

fn persist_intent(
    store: &RuntimeStore,
    operation: &mut Value,
    stage: Stage,
    operation_id: &str,
    method: Method,
    params: &Value,
) -> Result<bool, Error> {
    operation["stage"] = json!(stage.as_str());
    operation["dispatch"] = json!("intent");
    operation["rpcMethod"] = json!(method.as_str());
    operation["operationId"] = json!(operation_id);
    operation["requestParams"] = params.clone();
    operation["updated"] = json!(unix_seconds());
    store.transaction_with(|tx| {
        let current =
            read_retry(tx, required_str(operation, "id")?)?.ok_or(Error::InvalidStoredState)?;
        if current.get("stage").and_then(Value::as_str) != Some(stage.as_str()) {
            *operation = current;
            return Ok(false);
        }
        let mut agent = read_agent(tx, required_str(operation, "agent")?)?;
        validate_current(&agent, operation, stage)?;
        if stage == Stage::Start {
            let attempt = json!({
                "id":required_str(operation,"attemptId")?,
                "epoch":operation.get("epoch").cloned().ok_or(Error::InvalidStoredState)?,
                "events":[],"action":"safety","submitted":true,
                "accountKey":required_str(operation,"accountKey")?,
                "connectionId":required_str(operation,"connectionId")?,
                "threadId":required_str(operation,"newThreadId")?
            });
            let program = if agent.get("daybreakEnabled").and_then(Value::as_bool) == Some(true) {
                agent
                    .get("cyberAccessProgram")
                    .cloned()
                    .unwrap_or_else(|| json!("standard"))
            } else {
                json!("standard")
            };
            agent["cyberAccessProgram"] = program;
            agent["status"] = json!("starting");
            agent["inFlight"] = json!(true);
            agent["turnEpoch"] = operation
                .get("epoch")
                .cloned()
                .ok_or(Error::InvalidStoredState)?;
            agent["error"] = Value::Null;
            agent["startAttempt"] = attempt;
        }
        save_operation(tx, &mut agent, operation)?;
        Ok(true)
    })
}

fn validate_current(agent: &Value, operation: &Value, stage: Stage) -> Result<(), Error> {
    let expected_thread = if matches!(stage, Stage::Start) {
        operation.get("newThreadId").and_then(Value::as_str)
    } else {
        operation.get("threadId").and_then(Value::as_str)
    };
    let projection = agent.get("nativeSafetyRetry").and_then(Value::as_object);
    if agent.get("deletedAt").is_some_and(|value| !value.is_null())
        || agent.get("epoch") != operation.get("epoch")
        || agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != operation
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
        || agent.get("threadId").and_then(Value::as_str) != expected_thread
        || agent.get("autoWake").and_then(Value::as_bool) != Some(true)
        || agent
            .get("accountTransferId")
            .is_some_and(|value| !value.is_null())
        || projection.and_then(|record| record.get("id")) != operation.get("id")
    {
        return Err(Error::StaleRetry);
    }
    if crate::continuation::native_thread_blocked(agent) {
        return Err(Error::ThreadNotOpen);
    }
    if stage == Stage::Interrupt {
        let buffering = agent
            .get("nativeSafetyBuffering")
            .filter(|value| !value.is_null())
            .ok_or(Error::Inactive)?;
        if buffering.get("showBufferingUi").and_then(Value::as_bool) != Some(true)
            || buffering.get("responseStarted").and_then(Value::as_bool) == Some(true)
            || agent.get("nativeResponseTurn").and_then(Value::as_str)
                == operation.get("turnId").and_then(Value::as_str)
            || agent.get("turnId") != operation.get("turnId")
        {
            return Err(Error::Inactive);
        }
    }
    Ok(())
}

fn advance_stage(
    store: &RuntimeStore,
    operation: &mut Value,
    stage: Stage,
    response: Value,
    lifecycle: Option<&dyn LifecyclePorts>,
) -> Result<bool, Error> {
    let mut committed_interrupt = false;
    store
        .transaction_with(|tx| {
            let id = required_str(operation, "id")?;
            let current = read_retry(tx, id)?.ok_or(Error::InvalidStoredState)?;
            if current.get("stage").and_then(Value::as_str) != Some(stage.as_str()) {
                *operation = current;
                return Ok(false);
            }
            let mut agent = read_agent(tx, required_str(operation, "agent")?)?;
            if stage != Stage::Start {
                validate_current(&agent, operation, stage)?;
            }
            match stage {
                Stage::Turns | Stage::VerifyTurns => {
                    let turns = response
                        .get("data")
                        .and_then(Value::as_array)
                        .ok_or(Error::ProviderRejected)?;
                    let latest = turns.first().ok_or(Error::Inactive)?;
                    if latest.get("id").and_then(Value::as_str)
                        != operation.get("turnId").and_then(Value::as_str)
                    {
                        return Err(Error::Inactive);
                    }
                    if stage == Stage::VerifyTurns
                        && latest.get("status").and_then(Value::as_str) != Some("interrupted")
                    {
                        return Err(Error::ProviderRejected);
                    }
                    operation["stage"] = json!(if stage == Stage::Turns {
                        "items"
                    } else {
                        "verify_items"
                    });
                }
                Stage::Items | Stage::VerifyItems => {
                    let entries = response
                        .get("data")
                        .and_then(Value::as_array)
                        .ok_or(Error::ProviderRejected)?;
                    if response
                        .get("nextCursor")
                        .is_some_and(|cursor| !cursor.is_null() && cursor != "")
                    {
                        return Err(Error::ProviderRejected);
                    }
                    let mut input = Vec::new();
                    for entry in entries {
                        if entry.get("turnId").and_then(Value::as_str)
                            != operation.get("turnId").and_then(Value::as_str)
                        {
                            return Err(Error::ProviderRejected);
                        }
                        let item = entry.get("item").ok_or(Error::ProviderRejected)?;
                        match item.get("type").and_then(Value::as_str) {
                            Some("userMessage") => {
                                if let Some(content) = item.get("content").and_then(Value::as_array)
                                {
                                    input.extend(content.iter().cloned());
                                }
                            }
                            Some("reasoning") => {}
                            _ => return Err(Error::ProviderRejected),
                        }
                    }
                    if input.is_empty() {
                        return Err(Error::ProviderRejected);
                    }
                    operation["input"] = Value::Array(input);
                    operation["stage"] = json!(if stage == Stage::Items {
                        "interrupt"
                    } else {
                        "fork"
                    });
                }
                Stage::Interrupt => operation["stage"] = json!("verify_turns"),
                Stage::Fork => {
                    let thread = response
                        .pointer("/thread/id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or(Error::ProviderUnknown)?;
                    if operation.get("threadId").and_then(Value::as_str) == Some(thread) {
                        return Err(Error::ProviderUnknown);
                    }
                    operation["newThreadId"] = json!(thread);
                    operation["stage"] = json!("start");
                    agent["threadId"] = json!(thread);
                    agent["model"] = operation
                        .get("model")
                        .cloned()
                        .ok_or(Error::InvalidStoredState)?;
                    agent["effort"] = json!("low");
                    agent["nativeEffort"] = json!("low");
                    agent["turnId"] = Value::Null;
                    agent["inFlight"] = json!(false);
                    agent["status"] = json!("starting");
                    agent["nativeFailureHold"] = json!(false);
                    agent["activeTools"] = json!([]);
                    agent["error"] = Value::Null;
                    agent
                        .as_object_mut()
                        .ok_or(Error::InvalidStoredState)?
                        .remove("nativeSafetyBuffering");
                    agent
                        .as_object_mut()
                        .ok_or(Error::InvalidStoredState)?
                        .remove("startAttempt");
                }
                Stage::Start => {
                    let accepted = response
                        .pointer("/turn/id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or(Error::ProviderUnknown)?;
                    let request_id = operation
                        .get("requestId")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<RequestId>(value).ok())
                        .ok_or(Error::InvalidStoredState)?;
                    let request_id = request_id_string(&request_id);
                    let generation = operation
                        .get("requestGeneration")
                        .and_then(Value::as_u64)
                        .ok_or(Error::InvalidStoredState)?;
                    let settled = SchedulerRepository::settle_safety_started_provider(
                        tx,
                        required_str(operation, "agent")?,
                        operation
                            .get("epoch")
                            .and_then(Value::as_i64)
                            .ok_or(Error::InvalidStoredState)?,
                        required_str(operation, "attemptId")?,
                        &request_id,
                        required_str(operation, "accountKey")?,
                        generation,
                        required_str(operation, "newThreadId")?,
                        accepted,
                    )?;
                    // The lifecycle owner may decline a stale acceptance or request an
                    // interrupt after an intervening stop. Keep the durable start receipt
                    // at this stage so recovery can reapply the same correlated decision.
                    if settled.is_none() {
                        return Ok(false);
                    }
                    if settled == Some(true) {
                        let ports =
                            lifecycle.ok_or(Error::Requires("account-worker/agent-interrupt"))?;
                        let accepted_agent = read_agent(tx, required_str(operation, "agent")?)?;
                        let mut before = accepted_agent.clone();
                        before["epoch"] = operation
                            .get("epoch")
                            .cloned()
                            .ok_or(Error::InvalidStoredState)?;
                        before["status"] = json!("starting");
                        before["autoWake"] = json!(true);
                        before["inFlight"] = json!(true);
                        before["turnId"] = json!(accepted);
                        before["connectionGeneration"] = json!(generation);
                        let effect = LifecycleEffect::new(
                            LifecycleEffectKind::Interrupt,
                            before,
                            accepted_agent,
                            unix_seconds(),
                        );
                        ports.stage(tx, &effect).map_err(lifecycle_error)?;
                        committed_interrupt = true;
                    }
                    operation["stage"] = json!("running");
                    operation["acceptedTurnId"] = json!(accepted);
                    operation["error"] = Value::Null;
                    agent = read_agent(tx, required_str(operation, "agent")?)?;
                }
                _ => return Err(Error::InvalidStoredState),
            }
            operation["dispatch"] = json!("ready");
            operation["updated"] = json!(unix_seconds());
            save_operation(tx, &mut agent, operation)?;
            Ok(true)
        })
        .and_then(|advanced| {
            if committed_interrupt {
                lifecycle
                    .ok_or(Error::Requires("account-worker/agent-interrupt"))?
                    .committed()
                    .map_err(lifecycle_error)?;
            }
            Ok(advanced)
        })
}

fn validate_receipt(
    receipt: &SessionReceipt,
    operation_id: &str,
    scope: &crate::account_session_runtime::ConnectionScope,
) -> Result<(), Error> {
    if receipt.operation != operation_id || receipt.scope != *scope {
        return Err(Error::StaleRetry);
    }
    Ok(())
}

fn settle_start_rejected(
    store: &RuntimeStore,
    operation: &mut Value,
    ticket: Option<&studio_runtime_providers::RequestTicket>,
    reason: &str,
) -> Result<bool, Error> {
    let id = required_str(operation, "id")?.to_owned();
    let (current, settled) = store.transaction_with::<_, Error>(|tx| {
        let mut current = read_retry(tx, &id)?.ok_or(Error::InvalidStoredState)?;
        if current.get("stage").and_then(Value::as_str) != Some(Stage::Start.as_str()) {
            return Ok((current, false));
        }
        let agent_id = required_str(&current, "agent")?.to_owned();
        let epoch = current
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(Error::InvalidStoredState)?;
        let attempt_id = required_str(&current, "attemptId")?.to_owned();
        let account = required_str(&current, "accountKey")?.to_owned();
        let thread = required_str(&current, "newThreadId")?.to_owned();
        let request_id = ticket
            .map(|ticket| request_id_string(&ticket.id))
            .or_else(|| {
                current
                    .get("requestId")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<RequestId>(value).ok())
                    .map(|id| request_id_string(&id))
            })
            .ok_or(Error::InvalidStoredState)?;
        let generation = ticket
            .map(|ticket| ticket.generation)
            .or_else(|| current.get("requestGeneration").and_then(Value::as_u64))
            .ok_or(Error::InvalidStoredState)?;
        SchedulerRepository::settle_safety_start_rejected_provider(
            tx,
            &agent_id,
            epoch,
            &attempt_id,
            &request_id,
            &account,
            generation,
            &thread,
            reason,
        )?;
        current["stage"] = json!(Stage::Failed.as_str());
        current["dispatch"] = json!("rejected");
        current["error"] = json!(reason);
        current["updated"] = json!(unix_seconds());
        let mut agent = read_agent(tx, &agent_id)?;
        save_operation(tx, &mut agent, &current)?;
        Ok((current, true))
    })?;
    *operation = current;
    Ok(settled)
}

fn persist_stage_receipt(
    store: &RuntimeStore,
    operation: &mut Value,
    stage: Stage,
    receipt: &SessionReceipt,
) -> Result<bool, Error> {
    let result = match &receipt.result {
        Some(ProviderResponse::Reply(value)) => value.clone(),
        Some(ProviderResponse::Rejected(reason)) => json!({"rejected":reason}),
        None => Value::Null,
    };
    let receipt_value = json!({
        "operation":receipt.operation,
        "scope":receipt.scope,
        "ticket":receipt.ticket.as_ref().map(|ticket| json!({
            "generation":ticket.generation,
            "id":ticket.id,
            "method":ticket.method.as_str()
        })),
        "certainty":match receipt.certainty {
            RequestCertainty::Written => "written",
            RequestCertainty::NotSent => "not_sent",
            RequestCertainty::Unknown => "unknown"
        },
        "notSentReason":receipt.not_sent_reason.map(|reason| format!("{reason:?}")),
        "result":result
    });
    let id = required_str(operation, "id")?.to_owned();
    let (current, still_active) = store.transaction_with::<_, Error>(|tx| {
        let mut current = read_retry(tx, &id)?.ok_or(Error::InvalidStoredState)?;
        if current.get("agent") != operation.get("agent")
            || current.get("epoch") != operation.get("epoch")
            || current.get("accountKey") != operation.get("accountKey")
        {
            return Err(Error::StaleRetry);
        }
        current
            .get_mut("receipts")
            .and_then(Value::as_object_mut)
            .ok_or(Error::InvalidStoredState)?
            .entry(stage.as_str())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(Error::InvalidStoredState)?
            .push(receipt_value.clone());
        let still_active = current.get("stage").and_then(Value::as_str) == Some(stage.as_str());
        if still_active {
            if let Some(ticket) = receipt.ticket.as_ref() {
                current["requestId"] =
                    serde_json::to_value(&ticket.id).map_err(|_| Error::InvalidStoredState)?;
                current["requestGeneration"] = json!(ticket.generation);
                if stage == Stage::Start {
                    if ticket.method != Method::TurnStart
                        || ticket.generation != receipt.scope.generation
                    {
                        return Err(Error::StaleRetry);
                    }
                }
            }
            if let Some(ProviderResponse::Reply(value)) = receipt.result.as_ref() {
                current
                    .get_mut("responses")
                    .and_then(Value::as_object_mut)
                    .ok_or(Error::InvalidStoredState)?
                    .insert(stage.as_str().into(), value.clone());
            }
            current["dispatch"] = json!("received");
            current["updated"] = json!(unix_seconds());
            let mut agent = read_agent(tx, required_str(&current, "agent")?)?;
            if stage == Stage::Start {
                if let Some(ticket) = receipt.ticket.as_ref() {
                    let attempt = agent
                        .get_mut("startAttempt")
                        .and_then(Value::as_object_mut)
                        .ok_or(Error::InvalidStoredState)?;
                    if attempt.get("id").and_then(Value::as_str)
                        != current.get("attemptId").and_then(Value::as_str)
                    {
                        return Err(Error::StaleRetry);
                    }
                    attempt.insert("requestTicket".into(), json!(request_id_string(&ticket.id)));
                    attempt.insert("connectionGeneration".into(), json!(ticket.generation));
                }
            }
            save_operation(tx, &mut agent, &current)?;
        } else {
            tx.execute(
                "UPDATE runtime_safety_retries SET record=?1 WHERE id=?2",
                params![current.to_string(), id],
            )?;
        }
        Ok((current, still_active))
    })?;
    *operation = current;
    Ok(still_active)
}

fn recorded_written_response(
    operation: &Value,
    stage: Stage,
    operation_id: &str,
    scope: &crate::account_session_runtime::ConnectionScope,
) -> Result<Option<Value>, Error> {
    let Some(receipts) = operation
        .get("receipts")
        .and_then(Value::as_object)
        .and_then(|receipts| receipts.get(stage.as_str()))
        .and_then(Value::as_array)
    else {
        return Ok(None);
    };
    let Some(last) = receipts.last() else {
        return Ok(None);
    };
    if last.get("certainty").and_then(Value::as_str) != Some("written") {
        return Ok(None);
    }
    if last.get("operation").and_then(Value::as_str) != Some(operation_id)
        || last.get("scope")
            != Some(&serde_json::to_value(scope).map_err(|_| Error::InvalidStoredState)?)
    {
        return Err(Error::InvalidStoredState);
    }
    Ok(operation
        .get("responses")
        .and_then(Value::as_object)
        .and_then(|responses| responses.get(stage.as_str()))
        .cloned())
}

fn persist_unknown_receipt(
    store: &RuntimeStore,
    operation: &mut Value,
    stage: Stage,
    operation_id: &str,
    certainty: &str,
) -> Result<bool, Error> {
    let id = required_str(operation, "id")?.to_owned();
    let (current, still_active) = store.transaction_with::<_, Error>(|tx| {
        let mut current = read_retry(tx, &id)?.ok_or(Error::InvalidStoredState)?;
        current
            .get_mut("receipts")
            .and_then(Value::as_object_mut)
            .ok_or(Error::InvalidStoredState)?
            .entry(stage.as_str())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(Error::InvalidStoredState)?
            .push(json!({"operation":operation_id,"certainty":certainty}));
        let still_active = current.get("stage").and_then(Value::as_str) == Some(stage.as_str());
        if still_active {
            current["updated"] = json!(unix_seconds());
            let mut agent = read_agent(tx, required_str(&current, "agent")?)?;
            save_operation(tx, &mut agent, &current)?;
        } else {
            tx.execute(
                "UPDATE runtime_safety_retries SET record=?1 WHERE id=?2",
                params![current.to_string(), id],
            )?;
        }
        Ok((current, still_active))
    })?;
    *operation = current;
    Ok(still_active)
}

fn persist_operation(store: &RuntimeStore, operation: &mut Value) -> Result<(), Error> {
    operation["updated"] = json!(unix_seconds());
    let updated = store.transaction_with::<_, Error>(|tx| {
        let agent_id = required_str(operation, "agent")?;
        if let Some(current) = read_retry(tx, required_str(operation, "id")?)?
            && current.get("stage").and_then(Value::as_str) == Some(Stage::Cancelled.as_str())
        {
            return Ok(current);
        }
        let mut agent = read_agent(tx, agent_id)?;
        save_operation(tx, &mut agent, operation)?;
        Ok(operation.clone())
    })?;
    *operation = updated;
    Ok(())
}

fn request_id_string(id: &RequestId) -> String {
    match id {
        RequestId::String(value) => value.clone(),
        RequestId::Number(value) => value.to_string(),
    }
}

fn lifecycle_error(error: crate::agent_management::ManagementError) -> Error {
    match error {
        crate::agent_management::ManagementError::Unavailable(dependency) => {
            Error::Requires(dependency)
        }
        crate::agent_management::ManagementError::Invalid(_)
        | crate::agent_management::ManagementError::Stored { .. }
        | crate::agent_management::ManagementError::Store(_) => Error::Persistence,
    }
}

fn save_operation(tx: &Transaction<'_>, agent: &mut Value, operation: &Value) -> Result<(), Error> {
    let id = required_str(operation, "id")?;
    tx.execute(
        "INSERT INTO runtime_safety_retries(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
        params![id, operation.to_string()],
    )?;
    agent["nativeSafetyRetry"] = public_receipt(operation)?;
    put_agent(tx, agent)
}

fn read_retry(tx: &Transaction<'_>, id: &str) -> Result<Option<Value>, Error> {
    let raw = tx
        .query_row(
            "SELECT record FROM runtime_safety_retries WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(|_| Error::InvalidStoredState))
        .transpose()
}

fn read_agent(tx: &Transaction<'_>, id: &str) -> Result<Value, Error> {
    let record = AgentRepository::get(tx, &AgentId::from(id.to_owned()))?.ok_or(Error::Inactive)?;
    Ok(record.value())
}

fn put_agent(tx: &Transaction<'_>, value: &Value) -> Result<(), Error> {
    let record = AgentRecord::from_value(value.clone()).map_err(|_| Error::InvalidStoredState)?;
    AgentRepository::put(tx, &record)?;
    Ok(())
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(Error::InvalidStoredState)
}

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

impl From<StoreError> for Error {
    fn from(_: StoreError) -> Self {
        Self::Persistence
    }
}

impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::Persistence
    }
}
