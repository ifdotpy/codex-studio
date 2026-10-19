//! Managed-tool dispatch policy after callback admission commits.
//!
//! Concrete sibling runtimes enter only through these ports. This module owns
//! operation routing, missing-capability refusals, monitor ownership checks,
//! and durable input receipt interpretation.

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

use crate::managed_child_create::ManagedCreationEnvironmentError as CreationError;
use crate::managed_host_tools::{
    Actor, DeliveryCertainty, DispatchedOperation, Error, OperationResult, WakeSignal,
};

type ModelPrice = (f64, f64);
type WorkerModelPrices = Option<(Option<ModelPrice>, Option<ModelPrice>)>;

/// What a worker creation asks of account selection.
pub struct WorkerAccountRequest<'a> {
    pub parent_account_key: &'a str,
    pub requested_account_key: Option<&'a str>,
    pub default_account_key: Option<&'a str>,
    pub model: &'a str,
    pub excluded_account_keys: &'a [String],
    pub project_account_keys: Option<&'a [String]>,
    /// Working (not paused) agents per account key, including workers chosen earlier
    /// in the same batch. Implicit selection picks the fewest (unit: agents).
    pub active_agents: &'a std::collections::HashMap<String, i64>,
}

pub struct WorkerAccountSelection<'a> {
    pub accounts: &'a dyn crate::provider_callbacks::ManagedCreationAccountRead,
    pub request: WorkerAccountRequest<'a>,
    pub model_available: &'a mut dyn FnMut(
        &str,
        &crate::provider_callbacks::ManagedAccountIdentity,
        &str,
    ) -> Result<bool, CreationError>,
}

pub fn select_worker_account(
    selection: WorkerAccountSelection<'_>,
) -> Result<(String, crate::provider_callbacks::ManagedAccountIdentity), CreationError> {
    let WorkerAccountSelection {
        accounts,
        request:
            WorkerAccountRequest {
                parent_account_key,
                requested_account_key,
                default_account_key,
                model,
                excluded_account_keys,
                project_account_keys,
                active_agents,
            },
        model_available,
    } = selection;
    let selected_default = default_account_key.filter(|key| !key.is_empty());
    if selected_default.is_some()
        && requested_account_key.is_some()
        && requested_account_key != selected_default
    {
        return Err(CreationError::Invalid(
            "Use the subagent account selected in chat settings",
        ));
    }
    if let Some(account_key) = requested_account_key.or(selected_default) {
        validate_selected_account(account_key, excluded_account_keys, project_account_keys)?;
        let identity = accounts
            .resolve_identity(account_key, parent_account_key)
            .map_err(account_read_error)?;
        if !model_available(account_key, &identity, model)? {
            return Err(CreationError::Invalid(
                "This model is not available for the selected worker accounts",
            ));
        }
        return Ok((account_key.to_owned(), identity));
    }

    let listing = accounts.worker_accounts().map_err(account_read_error)?;
    let candidates = ordered_worker_account_candidates(
        &listing,
        excluded_account_keys,
        project_account_keys,
        active_agents,
    );
    let provider = worker_model_provider(model);
    let mut saw_eligible = false;
    let mut saw_catalog = false;
    for candidate in candidates {
        if provider.is_some_and(|provider| candidate.identity.provider.key() != provider) {
            continue;
        }
        saw_eligible = true;
        match model_available(&candidate.account_key, &candidate.identity, model) {
            Ok(true) => {
                return Ok((candidate.account_key.clone(), candidate.identity.clone()));
            }
            Ok(false) => saw_catalog = true,
            Err(CreationError::Unavailable(crate::model_catalog::MODEL_CATALOG_DEPENDENCY)) => {}
            Err(error) => return Err(error),
        }
    }
    if !saw_eligible || saw_catalog {
        return Err(CreationError::Invalid(
            "This model is not available for the selected worker accounts",
        ));
    }
    Err(CreationError::Unavailable(
        crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
    ))
}

/// Apply Python's explicit subagent model-provider and cached-price limits.
/// `prices` is absent before the first pricing snapshot, matching Python's
/// permissive first-run behavior.
pub fn validate_worker_model_choice(
    requested: &str,
    default: &str,
    prices: WorkerModelPrices,
) -> Result<(), CreationError> {
    if requested == default {
        return Ok(());
    }
    let provider = worker_model_provider(default);
    if provider.is_some() && worker_model_provider(requested) != provider {
        // The worker provider follows the chat's default subagent model, not
        // the lead's provider; tell the caller which family is allowed.
        return Err(CreationError::Invalid(if provider == Some("codex") {
            "Choose a gpt-* (Codex) model: chat settings fix the worker provider to Codex"
        } else {
            "Choose default, sonnet, opus, haiku or a claude-* model: chat settings fix the worker provider to Claude"
        }));
    }
    let Some((Some(requested), Some(default))) = prices else {
        if prices.is_some() {
            return Err(CreationError::Invalid(
                "Studio cannot compare this model's price with the default subagent model",
            ));
        }
        return Ok(());
    };
    if requested.0 > default.0 || requested.1 > default.1 {
        return Err(CreationError::Invalid(
            "Choose a model that costs no more than the default subagent model",
        ));
    }
    Ok(())
}

fn account_read_error(
    error: crate::provider_callbacks::ManagedCreationAccountError,
) -> CreationError {
    use crate::provider_callbacks::ManagedCreationAccountError as AccountError;
    match error {
        AccountError::Unknown => CreationError::Invalid("Unknown worker account"),
        AccountError::Deleted => CreationError::Invalid("This worker account was deleted"),
        AccountError::Disconnected => {
            CreationError::Invalid("Sign in to the worker account before creating a worker")
        }
        AccountError::SignInRequired => {
            CreationError::Invalid("Sign in to the worker account before creating a worker")
        }
        AccountError::Unavailable => {
            CreationError::Unavailable("account-driver/managed-child-account")
        }
    }
}

fn validate_selected_account(
    account_key: &str,
    excluded: &[String],
    project_keys: Option<&[String]>,
) -> Result<(), CreationError> {
    if account_key.is_empty() {
        return Err(CreationError::Invalid("Select an available worker account"));
    }
    if excluded.iter().any(|key| key == account_key) {
        return Err(CreationError::Invalid(
            "This account is disabled for this chat",
        ));
    }
    if project_keys
        .is_some_and(|keys| !keys.is_empty() && !keys.iter().any(|key| key == account_key))
    {
        return Err(CreationError::Invalid(
            "Select a worker account of this project",
        ));
    }
    Ok(())
}

/// Ready, usable accounts that the chat and project allow, least loaded first.
/// Ties break on the account key so the order is deterministic.
fn ordered_worker_account_candidates<'a>(
    listing: &'a crate::provider_callbacks::ManagedCreationAccountListing,
    excluded: &[String],
    project_keys: Option<&[String]>,
    active_agents: &std::collections::HashMap<String, i64>,
) -> Vec<&'a crate::provider_callbacks::ManagedCreationAccountCandidate> {
    let mut ordered = listing
        .accounts
        .iter()
        .filter(|row| {
            let key = row.account_key.as_str();
            row.ready
                && !(row.disconnected || row.deleted || row.duplicate)
                && !excluded.iter().any(|excluded| excluded == key)
                && project_keys.is_none_or(|allowed| {
                    allowed.is_empty() || allowed.iter().any(|item| item == key)
                })
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|row| {
        (
            active_agents.get(&row.account_key).copied().unwrap_or(0),
            row.account_key.as_str(),
        )
    });
    ordered
}

fn worker_model_provider(model: &str) -> Option<&'static str> {
    crate::provider_adapter::model_provider(model).map(studio_runtime_providers::ProviderKind::key)
}

#[derive(Clone, Debug)]
pub(crate) struct RulesActionResult {
    pub(crate) value: Value,
    pub(crate) cancel_monitor_ids: Vec<String>,
}

pub(crate) trait StatusWatchPort: Send + Sync {
    fn status<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        now: f64,
    ) -> BoxFuture<'a, Result<Value, Error>>;

    fn rules<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<RulesActionResult, Error>>;
}

pub(crate) trait ChildCreationPort: Send + Sync {
    fn spawn<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<OperationResult, Error>>;

    fn review<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<OperationResult, Error>>;
}

pub(crate) trait MonitorCreatePort: Send + Sync {
    fn create<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<Value, Error>>;
}

pub(crate) trait InterruptPort: Send + Sync {
    fn stop_by_agent<'a>(
        &'a self,
        store: std::sync::Arc<RuntimeStore>,
        actor: &'a Actor,
        target_id: &'a str,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<DispatchedOperation, Error>>;
}

pub(crate) trait MonitorInputPort: Send + Sync {
    /// The authenticated caller is explicit even though the retained native
    /// runtime's input method takes the owner from the persisted monitor.
    fn input<'a>(&'a self, request: MonitorInputRequest<'a>)
    -> BoxFuture<'a, Result<Value, Error>>;
}

pub(crate) struct MonitorInputRequest<'a> {
    pub(crate) caller_id: &'a str,
    pub(crate) caller_epoch: i64,
    pub(crate) monitor_id: &'a str,
    pub(crate) ticket: &'a str,
    pub(crate) input: &'a Value,
    pub(crate) now: f64,
}

pub(crate) trait MonitorCancelPort: Send + Sync {
    fn cancel_monitor<'a>(
        &'a self,
        monitor_id: &'a str,
        owner: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<Value, Error>>;
}

/// The outcome of one lead or worker task action (claim, submit, accept).
#[derive(Clone, Debug, PartialEq)]
pub enum WorkActionOutcome {
    /// The action committed; the full task record.
    Written(Value),
    /// The action was refused before any write; the message for the model.
    Refused(String),
    /// The action may have committed; the message for the model.
    Unknown(String),
}

/// Python `model_work` hands claim, submit and accept to `work_action`, the
/// same owner as `POST /api/work`. The HTTP host supplies that owner here, so
/// the tool and the route share one implementation (receipts, VM evidence,
/// rule checks, result files, and the archive after accept).
pub trait WorkActionPort: Send + Sync {
    fn act<'a>(
        &'a self,
        actor_id: &'a str,
        args: &'a Value,
        request_id: &'a str,
    ) -> BoxFuture<'a, WorkActionOutcome>;
}

/// Task actions that need the work-action owner instead of the in-transaction
/// tool service.
pub(crate) fn work_action_tool(tool: &str, args: &Value) -> bool {
    tool == "orchestration_task"
        && matches!(args["action"].as_str(), Some("claim" | "submit" | "accept"))
}

/// Python `task_brief`.
fn task_brief(task: &Value) -> Value {
    let mut brief = serde_json::Map::new();
    for key in ["id", "title", "owner", "status", "version", "blockedBy"] {
        brief.insert(key.into(), task.get(key).cloned().unwrap_or(Value::Null));
    }
    if let Some(archive) = task
        .get("archive")
        .filter(|v| studio_runtime_store::truthy(Some(v)))
    {
        let mut kept = serde_json::Map::new();
        for key in ["status", "reason"] {
            if let Some(value) = archive.get(key).filter(|value| !value.is_null()) {
                kept.insert(key.into(), value.clone());
            }
        }
        brief.insert("archive".into(), Value::Object(kept));
    }
    Value::Object(brief)
}

#[derive(Clone)]
pub(crate) struct ManagedToolRuntimePorts {
    pub(crate) status_watch: Option<std::sync::Arc<dyn StatusWatchPort>>,
    pub(crate) child_creation: Option<std::sync::Arc<dyn ChildCreationPort>>,
    pub(crate) monitor_create: Option<std::sync::Arc<dyn MonitorCreatePort>>,
    pub(crate) interrupt: Option<std::sync::Arc<dyn InterruptPort>>,
    pub(crate) monitor_input: Option<std::sync::Arc<dyn MonitorInputPort>>,
    pub(crate) monitor_cancel: Option<std::sync::Arc<dyn MonitorCancelPort>>,
    pub(crate) work_action: Option<std::sync::Arc<dyn WorkActionPort>>,
    /// Services that the host attaches after the HTTP state exists. Every
    /// clone of these ports shares the same slots.
    pub(crate) late: LatePorts,
}

/// Late-bound managed-tool services. The paired-server driver is created with
/// the HTTP state, after the account worker registry owns these ports.
#[derive(Clone, Default)]
pub(crate) struct LatePorts {
    pub(crate) multi_server: std::sync::Arc<
        std::sync::OnceLock<std::sync::Arc<dyn crate::multi_server_tools::MultiServerToolPort>>,
    >,
    /// The provider-session voice adapter for `orchestration_speak` playback.
    pub(crate) voice: std::sync::Arc<
        std::sync::OnceLock<std::sync::Arc<dyn crate::voice_runtime::VoiceRequestPort>>,
    >,
}

/// The dependency that a remote worker's forwarded tool names until the host
/// attaches the paired-server driver.
pub(crate) const WORKER_CALL_DEPENDENCY: &str = "multi-server/worker-call";

/// The dependency that `orchestration_servers` names until the host attaches
/// the paired-server driver.
pub(crate) const SERVERS_DEPENDENCY: &str = "multi-server/tool-coordinator";

pub(super) struct DispatchRequest<'a> {
    pub(super) store: std::sync::Arc<RuntimeStore>,
    pub(super) actor: &'a Actor,
    pub(super) context: &'a crate::provider_callbacks::CallbackContext,
    pub(super) tool: &'a str,
    pub(super) args: &'a Value,
    pub(super) request_id: &'a str,
    pub(super) now: f64,
    pub(super) ports: &'a ManagedToolRuntimePorts,
}

pub(super) async fn dispatch(
    request: DispatchRequest<'_>,
) -> Result<Option<DispatchedOperation>, Error> {
    let DispatchRequest {
        store,
        actor,
        context,
        tool,
        args,
        request_id,
        now,
        ports,
    } = request;
    let store_ref = store.as_ref();
    let actor_id = actor.record().id.0.as_str();
    let actor_epoch = actor.record().epoch;
    let execution_move = studio_runtime_store::truthy(actor.record().extra.get("executionMove"));
    // Python `dynamic`: a remote worker's send to its parent, its lead or any
    // agent that is not its descendant goes through `chat_message`, which the
    // home server answers as the `message` worker call.
    let remote_send = if tool == "orchestration_send"
        && studio_runtime_store::truthy(actor.record().extra.get("remoteOrigin"))
        && !studio_runtime_store::truthy(actor.record().extra.get("movedFrom"))
        && let Some(target) = args.get("agent_id").and_then(Value::as_str)
        && !(target == "user" || target.starts_with("remote:") || target.starts_with("federated:"))
    {
        if matches!(target, "parent" | "lead" | "broadcast" | "all") {
            Some(target.to_owned())
        } else {
            match super::services::resolve_send_target(store_ref, context, target).await {
                Ok((resolved, false)) => Some(resolved),
                Ok((_, true)) => None,
                Err(error) => return refused_before_write(error),
            }
        }
    } else {
        None
    };
    let route = match remote_send {
        Some(target) => Some(crate::multi_server_tools::worker_message(
            &target,
            &json!({"text": args.get("text")}),
        )),
        None => crate::multi_server_tools::worker_route(tool, args, actor.record()),
    };
    if let Some(route) = route {
        if execution_move {
            return Ok(Some(not_sent(Error::Invalid(
                "The move is accepted. Finish this turn before the next task",
            ))));
        }
        let (action, call_args) = match route {
            Ok(route) => route,
            Err(message) => return Ok(Some(not_sent(Error::Invalid(message)))),
        };
        let Some(port) = ports.late.multi_server.get() else {
            return Ok(Some(not_sent(Error::Unavailable(WORKER_CALL_DEPENDENCY))));
        };
        return Ok(Some(
            port.worker_call(actor, action, &call_args, request_id)
                .await?,
        ));
    }
    let dispatched = match tool {
        "orchestration_speak" => {
            if execution_move {
                return Ok(Some(not_sent(Error::Invalid(
                    "The move is accepted. Finish this turn before the next task",
                ))));
            }
            speak(
                std::sync::Arc::clone(&store),
                actor,
                args,
                request_id,
                ports.late.voice.get().cloned(),
            )
            .await
        }
        "orchestration_servers" => {
            if execution_move {
                return Ok(Some(not_sent(Error::Invalid(
                    "The move is accepted. Finish this turn before the next task",
                ))));
            }
            let Some(port) = ports.late.multi_server.get() else {
                return Ok(Some(not_sent(Error::Unavailable(SERVERS_DEPENDENCY))));
            };
            port.servers(actor, args, request_id, now).await?
        }
        "orchestration_status" => {
            let Some(port) = ports.status_watch.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "model-directory/status-and-dispatch-capacity",
                ))));
            };
            written(port.status(store_ref, actor, args, now).await?, Vec::new())
        }
        "orchestration_watch" => {
            let action = args["action"].as_str().unwrap_or("list");
            if action != "list" && ports.monitor_cancel.is_none() {
                return Ok(Some(not_sent(Error::Unavailable(
                    "monitor-runtime/rule-cancel",
                ))));
            }
            let Some(port) = ports.status_watch.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "model-directory/status-and-dispatch-capacity",
                ))));
            };
            let result = port.rules(store_ref, actor, args, request_id, now).await?;
            for monitor_id in result.cancel_monitor_ids {
                let Some(monitor_port) = ports.monitor_cancel.as_ref() else {
                    return Ok(Some(not_sent(Error::Unavailable(
                        "monitor-runtime/rule-cancel",
                    ))));
                };
                monitor_port
                    .cancel_monitor(&monitor_id, actor_id, now)
                    .await?;
            }
            written(result.value, Vec::new())
        }
        "orchestration_spawn" | "orchestration_review" => {
            let Some(port) = ports.child_creation.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "agent-create/managed-environment",
                ))));
            };
            let result = if tool == "orchestration_spawn" {
                port.spawn(store_ref, actor, args, request_id, now).await
            } else {
                port.review(store_ref, actor, args, request_id, now).await
            };
            match result {
                Ok(result) => child_creation_result(result),
                Err(error) => child_creation_refusal(store_ref, tool, request_id, error)?,
            }
        }
        "orchestration_interrupt" => {
            let Some(port) = ports.interrupt.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "agent-lifecycle/production-ports",
                ))));
            };
            let Some(target) = args.get("agent_id").and_then(Value::as_str) else {
                return Ok(Some(not_sent(Error::Invalid(
                    "Supply agent_id for orchestration_interrupt",
                ))));
            };
            let target =
                match super::services::resolve_interrupt_target(store_ref, context, target).await {
                    Ok(target) => target,
                    Err(error) => return refused_before_write(error),
                };
            port.stop_by_agent(
                std::sync::Arc::clone(&store),
                actor,
                &target,
                request_id,
                now,
            )
            .await?
        }
        "orchestration_monitor" if args["action"].as_str() != Some("cancel") => {
            let Some(port) = ports.monitor_create.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "workspace-state/monitor-admission",
                ))));
            };
            written(
                port.create(store_ref, actor, args, request_id, now).await?,
                Vec::new(),
            )
        }
        "orchestration_monitor_input" => {
            let Some(port) = ports.monitor_input.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "monitor-runtime/managed-input",
                ))));
            };
            let Some(monitor_id) = args.get("monitor_id").and_then(Value::as_str) else {
                return Ok(Some(not_sent(Error::Invalid("Missing monitor_id"))));
            };
            let input = args.clone();
            if let Err(error) = authorize_monitor_input(store_ref, actor, monitor_id) {
                return refused_before_write(error);
            }
            match port
                .input(MonitorInputRequest {
                    caller_id: actor_id,
                    caller_epoch: actor_epoch,
                    monitor_id,
                    ticket: request_id,
                    input: &input,
                    now,
                })
                .await
            {
                Ok(value) => written(value, Vec::new()),
                Err(error) => match input_attempt(store_ref, monitor_id, request_id) {
                    Ok(Some(record)) => managed_input_receipt(&record, request_id)?,
                    Ok(None) => not_sent(error),
                    Err(_) => unknown_input(),
                },
            }
        }
        "orchestration_task" if work_action_tool(tool, args) => {
            // The same refusals as the in-transaction tool service.
            if studio_runtime_store::truthy(actor.record().extra.get("executionMove")) {
                return Ok(Some(not_sent(Error::Invalid(
                    "The move is accepted. Finish this turn before the next task",
                ))));
            }
            if studio_runtime_store::truthy(actor.record().extra.get("remoteOrigin")) {
                return Ok(Some(not_sent(Error::Unavailable(
                    "multi-server/worker-call",
                ))));
            }
            let Some(port) = ports.work_action.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "work-board/assignment-and-result-driver",
                ))));
            };
            match port.act(actor_id, args, request_id).await {
                WorkActionOutcome::Written(record) => {
                    let mut value = task_brief(&record);
                    value["requestId"] = json!(request_id);
                    value["detail"] =
                        json!({"tool":"orchestration_task","action":"get","task_id":record["id"]});
                    written(value, Vec::new())
                }
                WorkActionOutcome::Refused(message) => DispatchedOperation {
                    result: OperationResult {
                        value: json!(message),
                        signals: Vec::new(),
                    },
                    certainty: DeliveryCertainty::NotSent,
                    applied: Some(false),
                },
                WorkActionOutcome::Unknown(message) => unknown(json!(message), Vec::new()),
            }
        }
        "orchestration_agent_manage" if super::services::manage::lifecycle_action(args) => {
            return lifecycle_management(store, actor, args, now)
                .await
                .map(Some);
        }
        _ => return Ok(None),
    };
    Ok(Some(dispatched))
}

/// Python `manage_agent` archive and restore. The shared lifecycle owner
/// commits the decision, then completes the workspace cleanup it queued.
async fn lifecycle_management(
    store: std::sync::Arc<RuntimeStore>,
    actor: &Actor,
    args: &Value,
    now: f64,
) -> Result<DispatchedOperation, Error> {
    // The same refusals as the in-transaction tool service.
    if studio_runtime_store::truthy(actor.record().extra.get("executionMove")) {
        return Ok(not_sent(Error::Invalid(
            "The move is accepted. Finish this turn before the next task",
        )));
    }
    if studio_runtime_store::truthy(actor.record().extra.get("remoteOrigin")) {
        return Ok(not_sent(Error::Unavailable("multi-server/worker-call")));
    }
    let actor_value = actor.record().value();
    let checked = store.read_transaction_with_error(|tx| {
        let args = super::services::manage::resolved_args(tx, &actor_value, args)
            .map_err(callback_refusal)?;
        let (action, unassign_work) =
            super::services::manage::checked_action(&args).map_err(callback_refusal)?;
        let target = args["agent_id"]
            .as_str()
            .filter(|id| {
                studio_runtime_store::system_reads::get(tx, "runtime_agents", id)
                    .is_ok_and(|agent| agent.is_some())
            })
            .ok_or(Error::Invalid("Unknown managed agent"))?
            .to_owned();
        let actor_id = actor.record().id.0.clone();
        let epoch = actor.record().epoch;
        Ok::<_, Error>(if action == "archive" {
            crate::agent_management::Action::Archive {
                actor: actor_id,
                epoch,
                target,
                reason: args["reason"].as_str().unwrap_or_default().to_owned(),
                unassign_work,
            }
        } else {
            crate::agent_management::Action::Restore {
                actor: actor_id,
                epoch,
                target,
            }
        })
    });
    let action = match checked {
        Ok(action) => action,
        Err(error @ (Error::Store(_) | Error::Sqlite(_) | Error::Work(_))) => return Err(error),
        // These checks only read; a refusal proves that nothing was applied.
        Err(error) => return Ok(not_sent(error)),
    };
    match crate::agent_management_store::execute(store, &action, now).await {
        Ok(value) => Ok(written(value, Vec::new())),
        // Every decision refusal precedes its single writer transaction.
        Err(crate::agent_management::ManagementError::Invalid(message)) => {
            Ok(not_sent(Error::Invalid(message)))
        }
        Err(crate::agent_management::ManagementError::Unavailable(name)) => {
            Ok(not_sent(Error::Unavailable(name)))
        }
        Err(crate::agent_management::ManagementError::Stored { .. }) => Ok(not_sent(
            Error::Invalid("A stored team record is invalid; inspect it before managing workers"),
        )),
        // A store failure can follow the commit, for example in workspace cleanup.
        Err(crate::agent_management::ManagementError::Store(error)) => Err(Error::Store(error)),
    }
}

fn callback_refusal(error: crate::provider_callbacks::CallbackError) -> Error {
    use crate::provider_callbacks::CallbackError as C;
    match error {
        C::Invalid(message) => Error::Invalid(message),
        C::Dependency(name) => Error::Unavailable(name),
        C::Store(error) => Error::Store(error),
        _ => Error::Stale,
    }
}

/// Python `VoiceStore.speak` through `Runtime.dynamic`.
async fn speak(
    store: std::sync::Arc<RuntimeStore>,
    actor: &Actor,
    args: &Value,
    request_id: &str,
    provider: Option<std::sync::Arc<dyn crate::voice_runtime::VoiceRequestPort>>,
) -> DispatchedOperation {
    use crate::voice::provider::VoiceProviderError as V;
    let Some(text) = args.get("text").and_then(Value::as_str) else {
        return not_sent(Error::Invalid(
            "Speech text must contain 1 to 4096 characters",
        ));
    };
    match crate::voice::speak(crate::voice::SpeakRequest {
        store,
        provider,
        agent_id: actor.record().id.0.clone(),
        text: text.to_owned(),
        request_id: request_id.to_owned(),
        epoch: Some(actor.record().epoch),
    })
    .await
    {
        Ok(value) => written(value, Vec::new()),
        Err(V::Voice(crate::voice::VoiceError::InvalidRequest(message)))
        | Err(V::Voice(crate::voice::VoiceError::Conflict(message))) => {
            not_sent(Error::Invalid(message))
        }
        Err(V::RequiresDependency(name)) => not_sent(Error::Unavailable(name)),
        Err(error @ (V::Unknown | V::Store(_) | V::Voice(crate::voice::VoiceError::Store(_)))) => {
            unknown(json!(error.to_string()), Vec::new())
        }
        Err(error) => DispatchedOperation {
            result: OperationResult {
                value: json!(error.to_string()),
                signals: Vec::new(),
            },
            certainty: DeliveryCertainty::Written,
            applied: Some(false),
        },
    }
}

fn input_attempt(
    store: &RuntimeStore,
    monitor_id: &str,
    request_id: &str,
) -> Result<Option<Value>, Error> {
    store
        .read_transaction_with_error(|connection| {
            let record = studio_runtime_store::MonitorsRepository::get(connection, monitor_id)?;
            Ok(record.and_then(|record| {
                let value = record.value();
                [&value["lastInputAttempt"], &value["inputAttempt"]]
                    .into_iter()
                    .any(|receipt| receipt.get("id").and_then(Value::as_str) == Some(request_id))
                    .then(|| value.clone())
            }))
        })
        .map_err(Error::Store)
}

pub(crate) fn authorize_monitor_input(
    store: &RuntimeStore,
    caller: &Actor,
    monitor_id: &str,
) -> Result<(), Error> {
    store.read_transaction_with_error(|tx| {
        authorize_monitor_input_in_transaction(tx, caller, monitor_id)
    })
}

pub(crate) fn authorize_monitor_input_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    caller: &Actor,
    monitor_id: &str,
) -> Result<(), Error> {
    let caller_record = studio_runtime_store::system_reads::get(
        tx,
        "runtime_agents",
        caller.record().id.0.as_str(),
    )?
    .ok_or(Error::Stale)?;
    if caller_record.get("epoch").and_then(Value::as_i64) != Some(caller.record().epoch)
        || caller_record.get("autoWake") != Some(&Value::Bool(true))
        || studio_runtime_store::truthy(caller_record.get("deletedAt"))
    {
        return Err(Error::Stale);
    }
    let (snapshot, owner) = crate::monitor_runtime::snapshot(tx, monitor_id)
        .map_err(|_| Error::Invalid("Unknown monitor in this caller's scope"))?;
    let monitor_epoch = snapshot.record().get("epoch").and_then(Value::as_i64);
    if snapshot.agent() != caller.record().id.0
        || monitor_epoch != Some(caller.record().epoch)
        || owner.epoch != caller.record().epoch
        || owner.extra.get("autoWake") != Some(&Value::Bool(true))
        || studio_runtime_store::truthy(owner.extra.get("deletedAt"))
    {
        return Err(Error::Invalid(
            "You can input only to your own active monitor",
        ));
    }
    Ok(())
}

pub(crate) fn managed_input_receipt(
    record: &Value,
    request_id: &str,
) -> Result<DispatchedOperation, Error> {
    let receipt = [&record["lastInputAttempt"], &record["inputAttempt"]]
        .into_iter()
        .find(|receipt| receipt.get("id").and_then(Value::as_str) == Some(request_id));
    let (value, certainty, applied) = match receipt {
        Some(receipt) if receipt["status"] == "acknowledged" => (
            receipt.get("response").cloned().unwrap_or(Value::Null),
            DeliveryCertainty::Written,
            Some(true),
        ),
        Some(receipt) if receipt["status"] == "failed" => (
            json!("Monitor input was rejected"),
            DeliveryCertainty::NotSent,
            Some(false),
        ),
        Some(_) => (
            json!("Monitor input outcome unknown; do not repeat it"),
            DeliveryCertainty::Unknown,
            None,
        ),
        None => (
            json!("Monitor input was not submitted"),
            DeliveryCertainty::NotSent,
            Some(false),
        ),
    };
    Ok(DispatchedOperation {
        result: OperationResult {
            value,
            signals: Vec::new(),
        },
        certainty,
        applied,
    })
}

/// A store failure may hide a committed write; every other refusal raised by a
/// read-only check before the operation's first write proves it was not applied.
fn refused_before_write(error: Error) -> Result<Option<DispatchedOperation>, Error> {
    match error {
        Error::Store(_) | Error::Sqlite(_) | Error::Work(_) => Err(error),
        error => Ok(Some(not_sent(error))),
    }
}

/// Child creation commits every child row and its initial intent in one
/// transaction; image provisioning is the only work after that commit. A
/// non-store refusal therefore proves no effect when the first deterministic
/// child row is absent. Review creation has no work after its commit.
fn child_creation_refusal(
    store: &RuntimeStore,
    tool: &str,
    request_id: &str,
    error: Error,
) -> Result<DispatchedOperation, Error> {
    if matches!(error, Error::Store(_) | Error::Sqlite(_) | Error::Work(_)) {
        return Err(error);
    }
    if tool == "orchestration_spawn" {
        let first = crate::managed_child_create::child_id(request_id, 0);
        let read = store.read()?;
        let committed = studio_runtime_store::AgentRepository::get(
            &read,
            &studio_runtime_store::AgentId::from(first.as_str()),
        )?;
        if committed.is_some() {
            return Err(error);
        }
    }
    Ok(not_sent(error))
}

/// Refusal text saved for a request that a transient model-catalog outage
/// stopped before any write. The not-applied receipt is final for this request
/// identity, so the caller must retry under a new one.
const MODEL_CATALOG_RETRY: &str = "The model catalog was temporarily unavailable, so nothing was created. Retry with a new request_id; replaying this request_id returns this refusal.";

fn not_sent(error: Error) -> DispatchedOperation {
    let value = match error {
        Error::Unavailable(crate::model_catalog::MODEL_CATALOG_DEPENDENCY) => {
            json!(MODEL_CATALOG_RETRY)
        }
        error => json!(error.to_string()),
    };
    DispatchedOperation {
        result: OperationResult {
            value,
            signals: Vec::new(),
        },
        certainty: DeliveryCertainty::NotSent,
        applied: Some(false),
    }
}

fn unknown_input() -> DispatchedOperation {
    unknown(
        json!("Monitor input outcome unknown; do not repeat it"),
        Vec::new(),
    )
}

fn unknown(value: Value, signals: Vec<WakeSignal>) -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult { value, signals },
        certainty: DeliveryCertainty::Unknown,
        applied: None,
    }
}

fn written(value: Value, signals: Vec<WakeSignal>) -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult { value, signals },
        certainty: DeliveryCertainty::Written,
        applied: Some(true),
    }
}

fn child_creation_result(result: OperationResult) -> DispatchedOperation {
    if result.value.get("outcome").and_then(Value::as_str) == Some("unknown") {
        unknown(result.value, result.signals)
    } else {
        written(result.value, result.signals)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CreationError, child_creation_result, managed_input_receipt, validate_worker_model_choice,
    };
    use crate::managed_host_tools::{DeliveryCertainty, OperationResult};
    use serde_json::json;

    #[test]
    fn transient_catalog_refusal_tells_the_caller_to_retry_with_a_new_request_id() {
        let refusal = super::not_sent(crate::managed_host_tools::Error::Unavailable(
            crate::model_catalog::MODEL_CATALOG_DEPENDENCY,
        ));
        assert_eq!(refusal.certainty, DeliveryCertainty::NotSent);
        assert_eq!(refusal.applied, Some(false));
        let text = refusal.result.value.as_str().expect("refusal text");
        assert!(text.contains("temporarily unavailable"), "{text}");
        assert!(text.contains("new request_id"), "{text}");
        let other = super::not_sent(crate::managed_host_tools::Error::Unavailable(
            "agent-create/managed-environment",
        ));
        assert_eq!(
            other.result.value,
            json!("Not implemented: requires agent-create/managed-environment")
        );
    }

    #[test]
    fn uncertain_guest_save_remains_unknown_at_the_tool_boundary() {
        let result = child_creation_result(OperationResult {
            value: json!({"outcome":"unknown","requestId":"review-1"}),
            signals: Vec::new(),
        });
        assert_eq!(result.certainty, DeliveryCertainty::Unknown);
        assert_eq!(result.applied, None);
        assert_eq!(result.result.value["outcome"], "unknown");
    }

    #[test]
    fn managed_input_receipts_preserve_written_not_sent_and_unknown() {
        for (status, expected) in [
            ("acknowledged", DeliveryCertainty::Written),
            ("failed", DeliveryCertainty::NotSent),
            ("pending", DeliveryCertainty::Unknown),
        ] {
            let record = json!({"lastInputAttempt":{"id":"stable-input","status":status,"response":{"ok":true}}});
            let result = managed_input_receipt(&record, "stable-input")
                .expect("known receipt has durable outcome");
            assert_eq!(result.certainty, expected, "status {status}");
            assert_eq!(
                result.applied,
                match expected {
                    DeliveryCertainty::Written => Some(true),
                    DeliveryCertainty::NotSent => Some(false),
                    DeliveryCertainty::Unknown => None,
                }
            );
        }
    }

    #[test]
    fn managed_spawn_refuses_cross_provider_and_more_expensive_model_choices() {
        assert_eq!(
            validate_worker_model_choice("claude-opus-5-5", "gpt-6-luna", None),
            Err(CreationError::Invalid(
                "Choose a gpt-* (Codex) model: chat settings fix the worker provider to Codex"
            ))
        );
        assert!(
            matches!(
                validate_worker_model_choice("gpt-6-luna", "claude-opus-5-5", None),
                Err(CreationError::Invalid(message)) if message.contains("claude-*")
            ),
            "a Claude chat default names the Claude family"
        );
        assert!(
            validate_worker_model_choice(
                "gpt-6-expensive",
                "gpt-6-default",
                Some((Some((2.0, 3.0)), Some((1.0, 1.0)))),
            )
            .is_err()
        );
        assert!(
            validate_worker_model_choice(
                "gpt-6-cheaper",
                "gpt-6-default",
                Some((Some((0.5, 0.8)), Some((1.0, 1.0)))),
            )
            .is_ok()
        );
    }
}

#[cfg(test)]
mod account_identity_tests {
    use super::{WorkerAccountRequest, WorkerAccountSelection, select_worker_account};
    use crate::provider_callbacks::{
        ManagedAccountIdentity, ManagedCreationAccountCandidate as Candidate,
        ManagedCreationAccountError, ManagedCreationAccountListing as Listing,
        ManagedCreationAccountRead,
    };

    struct Accounts(Listing);

    impl ManagedCreationAccountRead for Accounts {
        fn worker_accounts(&self) -> Result<Listing, ManagedCreationAccountError> {
            Ok(self.0.clone())
        }

        fn resolve_identity(
            &self,
            account_key: &str,
            _: &str,
        ) -> Result<ManagedAccountIdentity, ManagedCreationAccountError> {
            self.0
                .accounts
                .iter()
                .find(|account| account.account_key == account_key)
                .map(|account| account.identity.clone())
                .ok_or(ManagedCreationAccountError::Unknown)
        }
    }

    use studio_runtime_providers::ProviderKind;

    fn candidate(key: &str, provider: ProviderKind, ready: bool) -> Candidate {
        Candidate {
            account_key: key.to_owned(),
            identity: ManagedAccountIdentity {
                provider,
                home: None,
                account_id: None,
                email: None,
            },
            ready,
            can_attempt_native_proof: false,
            disconnected: false,
            deleted: false,
            duplicate: false,
        }
    }

    fn accounts(rows: &[(&str, ProviderKind, bool)]) -> Accounts {
        Accounts(Listing {
            accounts: rows
                .iter()
                .map(|(key, provider, ready)| candidate(key, *provider, *ready))
                .collect(),
        })
    }

    type Counts = std::collections::HashMap<String, i64>;

    fn select(
        accounts: &Accounts,
        model: &str,
        requested: Option<&str>,
        counts: &Counts,
        available: &mut dyn FnMut(&str) -> bool,
    ) -> Result<String, crate::managed_child_create::ManagedCreationEnvironmentError> {
        let mut model_available =
            |key: &str, _: &ManagedAccountIdentity, _: &str| Ok(available(key));
        select_worker_account(WorkerAccountSelection {
            accounts,
            request: WorkerAccountRequest {
                parent_account_key: "parent",
                requested_account_key: requested,
                default_account_key: None,
                model,
                excluded_account_keys: &[],
                project_account_keys: None,
                active_agents: counts,
            },
            model_available: &mut model_available,
        })
        .map(|selected| selected.0)
    }

    /// Spawn `workers` with no account the way a batch does: each pick raises
    /// that account's count before the next selection.
    fn spread(accounts: &Accounts, model: &str, workers: usize) -> Vec<(String, i64)> {
        let mut counts = Counts::new();
        for _ in 0..workers {
            let key = select(accounts, model, None, &counts, &mut |_| true).expect("selected");
            *counts.entry(key).or_default() += 1;
        }
        let mut result = counts.into_iter().collect::<Vec<_>>();
        result.sort();
        result
    }

    #[test]
    fn thirty_workers_spread_evenly_over_three_accounts_per_provider() {
        use ProviderKind::{Claude, Codex};
        let codex = accounts(&[
            ("parent", Codex, true),
            ("b", Codex, true),
            ("a", Codex, true),
            ("claude", Claude, true),
        ]);
        let expected = |keys: [&str; 3]| keys.map(|key| (key.to_owned(), 10)).to_vec();
        assert_eq!(spread(&codex, "gpt-5", 30), expected(["a", "b", "parent"]));
        let claude = accounts(&[
            ("c3", Claude, true),
            ("c1", Claude, true),
            ("c2", Claude, true),
            ("codex", Codex, true),
        ]);
        assert_eq!(spread(&claude, "sonnet", 30), expected(["c1", "c2", "c3"]));
    }

    #[test]
    fn implicit_selection_prefers_the_least_loaded_ready_account() {
        use ProviderKind::Codex;
        let accounts = accounts(&[
            ("busy", Codex, true),
            ("idle", Codex, true),
            ("offline", Codex, false),
            ("parent", Codex, true),
        ]);
        let counts = Counts::from([("busy".to_owned(), 4), ("parent".to_owned(), 2)]);
        let pick = |available: &mut dyn FnMut(&str) -> bool| {
            select(&accounts, "gpt-5", None, &counts, available)
        };
        assert_eq!(pick(&mut |_| true).expect("idle"), "idle");
        // The lead's account gets no preference, and a non-ready account is skipped.
        assert_eq!(pick(&mut |key| key != "idle").expect("parent"), "parent");
        assert_eq!(
            pick(&mut |key| key == "offline" || key == "busy").expect("busy"),
            "busy"
        );
    }

    #[test]
    fn explicit_worker_account_never_falls_back() {
        use ProviderKind::Codex;
        let accounts = accounts(&[("offline", Codex, false), ("idle", Codex, true)]);
        let counts = Counts::new();
        assert_eq!(
            select(&accounts, "gpt-5", Some("offline"), &counts, &mut |_| true)
                .expect("explicit account is honoured even when busier or not ready"),
            "offline"
        );
        assert!(select(&accounts, "gpt-5", Some("offline"), &counts, &mut |_| false).is_err());
    }
}
