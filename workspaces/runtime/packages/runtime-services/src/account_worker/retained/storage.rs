use studio_runtime_schema::provider_call_contract as contract;
pub(in crate::account_worker) type TransportRequest = contract::TransportRequest<Method>;
pub(in crate::account_worker) type RetainedCall =
    contract::RetainedCall<Method, AdmittedRequest, ConnectionScope>;
type RetainedReply = contract::RetainedReply<Method, AdmittedRequest, ConnectionScope>;
type ThreadProvenance = contract::ThreadProvenance<ConnectionScope>;
use super::*;
use contract::{CommittedAck, RetainedOutcome};
use rusqlite::OptionalExtension;
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, OwnerKind};
use studio_runtime_store::{
    outbox::{Outbox, OutboxRow},
    request_results::{
        Admission, AdmittedRequest, RequestResultError, RequestResults, ResultEnvelope, StoredReply,
    },
};

impl From<&RecoveredRequest> for TransportRequest {
    fn from(request: &RecoveredRequest) -> Self {
        Self {
            id: request.ticket.id.clone(),
            generation: request.ticket.generation,
            method: request.ticket.method,
            operation: request.operation_id.0.clone(),
            thread: request.thread_id.clone(),
            lightweight: request.lightweight,
            deadline: request.deadline_ms,
            unknown: request.unknown,
            remote_id: request.remote_id,
        }
    }
}
pub(in crate::account_worker) trait TransportRequestExt {
    fn recovered(&self) -> RecoveredRequest;
}
impl TransportRequestExt for TransportRequest {
    fn recovered(&self) -> RecoveredRequest {
        RecoveredRequest {
            ticket: RequestTicket {
                id: self.id.clone(),
                generation: self.generation,
                method: self.method,
            },
            operation_id: OperationId(self.operation.clone()),
            thread_id: self.thread.clone(),
            lightweight: self.lightweight,
            deadline_ms: self.deadline,
            unknown: true,
            remote_id: self.remote_id,
        }
    }
}

type RetainedRow = OutboxRow<store::ProviderCall>;
fn outbox_error(error: studio_runtime_store::outbox::OutboxError) -> WorkerError {
    WorkerError::Store(error.into())
}
fn result_error(error: studio_runtime_store::request_results::RequestResultError) -> WorkerError {
    WorkerError::Store(error.into())
}
fn row(tx: &rusqlite::Connection, key: &str) -> Result<Option<RetainedRow>, WorkerError> {
    Outbox::get(tx, key).map_err(outbox_error)
}
fn retained(row: &RetainedRow) -> Result<&RetainedCall, WorkerError> {
    match &row.payload {
        store::ProviderCall::Retained(call) => Ok(call),
        store::ProviderCall::Account(_) => Err(WorkerError::Capacity),
    }
}
fn put(tx: &rusqlite::Transaction<'_>, row: &RetainedRow) -> Result<(), WorkerError> {
    Outbox::put(tx, row).map_err(outbox_error)
}
pub(super) fn hold_unclean_pending(tx: &rusqlite::Transaction<'_>) -> Result<(), WorkerError> {
    let mut statement = tx.prepare("SELECT id FROM runtime_outbox WHERE kind=?1 AND status=?2")?;
    let ids = statement
        .query_map(
            rusqlite::params![OutboxKind::ProviderCall, OutboxStatus::Pending],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in ids {
        let Some(mut row) = row(tx, &id)? else {
            continue;
        };
        row.status = OutboxStatus::Unknown;
        row.next_at = None;
        put(tx, &row)?;
    }
    Ok(())
}
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// SQL for the account that owns a monitor's provider command. A rule monitor
/// stores `accountKey`; a submitted command stores `operation.accountKey`. A tool
/// monitor that is not submitted yet stores neither, so its owner's current
/// account applies, as Python `run_monitor` uses `agent.accountKey` when it
/// starts the command. `?{agent_param}` binds that owner account.
fn monitor_account_sql(agent_param: usize) -> String {
    format!(
        "COALESCE(json_extract(record,'$.accountKey'),json_extract(record,'$.operation.accountKey'),?{agent_param})"
    )
}

/// Whether `agent` runs its provider in a VM. Only such an actor has its own
/// session and journal lane. Every other authorized actor's request is sent
/// and answered on the host session.
pub fn vm_actor(connection: &rusqlite::Connection, agent: &str) -> Result<bool, WorkerError> {
    let record = AgentRepository::get(connection, &AgentId::from(agent))?;
    Ok(record.is_some_and(|record| {
        record.execution_mode == Some(studio_runtime_store::AgentExecutionMode::Vm)
    }))
}

pub(super) fn authorize(tx: &rusqlite::Transaction<'_>, call: &Call) -> Result<bool, WorkerError> {
    let Some(actor) = call.actor.as_ref() else {
        if call.transfer.is_none()
            && call.deleted_actor_session.is_none()
            && super::account_refresh_shape(
                &call.scope,
                &call.root_request_id,
                &call.operation,
                call.method,
                &call.params,
            )
        {
            let Some(token) = &call.admitted_request else {
                return Ok(false);
            };
            let saved = recorded_operation(tx, &call.key(), Some(token.id()))?;
            return Ok(saved.is_some_and(|record| same_replay_identity(record.call(), call)));
        }

        if let Some(transfer) = call.transfer.as_ref() {
            let Some(operation) =
                studio_runtime_store::AccountTransferRepository::get(tx, &transfer.transfer_id)?
            else {
                return Ok(false);
            };
            let member = &operation["members"][&transfer.member];
            return Ok(operation["status"] == "pending"
                && operation["id"] == transfer.transfer_id
                && member.is_object()
                && member["sourceAccountKey"] == transfer.source_account
                && member["sourceThreadId"] == transfer.thread_id
                && call.scope.account == transfer.source_account
                && call.params.get("threadId").and_then(Value::as_str)
                    == Some(transfer.thread_id.as_str())
                && crate::account_transfer::valid_source_preflight_params(
                    call.method.as_str(),
                    &transfer.thread_id,
                    &call.params,
                )
                && call.operation.starts_with(&format!(
                    "transfer:{}:{}:",
                    transfer.transfer_id, transfer.member
                ))
                && matches!(
                    call.method,
                    Method::ThreadQueueList
                        | Method::ThreadBackgroundTerminalsList
                        | Method::ThreadUnsubscribe
                ));
        }
        return Ok(super::valid_actorless_request_shape(
            call.method,
            &call.params,
            &call.operation,
        ));
    };
    let Some(agent_record) = AgentRepository::get(tx, &AgentId::from(actor.agent.as_str()))? else {
        return Ok(false);
    };
    if call.method == Method::ThreadUnsubscribe {
        return crate::agent_management_recovery::authorize_release(
            tx,
            actor,
            &call.scope,
            &call.operation,
            &call.params,
        )
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()));
    }
    let agent_account = agent_record
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let common_identity = agent_account == call.scope.account
        && !studio_runtime_store::truthy(agent_record.extra.get("deletedAt"))
        && agent_record.extra.get("deleted") != Some(&json!(true));
    let actor_identity = common_identity && agent_record.epoch == actor.epoch;
    if call.method == Method::ClaudeShellPolicy {
        let settings = studio_runtime_store::OrchestrationSettingsRepository::shell_policy(tx)?;
        if call.params["settings"] != serde_json::to_value(settings).map_err(StoreError::from)?
            || call.params["eligible"].as_bool().is_none()
            || call.params["critical"].as_bool().is_none()
            || !call.operation.starts_with("shell-policy:")
        {
            return Ok(false);
        }
    }
    if call.method == Method::ClaudeShellPolicy
        && call.params["eligible"] == true
        && !crate::shell_policy::eligible(tx, &agent_record.value())?
    {
        return Ok(false);
    }
    if call.method == Method::CommandExecTerminate
        && let Some(process) = call.params["processId"].as_str()
        && let Some(monitor) = studio_runtime_store::MonitorsRepository::get(tx, process)?
        && !crate::shell_policy::monitor_cleanup_allowed(
            tx,
            &agent_record.value(),
            monitor.value(),
            crate::idle_compaction::wall_now(),
        )?
    {
        return Ok(false);
    }
    let deleted_voice_stop = authorize_deleted_voice_stop(tx, call, actor, &agent_record.extra)?;
    let repair_fork = if call.method == Method::ThreadFork {
        let repair = agent_record
            .extra
            .get("contextRepair")
            .unwrap_or(&Value::Null);
        let source = &repair["source"];
        let snapshot = &repair["snapshot"];
        let fork_params = &repair["forkParams"];
        let path_is_absolute = |value: &Value| {
            value
                .as_str()
                .is_some_and(|path| std::path::Path::new(path).is_absolute())
        };
        repair["id"].as_str().is_some_and(|id| {
            !id.is_empty()
                && repair["agent"] == actor.agent
                && repair["phase"] == "submitted"
                && repair["rpcMethod"] == "thread/fork"
                && repair["forkOperation"] == call.operation
                && source["id"] == actor.agent
                && source["epoch"].as_i64() == Some(actor.epoch)
                && source["accountKey"].as_str() == Some(call.scope.account.as_str())
                && source["threadId"].as_str()
                    == agent_record.extra.get("threadId").and_then(Value::as_str)
                && path_is_absolute(&snapshot["sourcePath"])
                && path_is_absolute(&snapshot["copyPath"])
                && snapshot["sourceSha256"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
                && snapshot["sourceBytes"].as_u64().is_some()
                && snapshot["sourceFileIdentity"].is_object()
                && snapshot["copySha256"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
                && snapshot["copyBytes"].as_u64().is_some()
                && snapshot["eventIds"].is_array()
                && snapshot["importThreadId"].as_str()
                    == call.params.get("threadId").and_then(Value::as_str)
                && snapshot["copyPath"].as_str() == call.params.get("path").and_then(Value::as_str)
                && fork_params == &call.params
                && call.params.get("excludeTurns") == Some(&json!(true))
                && call.params.get("deferGoalContinuation") == Some(&json!(true))
        })
    } else {
        false
    };
    // Stop and delete commit the next epoch before asking the retained provider
    // session to end the previous turn's work: turn/interrupt, then the
    // thread's command processes that outlive an interrupt. Permit only these
    // requests from that exact prior epoch, while the agent can no longer wake.
    // A delete tombstone keeps this authority, so deletion cannot leave the
    // turn and its commands running; it grants nothing else to a deleted actor.
    let stopped_turn_cleanup = agent_account == call.scope.account
        && matches!(
            call.method,
            Method::TurnInterrupt | Method::ThreadBackgroundTerminalsClean
        )
        && actor.epoch.checked_add(1) == Some(agent_record.epoch)
        && agent_record.auto_wake == Some(false);
    let thread_matches = call.params["threadId"].as_str().is_some_and(|thread| {
        agent_record.extra.get("threadId").and_then(Value::as_str) == Some(thread)
    });
    let thread_start = call.method == Method::ThreadStart && call.params.get("threadId").is_none();
    let ephemeral_turn = if call.method == Method::TurnStart {
        match call.params["threadId"].as_str() {
            Some(thread) => thread_started_for_actor(tx, call, thread)?,
            None => false,
        }
    } else {
        false
    };
    let config_operation = call.operation.strip_prefix("monitor-config:");
    let config_monitor_id = config_operation.and_then(|rest| rest.split(':').next());
    let config_read = call.method == Method::ConfigRead
        && config_operation.is_some_and(|rest| rest.split(':').count() == 2)
        && config_monitor_id.is_some_and(|id| !id.is_empty())
        && call.params.get("includeLayers") == Some(&json!(false))
        && call.params.as_object().is_some_and(|params| {
            params
                .keys()
                .all(|key| matches!(key.as_str(), "cwd" | "includeLayers"))
        })
        && call.params.get("cwd").is_some_and(|cwd| {
            cwd.as_str().is_some_and(|cwd| {
                agent_record.extra.get("cwd").and_then(Value::as_str) == Some(cwd)
            })
        })
        && if let Some(monitor_id) = config_monitor_id {
            tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM runtime_monitors WHERE id=?1 AND json_extract(record,'$.agent')=?2 AND json_extract(record,'$.epoch')=?3 AND {}=?4 AND json_extract(record,'$.cwd')=?5 AND json_extract(record,'$.status') IN ('starting','running'))", monitor_account_sql(6)),
                rusqlite::params![monitor_id, actor.agent, actor.epoch, call.scope.account, call.params["cwd"].as_str(), agent_account],
                |row| row.get::<_, i64>(0),
            )? != 0
        } else {
            false
        };
    let monitor_method = matches!(
        call.method,
        Method::CommandExec
            | Method::CommandExecWrite
            | Method::CommandExecResize
            | Method::CommandExecTerminate
    );
    let process_id = call.params.get("processId").and_then(Value::as_str);
    let (monitor_id, liveness_generation) = match call.operation.strip_prefix("liveness:") {
        Some(rest) => rest
            .split_once(':')
            .map_or((process_id, None), |(id, generation)| {
                (Some(id), Some(generation))
            }),
        None => (process_id, None),
    };
    let monitor_matches = if monitor_method {
        if let Some(monitor_id) = monitor_id {
            let monitor = tx
                .query_row(
                    &format!(
                        "SELECT CASE WHEN json_valid(record) THEN json_extract(record,'$.agent') END, \
                            CASE WHEN json_valid(record) THEN json_extract(record,'$.epoch') END, \
                            CASE WHEN json_valid(record) THEN {} END, \
                            CASE WHEN json_valid(record) THEN json_extract(record,'$.cwd') END, \
                            CASE WHEN json_valid(record) THEN json_extract(record,'$.status') END \
                     FROM runtime_monitors WHERE id=?1",
                        monitor_account_sql(2)
                    ),
                    rusqlite::params![monitor_id, agent_account],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()?;
            monitor.is_some_and(|(agent, epoch, account, cwd, status)| {
                agent.as_deref() == Some(actor.agent.as_str())
                    && epoch == Some(actor.epoch)
                    && account.as_deref().unwrap_or("default") == call.scope.account
                    && if call.method == Method::CommandExec {
                        let regular = call.operation == format!("monitor:{monitor_id}")
                            && process_id == Some(monitor_id)
                            && matches!(status.as_deref(), Some("starting" | "running"));
                        let liveness = liveness_generation.is_some_and(|generation| {
                            !generation.is_empty()
                                && process_id == Some(call.operation.as_str())
                                && status.as_deref() == Some("running")
                        });
                        (regular || liveness)
                            && call.params.get("cwd").and_then(Value::as_str) == cwd.as_deref()
                    } else if matches!(
                        call.method,
                        Method::CommandExecWrite | Method::CommandExecResize
                    ) {
                        call.operation
                            .starts_with(&format!("monitor-input:{monitor_id}:"))
                            && call.operation.len() > format!("monitor-input:{monitor_id}:").len()
                            && status.as_deref() == Some("running")
                    } else {
                        call.method == Method::CommandExecTerminate
                            && call.operation == format!("monitor-cancel:{monitor_id}")
                            && status.as_deref() == Some("running")
                    }
            })
        } else {
            false
        }
    } else {
        false
    };
    Ok(
        authorize_retained_monitor_control(tx, call, actor, &agent_record)?
            || authorize_recovered_monitor_cancel(tx, call, actor, &agent_record)?
            || (actor_identity || stopped_turn_cleanup || deleted_voice_stop)
                && (thread_matches
                    || (actor_identity && repair_fork)
                    || thread_start
                    || ephemeral_turn
                    || config_read
                    || monitor_matches
                    || deleted_voice_stop),
    )
}

/// Existing process controls stay on the submitted command's account and
/// provider generation. This grants no command start or general source access.
fn authorize_retained_monitor_control(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
    actor: &RequestActor,
    current: &studio_runtime_store::AgentRecord,
) -> Result<bool, WorkerError> {
    if !matches!(
        call.method,
        Method::CommandExecTerminate | Method::CommandExecWrite | Method::CommandExecResize
    ) {
        return Ok(false);
    }
    let Some(id) = call.params.get("processId").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(record) = studio_runtime_store::MonitorsRepository::get(tx, id)? else {
        return Ok(false);
    };
    let record = record.value();
    let Some(operation) = record
        .get("operation")
        .cloned()
        .and_then(|value| serde_json::from_value::<crate::monitor_policy::Operation>(value).ok())
    else {
        return Ok(false);
    };
    if record["status"] != "running"
        || record["agent"] != actor.agent
        || record["epoch"].as_i64() != Some(actor.epoch)
        || operation.agent != actor.agent
        || operation.epoch != actor.epoch
        || operation.account_key != call.scope.account
        || vm_actor(tx, &actor.agent)?
    {
        return Ok(false);
    }
    let Some(original) = monitor_command_scope(tx, id, &operation)? else {
        return Ok(false);
    };
    if original.account != call.scope.account || original.generation != call.scope.generation {
        return Ok(false);
    }
    if call.method == Method::CommandExecTerminate {
        return Ok(record["cancelRequested"] == true
            && call.operation == format!("monitor-cancel:{id}")
            && call.params == json!({"processId":id}));
    }
    let attempt = &record["inputAttempt"];
    let ticket = attempt["id"].as_str().unwrap_or_default();
    let exact_operation = ["agent", "epoch", "accountKey", "connectionId"]
        .iter()
        .all(|field| attempt.get(*field) == record["operation"].get(*field));
    Ok(current.epoch == actor.epoch
        && current.auto_wake == Some(true)
        && !studio_runtime_store::truthy(current.extra.get("deletedAt"))
        && current.extra.get("deleted") != Some(&json!(true))
        && record["interactive"] == true
        && record["cancelRequested"] != true
        && !ticket.is_empty()
        && attempt["status"] == "submitted"
        && exact_operation
        && attempt["method"] == call.method.as_str()
        && attempt["params"] == call.params
        && call.operation == format!("monitor-input:{id}:{ticket}"))
}

/// No orphan processes after owner deletion across restart: only a latched
/// cancellation of the exact proved monitor handle may cross the inactive-owner boundary.
fn authorize_recovered_monitor_cancel(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
    actor: &RequestActor,
    current: &studio_runtime_store::AgentRecord,
) -> Result<bool, WorkerError> {
    let inactive = current.epoch != actor.epoch
        || current.auto_wake != Some(true)
        || studio_runtime_store::truthy(current.extra.get("deletedAt"))
        || current.extra.get("deleted") == Some(&json!(true));
    if call.method != Method::CommandExecTerminate || !inactive {
        return Ok(false);
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct CancelParams {
        process_id: String,
    }
    let Ok(params) = serde_json::from_value::<CancelParams>(call.params.clone()) else {
        return Ok(false);
    };
    if call.operation != format!("monitor-cancel:{}", params.process_id) {
        return Ok(false);
    }
    let Some(record) = studio_runtime_store::MonitorsRepository::get(tx, &params.process_id)?
    else {
        return Ok(false);
    };
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RecoveredCancelMonitor {
        agent: String,
        epoch: i64,
        operation: crate::monitor_policy::Operation,
        status: crate::monitor_policy::MonitorStatus,
        cancel_requested: bool,
        reattached_connection_id: String,
    }
    let Ok(monitor) = serde_json::from_value::<RecoveredCancelMonitor>(record.value().clone())
    else {
        return Ok(false);
    };
    Ok(monitor.agent == actor.agent
        && monitor.epoch == actor.epoch
        && monitor.operation.agent == actor.agent
        && monitor.operation.epoch == actor.epoch
        && monitor.operation.account_key == call.scope.account
        && monitor.status == crate::monitor_policy::MonitorStatus::Running
        && monitor.cancel_requested
        && monitor.reattached_connection_id == call.scope.connection)
}

fn authorize_deleted_voice_stop(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
    actor: &RequestActor,
    agent_extra: &std::collections::BTreeMap<String, Value>,
) -> Result<bool, WorkerError> {
    let Some(session_id) = call.deleted_actor_session.as_deref() else {
        return Ok(false);
    };
    let Some(thread_id) = call.params.get("threadId").and_then(Value::as_str) else {
        return Ok(false);
    };
    let deleted = studio_runtime_store::truthy(agent_extra.get("deletedAt"))
        || agent_extra.get("deleted") == Some(&json!(true));
    if session_id.is_empty()
        || call.method != Method::ThreadRealtimeStop
        || call.operation != format!("voice:delete-stop:{session_id}")
        || call
            .params
            .as_object()
            .is_none_or(|params| params.len() != 1)
        || !deleted
        || actor.epoch < 0
        || agent_extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != call.scope.account
    {
        return Ok(false);
    }
    let sessions = crate::voice::provider::deleted_voice_sessions(tx, &actor.agent, actor.epoch)
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
    Ok(sessions.iter().any(|session| {
        session.session_id == session_id
            && session.agent_id == actor.agent
            && session.epoch == actor.epoch
            && session.account_key == call.scope.account
            && session.thread_id == thread_id
            && session.connection_id == call.scope.connection
            && session.generation == call.scope.generation
    }))
}

fn thread_started_for_actor(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
    thread: &str,
) -> Result<bool, WorkerError> {
    let Some(actor) = &call.actor else {
        return Ok(false);
    };
    let provenance: Option<String> = tx.query_row(
        "SELECT value FROM runtime_agents, json_each(record,'$.threadProvenance') WHERE runtime_agents.id=?1 AND key=?2",
        rusqlite::params![actor.agent, thread], |row| row.get(0),
    ).optional()?;
    let Some(provenance) = provenance else {
        return Ok(false);
    };
    let provenance: ThreadProvenance =
        serde_json::from_str(&provenance).map_err(StoreError::from)?;
    Ok(provenance.actor == *actor
        && provenance.scope.account == call.scope.account
        && provenance.scope.generation == call.scope.generation)
}

pub(super) async fn authorize_volatile_read(
    s: &RuntimeStore,
    call: &Call,
) -> Result<Result<(), PortError>, WorkerError> {
    let call = call.clone();
    s.read_async("runtime-services/retained_read_authorize", move |tx| {
        Ok(if authorize(tx, &call)? {
            Ok(())
        } else {
            Err(PortError::Identity)
        })
    })
    .await
}
pub(super) async fn replay(
    s: &RuntimeStore,
    call: &Call,
) -> Result<Result<Replay, PortError>, WorkerError> {
    let call = call.clone();
    s.read_async("runtime-services/retained_replay", move |tx| {
        replay_in_transaction(tx, &call)
    })
    .await
}
pub(super) fn replay_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
) -> Result<Result<Replay, PortError>, WorkerError> {
    if authorize(tx, call)? {
        return replay_recorded(tx, call);
    }
    if call.admitted_request.is_none()
        && let Err(error) = validate_root(tx, call)?
    {
        return Ok(Err(error));
    }
    Ok(Err(PortError::Identity))
}
fn validate_root(
    tx: &rusqlite::Connection,
    call: &Call,
) -> Result<Result<(), PortError>, WorkerError> {
    let floor = RequestResults::expired_before_ms(tx).map_err(result_error)?;
    Ok(call.identity().and_then(|identity| {
        identity
            .validate(now_ms(), floor)
            .map_err(|error| identity_error(&error))
    }))
}
fn identity_error(error: &studio_runtime_schema::persistence::RequestIdentityError) -> PortError {
    use studio_runtime_schema::persistence::RequestIdentityError;
    match error {
        RequestIdentityError::Expired { .. } => PortError::Expired,
        RequestIdentityError::InvalidUuidV7(_) | RequestIdentityError::FutureTimestamp { .. } => {
            PortError::Invalid
        }
    }
}
fn call_error(error: RequestResultError) -> Result<PortError, WorkerError> {
    match error {
        RequestResultError::Conflict { .. } => Ok(PortError::Identity),
        RequestResultError::Expired => Ok(PortError::Expired),
        RequestResultError::Identity(error) => Ok(identity_error(&error)),
        error @ (RequestResultError::Sql(_) | RequestResultError::Json(_)) => {
            Err(result_error(error))
        }
    }
}
pub(super) enum Replay {
    Ready,
    Return(SessionReceipt),
    Retry(RequestTicket),
}
fn same_replay_identity(prior: &Call, current: &Call) -> bool {
    prior.signature() == current.signature()
        && result_id(prior)
            .ok()
            .is_some_and(|id| result_id(current).as_ref() == Ok(&id))
}
fn result_id(call: &Call) -> Result<String, PortError> {
    match &call.admitted_request {
        Some(token) if token.signature() == call.signature() => Ok(token.id().to_owned()),
        Some(_) => Err(PortError::Identity),
        None => Ok(call.identity()?.id().to_owned()),
    }
}

fn saved_reply(
    tx: &rusqlite::Connection,
    id: &str,
) -> Result<Option<StoredReply<RetainedReply>>, WorkerError> {
    Ok(RequestResults::read(
        tx,
        id,
        "provider-call",
        &[],
        "",
        &[],
        now_ms(),
    )?)
}
fn replay_final(saved: &RetainedReply) -> Replay {
    let mut receipt = saved
        .call
        .receipt(saved.request.recovered().ticket, saved.result.clone());
    receipt.replayed = true;
    Replay::Return(receipt)
}
pub(super) fn replay_recorded(
    tx: &rusqlite::Transaction<'_>,
    call: &Call,
) -> Result<Result<Replay, PortError>, WorkerError> {
    if let Some(row) = row(tx, &call.key())? {
        let prior = retained(&row)?;
        if !same_replay_identity(&prior.call, call) {
            return Ok(Err(PortError::Identity));
        }
        if row.status == OutboxStatus::Unknown && prior.request.is_none() {
            return Ok(Err(PortError::Unknown));
        }
        if row.status == OutboxStatus::Unknown && proven_not_sent(prior, row.stream_id.as_deref()) {
            let request = prior.request.as_ref().ok_or(WorkerError::Capacity)?;
            let mut receipt = prior
                .call
                .receipt(request.recovered().ticket, RequestResult::Unknown);
            receipt.replayed = true;
            return Ok(Ok(Replay::Return(receipt)));
        }
        let replay = replay_pending(prior, row.stream_id.as_deref());
        if call.admitted_request.is_none()
            && matches!(replay, Replay::Ready | Replay::Retry(_))
            && let Err(error) = validate_root(tx, call)?
        {
            return Ok(Err(error));
        }
        return Ok(Ok(replay));
    }
    let id = match result_id(call) {
        Ok(id) => id,
        Err(error) => return Ok(Err(error)),
    };
    let saved = saved_reply(tx, &id)?;
    let Some(saved) = saved else {
        if call.admitted_request.is_some() {
            return Ok(Err(PortError::Unknown));
        }
        if let Err(error) = validate_root(tx, call)? {
            return Ok(Err(error));
        }
        return Ok(Ok(Replay::Ready));
    };
    if saved.signature != call.signature() || saved.reply.call.signature() != call.signature() {
        return Ok(Err(PortError::Identity));
    }
    Ok(Ok(replay_final(&saved.reply)))
}

fn replay_pending(prior: &RetainedCall, stream: Option<&str>) -> Replay {
    let Some(request) = &prior.request else {
        return Replay::Ready;
    };
    if proven_not_sent(prior, stream) {
        return Replay::Retry(request.recovered().ticket);
    }
    let mut receipt = prior
        .call
        .receipt(request.recovered().ticket, RequestResult::Unknown);
    receipt.replayed = true;
    Replay::Return(receipt)
}

pub(super) fn reserve(
    tx: &rusqlite::Transaction<'_>,
    call: Call,
    admitted: AdmittedRequest,
) -> Result<(), WorkerError> {
    if let Some(row) = row(tx, &call.key())? {
        if !same_replay_identity(&retained(&row)?.call, &call) {
            return Err(WorkerError::PreparationRefused(
                "retained-session/identity-changed".into(),
            ));
        }
        return Ok(());
    }
    if RequestResults::admitted_result(tx, &admitted)
        .map_err(result_error)?
        .is_some()
    {
        return Ok(());
    }
    let actor_id = match &call.actor {
        Some(actor) if vm_actor(tx, &actor.agent)? => Some(actor.agent.as_str()),
        _ => None,
    };
    let stream = actor_id.map_or_else(
        || call.scope.account.clone(),
        |actor| super::super::vm_journal_scope(&call.scope.account, actor),
    );
    let row = OutboxRow {
        id: call.key(),
        kind: OutboxKind::ProviderCall,
        owner_kind: if call.actor.is_some() {
            OwnerKind::Agent
        } else {
            OwnerKind::Account
        },
        owner_id: call
            .actor
            .as_ref()
            .map_or_else(|| call.scope.account.clone(), |actor| actor.agent.clone()),
        request_id: Some(admitted.id().into()),
        signature: call.signature(),
        status: OutboxStatus::Pending,
        account: Some(call.scope.account.clone()),
        stream_id: Some(stream),
        generation: Some(i64::try_from(call.scope.generation).map_err(|_| WorkerError::Capacity)?),
        ordinal: None,
        next_at: None,
        created: admitted.created(),
        payload: store::ProviderCall::Retained(Box::new(RetainedCall {
            call,
            request: None,
            admitted,
            written: false,
            outcome: None,
        })),
    };
    put(tx, &row)
}

pub(super) fn discard_unsent(
    tx: &rusqlite::Transaction<'_>,
    admitted: &AdmittedRequest,
) -> Result<(), WorkerError> {
    let rows = Outbox::for_request_kind::<store::ProviderCall>(
        tx,
        admitted.id(),
        OutboxKind::ProviderCall,
    )
    .map_err(outbox_error)?;
    for row in rows {
        let payload = retained(&row)?;
        if row.signature != admitted.signature()
            || row.status != OutboxStatus::Pending
            || (payload.request.is_some() && !proven_not_sent(payload, row.stream_id.as_deref()))
        {
            return Err(WorkerError::PreparationRefused(
                "retained-session/unknown".into(),
            ));
        }
        Outbox::delete_settled(tx, &row.id, &row.signature).map_err(outbox_error)?;
    }
    Ok(())
}

pub async fn submitted(
    s: &RuntimeStore,
    call: &Call,
    request: &RecoveredRequest,
    retry_ticket: Option<&RequestTicket>,
) -> Result<Result<(), PortError>, WorkerError> {
    let call = call.clone();
    let request = TransportRequest::from(request);
    let retry_ticket = retry_ticket.cloned();
    scheduler::lifecycle_transaction_with(s, move |tx| {
        if !authorize(tx, &call)? {
            return Ok(Err(PortError::Identity));
        }
        let mut row = if let Some(row) = row(tx, &call.key())? {
            row
        } else {
            if call.admitted_request.is_some() {
                return Ok(Err(PortError::Unknown));
            }
            let identity = match call.identity() {
                Ok(identity) => identity,
                Err(error) => return Ok(Err(error)),
            };
            let admitted = match RequestResults::admit(tx, &identity, &call.signature(), now_ms()) {
                Ok(Admission::Fresh(admitted)) => admitted,
                Ok(Admission::Replay(_)) => return Ok(Err(PortError::Unknown)),
                Err(error) => return Ok(Err(call_error(error)?)),
            };
            let mut reserved = call.clone();
            reserved.admitted_request = Some(admitted.clone());
            reserve(tx, reserved, admitted)?;
            row(tx, &call.key())?.ok_or(WorkerError::Capacity)?
        };
        let mut prior = retained(&row)?.clone();
        if !same_replay_identity(&prior.call, &call) {
            return Ok(Err(PortError::Identity));
        }
        if row.status != OutboxStatus::Pending {
            return Ok(Err(PortError::Unknown));
        }
        if request.generation != prior.call.scope.generation
            || row.generation != i64::try_from(request.generation).ok()
        {
            return Ok(Err(PortError::Unknown));
        }

        if let Some(original) = &prior.request {
            if retry_ticket.as_ref() != Some(&original.recovered().ticket)
                || request.recovered().ticket != original.recovered().ticket
                || !proven_not_sent(&prior, row.stream_id.as_deref())
            {
                return Ok(Err(PortError::Unknown));
            }
        } else if retry_ticket.is_some() {
            return Ok(Err(PortError::Unknown));
        }
        prior.request = Some(request);
        prior.outcome = None;
        prior.written = false;
        row.payload = store::ProviderCall::Retained(Box::new(prior));
        row.status = OutboxStatus::Unknown;
        row.next_at = None;
        put(tx, &row)?;
        Ok(Ok(()))
    })
    .await
}

/// Called in the journal-prefix transaction before its cursor evidence retires.
pub fn checkpoint_covered(
    tx: &rusqlite::Transaction<'_>,
    stream: &str,
    generation: u64,
    cursor: u64,
) -> Result<(), WorkerError> {
    let mut query=tx.prepare("SELECT id FROM runtime_outbox WHERE stream_id=?1 AND generation=?2 AND kind=?3 ORDER BY ordinal,id")?;
    let ids = query
        .query_map(
            rusqlite::params![
                stream,
                i64::try_from(generation).map_err(|_| WorkerError::Capacity)?,
                OutboxKind::ProviderCall
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let Some(mut row) = row(tx, &id)? else {
            continue;
        };
        let store::ProviderCall::Retained(payload) = &row.payload else {
            continue;
        };
        if payload
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.cursor <= cursor)
        {
            settle(tx, &mut row, stream, generation)?;
        }
    }
    Ok(())
}
fn settle(
    tx: &rusqlite::Transaction<'_>,
    row: &mut RetainedRow,
    stream: &str,
    generation: u64,
) -> Result<(), WorkerError> {
    let store::ProviderCall::Retained(payload) = &mut row.payload else {
        return Err(WorkerError::Capacity);
    };
    if row.status == OutboxStatus::Unknown && proven_not_sent(payload, row.stream_id.as_deref()) {
        return Ok(());
    }
    let Some(outcome) = &mut payload.outcome else {
        return Ok(());
    };
    let proof = CommittedAck {
        stream_id: stream.into(),
        generation,
        cursor: outcome.cursor,
    };
    if outcome.committed_ack.is_some() && !proof_matches(outcome, Some(stream), generation) {
        row.status = OutboxStatus::Unknown;
        row.next_at = None;
        return put(tx, row);
    }
    let desired = if outcome.result == RequestResult::NotSent {
        OutboxStatus::Pending
    } else {
        OutboxStatus::Unknown
    };
    let changed = outcome.committed_ack.is_none() || row.status != desired || row.next_at.is_some();
    outcome.committed_ack = Some(proof);
    row.status = desired;
    row.next_at = None;
    // Keep the raw reply and its exact committed slot even if an essential owner is malformed.
    // The account's contiguous ACK can now advance independently of that owner.
    if changed {
        put(tx, row)?;
    }
    let payload = retained(row)?;
    let outcome = payload.outcome.as_ref().ok_or(WorkerError::Capacity)?;
    if outcome.result == RequestResult::NotSent {
        return crate::voice::provider::settle_voice_stop(tx, &payload.call, &outcome.result)
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()));
    }
    if !matches!(
        outcome.result,
        RequestResult::Reply(_) | RequestResult::Rejected(_)
    ) {
        return Ok(());
    }
    if !crate::monitor_runtime::settle_provider_call(tx, &payload.call, &outcome.result)
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?
        || !super::super::stopped_turns::settle_stopped_turn(tx, &payload.call, &outcome.result)?
    {
        return Ok(());
    }
    if payload.call.method == Method::ThreadStart
        && let RequestResult::Reply(reply) = &outcome.result
    {
        let Some(thread) = reply["thread"]["id"]
            .as_str()
            .filter(|thread| !thread.is_empty())
        else {
            return Ok(());
        };
        let Some(actor) = &payload.call.actor else {
            return Ok(());
        };
        let mut agent = match AgentRepository::get(tx, &AgentId::from(actor.agent.as_str())) {
            Ok(Some(agent)) => agent,
            Ok(None)
            | Err(
                StoreError::InvalidStoredRecord { .. }
                | StoreError::InvalidStoredObject { .. }
                | StoreError::SchemaValidation(_)
                | StoreError::RecordContract(_)
                | StoreError::Json(_)
                | StoreError::InvalidSyncObject { .. }
                | StoreError::InvalidSyncObjectField { .. },
            ) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let provenance = agent
            .extra
            .entry("threadProvenance".into())
            .or_insert_with(|| json!({}));
        let Some(map) = provenance.as_object_mut() else {
            return Ok(());
        };
        map.insert(
            thread.into(),
            studio_runtime_store::projection_value(&ThreadProvenance {
                scope: payload.call.scope.clone(),
                actor: actor.clone(),
                operation: payload.call.operation.clone(),
            }),
        );
        AgentRepository::put(tx, &agent)?;
    }
    let reply = RetainedReply {
        call: payload.call.clone(),
        request: payload.request.clone().ok_or(WorkerError::Capacity)?,
        written: payload.written,
        result: outcome.result.clone(),
        ack: outcome.committed_ack.clone().ok_or(WorkerError::Capacity)?,
    };
    RequestResults::settle(
        tx,
        &payload.admitted,
        &ResultEnvelope {
            actor: payload.call.actor.as_ref().map(|actor| actor.agent.clone()),
            reply,
            references: vec![],
        },
    )
    .map_err(result_error)?;
    crate::voice::provider::settle_voice_stop(tx, &payload.call, &outcome.result)
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
    Outbox::delete_settled(tx, &row.id, &row.signature).map_err(outbox_error)?;
    Ok(())
}

fn proof_matches(outcome: &RetainedOutcome, stream: Option<&str>, generation: u64) -> bool {
    outcome.committed_ack.as_ref().is_some_and(|ack| {
        stream.is_some_and(|stream| ack.matches(stream, generation, outcome.cursor))
    })
}
fn proven_not_sent(payload: &RetainedCall, stream: Option<&str>) -> bool {
    payload.outcome.as_ref().is_some_and(|outcome| {
        outcome.result == RequestResult::NotSent
            && proof_matches(outcome, stream, payload.call.scope.generation)
    })
}

pub fn record_outcome(
    tx: &rusqlite::Transaction<'_>,
    key: &str,
    written: Option<RecoveredRequest>,
    value: Option<RequestResult>,
    cursor: u64,
) -> Result<Option<(RequestResult, u64)>, WorkerError> {
    let Some(mut row) = row(tx, key)? else {
        return Ok(None);
    };
    let mut payload = retained(&row)?.clone();
    if row.status == OutboxStatus::Unknown
        && proven_not_sent(&payload, row.stream_id.as_deref())
        && payload
            .outcome
            .as_ref()
            .is_some_and(|old| cursor <= old.cursor)
    {
        return Ok(None);
    }
    if payload.outcome.as_ref().is_some_and(|outcome| {
        outcome.committed_ack.is_some()
            && !proof_matches(
                outcome,
                row.stream_id.as_deref(),
                payload.call.scope.generation,
            )
    }) {
        row.status = OutboxStatus::Unknown;
        row.next_at = None;
        put(tx, &row)?;
        return Ok(None);
    }
    if let Some(written) = written {
        payload.request = Some(TransportRequest::from(&written));
        payload.written = true;
    }
    if let Some(value) = value
        && !payload.outcome.as_ref().is_some_and(|old| {
            matches!(
                old.result,
                RequestResult::Reply(_) | RequestResult::Rejected(_)
            )
        })
    {
        if matches!(value, RequestResult::Reply(_) | RequestResult::Rejected(_)) {
            payload.written = true;
        }
        payload.outcome = Some(RetainedOutcome {
            result: value,
            cursor,
            committed_ack: None,
        });
    }
    let accepted = payload
        .outcome
        .as_ref()
        .map(|outcome| (outcome.result.clone(), outcome.cursor));
    row.status = if proven_not_sent(&payload, row.stream_id.as_deref()) {
        OutboxStatus::Pending
    } else {
        OutboxStatus::Unknown
    };
    row.payload = store::ProviderCall::Retained(Box::new(payload));
    put(tx, &row)?;
    let stream = row.stream_id.clone().ok_or(WorkerError::Capacity)?;
    let generation = u64::try_from(row.generation.ok_or(WorkerError::Capacity)?)
        .map_err(|_| WorkerError::Capacity)?;
    let checkpoint = store::checkpoint(tx, &stream, generation)?;
    if accepted
        .as_ref()
        .is_some_and(|(_, cursor)| *cursor <= checkpoint)
    {
        settle(tx, &mut row, &stream, generation)?;
    }
    Ok(accepted)
}

pub(in crate::account_worker) async fn recover(
    s: &RuntimeStore,
    account: &str,
    generation: u64,
) -> Result<Vec<(PendingRequest, RecoveredRequest)>, WorkerError> {
    let account = account.to_owned();
    s.read_async("runtime-services/retained_recover", move |tx| {
        let mut recovered = Vec::new();
        for status in [OutboxStatus::Pending, OutboxStatus::Unknown] {
            for row in Outbox::for_account::<store::ProviderCall>(
                tx,
                &account,
                OutboxKind::ProviderCall,
                status,
                u32::MAX,
            )
            .map_err(outbox_error)?
            {
                let store::ProviderCall::Retained(payload) = row.payload else {
                    continue;
                };
                if payload.call.scope.generation != generation {
                    continue;
                }
                if proven_not_sent(&payload, row.stream_id.as_deref()) {
                    continue;
                }
                let Some(prepared) = payload.request.as_ref() else {
                    continue;
                };
                let mut request = prepared.recovered();
                request.lightweight =
                    !studio_runtime_schema::provider_method_policy::is_recovered_thread_serialized(
                        payload.call.method.as_str(),
                        &payload.call.params,
                    );
                let actor_id = match &payload.call.actor {
                    Some(actor) if vm_actor(tx, &actor.agent)? => Some(actor.agent.clone()),
                    _ => None,
                };
                recovered.push((
                    PendingRequest {
                        call: payload.call,
                        ticket: request.ticket.clone(),
                        actor_id,
                        reply: None,
                        result: payload
                            .outcome
                            .as_ref()
                            .map(|outcome| outcome.result.clone()),
                        result_cursor: payload.outcome.as_ref().map_or(0, |outcome| outcome.cursor),
                        volatile: false,
                    },
                    request,
                ));
            }
        }
        Ok(recovered)
    })
    .await
}

/// Read-only inspection for owners whose authorization outlives their provider wait.
#[cfg(test)]
pub(in crate::account_worker) fn recorded_call(
    tx: &rusqlite::Connection,
    key: &str,
) -> Result<Option<RetainedCall>, WorkerError> {
    row(tx, key)?.map(|row| retained(&row).cloned()).transpose()
}

pub(super) enum RecordedCall {
    Pending(RetainedCall, Option<String>),
    Final(RetainedReply),
}
impl RecordedCall {
    pub(super) fn call(&self) -> &Call {
        match self {
            Self::Pending(value, _) => &value.call,
            Self::Final(value) => &value.call,
        }
    }
    pub(super) fn request(&self) -> Option<&TransportRequest> {
        match self {
            Self::Pending(value, _) => value.request.as_ref(),
            Self::Final(value) => Some(&value.request),
        }
    }
    pub(super) fn written(&self) -> bool {
        match self {
            Self::Pending(value, _) => value.written,
            Self::Final(value) => value.written,
        }
    }
    pub(super) fn replay(&self) -> Replay {
        match self {
            Self::Pending(value, stream) => replay_pending(value, stream.as_deref()),
            Self::Final(value) => replay_final(value),
        }
    }
}
pub(super) fn recorded_operation(
    tx: &rusqlite::Connection,
    key: &str,
    result_id: Option<&str>,
) -> Result<Option<RecordedCall>, WorkerError> {
    if let Some(row) = row(tx, key)? {
        return Ok(Some(RecordedCall::Pending(
            retained(&row)?.clone(),
            row.stream_id,
        )));
    }
    let Some(id) = result_id else { return Ok(None) };
    Ok(saved_reply(tx, id)?.map(|saved| RecordedCall::Final(saved.reply)))
}

#[cfg(test)]
mod replay_identity_tests {
    use super::*;

    #[tokio::test]
    async fn unclean_pending_call_is_not_prepared_again() {
        let root = tempfile::tempdir().expect("root");
        let store = database(root.path()).await;
        store
            .transaction_with(|tx| {
                let mut call = call();
                call.actor = None;
                let identity =
                    RequestIdentity::new("retained-fixture", &[], &call.root_request_id, &[])
                        .expect("identity");
                let owner = match RequestResults::admit(tx, &identity, "fixture", now_ms())
                    .map_err(result_error)?
                {
                    Admission::Fresh(owner) => owner,
                    Admission::Replay(_) => return Err(WorkerError::Capacity),
                };
                call.admitted_request = Some(super::super::reserve_provider_call(
                    tx,
                    &owner,
                    call.clone(),
                )?);
                assert!(matches!(replay_recorded(tx, &call)?, Ok(Replay::Ready)));
                hold_unclean_pending(tx)?;
                assert!(matches!(
                    replay_recorded(tx, &call)?,
                    Err(PortError::Unknown)
                ));
                Ok::<_, WorkerError>(())
            })
            .await
            .expect("recovery");
    }

    #[tokio::test]
    async fn unclean_retry_does_not_reuse_old_not_sent_proof() {
        let root = tempfile::tempdir().expect("root");
        let store = database(root.path()).await;
        store.transaction_with(|tx| {
            let mut call = call();
            call.actor = None;
            let key = call.key();
            let request = RecoveredRequest {
                ticket: RequestTicket { generation: 4, id: RequestId::String(key.clone()), method: call.method },
                operation_id: OperationId(key.clone()), thread_id: None, lightweight: true,
                deadline_ms: 1000, unknown: true, remote_id: None,
            };
            let outcome = RetainedOutcome {
                result: RequestResult::NotSent, cursor: 1,
                committed_ack: Some(CommittedAck { stream_id: "default".into(), generation: 4, cursor: 1 }),
            };
            let call = seed(tx, call, &request, false, Some(outcome))?;
            let mut saved = row(tx, &key)?.ok_or(WorkerError::Capacity)?;
            saved.status = OutboxStatus::Pending;
            put(tx, &saved)?;
            assert!(matches!(replay_recorded(tx, &call)?, Ok(Replay::Retry(_))));
            hold_unclean_pending(tx)?;
            assert!(matches!(replay_recorded(tx, &call)?, Ok(Replay::Return(receipt)) if receipt.result == RequestResult::Unknown));
            assert!(retained(&row(tx, &key)?.ok_or(WorkerError::Capacity)?)?.outcome.is_some());
            record_outcome(tx, &key, None, Some(RequestResult::NotSent), 1)?;
            assert_eq!(row(tx, &key)?.ok_or(WorkerError::Capacity)?.status, OutboxStatus::Unknown);
            let mut saved = row(tx, &key)?.ok_or(WorkerError::Capacity)?;
            settle(tx, &mut saved, "default", 4)?;
            assert_eq!(row(tx, &key)?.ok_or(WorkerError::Capacity)?.status, OutboxStatus::Unknown);
            Ok::<_, WorkerError>(())
        }).await.expect("recovery");
    }

    #[tokio::test]
    async fn migrated_submitted_without_request_stays_unknown_without_preparation() {
        use serde::Serialize;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Agent<'a> {
            id: &'a str,
            root_id: &'a str,
            epoch: i64,
            status: &'a str,
            auto_wake: bool,
            account_key: &'a str,
            provider: &'a str,
        }
        #[derive(Serialize)]
        struct Monitor<'a> {
            id: &'a str,
            agent: &'a str,
            epoch: i64,
            status: &'a str,
            cwd: &'a std::path::Path,
            operation: crate::monitor_policy::Operation,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Params<'a> {
            process_id: &'a str,
            cwd: &'a std::path::Path,
            command: [&'a str; 1],
        }
        #[derive(Serialize)]
        struct Submitted {
            call: Call,
        }
        let root = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("bootstrap");
        let request_root = RequestIdentity::issue_root();
        let operation = crate::monitor_policy::Operation {
            agent: "agent-1".into(),
            epoch: 7,
            account_key: "default".into(),
            connection_id: "old-connection".into(),
            evidence: crate::monitor_policy::OperationEvidence {
                thread_id: None,
                root_request_id: Some(request_root.clone()),
                provider_generation: Some(4),
                provider_request: None,
            },
        };
        let mut call = call();
        call.root_request_id = request_root;
        call.operation = "monitor:watch".into();
        call.method = Method::CommandExec;
        call.params = studio_runtime_store::projection_value(&Params {
            process_id: "watch",
            cwd: root.path(),
            command: ["true"],
        });
        let agent = studio_runtime_store::projection_value(&Agent {
            id: "agent-1",
            root_id: "agent-1",
            epoch: 7,
            status: "waiting",
            auto_wake: true,
            account_key: "default",
            provider: "codex",
        });
        let watch = studio_runtime_store::projection_value(&Monitor {
            id: "watch",
            agent: "agent-1",
            epoch: 7,
            status: "running",
            cwd: root.path(),
            operation,
        });
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(agent)?)?;
                studio_runtime_store::Table::new(tx)
                    .put(&studio_runtime_store::MonitorRecord::from_value(watch)?)
            })
            .await
            .expect("owners");
        drop(store);
        let db = rusqlite::Connection::open(root.path().join("canvas.sqlite3")).expect("database");
        db.execute_batch("DELETE FROM runtime_payload_migrations WHERE name LIKE 'receipts:%'; CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL)").expect("legacy schema");
        let key = call.key();
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            rusqlite::params![
                format!("account-worker:{key}:submitted"),
                serde_json::to_string(&Submitted { call }).expect("submitted")
            ],
        )
        .expect("legacy submitted");
        drop(db);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("migrate held submission");
        store
            .read_async("test/held-replay", move |tx| {
                let row = row(tx, &key)?.expect("held provider call");
                assert_eq!(row.status, OutboxStatus::Unknown);
                let prior = retained(&row)?;
                assert!(prior.request.is_none(), "no request was prepared");
                assert!(
                    authorize(tx, &prior.call)?,
                    "the original monitor scope is authorized"
                );
                assert!(
                    matches!(
                        replay_in_transaction(tx, &prior.call)?,
                        Err(PortError::Unknown)
                    ),
                    "caller must stop before preparation or send"
                );
                let pending = Outbox::pending::<store::ProviderCall>(
                    tx,
                    OutboxKind::ProviderCall,
                    now_ms(),
                    100,
                )
                .map_err(outbox_error)?;
                assert!(pending.is_empty(), "no provider call can be sent");
                Ok::<_, WorkerError>(())
            })
            .await
            .expect("held replay");
    }

    async fn database(path: &std::path::Path) -> RuntimeStore {
        let store = RuntimeStore::open(Some(path)).await.expect("store");
        store
            .transaction(|tx| {
                studio_runtime_store::receipt_schema::create(tx)?;
                Ok(())
            })
            .await
            .expect("target schema");
        store
    }
    fn seed(
        tx: &rusqlite::Transaction<'_>,
        mut call: Call,
        request: &RecoveredRequest,
        written: bool,
        outcome: Option<RetainedOutcome>,
    ) -> Result<Call, WorkerError> {
        let owner_id = RequestIdentity::new("retained-fixture", &[], &call.root_request_id, &[])
            .expect("root");
        let owner = match RequestResults::admit(tx, &owner_id, "fixture", now_ms())
            .map_err(result_error)?
        {
            Admission::Fresh(owner) => owner,
            Admission::Replay(_) => return Err(WorkerError::Capacity),
        };
        let admitted = super::super::reserve_provider_call(tx, &owner, call.clone())?;
        call.admitted_request = Some(admitted.clone());
        let mut row = row(tx, &call.key())?.ok_or(WorkerError::Capacity)?;
        row.status = OutboxStatus::Unknown;
        row.payload = store::ProviderCall::Retained(Box::new(RetainedCall {
            call: call.clone(),
            request: Some(TransportRequest::from(request)),
            admitted,
            written,
            outcome,
        }));
        put(tx, &row)?;
        Ok(call)
    }

    fn call() -> Call {
        Call {
            root_request_id: RequestIdentity::issue_root(),
            admitted_request: None,
            scope: ConnectionScope {
                account: "default".into(),
                connection: "old-connection".into(),
                generation: 4,
            },
            operation: "operation-1".into(),
            actor: Some(RequestActor {
                agent: "agent-1".into(),
                epoch: 7,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: Method::ThreadRead,
            params: json!({"threadId":"thread-1","includeTurns":false}),
        }
    }

    #[test]
    fn stored_thread_provenance_is_scoped_to_actor_and_generation() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)")
            .expect("table");
        let tx = db.transaction().expect("transaction");
        let call = call();
        let provenance = ThreadProvenance {
            scope: call.scope.clone(),
            actor: call.actor.clone().expect("actor"),
            operation: call.operation.clone(),
        };
        let raw = serde_json::to_string(&provenance).expect("provenance");
        tx.execute(
            "INSERT INTO runtime_agents VALUES('agent-1',?1)",
            [format!(r#"{{"threadProvenance":{{"thread-1":{raw}}}}}"#)],
        )
        .expect("agent");
        assert!(thread_started_for_actor(&tx, &call, "thread-1").expect("matching provenance"));
        assert!(!thread_started_for_actor(&tx, &call, "missing").expect("missing provenance"));
        let mut other = call;
        other.scope.generation += 1;
        assert!(!thread_started_for_actor(&tx, &other, "thread-1").expect("different generation"));
    }

    #[tokio::test]
    async fn shell_policy_handoff_rechecks_activity_and_stored_settings() {
        let root = tempfile::tempdir().expect("root");
        let store = database(root.path()).await;
        store.transaction(|tx| {
            let mut request = call();
            request.method = Method::ClaudeShellPolicy;
            request.operation = "shell-policy:agent-1:tick".into();
            request.params = json!({"threadId":"thread-1","settings":studio_runtime_schema::shell_policy::Settings::default(),"eligible":true,"critical":true});
            let mut agent = studio_runtime_store::AgentRecord::from_value(json!({"provider":"codex","id":"agent-1","rootId":"agent-1","epoch":7,"threadId":"thread-1","status":"idle","autoWake":true}))?;
            AgentRepository::put(tx,&agent)?;
            assert!(authorize(tx,&request).expect("eligible handoff"));
            agent.in_flight = Some(true); AgentRepository::put(tx,&agent)?;
            assert!(!authorize(tx,&request).expect("active handoff refused"));
            agent.in_flight = Some(false); AgentRepository::put(tx,&agent)?;
            request.params["settings"]["pressureCleanup"] = json!(false);
            assert!(!authorize(tx,&request).expect("settings mismatch refused"));
            Ok::<_,StoreError>(())
        }).await.expect("policy boundaries");
    }

    #[tokio::test]
    async fn unknown_serialized_mutations_recover_all_owners_without_sending() {
        use studio_runtime_providers::{Action, Frame, ResponseBody, SessionState};
        let root = tempfile::tempdir().expect("root");
        let database = database(root.path()).await;
        database
            .transaction_with(|tx| {
                for (index, (method, params)) in [
                    (Method::ClaudeSettings, json!({"threadId":"thread-1"})),
                    (Method::ClaudeState, json!({"threadId":"thread-1"})),
                    (
                        Method::ThreadRead,
                        json!({"threadId":"thread-1","includeTurns":true}),
                    ),
                    (Method::ThreadRead, Value::Null),
                    (Method::ThreadRead, json!("unreadable")),
                    (Method::ThreadRead, json!([])),
                ]
                .into_iter()
                .enumerate()
                {
                    let mut call = call();
                    call.actor = None;
                    call.operation = format!("unknown:{index}");
                    call.method = method;
                    call.params = params;
                    let key = call.key();
                    let request = RecoveredRequest {
                        ticket: RequestTicket {
                            generation: 4,
                            id: studio_runtime_providers::RequestId::String(key.clone()),
                            method,
                        },
                        operation_id: studio_runtime_process::OperationId(key.clone()),
                        thread_id: Some("thread-1".into()),
                        lightweight: true,
                        deadline_ms: 1000,
                        unknown: true,
                        remote_id: None,
                    };
                    seed(tx, call, &request, index == 1, None)?;
                }
                Ok::<_, WorkerError>(())
            })
            .await
            .expect("persist typed unknown calls");
        let restored = recover(&database, "default", 4)
            .await
            .expect("recover unknown calls");
        assert_eq!(restored.len(), 6);
        assert_eq!(
            restored
                .iter()
                .map(|(_, request)| request.lightweight)
                .collect::<Vec<_>>(),
            vec![false; 6],
            "null, string and array parameters must retain ownership"
        );
        let restored_count = restored.len();
        let (mut session, initialize) = SessionState::new(
            4,
            studio_runtime_providers::ProviderKind::Claude.wire(),
            0,
            1000,
            studio_runtime_providers::initialize_params(
                studio_runtime_providers::ClientName::AccountSession,
            ),
        )
        .expect("session");
        for (_, request) in &restored {
            session
                .restore_request(
                    &request.ticket,
                    request.thread_id.clone(),
                    request.deadline_ms,
                    request.unknown,
                    request.lightweight,
                )
                .expect("restore owner");
        }
        let [Action::Write(Frame::Request(initialize))] = initialize.as_slice() else {
            panic!("only initialization may write");
        };
        session
            .receive(Frame::Response {
                id: initialize.id.clone(),
                body: ResponseBody::Result(json!({})),
                fields: Default::default(),
            })
            .expect("initialize");
        session
            .mark_initialized_notification_written()
            .expect("ready");
        let (fresh, actions) = session
            .submit(
                Method::ThreadCompactStart,
                json!({"threadId":"thread-1"}),
                0,
                1000,
                Some("thread-1".into()),
                false,
            )
            .expect("fresh work");
        assert!(
            actions.is_empty(),
            "stored lightweight flags must not bypass restored ownership"
        );
        for (index, (_, request)) in restored.into_iter().rev().enumerate() {
            let actions = session
                .receive(Frame::Response {
                    id: request.ticket.id,
                    body: ResponseBody::Result(json!({})),
                    fields: Default::default(),
                })
                .expect("settle owner");
            let writes = actions
                .iter()
                .filter_map(|action| match action {
                    Action::Write(Frame::Request(frame)) => Some(frame),
                    Action::Write(_)
                    | Action::Resolve { .. }
                    | Action::Dispatch(_)
                    | Action::GenerationClosed { .. }
                    | Action::ThreadReleased { .. } => None,
                })
                .collect::<Vec<_>>();
            if index + 1 < restored_count {
                assert!(writes.is_empty());
            } else {
                assert_eq!(writes.len(), 1);
                assert_eq!(
                    writes[0].id, fresh.id,
                    "only fresh work writes; unknown identities never replay"
                );
            }
        }
    }

    #[tokio::test]
    async fn stopped_and_deleted_prior_epoch_may_only_end_its_turn_work() {
        let root = tempfile::tempdir().expect("root");
        let store = database(root.path()).await;
        store
            .transaction(|tx| {
                for record in [
                    json!({"id":"stopped","rootId":"stopped","epoch":8,"status":"paused","autoWake":false,"threadId":"thread-1","provider":studio_runtime_schema::ProviderKind::Codex}),
                    json!({"id":"deleted","rootId":"deleted","epoch":8,"deletedAt":1,"status":"paused","autoWake":false,"threadId":"thread-1","provider":studio_runtime_schema::ProviderKind::Codex}),
                    json!({"id":"woken","rootId":"woken","epoch":8,"status":"queued","autoWake":true,"threadId":"thread-1","provider":studio_runtime_schema::ProviderKind::Codex}),
                ] {
                    AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(record)?,
                    )?;
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed stopped, deleted and woken owners");
        store
            .read_async("test/stopped-turn-authority", |tx| {
                let work = [
                    (
                        "turn/interrupt",
                        json!({"threadId":"thread-1","turnId":"turn-1"}),
                    ),
                    (
                        "thread/backgroundTerminals/clean",
                        json!({"threadId":"thread-1"}),
                    ),
                ];
                for agent in ["stopped", "deleted"] {
                    for (method, params) in &work {
                        let mut exact = call();
                        exact.actor = Some(RequestActor {
                            agent: agent.into(),
                            epoch: 7,
                        });
                        exact.method = Method::from_wire(method).expect("method");
                        exact.params = params.clone();
                        assert!(authorize(tx, &exact)?, "{agent} ends {method}");
                        let mut invalid = exact.clone();
                        invalid.params["threadId"] = json!("thread-2");
                        assert!(!authorize(tx, &invalid)?, "{agent} {method} other thread");
                        let mut invalid = exact.clone();
                        invalid.actor.as_mut().expect("actor").epoch = 6;
                        assert!(!authorize(tx, &invalid)?, "{agent} {method} older epoch");
                        let mut invalid = exact.clone();
                        invalid.scope.account = "other".into();
                        assert!(!authorize(tx, &invalid)?, "{agent} {method} other account");
                    }
                    let mut read = call();
                    read.actor = Some(RequestActor {
                        agent: agent.into(),
                        epoch: 7,
                    });
                    assert!(!authorize(tx, &read)?, "{agent} gets no other request");
                }
                let mut deleted_current = call();
                deleted_current.actor = Some(RequestActor {
                    agent: "deleted".into(),
                    epoch: 8,
                });
                deleted_current.method = Method::TurnInterrupt;
                deleted_current.params = json!({"threadId":"thread-1","turnId":"turn-1"});
                assert!(!authorize(tx, &deleted_current)?);
                for (method, params) in &work {
                    let mut woken = call();
                    woken.actor = Some(RequestActor {
                        agent: "woken".into(),
                        epoch: 7,
                    });
                    woken.method = Method::from_wire(method).expect("method");
                    woken.params = params.clone();
                    assert!(
                        !authorize(tx, &woken)?,
                        "a woken agent keeps no stop authority"
                    );
                }
                Ok::<_, WorkerError>(())
            })
            .await
            .expect("stop authority witnesses");
    }

    #[tokio::test]
    async fn monitor_completion_recovered_cancel_authority_is_exact_and_cannot_resume() {
        let root = tempfile::tempdir().expect("root");
        let store = database(root.path()).await;
        store.transaction(|tx| {
            AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(json!({"id":"agent-1","rootId":"agent-1","epoch":9,"deletedAt":1,"status":"interrupted","autoWake":false,"provider":studio_runtime_schema::ProviderKind::Codex}))?)?;
            studio_runtime_store::Table::new(tx).put(&studio_runtime_store::MonitorRecord::from_value(json!({"id":"watch","agent":"agent-1","epoch":7,"status":"running","cancelRequested":true,"reattachedConnectionId":"new-connection","operation":{"agent":"agent-1","epoch":7,"accountKey":"default","connectionId":"old"}}))?)?;
            Ok::<_,studio_runtime_store::StoreError>(())
        }).await.expect("seed deleted owner and proved handle");
        store
            .read_async("test/cancel-authority", |tx| {
                let mut exact = call();
                exact.scope.connection = "new-connection".into();
                exact.operation = "monitor-cancel:watch".into();
                exact.method = Method::CommandExecTerminate;
                exact.params = json!({"processId":"watch"});
                assert!(authorize(tx, &exact)?);
                for method in [
                    "command/exec",
                    "command/exec/write",
                    "command/exec/resize",
                    "thread/read",
                ] {
                    let mut invalid = exact.clone();
                    invalid.method = Method::from_wire(method).expect("method");
                    assert!(
                        !authorize(tx, &invalid)?,
                        "deleted actor must not resume {method}"
                    );
                }
                let mut invalid = exact.clone();
                invalid.operation = "monitor-cancel:other".into();
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.params = json!({"processId":"other"});
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.actor.as_mut().expect("actor").epoch = 9;
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.scope.connection = "unproved".into();
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact.clone();
                invalid.scope.account = "other".into();
                assert!(!authorize(tx, &invalid)?);
                let mut invalid = exact;
                invalid.params = json!({"processId":"watch","command":["true"]});
                assert!(!authorize(tx, &invalid)?);
                Ok::<_, WorkerError>(())
            })
            .await
            .expect("authority witnesses");
    }

    #[test]
    fn replay_identity_normalizes_only_connection_generation() {
        let prior = call();
        let mut reconnected = prior.clone();
        reconnected.scope.connection = "new-connection".into();
        reconnected.scope.generation = 5;
        assert!(same_replay_identity(&prior, &reconnected));

        let mut changed = prior.clone();
        changed.scope.account = "other-account".into();
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.operation.push_str(":other");
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.actor = Some(RequestActor {
            agent: "agent-1".into(),
            epoch: 8,
        });
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.transfer = Some(TransferAuthorization {
            transfer_id: "transfer-1".into(),
            member: "member-1".into(),
            source_account: "default".into(),
            thread_id: "thread-1".into(),
        });
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.deleted_actor_session = Some("voice-session-1".into());
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior.clone();
        changed.method = Method::ThreadTurnsList;
        assert!(!same_replay_identity(&prior, &changed));
        let mut changed = prior;
        changed.params["threadId"] = json!("thread-2");
        assert!(!same_replay_identity(&call(), &changed));
    }

    #[tokio::test]
    async fn transferred_monitor_controls_require_the_original_retained_generation() {
        thread_local! { static SUBMITTED_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
        let root = tempfile::tempdir().expect("root");
        let store = database(root.path()).await;
        let cwd = root.path().to_owned();
        store.transaction_with(move |tx| {
            AgentRepository::put(tx,&studio_runtime_store::AgentRecord::from_value(json!({"provider":"codex","id":"agent-1","rootId":"agent-1","epoch":7,"accountKey":"target","status":"queued","autoWake":true})).expect("actor"))?;
            let original = Call {operation:"monitor:watch".into(),method:Method::CommandExec,params:json!({"processId":"watch","command":["true"],"cwd":cwd}),..call()};
            let key=original.key();
            let request=RecoveredRequest{ticket:RequestTicket{id:RequestId::String(key.clone()),generation:4,method:Method::CommandExec},operation_id:OperationId(key),thread_id:None,lightweight:true,deadline_ms:60000,unknown:true,remote_id:None};
            seed(tx,original,&request,false,None)?;
            studio_runtime_store::Table::new(tx).put(&studio_runtime_store::MonitorRecord::from_value(json!({"id":"watch","agent":"agent-1","epoch":7,"status":"running","interactive":true,"cancelRequested":true,"reattachedConnectionId":"reconnected-source","operation":{"agent":"agent-1","epoch":7,"accountKey":"default","connectionId":"old-connection"}})).expect("monitor"))?;
            Ok::<_,WorkerError>(())
        }).await.expect("original command and transferred owner");
        store.transaction_with(|tx| {
            let mut exact = call();
            exact.scope.connection="reconnected-source".into();
            exact.operation="monitor-cancel:watch".into();
            exact.method=Method::CommandExecTerminate;
            exact.params=json!({"processId":"watch"});
            assert!(!authorize(tx,&exact)?,"a submitted-only intent does not prove a running native process");
            let original_key=Call::operation_key("default","monitor:watch");
            let mut saved=row(tx,&original_key)?.expect("original submission");
            let store::ProviderCall::Retained(payload)=&mut saved.payload else {return Err(WorkerError::Capacity)};
            payload.written=true;put(tx,&saved)?;
            SUBMITTED_READS.with(|count| count.set(0));
            tx.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, Some(|event| {
                if let rusqlite::trace::TraceEvent::Stmt(statement, _) = event
                    && statement.sql().starts_with("SELECT id,kind,owner_kind")
                    && statement.expanded_sql().is_some_and(|sql| sql.contains("retained:default:"))
                {
                    SUBMITTED_READS.with(|count| count.set(count.get() + 1));
                }
            }));
            let operation = serde_json::from_value(json!({"agent":"agent-1","epoch":7,"accountKey":"default","connectionId":"old-connection"})).expect("operation");
            assert!(super::super::monitor_command_scope(tx,"watch",&operation)?.is_some());
            tx.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, None);
            assert_eq!(SUBMITTED_READS.with(std::cell::Cell::get), 1);
            assert!(authorize(tx,&exact)?,"only the original written process is cancelled after transfer");
            for field in ["account","generation","epoch","process","operation","method","shape"] {
                let mut invalid=exact.clone();
                match field {
                    "account"=>invalid.scope.account="target".into(),
                    "generation"=>invalid.scope.generation+=1,
                    "epoch"=>invalid.actor.as_mut().expect("request actor").epoch+=1,
                    "process"=>invalid.params=json!({"processId":"other"}),
                    "operation"=>invalid.operation="monitor-cancel:other".into(),
                    "method"=>invalid.method=Method::CommandExec,
                    "shape"=>invalid.params=json!({"processId":"watch","command":["true"]}),
                    _=>unreachable!(),
                }
                assert!(!authorize(tx,&invalid)?,"{field}");
            }
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.executionMode','vm') WHERE id='agent-1'",[])?;
            assert!(!authorize(tx,&exact)?,"a host generation does not prove a guest process identity");
            tx.execute("UPDATE runtime_agents SET record=json_remove(record,'$.executionMode') WHERE id='agent-1'",[])?;
            tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.epoch',8) WHERE id='watch'",[])?;
            assert!(!crate::account_worker::retained::monitor_survives_account_transfer(tx,"watch","agent-1")?,"the monitor row epoch must match its actor and submitted operation");
            Ok::<_,WorkerError>(())
        }).await.expect("exact source authority");
    }

    #[tokio::test]
    async fn concurrent_submissions_with_different_call_identity_are_refused_without_replacing_intent()
     {
        let state = tempfile::tempdir().expect("retained receipt test state directory");
        let store = database(state.path()).await;
        let first = Call {
            root_request_id: RequestIdentity::issue_root(),
            admitted_request: None,
            scope: ConnectionScope {
                account: "default".into(),
                connection: "connection-1".into(),
                generation: 1,
            },
            operation: "import:fixture:read".into(),
            actor: None,
            transfer: None,
            deleted_actor_session: None,
            method: Method::ThreadRead,
            params: json!({"threadId":"source-thread","includeTurns":false}),
        };
        let conflicting = Call {
            params: json!({"threadId":"other-source-thread","includeTurns":false}),
            ..first.clone()
        };
        let recovered = |call: &Call| {
            let id = call.key();
            RecoveredRequest {
                ticket: RequestTicket {
                    id: studio_runtime_providers::RequestId::String(id.clone()),
                    generation: call.scope.generation,
                    method: Method::ThreadRead,
                },
                operation_id: studio_runtime_process::OperationId(id),
                thread_id: call.params["threadId"].as_str().map(str::to_owned),
                lightweight: true,
                deadline_ms: 60_000,
                unknown: true,
                remote_id: None,
            }
        };

        // Model the valid interleaving: both reads finish before either
        // transaction commits the submitted intent.
        assert!(matches!(
            replay(&store, &first)
                .await
                .expect("read first replay state")
                .expect("first replay should miss"),
            Replay::Ready
        ));
        assert!(matches!(
            replay(&store, &conflicting)
                .await
                .expect("read conflicting replay state")
                .expect("conflicting replay should miss before admission"),
            Replay::Ready
        ));
        assert_eq!(
            submitted(&store, &first, &recovered(&first), None)
                .await
                .expect("persist first submitted intent"),
            Ok(())
        );
        assert_eq!(
            submitted(&store, &conflicting, &recovered(&conflicting), None)
                .await
                .expect("inspect conflicting submitted intent"),
            Err(PortError::Identity)
        );

        // The first durable identity remains replayable; the losing identity
        // cannot replace it or advance into a provider submission.
        assert!(matches!(
            replay(&store, &first)
                .await
                .expect("read first durable intent")
                .expect("first durable intent should be replayable"),
            Replay::Return(_)
        ));
        assert!(matches!(
            replay(&store, &conflicting)
                .await
                .expect("read changed identity outcome"),
            Err(PortError::Identity)
        ));

        let exact = Call {
            operation: "import:fixture:exact-read".into(),
            ..first.clone()
        };
        let exact_retry = exact.clone();
        assert!(matches!(
            replay(&store, &exact)
                .await
                .expect("read exact operation replay state")
                .expect("first exact operation replay should miss"),
            Replay::Ready
        ));
        assert!(matches!(
            replay(&store, &exact_retry)
                .await
                .expect("read exact retry replay state")
                .expect("concurrent exact retry should miss before admission"),
            Replay::Ready
        ));
        assert_eq!(
            submitted(&store, &exact, &recovered(&exact), None)
                .await
                .expect("persist exact submitted intent"),
            Ok(())
        );
        assert_eq!(
            submitted(&store, &exact_retry, &recovered(&exact_retry), None)
                .await
                .expect("inspect exact concurrent submitted intent"),
            Err(PortError::Unknown)
        );
        assert!(matches!(
            replay(&store, &exact_retry)
                .await
                .expect("read durable exact retry outcome")
                .expect("durable exact retry should replay"),
            Replay::Return(SessionReceipt {
                result: RequestResult::Unknown,
                replayed: true,
                ..
            })
        ));
    }
}
