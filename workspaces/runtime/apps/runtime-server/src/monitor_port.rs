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

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

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
    Effect, MonitorError, MonitorSnapshot, Operation, ReattachProof,
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

pub(crate) struct RetainedMonitorPort {
    store: Arc<RuntimeStore>,
    session: Arc<dyn AccountSessionPort>,
    preparation: RwLock<
        Option<Arc<studio_runtime_services::account_worker::preparation::PreparationResources>>,
    >,
    dispatch_wake: RwLock<Option<Arc<Notify>>>,
    input_replies: Mutex<BTreeMap<String, InputReply>>,
}

impl RetainedMonitorPort {
    pub(crate) fn new(store: Arc<RuntimeStore>, session: Arc<dyn AccountSessionPort>) -> Self {
        Self {
            store,
            session,
            preparation: RwLock::new(None),
            dispatch_wake: RwLock::new(None),
            input_replies: Mutex::new(BTreeMap::new()),
        }
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
            .current_scope(account)
            .ok_or(MonitorError::Unavailable("provider-session/monitor-start"))?;
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
            .map_err(|_| MonitorError::Unavailable("provider-session/monitor-config"))?;
        if config_receipt.operation != format!("monitor-config:{id}:{epoch}")
            || config_receipt.scope != scope
        {
            return Err(MonitorError::Unavailable("provider-session/monitor-config"));
        }
        let config = match config_receipt.result {
            Some(ProviderResponse::Reply(value))
                if config_receipt.certainty == RequestCertainty::Written =>
            {
                value["config"].clone()
            }
            _ => return Err(MonitorError::Unavailable("provider-session/monitor-config")),
        };
        if !config.is_object() {
            return Err(MonitorError::Refused("Monitor configuration is invalid"));
        }
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
                let (code, error) = match receipt.result {
                    Some(ProviderResponse::Reply(result)) => (
                        result["exitCode"].as_i64(),
                        result["error"].as_str().map(str::to_owned),
                    ),
                    Some(ProviderResponse::Rejected(result)) => (None, Some(result.to_string())),
                    None => return Submission::Unknown,
                };
                if code.is_none() && error.as_deref().is_none_or(str::is_empty) {
                    return Submission::Unknown;
                }
                if monitor_event_queue::enqueue_exit(
                    &self.store,
                    id,
                    operation,
                    &format!("provider-result:{identity}"),
                    code,
                    error.as_deref(),
                    studio_runtime_services::event_write::unix_time().unwrap_or(0.0),
                )
                .await
                .is_err()
                {
                    return Submission::Unknown;
                }
                self.wake_dispatch();
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
            match self.launch_rule_check(check).await {
                Ok(submission) => submission,
                Err(error) => match self.fail_unsubmitted_rule_check(check, &error).await {
                    Some(true) | None => CheckSubmission::NotSubmitted { error },
                    Some(false) => CheckSubmission::Unknown,
                },
            }
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
        Box::pin(self.dispatch_monitor_start(id, now))
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
    ) -> futures_util::future::BoxFuture<'a, Result<Option<ReattachProof>, MonitorError>> {
        Box::pin(async move {
            let now = studio_runtime_services::event_write::unix_time()
                .map_err(|_| MonitorError::Refused("System clock unavailable"))?;
            let Some(operation) = snapshot.operation() else {
                // Without a recorded operation there is nothing to reattach to.
                studio_runtime_services::monitor_runtime::settle_reattach(
                    &self.store,
                    snapshot.id(),
                    "no_operation",
                    now,
                )
                .await?;
                return Ok(None);
            };
            let operation_id = format!("monitor:{}", snapshot.id());
            let Some(scope) = self.session.current_scope(&operation.account_key) else {
                return Ok(None);
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
                Err(AccountSessionError::Unknown) => return Ok(None),
                Err(AccountSessionError::Requires(name)) => {
                    return Err(MonitorError::Unavailable(name));
                }
                Err(_) => return Ok(None),
            };
            if receipt.certainty != RequestCertainty::Written {
                return Ok(None);
            }
            let result = match receipt.result {
                Some(ProviderResponse::Reply(result)) => result,
                Some(ProviderResponse::Rejected(_)) => {
                    // The provider answered that it cannot show this operation.
                    studio_runtime_services::monitor_runtime::settle_reattach(
                        &self.store,
                        snapshot.id(),
                        "provider_rejected",
                        now,
                    )
                    .await?;
                    return Ok(None);
                }
                None => return Ok(None),
            };
            if result["accepted"] != true {
                let reason = result["reason"]
                    .as_str()
                    .unwrap_or("operation_not_accepted");
                studio_runtime_services::monitor_runtime::settle_reattach(
                    &self.store,
                    snapshot.id(),
                    reason,
                    now,
                )
                .await?;
                return Ok(None);
            }
            let Some(native_id) = result["nativeId"].as_u64() else {
                return Ok(None);
            };
            if let Some(response) = result.get("response").filter(|value| !value.is_null()) {
                if self
                    .session
                    .callback_barrier(
                        &receipt.scope.account,
                        &receipt.scope,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                    .is_err()
                {
                    return Ok(None);
                }
                let response_value = response.get("result").unwrap_or(response);
                let code = response_value["exitCode"].as_i64();
                let error = response_value["error"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| response.get("error").map(Value::to_string));
                if code.is_none() && error.as_deref().is_none_or(str::is_empty) {
                    return Ok(None);
                }
                monitor_event_queue::enqueue_exit(
                    &self.store,
                    snapshot.id(),
                    operation,
                    &format!("operation-status:{operation_id}"),
                    code,
                    error.as_deref(),
                    studio_runtime_services::event_write::unix_time()
                        .map_err(|_| MonitorError::Refused("System clock unavailable"))?,
                )
                .await?;
                self.wake_dispatch();
                return Ok(None);
            }
            Ok(Some(ReattachProof {
                operation: operation.clone(),
                operation_id,
                native_id,
                current_connection: scope.connection,
            }))
        })
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
        let expected = studio_runtime_services::monitor_output::log_root(root.path())
            .join(format!("{}.log", check.monitor_id));
        assert_eq!(monitor["log"].as_str(), expected.to_str());
    }
}
