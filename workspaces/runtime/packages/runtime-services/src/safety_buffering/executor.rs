use super::{
    SafetyBufferingError as Error, SafetyDecision, SafetyRetryStage as Stage, SafetySnapshot,
    decide_action,
};
use crate::account_session_runtime::SessionOutcome as ReceiptOutcome;
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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use studio_runtime_providers::{Method, RequestId};
use studio_runtime_schema::persistence::{RequestIdentity, SafetyRetryCertainty};
use studio_runtime_store::request_results::{Admission, AdmittedRequest, RequestResults};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, RuntimeStore, SchedulerRepository, StoreError,
};

// Field order is the initial admission signature contract.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SafetyInitialReceipt {
    account_key: String,
    agent: String,
    attempt_id: String,
    connection_id: String,
    created: f64,
    epoch: i64,
    id: String,
    model: String,
    root_request_id: String,
    source_thread_id: String,
    stage: Stage,
    thread_id: String,
    turn_id: String,
    updated: f64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafetyOperation {
    #[serde(flatten)]
    initial: SafetyInitialReceipt,
    #[serde(flatten)]
    dispatch: Dispatch,
    scope: crate::account_session_runtime::ConnectionScope,
    receipts: BTreeMap<String, Vec<StageReceipt>>,
    responses: BTreeMap<String, Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rpc_method: Option<Method>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_params: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_hold: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished: Option<f64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Dispatch {
    admission: AdmittedRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    admitted_request: Option<AdmittedRequest>,
    #[serde(flatten)]
    state: DispatchState,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "dispatch", rename_all = "snake_case", deny_unknown_fields)]
enum DispatchState {
    Ready,
    Intent,
    Received,
    Rejected,
    Unknown,
    Cancelled,
}

#[derive(Clone, Deserialize, Serialize)]
struct StageTicket {
    generation: u64,
    id: RequestId,
    method: Method,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StageReceipt {
    operation: String,
    certainty: SafetyRetryCertainty,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<crate::account_session_runtime::ConnectionScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ticket: Option<StageTicket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    not_sent_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct TurnsPage {
    data: Vec<Turn>,
}
#[derive(Deserialize)]
struct Turn {
    id: String,
    status: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemsPage {
    data: Vec<ItemEntry>,
    next_cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemEntry {
    turn_id: String,
    item: Item,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Item {
    UserMessage { content: Option<Vec<Value>> },
    Reasoning,
}
#[derive(Deserialize)]
struct Identified {
    id: String,
}
#[derive(Deserialize)]
struct ForkReply {
    thread: Identified,
}
#[derive(Deserialize)]
struct StartReply {
    turn: Identified,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const NOT_SENT_RETRIES: usize = 1;

pub struct SafetyAction<'a> {
    pub agent_id: &'a str,
    pub actor: RequestActor,
    pub turn_id: &'a str,
    pub choice: &'a str,
}

/// Execute the complete retry action against the agent's retained Codex session.
/// Every request is journaled in runtime_safety_retries before hand-off.
pub async fn execute_action(
    store: Arc<RuntimeStore>,
    provider: &dyn AccountSessionPort,
    resources: &Arc<PreparationResources>,
    lifecycle: Option<Arc<dyn LifecyclePorts>>,
    action: SafetyAction<'_>,
) -> Result<Value, Error> {
    let SafetyAction {
        agent_id,
        actor,
        turn_id,
        choice,
    } = action;
    if actor.agent != agent_id || turn_id.is_empty() {
        return Err(Error::Inactive);
    }
    if !matches!(choice, "wait" | "retry" | "cancel") {
        return Err(Error::InvalidChoice);
    }
    let agent_id = agent_id.to_owned();
    let turn_id = turn_id.to_owned();
    let choice = choice.to_owned();
    let account_agent_id = agent_id.clone();
    let actor_epoch = actor.epoch;
    let account = store
        .transaction_with(move |tx| {
            let agent = read_agent(tx, &account_agent_id)?;
            if agent.get("epoch").and_then(Value::as_i64) != Some(actor_epoch) {
                return Err(Error::StaleRetry);
            }
            Ok(agent
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned())
        })
        .await?;
    let scope = provider.current_scope(&account);
    if scope.as_ref().is_some_and(|scope| scope.account != account) {
        return Err(Error::StaleRetry);
    }
    let identity = format!("{agent_id}:{turn_id}");
    let now = unix_seconds();
    let fresh_attempt = RequestIdentity::issue_root();
    let decision_agent_id = agent_id.clone();
    let decision_turn_id = turn_id.clone();
    let decision_choice = choice.clone();
    let decision_account = account.clone();
    let decision_scope = scope.clone();
    let decision_identity = identity.clone();
    let decision_attempt = fresh_attempt.clone();
    let decision_actor = actor.clone();
    let decision = store
        .transaction_with(move |tx| {
            let mut agent = read_agent(tx, &decision_agent_id)?;
            if agent.get("epoch").and_then(Value::as_i64) != Some(decision_actor.epoch)
                || agent
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    != decision_account
            {
                return Err(Error::StaleRetry);
            }
            let existing = read_retry(tx, &decision_identity)?;
            if let Some(mut operation) = existing {
                let mut stage = operation.initial.stage;
                if operation.initial.account_key != decision_account
                    || operation.initial.epoch != decision_actor.epoch
                    || operation.initial.agent != decision_agent_id
                    || operation.initial.turn_id != decision_turn_id
                    || operation.initial.id != decision_identity
                {
                    return Err(Error::StaleRetry);
                }
                if operation_has_unknown_receipt(&operation, stage)? {
                    operation.initial.stage = Stage::Unknown;
                    operation.dispatch.state = DispatchState::Unknown;
                    operation.error = Some("Provider request outcome is unknown".into());
                    operation.initial.updated = now;
                    save_operation(tx, &mut agent, &operation)?;
                    stage = Stage::Unknown;
                }
                if decision_choice == "cancel" && super::ACTIVE_STAGES.contains(&stage) {
                    if operation.rpc_method == Some(Method::TurnStart) {
                        return Err(Error::StartAlreadySubmitted);
                    }
                    operation.initial.stage = Stage::Cancelled;
                    operation.dispatch.state = DispatchState::Cancelled;
                    operation.error = None;
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
                    return Ok(ActionPlan::Return(operation_public(&operation)?));
                }
                let public = operation_public(&operation)?;
                if decision_choice == "retry"
                    && !matches!(
                        stage,
                        Stage::Unknown | Stage::Running | Stage::Failed | Stage::Cancelled
                    )
                    && !matches!(operation.dispatch.state, DispatchState::Unknown)
                {
                    return Ok(ActionPlan::Drive(Box::new(operation)));
                }
                return Ok(ActionPlan::Return(public));
            }
            if decision_choice == "cancel" {
                return Err(Error::NoRetryToCancel);
            }
            let scope = decision_scope
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
                &decision_choice,
                &decision_turn_id,
                None,
                &decision_identity,
                &decision_attempt,
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
                SafetyDecision::BeginRetry { mut receipt } => {
                    receipt
                        .as_object_mut()
                        .ok_or(Error::InvalidStoredState)?
                        .insert(
                            "rootRequestId".into(),
                            Value::String(decision_attempt.clone()),
                        );
                    let initial = SafetyInitialReceipt::deserialize(&receipt)
                        .map_err(|_| Error::InvalidStoredState)?;
                    let identity = RequestIdentity::new(
                        "safety-retry",
                        &[&decision_agent_id],
                        &decision_attempt,
                        &[&decision_identity],
                    )
                    .map_err(|_| Error::InvalidStoredState)?;
                    let signature =
                        serde_json::to_string(&initial).map_err(|_| Error::InvalidStoredState)?;
                    let Admission::Fresh(admission) = RequestResults::admit(
                        tx,
                        &identity,
                        &signature,
                        (unix_seconds() * 1000.0) as i64,
                    )
                    .map_err(|_| Error::Persistence)?
                    else {
                        return Err(Error::InvalidStoredState);
                    };
                    let operation = SafetyOperation {
                        initial,
                        dispatch: Dispatch {
                            admission,
                            admitted_request: None,
                            state: DispatchState::Ready,
                        },
                        scope: scope.clone(),
                        receipts: BTreeMap::new(),
                        responses: BTreeMap::new(),
                        rpc_method: None,
                        operation_id: None,
                        request_params: None,
                        input: None,
                        new_thread_id: None,
                        accepted_turn_id: None,
                        request_id: None,
                        request_generation: None,
                        error: None,
                        terminal_hold: None,
                        finished: None,
                    };
                    save_operation(tx, &mut agent, &operation)?;
                    Ok(ActionPlan::Drive(Box::new(operation)))
                }
            }
        })
        .await?;
    match decision {
        ActionPlan::Return(receipt) => Ok(receipt),
        ActionPlan::Drive(operation) => {
            drive(store, provider, resources, lifecycle, *operation).await
        }
    }
}

enum ActionPlan {
    Return(Value),
    Drive(Box<SafetyOperation>),
}

async fn drive(
    store: Arc<RuntimeStore>,
    provider: &dyn AccountSessionPort,
    resources: &Arc<PreparationResources>,
    lifecycle: Option<Arc<dyn LifecyclePorts>>,
    mut operation: SafetyOperation,
) -> Result<Value, Error> {
    loop {
        let stage = operation.initial.stage;
        if matches!(
            stage,
            Stage::Running | Stage::Failed | Stage::Cancelled | Stage::Unknown
        ) {
            return operation_public(&operation);
        }
        let account = operation.initial.account_key.clone();
        let scope = provider
            .current_scope(&account)
            .ok_or(Error::Requires("provider-session/native-request"))?;
        let saved_scope = operation.scope.clone();
        if scope != saved_scope {
            return Err(Error::StaleRetry);
        }
        let operation_id = format!("{}:{}", operation.initial.id, stage.as_str());
        let recorded = recorded_written_response(&operation, stage, &operation_id, &saved_scope)?;
        let from_storage = recorded.is_some();
        let body = if let Some(response) = recorded {
            response
        } else {
            let (method, params) = request_for_stage(
                Arc::clone(&store),
                resources,
                &operation,
                stage,
                scope.generation,
            )
            .await?;
            if !persist_intent(
                &store,
                &mut operation,
                stage,
                &operation_id,
                method,
                &params,
            )
            .await?
            {
                return operation_public(&operation);
            }
            let request = ProviderRequest {
                root_request_id: operation.initial.root_request_id.clone(),
                admitted_request: operation.dispatch.admitted_request.clone(),
                account: account.clone(),
                operation: operation_id.clone(),
                actor: Some(RequestActor {
                    agent: operation.initial.agent.clone(),
                    epoch: operation.initial.epoch,
                }),
                deleted_actor_session: None,
                expected_scope: Some(scope),
                method,
                params: params.clone(),
                deadline: Instant::now() + REQUEST_TIMEOUT,
            };
            let mut sent_retry = 0;
            let receipt = loop {
                match provider.provider_request(request.clone()).await {
                    Ok(receipt) => {
                        validate_receipt(&receipt, &operation_id, &saved_scope)?;
                        if !persist_stage_receipt(&store, &mut operation, stage, &receipt).await? {
                            return operation_public(&operation);
                        }
                        if receipt.certainty() == RequestCertainty::NotSent
                            && sent_retry < NOT_SENT_RETRIES
                        {
                            sent_retry += 1;
                            operation.dispatch.state = DispatchState::Intent;
                            persist_operation(&store, &mut operation).await?;
                            if operation.initial.stage != stage {
                                return operation_public(&operation);
                            }
                            continue;
                        }
                        break receipt;
                    }
                    Err(AccountSessionError::Expired) => return Err(Error::Expired),
                    Err(AccountSessionError::Requires(name)) => return Err(Error::Requires(name)),
                    Err(AccountSessionError::Refused(_)) => {
                        if !persist_unknown_receipt(
                            &store,
                            &mut operation,
                            stage,
                            &operation_id,
                            SafetyRetryCertainty::Refused,
                        )
                        .await?
                        {
                            return operation_public(&operation);
                        }
                        if stage == Stage::Start
                            && !settle_start_rejected(
                                &store,
                                &mut operation,
                                None,
                                "Provider refused turn/start",
                            )
                            .await?
                        {
                            return operation_public(&operation);
                        }
                        operation.initial.stage = Stage::Failed;
                        operation.dispatch.state = DispatchState::Rejected;
                        operation.error = Some("Provider request refused".into());
                        persist_operation(&store, &mut operation).await?;
                        return Err(Error::ProviderRejected);
                    }
                    Err(_) => {
                        if !persist_unknown_receipt(
                            &store,
                            &mut operation,
                            stage,
                            &operation_id,
                            SafetyRetryCertainty::Unknown,
                        )
                        .await?
                        {
                            return operation_public(&operation);
                        }
                        operation.initial.stage = Stage::Unknown;
                        operation.dispatch.state = DispatchState::Unknown;
                        operation.error = Some("Provider request outcome is unknown".into());
                        persist_operation(&store, &mut operation).await?;
                        return operation_public(&operation);
                    }
                }
            };
            let (ticket, response) = match receipt.outcome {
                ReceiptOutcome::WorkerNotSent { .. }
                | ReceiptOutcome::NoLiveAccountWorker
                | ReceiptOutcome::ExpectedScopeMismatch
                | ReceiptOutcome::DeadlineExpired
                | ReceiptOutcome::NotHandedOff => {
                    operation.dispatch.state = DispatchState::Ready;
                    persist_operation(&store, &mut operation).await?;
                    return operation_public(&operation);
                }
                ReceiptOutcome::Unknown { .. } => {
                    operation.initial.stage = Stage::Unknown;
                    operation.dispatch.state = DispatchState::Unknown;
                    operation.error = Some("Provider request outcome is unknown".into());
                    persist_operation(&store, &mut operation).await?;
                    return operation_public(&operation);
                }
                ReceiptOutcome::Written { ticket, response } => (ticket, response),
            };
            let body = match response {
                ProviderResponse::Reply(value) => value,
                ProviderResponse::Rejected(_) => {
                    if stage == Stage::Start
                        && !settle_start_rejected(
                            &store,
                            &mut operation,
                            Some(&ticket),
                            "Provider rejected turn/start",
                        )
                        .await?
                    {
                        return operation_public(&operation);
                    }
                    operation.initial.stage = Stage::Failed;
                    operation.dispatch.state = DispatchState::Rejected;
                    operation.error = Some("Provider rejected the safety retry request".into());
                    persist_operation(&store, &mut operation).await?;
                    return Err(Error::ProviderRejected);
                }
            };
            operation.responses.insert(
                stage.as_str().into(),
                serde_json::value::to_raw_value(&body).map_err(|_| Error::InvalidStoredState)?,
            );
            operation.dispatch.state = DispatchState::Received;
            body
        };
        match advance_stage(
            &store,
            &mut operation,
            stage,
            body,
            lifecycle.clone(),
            from_storage,
        )
        .await
        {
            Ok(true) => (),
            Ok(false) => return operation_public(&operation),
            Err(error @ (Error::ProviderRejected | Error::Inactive)) => {
                operation.initial.stage = Stage::Failed;
                operation.dispatch.state = DispatchState::Rejected;
                operation.error = Some("Safety retry stage was refused".into());
                persist_operation(&store, &mut operation).await?;
                return Err(error);
            }
            Err(Error::ProviderUnknown) => {
                operation.initial.stage = Stage::Unknown;
                operation.dispatch.state = DispatchState::Unknown;
                operation.error =
                    Some("Provider response did not identify the resulting thread or turn".into());
                persist_operation(&store, &mut operation).await?;
                return operation_public(&operation);
            }
            Err(error) => return Err(error),
        }
    }
}

async fn request_for_stage(
    store: Arc<RuntimeStore>,
    resources: &Arc<PreparationResources>,
    operation: &SafetyOperation,
    stage: Stage,
    generation: u64,
) -> Result<(Method, Value), Error> {
    let thread = &operation.initial.thread_id;
    let method = match stage {
        Stage::Turns | Stage::VerifyTurns => Method::ThreadTurnsList,
        Stage::Items | Stage::VerifyItems => Method::ThreadItemsList,
        Stage::Interrupt => Method::TurnInterrupt,
        Stage::Fork => Method::ThreadFork,
        Stage::Start => Method::TurnStart,
        Stage::Running | Stage::Unknown | Stage::Failed | Stage::Cancelled => {
            return Err(Error::InvalidStoredState);
        }
    };
    let params = match stage {
        Stage::Turns | Stage::VerifyTurns => {
            json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"})
        }
        Stage::Items | Stage::VerifyItems => {
            json!({"threadId":thread,"turnId":&operation.initial.turn_id,"limit":1000,"sortDirection":"asc"})
        }
        Stage::Interrupt => json!({"threadId":thread,"turnId":&operation.initial.turn_id}),
        Stage::Fork | Stage::Start => {
            let thread = if stage == Stage::Fork {
                &operation.initial.source_thread_id
            } else {
                operation
                    .new_thread_id
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .ok_or(Error::InvalidStoredState)?
            };
            let mut prepared = prepare_safety_retry(
                store,
                resources,
                &SafetyRetryPreparation {
                    agent_id: operation.initial.agent.clone(),
                    attempt_id: operation.initial.attempt_id.clone(),
                    generation,
                    source_thread_id: thread.clone(),
                    turn_id: operation.initial.turn_id.clone(),
                    model: operation.initial.model.clone(),
                    input: serde_json::from_str(
                        operation
                            .input
                            .as_ref()
                            .ok_or(Error::InvalidStoredState)?
                            .get(),
                    )
                    .map_err(|_| Error::InvalidStoredState)?,
                },
            )
            .await
            .map_err(|error| Error::Preparation(error.to_string()))?;
            if stage == Stage::Fork {
                prepared.fork_params
            } else {
                prepared
                    .with_thread_id(thread)
                    .map_err(|error| Error::Preparation(error.to_string()))?;
                prepared.turn_params
            }
        }
        Stage::Running | Stage::Unknown | Stage::Failed | Stage::Cancelled => {
            return Err(Error::InvalidStoredState);
        }
    };
    Ok((method, params))
}

async fn persist_intent(
    store: &RuntimeStore,
    operation: &mut SafetyOperation,
    stage: Stage,
    operation_id: &str,
    method: Method,
    params: &Value,
) -> Result<bool, Error> {
    operation.initial.stage = stage;
    operation.dispatch.state = DispatchState::Intent;
    operation.rpc_method = Some(method);
    operation.operation_id = Some(operation_id.to_owned());
    operation.request_params =
        Some(serde_json::value::to_raw_value(params).map_err(|_| Error::InvalidStoredState)?);
    operation.initial.updated = unix_seconds();
    let mut pending = operation.clone();
    let reservation_params = params.clone();
    let operation_id = operation_id.to_owned();
    let id = pending.initial.id.clone();
    let agent_id = pending.initial.agent.clone();
    let result = store
        .transaction_with_error(move |tx| -> Result<_, Error> {
            let current = read_retry(tx, &id)?.ok_or(Error::InvalidStoredState)?;
            if current.initial.stage != stage {
                return Ok((current, false));
            }
            let mut agent = read_agent(tx, &agent_id)?;
            validate_current(&agent, &pending, stage)?;
            pending.dispatch.admitted_request = None;
            if matches!(stage, Stage::Interrupt | Stage::Fork | Stage::Start) {
                let admitted = if current.operation_id == pending.operation_id
                    && current.dispatch.admitted_request.is_some()
                {
                    current
                        .dispatch
                        .admitted_request
                        .clone()
                        .ok_or(Error::InvalidStoredState)?
                } else {
                    crate::account_worker::retained::reserve_provider_call(
                        tx,
                        &pending.dispatch.admission,
                        crate::account_worker::retained::ProviderCallIntent {
                            root_request_id: pending.initial.root_request_id.clone(),
                            admitted_request: None,
                            scope: pending.scope.clone(),
                            operation: operation_id.to_owned(),
                            actor: Some(RequestActor {
                                agent: agent_id.clone(),
                                epoch: pending.initial.epoch,
                            }),
                            transfer: None,
                            deleted_actor_session: None,
                            method,
                            params: reservation_params,
                        },
                    )
                    .map_err(|_| Error::Persistence)?
                };
                pending.dispatch.admitted_request = Some(admitted);
            }
            if stage == Stage::Start {
                let attempt = json!({
                    "id":&pending.initial.attempt_id,
                    "epoch":pending.initial.epoch,
                    "events":[],"action":"safety","submitted":true,
                    "accountKey":&pending.initial.account_key,
                    "connectionId":&pending.initial.connection_id,
                    "threadId":pending.new_thread_id.as_deref().ok_or(Error::InvalidStoredState)?
                });
                let program = if agent.get("daybreakEnabled").and_then(Value::as_bool) == Some(true)
                {
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
                agent["turnEpoch"] = json!(pending.initial.epoch);
                agent["error"] = Value::Null;
                agent["startAttempt"] = attempt;
            }
            save_operation(tx, &mut agent, &pending)?;
            Ok((pending, true))
        })
        .await?;
    *operation = result.0;
    Ok(result.1)
}

fn validate_current(agent: &Value, operation: &SafetyOperation, stage: Stage) -> Result<(), Error> {
    let expected_thread = if matches!(stage, Stage::Start) {
        operation.new_thread_id.as_deref()
    } else {
        Some(operation.initial.thread_id.as_str())
    };
    let projection = agent.get("nativeSafetyRetry").and_then(Value::as_object);
    if agent.get("deletedAt").is_some_and(|value| !value.is_null())
        || agent.get("epoch").and_then(Value::as_i64) != Some(operation.initial.epoch)
        || agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != operation.initial.account_key
        || agent.get("threadId").and_then(Value::as_str) != expected_thread
        || agent.get("autoWake").and_then(Value::as_bool) != Some(true)
        || agent
            .get("accountTransferId")
            .is_some_and(|value| !value.is_null())
        || projection
            .and_then(|record| record.get("id"))
            .and_then(Value::as_str)
            != Some(operation.initial.id.as_str())
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
                == Some(operation.initial.turn_id.as_str())
            || agent.get("turnId").and_then(Value::as_str)
                != Some(operation.initial.turn_id.as_str())
        {
            return Err(Error::Inactive);
        }
    }
    Ok(())
}

async fn advance_stage(
    store: &RuntimeStore,
    operation: &mut SafetyOperation,
    stage: Stage,
    response: Value,
    lifecycle: Option<Arc<dyn LifecyclePorts>>,
    from_storage: bool,
) -> Result<bool, Error> {
    let target = operation;
    let mut operation = target.clone();
    let lifecycle_after = lifecycle.clone();
    let (updated, advanced, committed_interrupt) = store
        .transaction_with(move |tx| {
            let mut committed_interrupt = false;
            let id = &operation.initial.id;
            let current = read_retry(tx, id)?.ok_or(Error::InvalidStoredState)?;
            if current.initial.stage != stage {
                operation = current;
                return Ok((operation, false, false));
            }
            let mut agent = read_agent(tx, &operation.initial.agent)?;
            if stage != Stage::Start {
                validate_current(&agent, &operation, stage)?;
            }
            match stage {
                Stage::Turns | Stage::VerifyTurns => {
                    let turns = TurnsPage::deserialize(&response).map_err(|_| {
                        if from_storage {
                            Error::InvalidStoredState
                        } else {
                            Error::ProviderRejected
                        }
                    })?;
                    let latest = turns.data.first().ok_or(Error::Inactive)?;
                    if Some(latest.id.as_str()) != Some(operation.initial.turn_id.as_str()) {
                        return Err(Error::Inactive);
                    }
                    if stage == Stage::VerifyTurns
                        && latest.status.as_deref() != Some("interrupted")
                    {
                        return Err(Error::ProviderRejected);
                    }
                    operation.initial.stage = if stage == Stage::Turns {
                        Stage::Items
                    } else {
                        Stage::VerifyItems
                    };
                }
                Stage::Items | Stage::VerifyItems => {
                    let entries = ItemsPage::deserialize(&response).map_err(|_| {
                        if from_storage {
                            Error::InvalidStoredState
                        } else {
                            Error::ProviderRejected
                        }
                    })?;
                    if entries
                        .next_cursor
                        .as_deref()
                        .is_some_and(|cursor| !cursor.is_empty())
                    {
                        return Err(Error::ProviderRejected);
                    }
                    let mut input = Vec::new();
                    for entry in entries.data {
                        if Some(entry.turn_id.as_str()) != Some(operation.initial.turn_id.as_str())
                        {
                            return Err(Error::ProviderRejected);
                        }
                        match entry.item {
                            Item::UserMessage {
                                content: Some(content),
                            } => input.extend(content),
                            Item::UserMessage { content: None } | Item::Reasoning => {}
                        }
                    }
                    if input.is_empty() {
                        return Err(Error::ProviderRejected);
                    }
                    operation.input = Some(
                        serde_json::value::to_raw_value(&input)
                            .map_err(|_| Error::InvalidStoredState)?,
                    );
                    operation.initial.stage = if stage == Stage::Items {
                        Stage::Interrupt
                    } else {
                        Stage::Fork
                    };
                }
                Stage::Interrupt => operation.initial.stage = Stage::VerifyTurns,
                Stage::Fork => {
                    let reply = ForkReply::deserialize(&response).map_err(|_| {
                        if from_storage {
                            Error::InvalidStoredState
                        } else {
                            Error::ProviderUnknown
                        }
                    })?;
                    let thread = reply.thread.id.as_str();
                    if thread.is_empty() {
                        return Err(Error::ProviderUnknown);
                    }
                    if Some(operation.initial.thread_id.as_str()) == Some(thread) {
                        return Err(Error::ProviderUnknown);
                    }
                    operation.new_thread_id = Some(thread.to_owned());
                    operation.initial.stage = Stage::Start;
                    agent["threadId"] = json!(thread);
                    agent["model"] = json!(operation.initial.model);
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
                    let reply = StartReply::deserialize(&response).map_err(|_| {
                        if from_storage {
                            Error::InvalidStoredState
                        } else {
                            Error::ProviderUnknown
                        }
                    })?;
                    let accepted = reply.turn.id.as_str();
                    if accepted.is_empty() {
                        return Err(Error::ProviderUnknown);
                    }
                    let request_id = request_id_string(
                        operation
                            .request_id
                            .as_ref()
                            .ok_or(Error::InvalidStoredState)?,
                    );
                    let generation = operation
                        .request_generation
                        .ok_or(Error::InvalidStoredState)?;
                    let settled = SchedulerRepository::settle_safety_started_provider(
                        tx,
                        &operation.initial.agent,
                        operation.initial.epoch,
                        &operation.initial.attempt_id,
                        &request_id,
                        &operation.initial.account_key,
                        generation,
                        operation
                            .new_thread_id
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .ok_or(Error::InvalidStoredState)?,
                        accepted,
                    )?;
                    // The lifecycle owner may decline a stale acceptance or request an
                    // interrupt after an intervening stop. Keep the durable start receipt
                    // at this stage so recovery can reapply the same correlated decision.
                    if settled.is_none() {
                        return Ok((operation, false, false));
                    }
                    if settled == Some(true) {
                        let ports =
                            lifecycle.ok_or(Error::Requires("account-worker/agent-interrupt"))?;
                        let accepted_agent = read_agent(tx, &operation.initial.agent)?;
                        let mut before = accepted_agent.clone();
                        before["epoch"] = json!(operation.initial.epoch);
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
                    operation.initial.stage = Stage::Running;
                    operation.error = None;
                    operation.accepted_turn_id = Some(accepted.to_owned());
                    agent = read_agent(tx, &operation.initial.agent)?;
                }
                Stage::Running | Stage::Unknown | Stage::Failed | Stage::Cancelled => {
                    return Err(Error::InvalidStoredState);
                }
            }
            operation.dispatch.state = DispatchState::Ready;
            operation.initial.updated = unix_seconds();
            save_operation(tx, &mut agent, &operation)?;
            Ok((operation, true, committed_interrupt))
        })
        .await?;
    *target = updated;
    if committed_interrupt {
        lifecycle_after
            .ok_or(Error::Requires("account-worker/agent-interrupt"))?
            .committed()
            .await
            .map_err(lifecycle_error)?;
    }
    Ok(advanced)
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

async fn settle_start_rejected(
    store: &RuntimeStore,
    operation: &mut SafetyOperation,
    ticket: Option<&studio_runtime_providers::RequestTicket>,
    reason: &str,
) -> Result<bool, Error> {
    let id = operation.initial.id.clone();
    let ticket = ticket.cloned();
    let reason = reason.to_owned();
    let (current, settled) = store
        .transaction_with::<_, Error>(move |tx| {
            let mut current = read_retry(tx, &id)?.ok_or(Error::InvalidStoredState)?;
            if current.initial.stage != Stage::Start {
                return Ok((current, false));
            }
            let agent_id = current.initial.agent.clone();
            let epoch = current.initial.epoch;
            let attempt_id = current.initial.attempt_id.clone();
            let account = current.initial.account_key.clone();
            let thread = current
                .new_thread_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or(Error::InvalidStoredState)?
                .to_owned();
            let request_id = ticket
                .as_ref()
                .map(|ticket| request_id_string(&ticket.id))
                .or_else(|| current.request_id.as_ref().map(request_id_string))
                .ok_or(Error::InvalidStoredState)?;
            let generation = ticket
                .as_ref()
                .map(|ticket| ticket.generation)
                .or(current.request_generation)
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
                &reason,
            )?;
            current.initial.stage = Stage::Failed;
            current.dispatch.state = DispatchState::Rejected;
            current.error = Some(reason);
            current.initial.updated = unix_seconds();
            let mut agent = read_agent(tx, &agent_id)?;
            save_operation(tx, &mut agent, &current)?;
            Ok((current, true))
        })
        .await?;
    *operation = current;
    Ok(settled)
}

async fn persist_stage_receipt(
    store: &RuntimeStore,
    operation: &mut SafetyOperation,
    stage: Stage,
    receipt: &SessionReceipt,
) -> Result<bool, Error> {
    let result = match &receipt.response().cloned() {
        Some(ProviderResponse::Reply(value)) => value.clone(),
        Some(ProviderResponse::Rejected(reason)) => json!({"rejected":reason}),
        None => Value::Null,
    };
    let receipt_value = StageReceipt {
        operation: receipt.operation.clone(),
        scope: Some(receipt.scope.clone()),
        ticket: receipt.ticket().map(|ticket| StageTicket {
            generation: ticket.generation,
            id: ticket.id.clone(),
            method: ticket.method,
        }),
        certainty: match receipt.certainty() {
            RequestCertainty::Written => SafetyRetryCertainty::Written,
            RequestCertainty::NotSent => SafetyRetryCertainty::NotSent,
            RequestCertainty::Unknown => SafetyRetryCertainty::Unknown,
        },
        not_sent_reason: receipt
            .not_sent_reason()
            .map(|reason| format!("{reason:?}")),
        result: Some(
            serde_json::value::to_raw_value(&result).map_err(|_| Error::InvalidStoredState)?,
        ),
    };
    let id = operation.initial.id.clone();
    let expected_agent = operation.initial.agent.clone();
    let expected_epoch = operation.initial.epoch;
    let expected_account = operation.initial.account_key.clone();
    let ticket = receipt.ticket().cloned();
    let certainty_unknown = receipt.certainty() == RequestCertainty::Unknown;
    let receipt_generation = receipt.scope.generation;
    let reply = match &receipt.response().cloned() {
        Some(ProviderResponse::Reply(value)) => Some(value.clone()),
        _ => None,
    };
    let (current, still_active) = store
        .transaction_with::<_, Error>(move |tx| {
            let mut current = read_retry(tx, &id)?.ok_or(Error::InvalidStoredState)?;
            if current.initial.agent != expected_agent
                || current.initial.epoch != expected_epoch
                || current.initial.account_key != expected_account
            {
                return Err(Error::StaleRetry);
            }
            current
                .receipts
                .entry(stage.as_str().into())
                .or_default()
                .push(receipt_value);
            let mut still_active = current.initial.stage == stage;
            if still_active {
                if let Some(ticket) = ticket.as_ref() {
                    current.request_id = Some(ticket.id.clone());
                    current.request_generation = Some(ticket.generation);
                    if stage == Stage::Start
                        && (ticket.method != Method::TurnStart
                            || ticket.generation != receipt_generation)
                    {
                        return Err(Error::StaleRetry);
                    }
                }
                if let Some(value) = reply.as_ref() {
                    current.responses.insert(
                        stage.as_str().into(),
                        serde_json::value::to_raw_value(value)
                            .map_err(|_| Error::InvalidStoredState)?,
                    );
                }
                if certainty_unknown {
                    current.initial.stage = Stage::Unknown;
                    current.dispatch.state = DispatchState::Unknown;
                    current.error = Some("Provider request outcome is unknown".into());
                    still_active = false;
                } else {
                    current.dispatch.state = DispatchState::Received;
                }
                current.initial.updated = unix_seconds();
                let mut agent = read_agent(tx, &current.initial.agent)?;
                if stage == Stage::Start
                    && let Some(ticket) = ticket.as_ref()
                {
                    let attempt = agent
                        .get_mut("startAttempt")
                        .and_then(Value::as_object_mut)
                        .ok_or(Error::InvalidStoredState)?;
                    if attempt.get("id").and_then(Value::as_str)
                        != Some(current.initial.attempt_id.as_str())
                    {
                        return Err(Error::StaleRetry);
                    }
                    attempt.insert("requestTicket".into(), json!(request_id_string(&ticket.id)));
                    attempt.insert("connectionGeneration".into(), json!(ticket.generation));
                }
                save_operation(tx, &mut agent, &current)?;
            } else {
                store_operation(tx, &current)?;
            }
            Ok((current, still_active))
        })
        .await?;
    *operation = current;
    Ok(still_active)
}

fn operation_has_unknown_receipt(operation: &SafetyOperation, stage: Stage) -> Result<bool, Error> {
    let expected = format!("{}:{}", operation.initial.id, stage.as_str());
    for receipt in operation.receipts.get(stage.as_str()).into_iter().flatten() {
        if receipt.certainty == SafetyRetryCertainty::Unknown {
            if receipt.operation != expected {
                return Err(Error::InvalidStoredState);
            }
            return Ok(true);
        }
    }
    Ok(false)
}

fn recorded_written_response(
    operation: &SafetyOperation,
    stage: Stage,
    operation_id: &str,
    scope: &crate::account_session_runtime::ConnectionScope,
) -> Result<Option<Value>, Error> {
    let Some(last) = operation
        .receipts
        .get(stage.as_str())
        .and_then(|receipts| receipts.last())
    else {
        return Ok(None);
    };
    if last.certainty != SafetyRetryCertainty::Written {
        return Ok(None);
    }
    if last.operation != operation_id || last.scope.as_ref() != Some(scope) {
        return Err(Error::InvalidStoredState);
    }
    operation
        .responses
        .get(stage.as_str())
        .map(|raw| serde_json::from_str(raw.get()).map_err(|_| Error::InvalidStoredState))
        .transpose()
}

async fn persist_unknown_receipt(
    store: &RuntimeStore,
    operation: &mut SafetyOperation,
    stage: Stage,
    operation_id: &str,
    certainty: SafetyRetryCertainty,
) -> Result<bool, Error> {
    let id = operation.initial.id.clone();
    let operation_id = operation_id.to_owned();
    let (current, still_active) = store
        .transaction_with::<_, Error>(move |tx| {
            let mut current = read_retry(tx, &id)?.ok_or(Error::InvalidStoredState)?;
            current
                .receipts
                .entry(stage.as_str().into())
                .or_default()
                .push(StageReceipt {
                    operation: operation_id,
                    certainty,
                    scope: None,
                    ticket: None,
                    not_sent_reason: None,
                    result: None,
                });
            let stage_active = current.initial.stage == stage;
            let mut still_active = stage_active;
            if stage_active {
                if certainty == SafetyRetryCertainty::Unknown {
                    current.initial.stage = Stage::Unknown;
                    current.dispatch.state = DispatchState::Unknown;
                    current.error = Some("Provider request outcome is unknown".into());
                    still_active = false;
                }
                current.initial.updated = unix_seconds();
                let mut agent = read_agent(tx, &current.initial.agent)?;
                save_operation(tx, &mut agent, &current)?;
            } else {
                store_operation(tx, &current)?;
            }
            Ok((current, still_active))
        })
        .await?;
    *operation = current;
    Ok(still_active)
}

async fn persist_operation(
    store: &RuntimeStore,
    operation: &mut SafetyOperation,
) -> Result<(), Error> {
    operation.initial.updated = unix_seconds();
    let pending = operation.clone();
    let updated = store
        .transaction_with::<_, Error>(move |tx| {
            let agent_id = &pending.initial.agent;
            if let Some(current) = read_retry(tx, &pending.initial.id)?
                && current.initial.stage == Stage::Cancelled
            {
                return Ok(current);
            }
            let mut agent = read_agent(tx, agent_id)?;
            save_operation(tx, &mut agent, &pending)?;
            Ok(pending)
        })
        .await?;
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
        crate::agent_management::ManagementError::Expired => Error::Expired,
        crate::agent_management::ManagementError::Unavailable(dependency) => {
            Error::Requires(dependency)
        }
        crate::agent_management::ManagementError::Invalid(_)
        | crate::agent_management::ManagementError::Stored { .. }
        | crate::agent_management::ManagementError::Store(_) => Error::Persistence,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicReceipt<'a> {
    id: &'a str,
    stage: Stage,
    model: &'a str,
    turn_id: &'a str,
    created: f64,
    updated: f64,
    epoch: i64,
    account_key: &'a str,
    error: Option<&'a str>,
    new_thread_id: Option<&'a str>,
    accepted_turn_id: Option<&'a str>,
    request_id: Option<&'a RequestId>,
    rpc_method: Option<Method>,
}

fn operation_public(operation: &SafetyOperation) -> Result<Value, Error> {
    let initial = &operation.initial;
    let error = operation.error.as_deref();
    serde_json::to_value(PublicReceipt {
        id: &initial.id,
        stage: initial.stage,
        model: &initial.model,
        turn_id: &initial.turn_id,
        created: initial.created,
        updated: initial.updated,
        epoch: initial.epoch,
        account_key: &initial.account_key,
        error,
        new_thread_id: operation.new_thread_id.as_deref(),
        accepted_turn_id: operation.accepted_turn_id.as_deref(),
        request_id: operation.request_id.as_ref(),
        rpc_method: operation.rpc_method,
    })
    .map_err(|_| Error::InvalidStoredState)
}

fn store_operation(tx: &Transaction<'_>, operation: &SafetyOperation) -> Result<(), Error> {
    let record = serde_json::to_string(operation).map_err(|_| Error::InvalidStoredState)?;
    tx.execute("INSERT INTO runtime_safety_retries(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![operation.initial.id, record])?;
    Ok(())
}

fn save_operation(
    tx: &Transaction<'_>,
    agent: &mut Value,
    operation: &SafetyOperation,
) -> Result<(), Error> {
    store_operation(tx, operation)?;
    agent["nativeSafetyRetry"] = operation_public(operation)?;
    put_agent(tx, agent)
}

fn read_retry(tx: &Transaction<'_>, id: &str) -> Result<Option<SafetyOperation>, Error> {
    let raw = tx
        .query_row(
            "SELECT record FROM runtime_safety_retries WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    raw.map(|raw| {
        let operation: SafetyOperation =
            serde_json::from_str(&raw).map_err(|_| Error::InvalidStoredState)?;
        let initial = &operation.initial;
        if [
            &initial.id,
            &initial.agent,
            &initial.account_key,
            &initial.attempt_id,
            &initial.connection_id,
            &initial.model,
            &initial.root_request_id,
            &initial.source_thread_id,
            &initial.thread_id,
            &initial.turn_id,
        ]
        .iter()
        .any(|value| value.is_empty())
        {
            return Err(Error::InvalidStoredState);
        }
        Ok(operation)
    })
    .transpose()
}

fn read_agent(tx: &Transaction<'_>, id: &str) -> Result<Value, Error> {
    let record = AgentRepository::get(tx, &AgentId::from(id.to_owned()))?.ok_or(Error::Inactive)?;
    Ok(record.value())
}

fn put_agent(tx: &Transaction<'_>, value: &Value) -> Result<(), Error> {
    let mut record =
        AgentRecord::from_value(value.clone()).map_err(|_| Error::InvalidStoredState)?;
    studio_runtime_store::ExecutionHistoryRepository::record_current(tx, &mut record)?;
    AgentRepository::put(tx, &record)?;
    Ok(())
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

#[cfg(test)]
#[path = "executor_publication_tests.rs"]
mod publication_tests;

#[cfg(test)]
mod initial_receipt_test {
    use super::*;

    #[test]
    fn signature_matches_the_stored_corpus_retry() {
        const STORED: &str = r#"{"accountKey":"acct","admission":{"created":1793154122023,"id":"[\"safety-retry\",[\"agent\"],\"01a18050-8127-776d-af7a-bd891ba05919\",[\"agent:turn\"]]","signature":"{\"accountKey\":\"acct\",\"agent\":\"agent\",\"attemptId\":\"01a18050-8127-776d-af7a-bd891ba05919\",\"connectionId\":\"connection\",\"created\":1793154122.023043,\"epoch\":7,\"id\":\"agent:turn\",\"model\":\"model-fast\",\"rootRequestId\":\"01a18050-8127-776d-af7a-bd891ba05919\",\"sourceThreadId\":\"thread\",\"stage\":\"turns\",\"threadId\":\"thread\",\"turnId\":\"turn\",\"updated\":1793154122.023043}"},"agent":"agent","attemptId":"01a18050-8127-776d-af7a-bd891ba05919","connectionId":"connection","created":1793154122.023043,"dispatch":"ready","epoch":7,"id":"agent:turn","model":"model-fast","receipts":{},"responses":{},"rootRequestId":"01a18050-8127-776d-af7a-bd891ba05919","scope":{"account":"acct","connection":"connection","generation":1},"sourceThreadId":"thread","stage":"turns","threadId":"thread","turnId":"turn","updated":1793154122.023043}"#;
        #[derive(serde::Deserialize)]
        struct Stored {
            admission: AdmittedRequest,
        }
        let operation: SafetyOperation = serde_json::from_str(STORED).unwrap();
        let round_trip = serde_json::to_string(&operation).unwrap();
        let before: Value = serde_json::from_str(STORED).unwrap();
        let after: Value = serde_json::from_str(&round_trip).unwrap();
        assert_eq!(before, after);
        let agent =
            json!({"id":"agent", "epoch":7, "accountKey":"acct", "nativeSafetyRetry":before});
        let held = super::super::decide_restart(&agent, Some(&before), 1793154123.0)
            .unwrap()
            .unwrap();
        let held_operation: SafetyOperation = serde_json::from_str(&held.to_string()).unwrap();
        assert_eq!(held, serde_json::to_value(held_operation).unwrap());
        let initial: SafetyInitialReceipt = serde_json::from_str(STORED).unwrap();
        let stored: Stored = serde_json::from_str(STORED).unwrap();
        assert_eq!(
            serde_json::to_string(&initial).unwrap(),
            stored.admission.signature()
        );
    }
}
