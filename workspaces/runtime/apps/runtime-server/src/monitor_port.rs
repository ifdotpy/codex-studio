//! Production monitor adapter over the retained account worker session.
//!
//! Native command intents are committed by P15 before this adapter calls the
//! broker. The broker owns request receipts; this layer maps their certainty and
//! result into P15 receipts and never retries `Unknown`.
//! The output/recovery path follows codex_runtime.py:3882-3928 (reattach via
//! operationStatus keyed by `monitor:<id>`), 7985-7987 (outputDelta dispatch),
//! and 10347-10442 (stall probes and exit notices). Command argv follows
//! codex_shell.py:41-91; monitor-specific settings follow codex_runtime.py:5710-5745
//! and its liveness preparation at 10391-10415.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_providers::Method;
use studio_runtime_services::account_session_runtime::{
    AccountSessionError, AccountSessionPort, NotSentReason as BrokerNotSentReason, ProviderRequest,
    ProviderResponse, RequestCertainty, SessionReceipt,
};
use studio_runtime_services::account_worker::retained::RequestActor;
use studio_runtime_services::monitor_driver::{MonitorNative, NotSentReason, Submission};
use studio_runtime_services::monitor_event_queue;
use studio_runtime_services::monitor_policy::{
    Effect, MonitorError, MonitorSnapshot, Operation, ReattachOutcome, ReattachProof,
};
use studio_runtime_services::monitor_runtime_wiring::{Delivery, InputReply, MonitorPort};
use studio_runtime_services::rules_engine::RuleCheck;
use studio_runtime_services::rules_tick::{
    CheckReconciliation, CheckSubmission, RuleMonitor, RulesTickError,
};
use studio_runtime_services::shell_command::monitor_command;
use studio_runtime_store::{MonitorsRepository, RuntimeStore};

const DEFAULT_MONITOR_TIMEOUT_MS: u64 = 3_600_000;
const MONITOR_TIMEOUT_GRACE_MS: u64 = 60_000;
const MAX_MONITOR_TIMEOUT_WITH_GRACE_MS: u64 = 86_460_000;
/// Wait after a managed start that could not reach the provider before the
/// monitor timer offers the same durable start again. Unit: seconds.
const MONITOR_START_RETRY_SECONDS: u64 = 5;
/// Wait after the outcome of a detached rule check launch could not be
/// recorded before its task records the same outcome again. Unit: seconds.
const RULE_CHECK_SETTLE_RETRY_SECONDS: u64 = 5;
/// Codex app-server refuses `command/exec/terminate` with this text when no
/// command with the process ID runs. Owner: the provider protocol.
const NO_ACTIVE_COMMAND_EXEC: &str = "no active command/exec for process id";

// These are projections of external provider responses, not owned input envelopes.
#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandExit {
    #[serde(default)]
    exit_code: Option<i64>,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum CommandResponse {
    Result { result: CommandExit },
    Error { error: Value },
    Direct(CommandExit),
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationObservation {
    accepted: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    native_id: Option<u64>,
    #[serde(default)]
    response: Option<CommandResponse>,
}
#[derive(Deserialize)]
struct ConfigurationResponse {
    config: serde_json::Map<String, Value>,
}
#[derive(Deserialize)]
struct ConfigurationFailure {
    #[serde(default)]
    code: Option<i64>,
    message: String,
}
#[derive(Deserialize)]
struct CommandTiming {
    created: f64,
    timeout_ms: u64,
}

pub(crate) struct RetainedMonitorPort {
    store: Arc<RuntimeStore>,
    session: Arc<dyn AccountSessionPort>,
    preparation: RwLock<
        Option<Arc<studio_runtime_services::account_worker::preparation::PreparationResources>>,
    >,
    dispatch_wake: RwLock<Option<Arc<Notify>>>,
    input_replies: Mutex<BTreeMap<String, InputReply>>,
    /// Monitor IDs whose managed start runs in its own task.
    starts: Mutex<BTreeSet<String>>,
    /// Monitor IDs whose rule check launch runs in its own task.
    rule_checks: Arc<Mutex<BTreeSet<String>>>,
    /// Process IDs whose command/exec reply, the exit receipt, this process
    /// still waits for.
    execs: Arc<Mutex<BTreeSet<String>>>,
    receipt_wake: Arc<Notify>,
    this: Weak<Self>,
}

/// Removes an ID from its in-flight set when the owning future ends in any
/// way: a waiting command/exec reply, or a detached rule check launch.
struct InFlightId {
    set: Arc<Mutex<BTreeSet<String>>>,
    id: String,
}

impl Drop for InFlightId {
    fn drop(&mut self) {
        lock(&self.set).remove(&self.id);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RetainedMonitorPort {
    pub(crate) fn new(store: Arc<RuntimeStore>, session: Arc<dyn AccountSessionPort>) -> Arc<Self> {
        let receipt_wake = Arc::new(Notify::new());
        let wake = Arc::clone(&receipt_wake);
        store.add_table_change_observer(
            studio_runtime_store::ObservedTable::OperationReceipts,
            move || wake.notify_waiters(),
        );
        Arc::new_cyclic(|this| Self {
            store,
            session,
            preparation: RwLock::new(None),
            dispatch_wake: RwLock::new(None),
            input_replies: Mutex::new(BTreeMap::new()),
            starts: Mutex::new(BTreeSet::new()),
            rule_checks: Arc::new(Mutex::new(BTreeSet::new())),
            execs: Arc::new(Mutex::new(BTreeSet::new())),
            receipt_wake,
            this: this.clone(),
        })
    }

    /// The only command-result enqueue path, shared by the first request and
    /// the restored future over its durable retained receipt.
    async fn command_result(
        &self,
        id: &str,
        operation: &Operation,
        response: ProviderResponse,
    ) -> Result<(), MonitorError> {
        let (code, error) = match response {
            ProviderResponse::Reply(value) => {
                let result: CommandExit = serde_json::from_value(value)
                    .map_err(|_| MonitorError::Refused("Invalid monitor exit response"))?;
                (result.exit_code, result.error)
            }
            ProviderResponse::Rejected(value) => (None, Some(value.to_string())),
        };
        if code.is_none() && error.as_deref().is_none_or(str::is_empty) {
            return Err(MonitorError::Refused(
                "Monitor result has no definitive outcome",
            ));
        }
        monitor_event_queue::enqueue_exit(
            &self.store,
            id,
            operation,
            &format!("provider-result:monitor:{id}"),
            code,
            error.as_deref(),
            studio_runtime_services::event_write::unix_time()?,
        )
        .await?;
        self.wake_dispatch();
        Ok(())
    }

    /// Restore event ownership of a retained command without a provider write.
    fn await_retained_exit(
        &self,
        id: &str,
        operation: &Operation,
        timeout: Duration,
    ) -> Result<(), MonitorError> {
        let port = self
            .this
            .upgrade()
            .ok_or(MonitorError::Unavailable("provider-session/monitor-result"))?;
        if !lock(&self.execs).insert(id.to_owned()) {
            return Ok(());
        }
        let guard = InFlightId {
            set: Arc::clone(&self.execs),
            id: id.to_owned(),
        };
        let (id, operation) = (id.to_owned(), operation.clone());
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + timeout;
            let result = async {
                loop {
                    let changed = port.receipt_wake.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if let Some(response) =
                        studio_runtime_services::account_worker::retained::monitor_result(
                            &port.store,
                            &id,
                            &operation,
                        )
                        .await
                        .map_err(|_| MonitorError::Unavailable("provider-session/monitor-result"))?
                    {
                        return port.command_result(&id, &operation, response).await;
                    }
                    tokio::time::timeout_at(deadline, changed)
                        .await
                        .map_err(|_| {
                            MonitorError::Unavailable("provider-session/monitor-result-deadline")
                        })?;
                }
            }
            .await;
            drop(guard);
            if let Err(error) = result {
                eprintln!("monitor {id} retained exit wait ended: {error}");
                if let Err(error) = async {
                    let now = studio_runtime_services::event_write::unix_time()?;
                    studio_runtime_services::monitor_runtime::reconcile_unknown(
                        &port.store,
                        &id,
                        now,
                    )
                    .await?;
                    studio_runtime_services::monitor_runtime::settle_reattach(
                        &port.store,
                        &id,
                        "retained_exit_unavailable",
                        now,
                    )
                    .await
                }
                .await
                {
                    eprintln!("monitor {id} reconciliation was not recorded: {error}");
                }
                port.wake_dispatch();
            }
        });
        Ok(())
    }

    /// Run one managed start in its own task. command/exec answers only when
    /// the process exits, so the monitor timer must not wait for it.
    fn spawn_monitor_start(&self, id: &str, now: f64) -> Result<(), MonitorError> {
        let port = self
            .this
            .upgrade()
            .ok_or(MonitorError::Unavailable("provider-session/monitor-start"))?;
        if !lock(&self.starts).insert(id.to_owned()) {
            return Ok(());
        }
        let id = id.to_owned();
        tokio::spawn(async move {
            let retry = match port.dispatch_monitor_start(&id, now).await {
                Ok(()) => false,
                // Python finishes a managed monitor whose preparation is refused.
                Err(MonitorError::Refused(reason)) => {
                    if let Err(error) = port.fail_unstarted_monitor(&id, reason).await {
                        eprintln!("monitor {id} start refusal was not recorded: {error}");
                        true
                    } else {
                        false
                    }
                }
                Err(error) => {
                    eprintln!("monitor {id} start deferred: {error}");
                    true
                }
            };
            if retry {
                tokio::time::sleep(Duration::from_secs(MONITOR_START_RETRY_SECONDS)).await;
            }
            lock(&port.starts).remove(&id);
            if retry {
                port.wake_dispatch();
            }
        });
        Ok(())
    }

    /// Run one rule check launch in its own task and record its outcome there,
    /// once. command/exec answers only when the process exits, so the monitor
    /// timer and event callers must not wait for it. While the task runs,
    /// reconciliation reports the check as submitted, so rule recovery cannot
    /// settle a launch that is still in progress.
    fn detach_rule_check<F>(
        &self,
        check: &RuleCheck,
        launch: impl FnOnce(Arc<Self>, RuleCheck) -> F,
    ) -> CheckSubmission
    where
        F: std::future::Future<Output = CheckSubmission> + Send + 'static,
    {
        let Some(port) = self.this.upgrade() else {
            return CheckSubmission::NotSubmitted {
                error: "The monitor port is unavailable".into(),
            };
        };
        if !lock(&self.rule_checks).insert(check.monitor_id.clone()) {
            return CheckSubmission::Detached;
        }
        // Removes the ID when the task ends in any way, also when it is aborted.
        let in_flight = InFlightId {
            set: Arc::clone(&self.rule_checks),
            id: check.monitor_id.clone(),
        };
        let launched = launch(Arc::clone(&port), check.clone());
        let check = check.clone();
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let submission = launched.await;
            loop {
                let settled = match studio_runtime_services::event_write::unix_time() {
                    Ok(now) => studio_runtime_services::rules_tick::settle_submission(
                        &port.store,
                        &check,
                        &submission,
                        now,
                    )
                    .await
                    .map_err(|error| error.to_string()),
                    Err(_) => Err("System clock unavailable".to_owned()),
                };
                match settled {
                    Ok(()) => break,
                    Err(error) => {
                        eprintln!(
                            "rule check {} outcome was not recorded: {error}",
                            check.monitor_id
                        );
                        tokio::time::sleep(Duration::from_secs(RULE_CHECK_SETTLE_RETRY_SECONDS))
                            .await;
                    }
                }
            }
        });
        CheckSubmission::Detached
    }

    /// Map a written terminate request. While this process waits for the
    /// command/exec reply, that reply finishes the monitor. Without a waiting
    /// reply no exit receipt can arrive, so a terminated or already gone process
    /// completes the cancellation now.
    fn terminate_submission(
        &self,
        process_id: &str,
        result: Option<ProviderResponse>,
    ) -> Submission {
        if lock(&self.execs).contains(process_id) {
            return Submission::Written;
        }
        match result {
            Some(ProviderResponse::Reply(_)) => Submission::NotSent {
                reason: NotSentReason::MonitorMissing,
                error: "The command was terminated. Its exit status is unknown because its result was lost".into(),
            },
            Some(ProviderResponse::Rejected(value))
                if value.to_string().contains(NO_ACTIVE_COMMAND_EXEC) =>
            {
                Submission::NotSent {
                    reason: NotSentReason::MonitorMissing,
                    error: "The command was no longer running when it was cancelled".into(),
                }
            }
            _ => Submission::Written,
        }
    }

    /// Fail a managed monitor that never submitted its command. A monitor that
    /// already started, finished or changed owner is left to its own path.
    async fn fail_unstarted_monitor(&self, id: &str, reason: &str) -> Result<(), MonitorError> {
        let id = id.to_owned();
        let error = format!("The command was not started: {reason}");
        let now = studio_runtime_services::event_write::unix_time()
            .map_err(|_| MonitorError::Refused("System clock unavailable"))?;
        self.store
            .transaction_with(move |tx| {
                let (snapshot, _) = studio_runtime_services::monitor_runtime::snapshot(tx, &id)?;
                if snapshot.record()["status"] != "starting"
                    || snapshot.operation().is_some()
                    || snapshot
                        .record()
                        .get("ruleId")
                        .is_some_and(|value| !value.is_null())
                {
                    return Ok(());
                }
                studio_runtime_services::monitor_runtime::finish_in_transaction(
                    tx,
                    &id,
                    None,
                    None,
                    Some(&error),
                    now,
                )
            })
            .await
    }

    pub(crate) fn set_dispatch_wake(&self, wake: Arc<Notify>) {
        *self
            .dispatch_wake
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wake);
    }

    fn wake_dispatch(&self) {
        if let Some(wake) = self
            .dispatch_wake
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            wake.notify_one();
        }
    }

    pub(crate) fn set_preparation_resources(
        &self,
        resources: Arc<studio_runtime_services::account_worker::preparation::PreparationResources>,
    ) {
        *self
            .preparation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(resources);
    }

    fn request<'a>(
        &'a self,
        operation: &'a Operation,
        request_operation: String,
        method: Method,
        params: Value,
        deadline: Instant,
        actor: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<SessionReceipt, AccountSessionError>> {
        let Some(scope) = self.session.current_scope(&operation.account_key) else {
            return Box::pin(async {
                Err(AccountSessionError::Refused("No live provider session"))
            });
        };
        if scope.account != operation.account_key {
            return Box::pin(async {
                Err(AccountSessionError::Refused(
                    "Provider account scope changed",
                ))
            });
        }
        Box::pin(async move {
            self.session
                .provider_request(ProviderRequest {
                    account: operation.account_key.clone(),
                    operation: request_operation,
                    actor: actor.then(|| RequestActor {
                        agent: operation.agent.clone(),
                        epoch: operation.epoch,
                    }),
                    deleted_actor_session: None,
                    expected_scope: Some(scope),
                    method,
                    params,
                    deadline,
                })
                .await
        })
    }

    async fn dispatch_monitor_start(&self, id: &str, now: f64) -> Result<(), MonitorError> {
        use studio_runtime_store::{AgentId, AgentRepository, MonitorsRepository};

        let monitor_id = id.to_owned();
        let (monitor, actor) = self
            .store
            .transaction_with(move |tx| {
                let (snapshot, _actor) =
                    studio_runtime_services::monitor_runtime::snapshot(tx, &monitor_id)?;
                if snapshot.record()["status"] != "starting"
                    || snapshot
                        .record()
                        .get("ruleId")
                        .is_some_and(|value| !value.is_null())
                    || snapshot.operation().is_some()
                {
                    return Err(MonitorError::Refused(
                        "Monitor is not awaiting its first start",
                    ));
                }
                let saved = MonitorsRepository::get(tx, &monitor_id)?
                    .ok_or(MonitorError::Refused("Unknown command watch"))?;
                let agent = AgentRepository::get(tx, &AgentId::from(snapshot.agent()))?
                    .ok_or(MonitorError::Refused("Unknown agent"))?;
                if agent.id().as_ref() != snapshot.agent() {
                    return Err(MonitorError::Refused("Monitor owner changed"));
                }
                Ok((
                    saved.value().clone(),
                    serde_json::to_value(agent).map_err(studio_runtime_store::StoreError::from)?,
                ))
            })
            .await?;
        let timing: CommandTiming = serde_json::from_value(monitor.clone())
            .map_err(studio_runtime_store::StoreError::from)?;
        if now >= timing.created + timing.timeout_ms as f64 / 1000.0 {
            return Err(MonitorError::Refused("Monitor startup deadline expired"));
        }
        let agent_id = monitor["agent"]
            .as_str()
            .ok_or(MonitorError::Refused("Monitor owner is invalid"))?;
        let epoch = monitor["epoch"]
            .as_u64()
            .ok_or(MonitorError::Refused("Monitor epoch is invalid"))?;
        if actor["epoch"].as_u64() != Some(epoch)
            || actor["autoWake"] != true
            || actor["deletedAt"]
                .as_f64()
                .is_some_and(|deleted| deleted != 0.0)
        {
            return Err(MonitorError::Refused("The agent turn was stopped"));
        }
        let cwd = monitor["cwd"]
            .as_str()
            .filter(|cwd| Path::new(cwd).is_absolute())
            .ok_or(MonitorError::Refused(
                "Monitor working directory is unavailable",
            ))?;
        let account = monitor["accountKey"]
            .as_str()
            .filter(|account| !account.is_empty())
            .or_else(|| actor["accountKey"].as_str())
            .unwrap_or("default");
        let scope = self
            .session
            .ensure_session(account)
            .await
            .map_err(|source| MonitorError::ProviderRequest {
                stage: "monitor session preparation",
                source,
            })?;
        let operation = Operation {
            agent: agent_id.to_owned(),
            epoch: i64::try_from(epoch)
                .map_err(|_| MonitorError::Refused("Monitor epoch is invalid"))?,
            account_key: account.to_owned(),
            connection_id: scope.connection.clone(),
            evidence: BTreeMap::new(),
        };
        let resources = self
            .preparation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(MonitorError::Unavailable(
                "account-worker/monitor-preparation",
            ))?;
        let prepared =
            studio_runtime_services::account_worker::preparation::prepare_monitor_command(
                Arc::clone(&self.store),
                &resources,
                agent_id,
                epoch,
            )
            .await
            .map_err(|_| MonitorError::Refused("Monitor command preparation was refused"))?;
        let config_receipt = self
            .request(
                &operation,
                format!("monitor-config:{id}:{epoch}"),
                Method::ConfigRead,
                json!({"cwd":cwd,"includeLayers":false}),
                Instant::now() + Duration::from_secs(20),
                true,
            )
            .await
            .map_err(|source| MonitorError::ProviderRequest {
                stage: "monitor configuration request",
                source,
            })?;
        // AccountSessionRuntime owns receipt identity/scope validation, including
        // committed replays from the original connection after a restart.
        let config = match config_receipt.result {
            Some(ProviderResponse::Reply(value))
                if config_receipt.certainty == RequestCertainty::Written =>
            {
                let response: ConfigurationResponse =
                    serde_json::from_value(value).map_err(|_| {
                        MonitorError::Refused("Monitor configuration response is invalid")
                    })?;
                Value::Object(response.config)
            }
            Some(ProviderResponse::Rejected(value)) => {
                let failure: ConfigurationFailure =
                    serde_json::from_value(value).map_err(|_| {
                        MonitorError::Refused("Provider configuration rejection is invalid")
                    })?;
                return Err(MonitorError::ConfigurationRejected {
                    code: failure.code,
                    message: failure.message,
                });
            }
            _ => {
                return Err(MonitorError::ConfigurationReceipt {
                    certainty: config_receipt.certainty,
                    reason: config_receipt.not_sent_reason,
                });
            }
        };
        let command = monitor["command"]
            .as_str()
            .ok_or(MonitorError::Refused("Monitor command is invalid"))?;
        let argv = monitor_command(command, &config, actor["executionMode"] == "vm")
            .map_err(|_| MonitorError::Refused("Monitor command preparation was refused"))?;
        let mut params = json!({
            "command":argv,
            "cwd":cwd,
            "processId":id,
            "streamStdoutStderr":true,
            "timeoutMs":monitor["timeout_ms"],
        });
        apply_turn_permissions(&mut params, &prepared.settings, &actor)
            .map_err(|_| MonitorError::Refused("Monitor sandbox policy refused the command"))?;
        let start = studio_runtime_services::monitor_runtime::PreparedStart {
            monitor,
            actor,
            operation,
            params,
        };
        let _ = studio_runtime_services::monitor_driver::start(&self.store, self, id, &start, now)
            .await?;
        Ok(())
    }

    fn submission(receipt: SessionReceipt, operation: &str) -> Submission {
        if receipt.operation != operation {
            return Submission::Unknown;
        }
        match receipt.certainty {
            RequestCertainty::Written => Submission::Written,
            RequestCertainty::Unknown => Submission::Unknown,
            RequestCertainty::NotSent => Submission::NotSent {
                reason: match receipt.not_sent_reason {
                    Some(BrokerNotSentReason::NoLiveAccountWorker)
                    | Some(BrokerNotSentReason::ExpectedScopeMismatch) => {
                        NotSentReason::SessionMissing
                    }
                    _ => NotSentReason::Transient,
                },
                error: "Provider proves the monitor request was not sent".into(),
            },
        }
    }

    async fn persist_rule_monitor(&self, check: &RuleCheck) -> Result<Value, String> {
        use studio_runtime_store::{AgentId, AgentRepository, RulesRepository};
        let request_id = format!("rule:{}:{}", check.rule_id, check.check_number);
        let request = json!({
            "command":check.command,
            "timeout_ms":check.timeout_ms,
            "stall_timeout_seconds":0,
            "liveness_command":"",
            "wake_on":"exit",
        });
        let log_root = studio_runtime_services::monitor_output::log_root(self.store.root())
            .to_string_lossy()
            .into_owned();
        let check = check.clone();
        let check_id = check.rule_id.clone();
        self.store
            .transaction_with(move |tx| {
                let rule = RulesRepository::get(tx, &check_id)?.ok_or(MonitorError::Refused(
                    "Rule was removed before monitor admission",
                ))?;
                let actor = AgentRepository::get(tx, &AgentId::from(check.agent_id.as_str()))?
                    .ok_or(MonitorError::Refused("Rule agent is unavailable"))?;
                let check_snapshot =
                    studio_runtime_services::rules_engine::RuleSnapshot::from_records(
                        rule.value().clone(),
                        actor.value(),
                    )
                    .map_err(|_| MonitorError::Refused("Rule check record is invalid"))?;
                if !check_snapshot.launch_allowed(&check) {
                    return Err(MonitorError::Refused(
                        "Rule check changed before monitor admission",
                    ));
                }
                let cwd =
                    actor
                        .extra
                        .get("cwd")
                        .and_then(Value::as_str)
                        .ok_or(MonitorError::Refused(
                            "Rule working directory is unavailable",
                        ))?;
                if !Path::new(cwd).is_absolute() {
                    return Err(MonitorError::Refused(
                        "Rule working directory is not absolute",
                    ));
                }
                let workspace = PreparedRuleWorkspace {
                    agent_id: &check.agent_id,
                    cwd,
                };
                let admission = studio_runtime_services::monitor_runtime::MonitorAdmission {
                    actor_id: &check.agent_id,
                    epoch: i64::try_from(check.epoch)
                        .map_err(|_| MonitorError::Refused("Rule epoch is invalid"))?,
                    request_id: &request_id,
                    request: &request,
                    log_root: &log_root,
                };
                let monitor = studio_runtime_services::monitor_runtime::create_in_transaction(
                    tx,
                    &admission,
                    &workspace,
                    studio_runtime_services::event_write::unix_time()
                        .map_err(|_| MonitorError::Refused("System clock unavailable"))?,
                )?;
                if monitor["id"].as_str() != Some(&check.monitor_id)
                    || monitor["command"].as_str() != Some(&check.command)
                {
                    return Err(MonitorError::Refused(
                        "Rule monitor identity does not match its check",
                    ));
                }
                studio_runtime_services::monitor_runtime::bind_rule_in_transaction(
                    tx,
                    &check.monitor_id,
                    &check.rule_id,
                    check.account_key.as_deref().unwrap_or("default"),
                )?;
                // A prior exact request can be recovered, but another rule/check may
                // never take over this deterministic monitor identity.
                let saved = MonitorsRepository::get(tx, &check.monitor_id)?
                    .ok_or(MonitorError::Refused("Monitor record disappeared"))?;
                if saved.get("ruleId").and_then(Value::as_str) != Some(&check.rule_id)
                    || saved.get("command").and_then(Value::as_str) != Some(&check.command)
                {
                    return Err(MonitorError::Refused(
                        "Rule monitor identity has different content",
                    ));
                }
                Ok(saved.value().clone())
            })
            .await
            .map_err(|error: MonitorError| error.to_string())
    }

    async fn launch_rule_check(&self, check: &RuleCheck) -> Result<CheckSubmission, String> {
        let agent_id = studio_runtime_store::AgentId::from(check.agent_id.as_str());
        let actor = self
            .store
            .read_async("runtime-server/rule-monitor-owner-read", move |db| {
                studio_runtime_store::AgentRepository::get(db, &agent_id)
            })
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Rule agent is unavailable".to_owned())?;
        let actor_value = serde_json::to_value(&actor).map_err(|error| error.to_string())?;
        let approved = actor_value["yoloMode"] == true
            || actor_value["approvalPolicy"].as_str() == Some("never");
        if check.liveness && !approved {
            return Ok(CheckSubmission::NotSubmitted {
                error: "Liveness command not run because monitor approval is required.".into(),
            });
        }
        let resources = self
            .preparation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "account-worker/monitor-preparation is unavailable".to_owned())?;
        let prepared_action =
            studio_runtime_services::account_worker::preparation::prepare_monitor_command(
                Arc::clone(&self.store),
                &resources,
                &check.agent_id,
                check.epoch,
            )
            .await
            .map_err(|error| error.to_string())?;
        let monitor = self.persist_rule_monitor(check).await?;
        if monitor["status"] == "approval" || monitor["status"] != "starting" {
            return Ok(CheckSubmission::Accepted);
        }
        let cwd = actor_value["cwd"]
            .as_str()
            .filter(|cwd| Path::new(cwd).is_absolute())
            .ok_or_else(|| "Rule working directory is unavailable".to_owned())?;
        let operation = Operation {
            agent: check.agent_id.clone(),
            epoch: i64::try_from(check.epoch).map_err(|_| "Rule epoch is invalid")?,
            account_key: check
                .account_key
                .clone()
                .unwrap_or_else(|| "default".into()),
            connection_id: self
                .session
                .current_scope(check.account_key.as_deref().unwrap_or("default"))
                .ok_or_else(|| "No live provider session".to_owned())?
                .connection,
            evidence: BTreeMap::new(),
        };
        let config = provider_request(
            self.session.as_ref(),
            &operation,
            format!("monitor-config:{}:{}", check.monitor_id, check.epoch),
            Method::ConfigRead,
            json!({"cwd":cwd,"includeLayers":false}),
            Instant::now() + Duration::from_secs(20),
        )
        .await
        .map_err(|error| format!("Monitor configuration was not read: {error}"))?;
        let config = match config.result {
            Some(ProviderResponse::Reply(value))
                if config.certainty == RequestCertainty::Written =>
            {
                value["config"].clone()
            }
            Some(ProviderResponse::Rejected(value)) => return Err(value.to_string()),
            _ => return Err("Monitor configuration read is unknown".into()),
        };
        if !config.is_object() {
            return Err("Monitor configuration is invalid".into());
        }
        let command = monitor_command(
            &check.command,
            &config,
            actor_value["executionMode"] == "vm",
        )?;
        let mut params = json!({
            "command":command,
            "cwd":cwd,
            "processId":check.monitor_id,
            "streamStdoutStderr":true,
            "timeoutMs":check.timeout_ms,
        });
        apply_turn_permissions(&mut params, &prepared_action.settings, &actor_value)?;
        let prepared = studio_runtime_services::monitor_runtime::PreparedStart {
            monitor: monitor.clone(),
            actor: actor_value,
            operation: operation.clone(),
            params,
        };
        let now = studio_runtime_services::event_write::unix_time()
            .map_err(|_| "System clock unavailable".to_owned())?;
        let result = match studio_runtime_services::monitor_driver::start(
            &self.store,
            self,
            &check.monitor_id,
            &prepared,
            now,
        )
        .await
        {
            Ok(result) => result,
            Err(error) if error.store_failure_for_task() => return Ok(CheckSubmission::Unknown),
            Err(error) => return Err(error.to_string()),
        };
        Ok(match result {
            Submission::Written => CheckSubmission::Accepted,
            Submission::Unknown => CheckSubmission::Unknown,
            // The driver committed a definitive failure receipt; its consumer
            // finishes the monitor and linked rule in the normal result path.
            Submission::NotSent { .. } => CheckSubmission::Accepted,
        })
    }

    async fn reconcile_rule_check(&self, check: &RuleCheck) -> CheckReconciliation {
        // A detached launch task owns this identity and records its outcome.
        if lock(&self.rule_checks).contains(&check.monitor_id) {
            return CheckReconciliation::Submitted;
        }
        let monitor_id = check.monitor_id.clone();
        let loaded: Result<Option<(Value, MonitorSnapshot)>, MonitorError> = self
            .store
            .transaction_with(move |tx| {
                let Some(record) = MonitorsRepository::get(tx, &monitor_id)? else {
                    return Ok(None);
                };
                let (snapshot, _) =
                    studio_runtime_services::monitor_runtime::snapshot(tx, &monitor_id)?;
                Ok(Some((record.value().clone(), snapshot)))
            })
            .await;
        let Ok(Some((record, snapshot))) = loaded else {
            return CheckReconciliation::Unknown;
        };
        if record["ruleId"].as_str() != Some(&check.rule_id)
            || record["command"].as_str() != Some(&check.command)
            || record["agent"].as_str() != Some(&check.agent_id)
            || record["epoch"].as_u64() != Some(check.epoch)
        {
            return CheckReconciliation::Unknown;
        }
        if matches!(record["status"].as_str(), Some("approval" | "starting")) {
            return CheckReconciliation::Submitted;
        }
        let Some(operation) = snapshot.operation() else {
            return CheckReconciliation::Unknown;
        };
        let operation_id = format!("monitor:{}", check.monitor_id);
        match self
            .request(
                operation,
                format!("monitor-status:{}", check.monitor_id),
                Method::OperationStatus,
                json!({"operationId":operation_id}),
                Instant::now() + Duration::from_secs(10),
                false,
            )
            .await
        {
            Ok(receipt) if receipt.certainty == RequestCertainty::Written => {
                let Some(ProviderResponse::Reply(result)) = receipt.result else {
                    return CheckReconciliation::Unknown;
                };
                if result["accepted"] == true {
                    if let Some(response) = result.get("response").filter(|value| !value.is_null())
                    {
                        let response = response.get("result").unwrap_or(response);
                        let code = response["exitCode"].as_i64();
                        let error = response["error"].as_str().map(str::to_owned);
                        if code.is_some() || error.is_some() {
                            let queued = monitor_event_queue::enqueue_exit(
                                &self.store,
                                &check.monitor_id,
                                operation,
                                &format!("operation-status:{operation_id}"),
                                code,
                                error.as_deref(),
                                studio_runtime_services::event_write::unix_time().unwrap_or(0.0),
                            )
                            .await;
                            if queued.is_err() {
                                return CheckReconciliation::Unknown;
                            }
                            self.wake_dispatch();
                        }
                    }
                    CheckReconciliation::Submitted
                } else if result["accepted"] == false {
                    let reason = result["reason"]
                        .as_str()
                        .unwrap_or("Native monitor command was not accepted")
                        .to_owned();
                    let now = studio_runtime_services::event_write::unix_time().unwrap_or(0.0);
                    let monitor_id = check.monitor_id.clone();
                    let operation = operation.clone();
                    let finished = self
                        .store
                        .transaction_with(move |tx| {
                            let (current, actor) =
                                studio_runtime_services::monitor_runtime::snapshot(
                                    tx,
                                    &monitor_id,
                                )?;
                            if current.operation() != Some(&operation) {
                                return Err(MonitorError::Refused(
                                    "Monitor operation identity changed",
                                ));
                            }
                            let decision = studio_runtime_services::monitor_policy::finish(
                                &current,
                                Some(&operation),
                                None,
                                Some(&reason),
                                now,
                            )?;
                            studio_runtime_services::monitor_runtime::apply(
                                tx, &current, &actor, &decision, now,
                            )
                        })
                        .await;
                    if finished.is_ok() {
                        CheckReconciliation::Submitted
                    } else {
                        CheckReconciliation::Unknown
                    }
                } else {
                    CheckReconciliation::Unknown
                }
            }
            _ => CheckReconciliation::Unknown,
        }
    }

    async fn fail_unsubmitted_rule_check(&self, check: &RuleCheck, error: &str) -> Option<bool> {
        let now = studio_runtime_services::event_write::unix_time().ok()?;
        let monitor_id = check.monitor_id.clone();
        let rule_id = check.rule_id.clone();
        let error = error.to_owned();
        let outcome: Result<Option<bool>, MonitorError> = self
            .store
            .transaction_with(move |tx| {
                let Some(record) = MonitorsRepository::get(tx, &monitor_id)? else {
                    return Ok(None);
                };
                if record.get("ruleId").and_then(Value::as_str) != Some(&rule_id)
                    || record.get("status").and_then(Value::as_str) != Some("starting")
                    || record
                        .get("operation")
                        .is_some_and(|operation| !operation.is_null())
                {
                    return Ok(Some(false));
                }
                studio_runtime_services::monitor_runtime::finish_in_transaction(
                    tx,
                    &monitor_id,
                    None,
                    None,
                    Some(&error),
                    now,
                )?;
                Ok(Some(true))
            })
            .await;
        outcome.ok().flatten()
    }
}

impl MonitorNative for RetainedMonitorPort {
    fn submit<'a>(
        &'a self,
        operation: &'a Operation,
        effect: &'a Effect,
    ) -> futures_util::future::BoxFuture<'a, Submission> {
        Box::pin(async move {
            let Effect::Native {
                identity,
                method,
                params,
            } = effect
            else {
                return Submission::NotSent {
                    reason: NotSentReason::Transient,
                    error: "Invalid monitor native effect".into(),
                };
            };
            let Some(method) = Method::from_wire(method) else {
                return Submission::NotSent {
                    reason: NotSentReason::Transient,
                    error: "Unknown monitor provider method".into(),
                };
            };
            let timeout = if method == Method::CommandExec {
                params["timeoutMs"]
                    .as_u64()
                    .unwrap_or(DEFAULT_MONITOR_TIMEOUT_MS)
                    .saturating_add(MONITOR_TIMEOUT_GRACE_MS)
                    .min(MAX_MONITOR_TIMEOUT_WITH_GRACE_MS)
            } else {
                30_000
            };
            let process_id = params["processId"].as_str().unwrap_or_default().to_owned();
            // Only the request future below can deliver this command's exit.
            let _exec_reply =
                (method == Method::CommandExec && !process_id.is_empty()).then(|| {
                    lock(&self.execs).insert(process_id.clone());
                    InFlightId {
                        set: Arc::clone(&self.execs),
                        id: process_id.clone(),
                    }
                });
            let result = self
                .request(
                    operation,
                    identity.clone(),
                    method,
                    params.clone(),
                    Instant::now() + Duration::from_millis(timeout),
                    true,
                )
                .await;
            let receipt = match result {
                Ok(receipt) => receipt,
                Err(AccountSessionError::Unknown) => return Submission::Unknown,
                Err(error) => {
                    return Submission::NotSent {
                        reason: NotSentReason::SessionMissing,
                        error: error.to_string(),
                    };
                }
            };
            if receipt.operation != *identity {
                return Submission::Unknown;
            }
            if receipt.certainty != RequestCertainty::Written {
                return Self::submission(receipt, identity);
            }
            if method == Method::CommandExecTerminate {
                return self.terminate_submission(&process_id, receipt.result);
            }
            if method == Method::CommandExec {
                if let Err(error) = self
                    .session
                    .callback_barrier(
                        &receipt.scope.account,
                        &receipt.scope,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                {
                    return if matches!(error, AccountSessionError::Unknown) {
                        Submission::Unknown
                    } else {
                        Submission::NotSent {
                            reason: NotSentReason::Transient,
                            error: error.to_string(),
                        }
                    };
                }
                let id = params["processId"].as_str().unwrap_or_default();
                if id.is_empty() {
                    return Submission::Unknown;
                }
                let Some(response) = receipt.result else {
                    return Submission::Unknown;
                };
                if self.command_result(id, operation, response).await.is_err() {
                    return Submission::Unknown;
                }
            } else if method == Method::CommandExecWrite || method == Method::CommandExecResize {
                let ticket = identity.rsplit(':').next().unwrap_or(identity).to_owned();
                let reply = match receipt.result {
                    Some(ProviderResponse::Reply(value)) => InputReply::Acknowledged(value),
                    Some(ProviderResponse::Rejected(value)) => {
                        InputReply::Rejected(value.to_string())
                    }
                    None => InputReply::Unknown("Input acknowledgement is pending".into()),
                };
                self.input_replies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(ticket, reply);
            }
            Submission::Written
        })
    }
}

impl RuleMonitor for RetainedMonitorPort {
    fn available(&self) -> bool {
        self.preparation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
    fn launch<'a>(
        &'a self,
        check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
        Box::pin(async move {
            self.detach_rule_check(check, |port, check| async move {
                match port.launch_rule_check(&check).await {
                    Ok(submission) => submission,
                    Err(error) => match port.fail_unsubmitted_rule_check(&check, &error).await {
                        Some(true) | None => CheckSubmission::NotSubmitted { error },
                        Some(false) => CheckSubmission::Unknown,
                    },
                }
            })
        })
    }
    fn reconcile<'a>(
        &'a self,
        check: &'a RuleCheck,
    ) -> futures_util::future::BoxFuture<'a, CheckReconciliation> {
        Box::pin(self.reconcile_rule_check(check))
    }
    fn cancel<'a>(
        &'a self,
        monitor_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
        Box::pin(async move {
            let monitor_id_owned = monitor_id.to_owned();
            let agent = self
                .store
                .read_async(
                    "runtime-server/rule-monitor-cancel-read",
                    move |db| -> Result<Option<String>, RulesTickError> {
                        let Some(monitor) = MonitorsRepository::get(db, &monitor_id_owned)? else {
                            return Ok(None);
                        };
                        let Some(agent) = monitor.get("agent").and_then(Value::as_str) else {
                            return Err(RulesTickError::MonitorUnavailable);
                        };
                        Ok(Some(agent.to_owned()))
                    },
                )
                .await?;
            let Some(agent) = agent else {
                return Ok(());
            };
            studio_runtime_services::monitor_driver::cancel(
                &self.store,
                self,
                monitor_id,
                Some(&agent),
                studio_runtime_services::event_write::unix_time()
                    .map_err(|_| RulesTickError::MonitorUnavailable)?,
            )
            .await
            .map_err(|_| RulesTickError::MonitorUnavailable)?;
            Ok(())
        })
    }
}

struct PreparedRuleWorkspace<'a> {
    agent_id: &'a str,
    cwd: &'a str,
}

impl studio_runtime_services::monitor_runtime::WorkspaceAdmission for PreparedRuleWorkspace<'_> {
    fn check(
        &self,
        _tx: &studio_runtime_store::Transaction<'_>,
        actor: &studio_runtime_store::AgentRecord,
    ) -> Result<(), MonitorError> {
        if actor.id().as_ref() == self.agent_id
            && actor.extra.get("cwd").and_then(Value::as_str) == Some(self.cwd)
            && Path::new(self.cwd).is_absolute()
        {
            Ok(())
        } else {
            Err(MonitorError::Refused(
                "Rule workspace changed during preparation",
            ))
        }
    }
}

impl MonitorPort for RetainedMonitorPort {
    fn start_monitor<'a>(
        &'a self,
        id: &'a str,
        now: f64,
    ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
        Box::pin(async move { self.spawn_monitor_start(id, now) })
    }

    fn next<'a>(
        &'a self,
    ) -> futures_util::future::BoxFuture<'a, Result<Option<Delivery>, MonitorError>> {
        Box::pin(async move { monitor_event_queue::next(&self.store).await })
    }

    fn acknowledge<'a>(
        &'a self,
        delivery: &'a Delivery,
    ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
        Box::pin(monitor_event_queue::acknowledge(&self.store, delivery))
    }

    fn reattach<'a>(
        &'a self,
        snapshot: &'a MonitorSnapshot,
    ) -> futures_util::future::BoxFuture<'a, Result<ReattachOutcome, MonitorError>> {
        Box::pin(async move {
            let Some(operation) = snapshot.operation() else {
                return Ok(ReattachOutcome::Gone("no_operation".into()));
            };
            let operation_id = format!("monitor:{}", snapshot.id());
            let Some(scope) = self.session.current_scope(&operation.account_key) else {
                return Ok(ReattachOutcome::Pending);
            };
            let receipt = match self
                .request(
                    operation,
                    format!("monitor-status:{}", snapshot.id()),
                    Method::OperationStatus,
                    json!({"operationId":operation_id}),
                    Instant::now() + Duration::from_secs(10),
                    false,
                )
                .await
            {
                Ok(receipt) => receipt,
                Err(_) => return Ok(ReattachOutcome::Pending),
            };
            if receipt.certainty != RequestCertainty::Written {
                return Ok(ReattachOutcome::Pending);
            }
            let observation: OperationObservation = match receipt.result {
                Some(ProviderResponse::Reply(value)) => match serde_json::from_value(value) {
                    Ok(observation) => observation,
                    Err(_) => {
                        return Ok(ReattachOutcome::Gone("invalid_provider_observation".into()));
                    }
                },
                Some(ProviderResponse::Rejected(_)) => {
                    return Ok(ReattachOutcome::Gone("provider_rejected".into()));
                }
                None => return Ok(ReattachOutcome::Pending),
            };
            if !observation.accepted {
                return Ok(ReattachOutcome::Gone(
                    observation
                        .reason
                        .unwrap_or_else(|| "operation_not_accepted".into()),
                ));
            }
            if let Some(response) = observation.response {
                let response = match response {
                    CommandResponse::Result { result } => ProviderResponse::Reply(
                        serde_json::to_value(result)
                            .map_err(studio_runtime_store::StoreError::from)?,
                    ),
                    CommandResponse::Error { error } => ProviderResponse::Rejected(error),
                    CommandResponse::Direct(result) => ProviderResponse::Reply(
                        serde_json::to_value(result)
                            .map_err(studio_runtime_store::StoreError::from)?,
                    ),
                };
                return match self
                    .command_result(snapshot.id(), operation, response)
                    .await
                {
                    Ok(()) => Ok(ReattachOutcome::TerminalQueued),
                    Err(MonitorError::Refused(_)) => {
                        Ok(ReattachOutcome::Gone("invalid_terminal_receipt".into()))
                    }
                    Err(error) => Err(error),
                };
            }
            let Some(native_id) = observation.native_id else {
                return Ok(ReattachOutcome::Pending);
            };
            Ok(ReattachOutcome::Running(ReattachProof {
                operation: operation.clone(),
                operation_id,
                native_id,
                current_connection: scope.connection,
            }))
        })
    }

    fn await_exit(&self, snapshot: &MonitorSnapshot) -> Result<(), MonitorError> {
        let operation = snapshot
            .operation()
            .ok_or(MonitorError::Refused("Missing retained command identity"))?;
        let timing: CommandTiming = serde_json::from_value(snapshot.record().clone())
            .map_err(studio_runtime_store::StoreError::from)?;
        let now = studio_runtime_services::event_write::unix_time()?;
        let remaining =
            ((timing.created + timing.timeout_ms as f64 / 1000.0 - now).max(0.0) * 1000.0) as u64;
        self.await_retained_exit(
            snapshot.id(),
            operation,
            Duration::from_millis(remaining.saturating_add(MONITOR_TIMEOUT_GRACE_MS)),
        )
    }

    fn probe<'a>(
        &'a self,
        id: &'a str,
        generation: i64,
        command: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
        Box::pin(async move {
            let monitor_id = id.to_owned();
            let (snapshot, actor) = self
                .store
                .transaction_with(move |tx| {
                    studio_runtime_services::monitor_runtime::snapshot(tx, &monitor_id)
                })
                .await?;
            let operation = snapshot
                .operation()
                .cloned()
                .ok_or(MonitorError::Refused("Monitor operation is unavailable"))?;
            let resources = self
                .preparation
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .ok_or(MonitorError::Unavailable(
                    "account-worker/monitor-preparation",
                ))?;
            let store = Arc::clone(&self.store);
            let session = Arc::clone(&self.session);
            let dispatch_wake = self
                .dispatch_wake
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let id = id.to_owned();
            let command = command.to_owned();
            let actor = serde_json::to_value(actor)
                .map_err(|_| MonitorError::Refused("Monitor actor is invalid"))?;
            let probe_task = tokio::spawn(async move {
                let prepare_store = Arc::clone(&store);
                let prepare_resources = Arc::clone(&resources);
                let prepare_agent = operation.agent.clone();
                let prepared_generation =
                    u64::try_from(generation).map_err(|_| "Invalid liveness generation".to_owned());
                let prepared = match prepared_generation {
                    Ok(generation) => prepare_monitor_permissions(
                        prepare_store,
                        prepare_resources,
                        &prepare_agent,
                        generation,
                    )
                    .await
                    .map_err(|error| error.to_string()),
                    Err(error) => Err(error),
                };
                let result = match prepared {
                    Ok(settings) => run_probe(
                        session.as_ref(),
                        MonitorProbeRequest {
                            id: &id,
                            generation,
                            command: &command,
                            operation: &operation,
                            actor,
                        },
                        settings,
                    )
                    .await
                    .unwrap_or_else(|error| json!({"error":error})),
                    Err(error) => json!({"error":error}),
                };
                if monitor_event_queue::enqueue_probe(&store, &id, &operation, generation, &result)
                    .await
                    .is_err()
                {
                    eprintln!("monitor liveness probe result could not be persisted");
                } else if let Some(wake) = dispatch_wake {
                    wake.notify_one();
                }
            });
            drop(probe_task);
            Ok(())
        })
    }

    fn input_reply<'a>(
        &'a self,
        _operation: &'a Operation,
        ticket: &'a str,
    ) -> futures_util::future::BoxFuture<'a, InputReply> {
        Box::pin(async move {
            self.input_replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(ticket)
                .unwrap_or_else(|| InputReply::Unknown("Input acknowledgement is unknown".into()))
        })
    }
}

/// Monitor command preparation uses the shared permission builder without a
/// turn-start attempt gate: `prepare_monitor_command(store, resources,
/// agent_id, epoch)`. Its settings select sandboxPolicy/permissionProfile.
async fn prepare_monitor_permissions(
    store: Arc<RuntimeStore>,
    resources: Arc<studio_runtime_services::account_worker::preparation::PreparationResources>,
    agent_id: &str,
    epoch: u64,
) -> Result<Value, String> {
    let prepared = studio_runtime_services::account_worker::preparation::prepare_monitor_command(
        store, &resources, agent_id, epoch,
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(prepared.settings)
}

struct MonitorProbeRequest<'a> {
    id: &'a str,
    generation: i64,
    command: &'a str,
    operation: &'a studio_runtime_services::monitor_policy::Operation,
    actor: Value,
}

async fn run_probe(
    session: &dyn AccountSessionPort,
    request: MonitorProbeRequest<'_>,
    settings: Value,
) -> Result<Value, String> {
    let MonitorProbeRequest {
        id,
        generation,
        command,
        operation,
        actor,
    } = request;
    if actor["yoloMode"]
        .as_bool()
        .or_else(|| actor["approvalPolicy"].as_str().map(|v| v == "never"))
        != Some(true)
    {
        return Err("Liveness command not run because monitor approval is required.".into());
    }
    let cwd = actor["cwd"]
        .as_str()
        .filter(|cwd| Path::new(cwd).is_absolute())
        .ok_or_else(|| "Thread working directory is unavailable".to_owned())?;
    let config_operation = format!("monitor-config:{id}:{generation}");
    let config_receipt = provider_request(
        session,
        operation,
        config_operation,
        Method::ConfigRead,
        json!({"cwd":cwd,"includeLayers":false}),
        Instant::now() + Duration::from_secs(20),
    )
    .await?;
    let config = match config_receipt.result {
        Some(ProviderResponse::Reply(value))
            if config_receipt.certainty == RequestCertainty::Written =>
        {
            value["config"].clone()
        }
        Some(ProviderResponse::Rejected(value)) => return Err(value.to_string()),
        _ => return Err("Provider configuration read is unknown".into()),
    };
    if !config.is_object() {
        return Err("Provider configuration is invalid; liveness command was not submitted".into());
    }
    let guest = actor["executionMode"].as_str() == Some("vm");
    let argv = monitor_command(command, &config, guest)?;
    let mut params = json!({
        "command":argv,
        "cwd":cwd,
        "processId":format!("liveness:{id}:{generation}"),
        "streamStdoutStderr":true,
        "timeoutMs":60000,
    });
    apply_turn_permissions(&mut params, &settings, &actor)?;
    let receipt = provider_request(
        session,
        operation,
        format!("liveness:{id}:{generation}"),
        Method::CommandExec,
        params,
        Instant::now() + Duration::from_secs(65),
    )
    .await?;
    if receipt.certainty != RequestCertainty::Written {
        return Err("Liveness command outcome is unknown; it will not be resent".into());
    }
    match receipt.result {
        Some(ProviderResponse::Reply(value)) => {
            let code = value["exitCode"].as_i64();
            let stdout = value["stdout"].as_str().unwrap_or_default();
            let stderr = value["stderr"].as_str().unwrap_or_default();
            if code.is_none() {
                return Err(value["error"]
                    .as_str()
                    .unwrap_or("Command returned no exit code; outcome unknown")
                    .into());
            }
            let output = format!("{stdout}{stderr}");
            let output = output
                .chars()
                .rev()
                .take(12_000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            Ok(json!({"exitCode":code,"output":output}))
        }
        Some(ProviderResponse::Rejected(value)) => Err(value.to_string()),
        None => Err("Liveness command outcome is unknown; it will not be resent".into()),
    }
}

async fn provider_request(
    session: &dyn AccountSessionPort,
    operation: &studio_runtime_services::monitor_policy::Operation,
    identity: String,
    method: Method,
    params: Value,
    deadline: Instant,
) -> Result<SessionReceipt, String> {
    let scope = session
        .current_scope(&operation.account_key)
        .ok_or_else(|| "No live provider session".to_owned())?;
    let expected_operation = identity.clone();
    let expected_scope = scope.clone();
    let receipt = session
        .provider_request(ProviderRequest {
            account: operation.account_key.clone(),
            operation: identity,
            actor: Some(RequestActor {
                agent: operation.agent.clone(),
                epoch: operation.epoch,
            }),
            deleted_actor_session: None,
            expected_scope: Some(scope),
            method,
            params,
            deadline,
        })
        .await
        .map_err(|error| error.to_string())?;
    if receipt.operation != expected_operation || receipt.scope != expected_scope {
        return Err("Provider receipt identity is unknown".into());
    }
    Ok(receipt)
}

fn apply_turn_permissions(
    params: &mut Value,
    settings: &Value,
    actor: &Value,
) -> Result<(), String> {
    if let Some(policy) = settings
        .get("sandboxPolicy")
        .filter(|value| !value.is_null())
    {
        params["sandboxPolicy"] = policy.clone();
        return Ok(());
    }
    let sandbox = actor
        .get("sandbox")
        .filter(|value| studio_runtime_store::truthy(Some(value)));
    if sandbox.is_none() {
        return Err("Thread sandbox is unknown; refusing liveness command".into());
    }
    if let Some(profile) = actor["profile"]["id"].as_str().filter(|id| !id.is_empty()) {
        params["permissionProfile"] = json!(profile);
    } else {
        params["sandboxPolicy"] = sandbox.cloned().unwrap_or(Value::Null);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_runtime_services::rules_engine::RuleSnapshot;
    use studio_runtime_store::{AgentRecord, AgentRepository, RuleRecord, RulesRepository};

    struct NoSession;
    impl AccountSessionPort for NoSession {}

    /// A provider whose command/exec answers only when released, and whose
    /// terminate answer the test selects.
    #[derive(Default)]
    struct ExecSession {
        terminate: Mutex<Option<ProviderResponse>>,
    }
    impl AccountSessionPort for ExecSession {
        fn current_scope(
            &self,
            account: &str,
        ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope> {
            Some(
                studio_runtime_services::account_session_runtime::ConnectionScope {
                    account: account.to_owned(),
                    connection: "connection".into(),
                    generation: 1,
                },
            )
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            SessionReceipt,
        > {
            Box::pin(async move {
                if request.method == Method::CommandExec {
                    std::future::pending::<()>().await;
                }
                Ok(SessionReceipt {
                    scope: request.expected_scope.clone().expect("scope"),
                    operation: request.operation,
                    ticket: None,
                    certainty: RequestCertainty::Written,
                    result: lock(&self.terminate).clone(),
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }

    fn owner(root: &Path, auto_wake: bool) -> Value {
        json!({"id":"owner","rootId":"owner","epoch":0,"status":"waiting",
            "autoWake":auto_wake,"cwd":root.to_string_lossy(),"accountKey":"default",
            "provider":"codex"})
    }

    fn starting_watch(root: &Path, id: &str) -> Value {
        let log = studio_runtime_services::monitor_output::log_root(root).join(format!("{id}.log"));
        json!({"id":id,"agent":"owner","epoch":0,"turnId":null,"requestId":"request",
            "command":"gh run watch 1 > watch.log 2>&1","cwd":root.to_string_lossy(),
            "timeout_ms":60000,"stallTimeoutSeconds":1800,"livenessCommand":"",
            "activityAt":1.0,"activityGeneration":0,"stallWakeGeneration":-1,"wakeOn":"exit",
            "successExitCodes":[0],"outputPatterns":[],"outputEventSequence":0,
            "outputEventCount":0,"outputEventIds":[],"status":"starting","created":1.0,
            "exitCode":null,"tail":"","bytes":0,"interactive":false,"ruleId":null,
            "log":log.to_string_lossy(),"cancelRequested":false})
    }

    #[tokio::test]
    async fn monitor_completion_waiting_owner_starts_queued_command_without_a_live_turn() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        struct Session {
            live: AtomicBool,
            executions: AtomicUsize,
            replay_config: bool,
            refuse_config_once: AtomicBool,
        }
        impl AccountSessionPort for Session {
            fn current_scope(
                &self,
                account: &str,
            ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope>
            {
                self.live.load(Ordering::SeqCst).then(|| {
                    studio_runtime_services::account_session_runtime::ConnectionScope {
                        account: account.into(),
                        connection: "new".into(),
                        generation: 2,
                    }
                })
            }
            fn ensure_session<'a>(
                &'a self,
                account: &'a str,
            ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
                'a,
                studio_runtime_services::account_session_runtime::ConnectionScope,
            > {
                Box::pin(async move {
                    self.live.store(true, Ordering::SeqCst);
                    Ok(self.current_scope(account).expect("ensured session"))
                })
            }
            fn provider_request<'a>(
                &'a self,
                request: ProviderRequest,
            ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
                'a,
                SessionReceipt,
            > {
                Box::pin(async move {
                    let mut scope = request.expected_scope.expect("scope");
                    let (result, replayed) = match request.method {
                        Method::ConfigRead => {
                            if self.refuse_config_once.swap(false, Ordering::SeqCst) {
                                return Err(AccountSessionError::Refused(
                                    "temporary fixture config refusal",
                                ));
                            }
                            if self.replay_config {
                                scope.connection = "old".into();
                                scope.generation = 1;
                            }
                            (
                                json!({"config":{"allow_login_shell":false}}),
                                self.replay_config,
                            )
                        }
                        Method::CommandExec => {
                            self.executions.fetch_add(1, Ordering::SeqCst);
                            (json!({"exitCode":0}), false)
                        }
                        _ => panic!("unexpected monitor preparation request"),
                    };
                    Ok(SessionReceipt {
                        scope,
                        operation: request.operation,
                        ticket: None,
                        certainty: RequestCertainty::Written,
                        result: Some(ProviderResponse::Reply(result)),
                        not_sent_reason: None,
                        replayed,
                    })
                })
            }
            fn callback_barrier<'a>(
                &'a self,
                _: &'a str,
                _: &'a studio_runtime_services::account_session_runtime::ConnectionScope,
                _: Instant,
            ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<'a, ()>
            {
                Box::pin(async { Ok(()) })
            }
        }
        for restart in [false, true] {
            for replay_config in [false, true] {
                let root = tempfile::tempdir().expect("root");
                let store = RuntimeStore::open(Some(root.path())).await.expect("store");
                let now = studio_runtime_services::event_write::unix_time().expect("clock");
                let mut owner = owner(root.path(), true);
                owner["status"] = json!("waiting_on_monitor");
                owner["threadId"] = json!("thread");
                owner["turnId"] = Value::Null;
                owner["inFlight"] = json!(false);
                owner["sandbox"] = json!({"type":"readOnly"});
                owner["model"] = json!("gpt-6");
                owner["isLead"] = json!(false);
                owner["role"] = json!("implementer");
                let mut watch = starting_watch(root.path(), "watch");
                watch["created"] = json!(now);
                store
                    .transaction(move |tx| {
                        AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                        studio_runtime_store::Table::new(tx)
                            .put(&studio_runtime_store::MonitorRecord::from_value(watch)?)?;
                        Ok::<_, studio_runtime_store::StoreError>(())
                    })
                    .await
                    .expect("queue start after owner turn completed");
                let store = if restart {
                    drop(store);
                    let reopened = RuntimeStore::open(Some(root.path())).await.expect("reopen");
                    studio_runtime_services::monitor_runtime::recover_on_startup(&reopened, now)
                        .await
                        .expect("recover queued intent");
                    reopened
                } else {
                    store
                };
                let store = Arc::new(store);
                let session = Arc::new(Session {
                    live: AtomicBool::new(false),
                    executions: AtomicUsize::new(0),
                    replay_config,
                    refuse_config_once: AtomicBool::new(replay_config),
                });
                let port = RetainedMonitorPort::new(
                    Arc::clone(&store),
                    Arc::clone(&session) as Arc<dyn AccountSessionPort>,
                );
                port.set_preparation_resources(Arc::new(studio_runtime_services::account_worker::preparation::PreparationResources::bundled(root.path().to_string_lossy().into(),studio_runtime_services::model_catalog::ModelCatalogService::new()).expect("resources")));
                if replay_config {
                    let error = port
                        .dispatch_monitor_start("watch", now)
                        .await
                        .expect_err("temporary config refusal");
                    assert!(
                        error
                            .to_string()
                            .contains("Refused(\"temporary fixture config refusal\")"),
                        "{error}"
                    );
                    assert_eq!(session.executions.load(Ordering::SeqCst), 0);
                }
                let runtime = studio_runtime_services::monitor_runtime_wiring::MonitorRuntime::new(
                    Arc::clone(&store),
                    Arc::clone(&port) as Arc<dyn MonitorPort>,
                    Arc::new(Notify::new()),
                );
                port.set_dispatch_wake(runtime.dispatch_wake_handle());
                let wake = runtime.dispatch_wake_handle();
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let changed = wake.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        runtime.tick(now).await.expect("dispatch queued start");
                        let record = store
                            .read_async("test/queued-start", |tx| {
                                MonitorsRepository::get(tx, "watch")
                            })
                            .await
                            .expect("read")
                            .expect("watch");
                        if record.value()["status"] == "completed" {
                            break;
                        }
                        changed.await;
                    }
                })
                .await
                .expect("queued monitor starts and finishes without a turn");
                runtime.tick(now + 1.0).await.expect("replay dispatch");
                assert_eq!(
                    session.executions.load(Ordering::SeqCst),
                    1,
                    "restart={restart} replay={replay_config}"
                );
                let agent = store
                    .read_async("test/owner-turn", |tx| {
                        AgentRepository::get(tx, &"owner".into())
                    })
                    .await
                    .expect("read")
                    .expect("owner");
                assert_eq!(agent.value()["turnId"], Value::Null);
            }
        }
    }

    #[tokio::test]
    async fn reattached_exit_uses_retained_receipt_events_without_another_provider_write() {
        for already_present in [false, true] {
            let root = tempfile::tempdir().expect("state root");
            let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
            let now = studio_runtime_services::event_write::unix_time().expect("clock");
            let operation = Operation {
                agent: "owner".into(),
                epoch: 0,
                account_key: "default".into(),
                connection_id: "old".into(),
                evidence: BTreeMap::new(),
            };
            let mut watch = starting_watch(root.path(), "watch");
            watch["status"] = json!("running");
            watch["created"] = json!(now);
            watch["operation"] = json!(operation);
            let owner = owner(root.path(), true);
            let key = format!(
                "retained:default:{}",
                studio_runtime_store::sha256_hex(b"monitor:watch")
            );
            let submitted = json!({
                "call":{"scope":{"account":"default","connection":"old","generation":1},
                    "operation":"monitor:watch","actor":{"agent":"owner","epoch":0},
                    "transfer":null,"deleted_actor_session":null,"method":"command/exec",
                    "params":{"processId":"watch","cwd":root.path().to_string_lossy(),"command":["true"]}},
                "request":{"id":key,"generation":1,"method":"command/exec","operation":key,
                    "thread":null,"lightweight":true,"deadline":60000,"remoteId":42}
            });
            let submitted_id = format!("account-worker:{key}:submitted");
            store.transaction(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                studio_runtime_store::Table::new(tx).put(&studio_runtime_store::MonitorRecord::from_value(watch)?)?;
                tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,'test',?2)", (submitted_id, submitted.to_string()))?;
                Ok::<_, studio_runtime_store::StoreError>(())
            }).await.expect("persist retained command before restart");
            studio_runtime_services::monitor_runtime::recover_on_startup(&store, now)
                .await
                .expect("startup");
            let (snapshot, _) = store
                .read_async("test/snapshot", |tx| {
                    studio_runtime_services::monitor_runtime::snapshot(tx, "watch")
                })
                .await
                .expect("snapshot");
            let proof = ReattachProof {
                operation: operation.clone(),
                operation_id: "monitor:watch".into(),
                native_id: 42,
                current_connection: "current".into(),
            };
            studio_runtime_services::monitor_runtime::reattach(&store, "watch", &proof, now)
                .await
                .expect("reattach");
            let port = RetainedMonitorPort::new(Arc::clone(&store), Arc::new(NoSession));
            let wake = Arc::new(Notify::new());
            port.set_dispatch_wake(Arc::clone(&wake));
            async fn receipt(store: &RuntimeStore, key: String) {
                store.transaction(move |tx| {
                    for (id, result) in [
                        (format!("account-worker:{key}:result"), json!({"result":{"Reply":{"exitCode":0}},"cursor":1})),
                        ("account-worker:cursor:default:1:ack".into(),json!(1)),
                    ] {
                        tx.execute("INSERT OR REPLACE INTO runtime_operation_receipts(id,signature,result) VALUES (?1,'test',?2)",(id,result.to_string()))?;
                    }
                    Ok::<_, studio_runtime_store::StoreError>(())
                }).await.expect("retained native reply and checkpoint commit");
            }
            if already_present {
                receipt(&store, key.clone()).await;
            }
            port.await_exit(&snapshot).expect("restore exit waiter");
            if !already_present {
                receipt(&store, key.clone()).await;
            }
            tokio::time::timeout(Duration::from_secs(10), wake.notified())
                .await
                .expect("receipt commit wakes restored waiter");
            let runtime = studio_runtime_services::monitor_runtime_wiring::MonitorRuntime::new(
                Arc::clone(&store),
                Arc::clone(&port) as Arc<dyn MonitorPort>,
                Arc::new(Notify::new()),
            );
            runtime.tick(now + 1.0).await.expect("terminal delivery");
            receipt(&store, key).await;
            runtime.tick(now + 2.0).await.expect("receipt replay");
            let count: i64 = store.read_async("test/wake-count",|tx| {
                tx.query_row("SELECT count(*) FROM runtime_events WHERE id='monitor:watch' AND kind='monitor_exit' AND status='pending'",[],|row|row.get(0)).map_err(studio_runtime_store::StoreError::from)
            }).await.expect("wake count");
            assert_eq!(count, 1);
            let owner = store
                .read_async("test/owner", |tx| AgentRepository::get(tx, &"owner".into()))
                .await
                .expect("owner read")
                .expect("owner");
            assert_eq!(owner.status, studio_runtime_store::AgentStatus::Queued);
        }
    }

    #[tokio::test]
    async fn reattached_exit_inactive_owner_terminates_survivor_without_waking() {
        struct Survivor {
            terminated: std::sync::atomic::AtomicUsize,
            confirmed: bool,
        }
        impl AccountSessionPort for Survivor {
            fn current_scope(
                &self,
                account: &str,
            ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope>
            {
                Some(
                    studio_runtime_services::account_session_runtime::ConnectionScope {
                        account: account.into(),
                        connection: "new".into(),
                        generation: 2,
                    },
                )
            }
            fn provider_request<'a>(
                &'a self,
                request: ProviderRequest,
            ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
                'a,
                SessionReceipt,
            > {
                Box::pin(async move {
                    let response = match request.method {
                        Method::OperationStatus => json!({"accepted":true,"nativeId":42}),
                        Method::CommandExecTerminate => {
                            assert_eq!(request.operation, "monitor-cancel:watch");
                            assert_eq!(request.params, json!({"processId":"watch"}));
                            self.terminated
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            json!({})
                        }
                        _ => panic!("inactive owner must never start or resume a command"),
                    };
                    let result =
                        if request.method == Method::CommandExecTerminate && !self.confirmed {
                            None
                        } else {
                            Some(ProviderResponse::Reply(response))
                        };
                    Ok(SessionReceipt {
                        scope: request.expected_scope.expect("scope"),
                        operation: request.operation,
                        ticket: None,
                        certainty: RequestCertainty::Written,
                        result,
                        not_sent_reason: None,
                        replayed: false,
                    })
                })
            }
        }
        for (boundary, confirmed) in [
            json!({"deletedAt":1}),
            json!({"autoWake":false,"status":"paused"}),
            json!({"epoch":1}),
        ]
        .into_iter()
        .flat_map(|boundary| [true, false].map(move |confirmed| (boundary.clone(), confirmed)))
        {
            let root = tempfile::tempdir().expect("root");
            let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
            let mut owner = owner(root.path(), true);
            for (key, value) in boundary.as_object().expect("boundary") {
                owner[key] = value.clone();
            }
            let mut watch = starting_watch(root.path(), "watch");
            watch["status"] = json!("running");
            watch["operation"] = json!(Operation {
                agent: "owner".into(),
                epoch: 0,
                account_key: "default".into(),
                connection_id: "old".into(),
                evidence: BTreeMap::new()
            });
            store
                .transaction(move |tx| {
                    AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                    studio_runtime_store::Table::new(tx)
                        .put(&studio_runtime_store::MonitorRecord::from_value(watch)?)?;
                    Ok::<_, studio_runtime_store::StoreError>(())
                })
                .await
                .expect("persist survivor");
            studio_runtime_services::monitor_runtime::recover_on_startup(&store, 2.0)
                .await
                .expect("restart");
            let session = Arc::new(Survivor {
                terminated: std::sync::atomic::AtomicUsize::new(0),
                confirmed,
            });
            let port = RetainedMonitorPort::new(
                Arc::clone(&store),
                Arc::clone(&session) as Arc<dyn AccountSessionPort>,
            );
            let runtime = studio_runtime_services::monitor_runtime_wiring::MonitorRuntime::new(
                Arc::clone(&store),
                port,
                Arc::new(Notify::new()),
            );
            runtime.tick(3.0).await.expect("reconcile and terminate");
            runtime.tick(4.0).await.expect("replay");
            assert_eq!(
                session.terminated.load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            let (watch, owner, event) = store
                .read_async("test/inactive-terminal", |tx| {
                    Ok::<_, studio_runtime_store::StoreError>((
                        MonitorsRepository::get(tx, "watch")?.expect("watch"),
                        AgentRepository::get(tx, &"owner".into())?.expect("owner"),
                        tx.query_row(
                            "SELECT status FROM runtime_events WHERE id='monitor:watch'",
                            [],
                            |row| row.get::<_, String>(0),
                        )?,
                    ))
                })
                .await
                .expect("terminal evidence");
            assert_eq!(
                watch.value()["status"],
                if confirmed { "cancelled" } else { "lost" }
            );
            if !confirmed {
                assert_eq!(
                    watch.value()["error"],
                    "Command outcome unknown after losing the provider result; not rerun."
                );
            }
            assert_ne!(owner.status, studio_runtime_store::AgentStatus::Queued);
            assert_eq!(event, "cancelled");
        }
    }

    #[tokio::test]
    async fn reattached_exit_unsupported_status_decides_loss_at_the_monitor_deadline() {
        struct UnsupportedStatus;
        impl AccountSessionPort for UnsupportedStatus {
            fn current_scope(
                &self,
                account: &str,
            ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope>
            {
                Some(
                    studio_runtime_services::account_session_runtime::ConnectionScope {
                        account: account.into(),
                        connection: "new".into(),
                        generation: 2,
                    },
                )
            }
        }
        let root = tempfile::tempdir().expect("state root");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let owner = owner(root.path(), true);
        let mut watch = starting_watch(root.path(), "watch");
        watch["status"] = json!("running");
        watch["operation"] = json!(Operation {
            agent: "owner".into(),
            epoch: 0,
            account_key: "default".into(),
            connection_id: "old".into(),
            evidence: BTreeMap::new()
        });
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                studio_runtime_store::Table::new(tx)
                    .put(&studio_runtime_store::MonitorRecord::from_value(watch)?)?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed command");
        studio_runtime_services::monitor_runtime::recover_on_startup(&store, 2.0)
            .await
            .expect("restart");
        let port = RetainedMonitorPort::new(Arc::clone(&store), Arc::new(UnsupportedStatus));
        let runtime = studio_runtime_services::monitor_runtime_wiring::MonitorRuntime::new(
            Arc::clone(&store),
            port,
            Arc::new(Notify::new()),
        );
        runtime.tick(2.0).await.expect("unresolved status");
        let now = 2.0 + studio_runtime_services::monitor_policy::MONITOR_REATTACH_WINDOW_SECONDS;
        runtime.tick(now).await.expect("bounded loss");
        runtime.tick(now + 1.0).await.expect("replay");
        let (watch, owner, count) = store.read_async("test/lost-status", |tx| {
            Ok::<_, studio_runtime_store::StoreError>((MonitorsRepository::get(tx,"watch")?.expect("watch"), AgentRepository::get(tx,&"owner".into())?.expect("owner"), tx.query_row("SELECT count(*) FROM runtime_events WHERE id='monitor:watch' AND kind='monitor_exit'",[],|row|row.get::<_,i64>(0))?))
        }).await.expect("loss evidence");
        assert_eq!(watch.value()["status"], "lost");
        assert_eq!(owner.status, studio_runtime_store::AgentStatus::Queued);
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn monitor_completion_expired_preparation_wakes_without_submitting() {
        let root = tempfile::tempdir().expect("state root");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let owner = owner(root.path(), true);
        let watch = starting_watch(root.path(), "watch");
        store
            .transaction(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                studio_runtime_store::Table::new(tx)
                    .put(&studio_runtime_store::MonitorRecord::from_value(watch)?)?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed expired preparation");
        let changed = Arc::new(Notify::new());
        let wake = Arc::clone(&changed);
        store.add_table_change_observer(studio_runtime_store::ObservedTable::Monitors, move || {
            wake.notify_one()
        });
        let port = RetainedMonitorPort::new(Arc::clone(&store), Arc::new(NoSession));
        port.start_monitor("watch", 62.0).await.expect("own start");
        tokio::time::timeout(Duration::from_secs(10), changed.notified())
            .await
            .expect("terminal commit");
        let (watch, owner, count) = store.read_async("test/expired-preparation", |tx| {
            Ok::<_, studio_runtime_store::StoreError>((MonitorsRepository::get(tx,"watch")?.expect("watch"), AgentRepository::get(tx,&"owner".into())?.expect("owner"), tx.query_row("SELECT count(*) FROM runtime_events WHERE id='monitor:watch' AND kind='monitor_exit'",[],|row|row.get::<_,i64>(0))?))
        }).await.expect("terminal evidence");
        assert_eq!(watch.value()["status"], "failed");
        assert_eq!(
            watch.value()["error"],
            "The command was not started: Monitor startup deadline expired"
        );
        assert_eq!(owner.status, studio_runtime_store::AgentStatus::Queued);
        assert_eq!(count, 1);
    }

    /// 2026-10-20: managed watches stayed "starting" for minutes. The monitor
    /// timer waited inside each start, and a start refused before submission
    /// stayed "starting" and was offered again on every tick.
    #[tokio::test]
    async fn a_refused_managed_start_fails_its_watch_without_holding_the_timer() {
        let root = tempfile::tempdir().expect("state root");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let owner = owner(root.path(), false);
        let watch = starting_watch(root.path(), "watch");
        store
            .transaction_with(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                studio_runtime_store::Table::new(tx)
                    .put(&studio_runtime_store::MonitorRecord::from_value(watch)?)?;
                Ok::<(), studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed owner and watch");
        let port = RetainedMonitorPort::new(Arc::clone(&store), Arc::new(NoSession));
        tokio::time::timeout(Duration::from_secs(1), port.start_monitor("watch", 2.0))
            .await
            .expect("the timer does not wait for the start")
            .expect("the start is owned by its own task");
        let deadline = Instant::now() + Duration::from_secs(10);
        let record = loop {
            let record = store
                .read_async("test", |db| MonitorsRepository::get(db, "watch"))
                .await
                .expect("read watch")
                .expect("watch")
                .value()
                .clone();
            if record["status"] != "starting" || Instant::now() >= deadline {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(record["status"], "failed");
        assert_eq!(
            record["error"],
            "The command was not started: The agent turn was stopped"
        );
    }

    /// 2026-10-20: cancelled watches whose command/exec reply was lost stayed
    /// "running" with a written terminate, and blocked the owner's archive.
    #[tokio::test]
    async fn terminate_without_a_waiting_exit_reply_completes_the_cancellation() {
        let root = tempfile::tempdir().expect("state root");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let session = Arc::new(ExecSession::default());
        let port =
            RetainedMonitorPort::new(store, Arc::clone(&session) as Arc<dyn AccountSessionPort>);
        let operation = Operation {
            agent: "owner".into(),
            epoch: 0,
            account_key: "default".into(),
            connection_id: "connection".into(),
            evidence: BTreeMap::new(),
        };
        let terminate = studio_runtime_services::monitor_driver::cancel_effect("watch");
        let missing = |submission: Submission| {
            matches!(
                submission,
                Submission::NotSent {
                    reason: NotSentReason::MonitorMissing,
                    ..
                }
            )
        };
        *lock(&session.terminate) = Some(ProviderResponse::Rejected(
            json!({"code":-32600,"message":"no active command/exec for process id watch"}),
        ));
        assert!(missing(port.submit(&operation, &terminate).await));
        *lock(&session.terminate) = Some(ProviderResponse::Reply(json!({})));
        assert!(missing(port.submit(&operation, &terminate).await));

        let exec = Effect::Native {
            identity: "monitor:watch".into(),
            method: "command/exec",
            params: json!({"processId":"watch","timeoutMs":60000}),
        };
        let mut running = port.submit(&operation, &exec);
        assert!(futures_util::FutureExt::now_or_never(&mut running).is_none());
        // The waiting command/exec reply finishes the watch after terminate.
        assert!(matches!(
            port.submit(&operation, &terminate).await,
            Submission::Written
        ));
        drop(running);
        assert!(missing(port.submit(&operation, &terminate).await));
    }

    /// Rehearsal 9: a rule monitor of agent 4c360fa0 named its log under
    /// `<state>/monitors`, but output delivery accepts only the monitor log
    /// directory, so the rule's output could never be delivered.
    #[tokio::test]
    async fn a_rule_monitor_names_its_log_in_the_monitor_log_directory() {
        let root = tempfile::tempdir().expect("state root");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let cwd = root.path().to_string_lossy().into_owned();
        let owner = json!({"id":"owner","rootId":"owner","epoch":0,"status":"waiting",
            "autoWake":true,"cwd":cwd,"accountKey":"default","provider":"codex"});
        let rule = json!({"id":"rule","agent":"owner","rootId":"owner","epoch":0,
            "name":"watch","kind":"interval","intervalSeconds":240,"nextAt":2.0,"at":1.0,
            "path":null,"event":"worker_completed","command":"true","text":"Report.",
            "status":"active","created":1.0,"inFlight":true,"checks":7,"wakes":0,
            "fingerprint":null,"lastAt":1.0,"lastExitCode":null,"error":null,
            "lastOutput":"","lastFinished":1.0});
        let check = RuleSnapshot::from_records(rule.clone(), owner.clone())
            .expect("rule snapshot")
            .check();
        store
            .transaction_with(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                RulesRepository::put(tx, &RuleRecord::from_value(rule)?)?;
                Ok::<(), studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed rule and owner");
        let port = RetainedMonitorPort::new(Arc::clone(&store), Arc::new(NoSession));
        let monitor = port
            .persist_rule_monitor(&check)
            .await
            .expect("rule monitor");
        // The store resolves its root: on macOS the temporary folder is under
        // the `/var` link, and the record names the resolved state root.
        let expected = studio_runtime_services::monitor_output::log_root(store.root())
            .join(format!("{}.log", check.monitor_id));
        assert_eq!(monitor["log"].as_str(), expected.to_str());
    }

    async fn seed_in_flight_rule(store: &RuntimeStore, root: &Path) -> RuleCheck {
        let cwd = root.to_string_lossy().into_owned();
        let owner = json!({"id":"owner","rootId":"owner","epoch":0,"status":"waiting",
            "autoWake":true,"cwd":cwd,"accountKey":"default","provider":"codex"});
        let mut rule = json!({"id":"rule","agent":"owner","rootId":"owner","epoch":0,
            "name":"watch","kind":"interval","intervalSeconds":240,"nextAt":2.0,"at":1.0,
            "path":null,"event":"worker_completed","command":"sleep 3600","text":"Report.",
            "status":"active","created":1.0,"inFlight":true,"checks":7,"wakes":0,
            "fingerprint":null,"lastAt":1.0,"lastExitCode":null,"error":null,
            "lastOutput":"","lastFinished":1.0});
        let check = RuleSnapshot::from_records(rule.clone(), owner.clone())
            .expect("rule snapshot")
            .check();
        rule["launchIntent"] = serde_json::to_value(&check).expect("launch intent");
        store
            .transaction_with(move |tx| {
                AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
                RulesRepository::put(tx, &RuleRecord::from_value(rule)?)?;
                Ok::<(), studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed rule and owner");
        check
    }

    async fn saved_rule(store: &RuntimeStore) -> Value {
        store
            .read_async("test", |db| RulesRepository::get(db, "rule"))
            .await
            .expect("read rule")
            .expect("rule")
            .value()
            .clone()
    }

    async fn settled_rule(store: &RuntimeStore) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rule = saved_rule(store).await;
            if rule["inFlight"] != true || Instant::now() >= deadline {
                return rule;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 2026-10-21: a rule check ran inside the monitor timer, and command/exec
    /// answers only when the command exits, so one long rule command held all
    /// other monitor work. The launch now runs in its own task, which records
    /// the outcome once.
    #[tokio::test]
    async fn a_rule_check_launch_runs_in_its_own_task_and_settles_once() {
        let root = tempfile::tempdir().expect("state root");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let check = seed_in_flight_rule(&store, root.path()).await;
        let port = RetainedMonitorPort::new(Arc::clone(&store), Arc::new(NoSession));
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let submission = port.detach_rule_check(&check, |_, _| async move {
            released.await.expect("release");
            CheckSubmission::NotSubmitted {
                error: "The command was not started".into(),
            }
        });
        assert!(matches!(submission, CheckSubmission::Detached));
        // The same identity in flight is not launched again.
        let duplicate = port.detach_rule_check(&check, |_, _| async move {
            panic!("a rule check in flight was launched again")
        });
        assert!(matches!(duplicate, CheckSubmission::Detached));
        // Recovery cannot settle a launch that its task still owns.
        assert_eq!(
            port.reconcile_rule_check(&check).await,
            CheckReconciliation::Submitted
        );
        assert_eq!(saved_rule(&store).await["inFlight"], true);

        release.send(()).expect("launch task waits");
        let settled = settled_rule(&store).await;
        assert_eq!(settled["inFlight"], false);
        assert_eq!(settled["error"], "The command was not started");
        assert!(lock(&port.rule_checks).is_empty());
        assert_eq!(
            port.reconcile_rule_check(&check).await,
            CheckReconciliation::Unknown
        );
        // A replay of the recorded outcome changes nothing.
        studio_runtime_services::rules_tick::settle_submission(
            &store,
            &check,
            &CheckSubmission::NotSubmitted {
                error: "replayed".into(),
            },
            3.0,
        )
        .await
        .expect("replay");
        assert_eq!(saved_rule(&store).await, settled);
    }

    /// The production launch returns before its preparation and command, and
    /// its task records a refusal as the check result.
    #[tokio::test]
    async fn a_refused_rule_check_launch_is_recorded_by_its_task() {
        let root = tempfile::tempdir().expect("state root");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let check = seed_in_flight_rule(&store, root.path()).await;
        let port = RetainedMonitorPort::new(Arc::clone(&store), Arc::new(NoSession));
        let submission = tokio::time::timeout(Duration::from_secs(1), port.launch(&check))
            .await
            .expect("the timer does not wait for the launch");
        assert!(matches!(submission, CheckSubmission::Detached));
        let settled = settled_rule(&store).await;
        assert_eq!(settled["inFlight"], false);
        assert_eq!(
            settled["error"],
            "account-worker/monitor-preparation is unavailable"
        );
    }
}
