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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

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
use studio_runtime_store::{MonitorsRepository, RuntimeStore};

const DEFAULT_MONITOR_TIMEOUT_MS: u64 = 3_600_000;
const MONITOR_TIMEOUT_GRACE_MS: u64 = 60_000;
const MAX_MONITOR_TIMEOUT_WITH_GRACE_MS: u64 = 86_460_000;

/// Port of `codex_shell.monitor_command` (codex_shell.py:41-91): command/exec applies account shell
/// environment policy but accepts argv and does not perform the native exec
/// shell startup. Apply Codex's login/rc startup inside the command sandbox.
fn monitor_command(command: &str, config: &Value, guest: bool) -> Result<Vec<String>, String> {
    let shell = if guest {
        PathBuf::from("/bin/bash")
    } else {
        default_shell()
            .ok_or_else(|| "No supported shell is available for the command".to_owned())?
    };
    let name = shell
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(name.as_str(), "pwsh" | "powershell") {
        return Ok(vec![
            shell.to_string_lossy().into_owned(),
            "-NoProfile".into(),
            "-Command".into(),
            command.into(),
        ]);
    }
    let login = config["allow_login_shell"].as_bool().unwrap_or(true);
    if !login {
        return Ok(vec![
            shell.to_string_lossy().into_owned(),
            "-c".into(),
            command.into(),
        ]);
    }
    let snapshot = config["features"]["shell_snapshot"]
        .as_bool()
        .unwrap_or(true);
    if !snapshot || !matches!(name.as_str(), "bash" | "zsh") {
        return Ok(vec![
            shell.to_string_lossy().into_owned(),
            "-lc".into(),
            command.into(),
        ]);
    }
    let startup = if name == "zsh" {
        "if [ -r \"${ZDOTDIR:-$HOME}/.zshrc\" ]; then . \"${ZDOTDIR:-$HOME}/.zshrc\"; fi"
    } else {
        "if [ -z \"$BASH_ENV\" ] && [ -r \"$HOME/.bashrc\" ]; then . \"$HOME/.bashrc\"; fi"
    };
    let mut lines = vec![format!("{{ {startup}; }} >/dev/null 2>&1")];
    if let Some(set) = config["shell_environment_policy"]["set"].as_object() {
        for (key, value) in set {
            if valid_environment_key(key) {
                lines.push(format!(
                    "export {key}={}",
                    shell_quote(&value_as_text(value))
                ));
            }
        }
    }
    #[cfg(target_os = "macos")]
    lines.push(
        "if [ -z \"${SDKROOT+x}\" ]; then if SDKROOT=\"$(/usr/bin/xcrun --sdk macosx --show-sdk-path 2>/dev/null)\" && [ -d \"$SDKROOT\" ]; then export SDKROOT; else unset SDKROOT; fi; fi".into(),
    );
    lines.push(format!(
        "exec {} -c {}",
        shell_quote(&shell.to_string_lossy()),
        shell_quote(command)
    ));
    Ok(vec![
        shell.to_string_lossy().into_owned(),
        "-lc".into(),
        lines.join("\n"),
    ])
}

fn valid_environment_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn value_as_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn default_shell() -> Option<PathBuf> {
    let preferred = nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|user| user.shell);
    if let Some(path) = preferred.filter(|path| supported_shell(path) && path.is_file()) {
        return Some(path);
    }
    let names = if cfg!(target_os = "macos") {
        ["zsh", "bash", "sh"]
    } else {
        ["bash", "zsh", "sh"]
    };
    for name in names {
        if let Some(path) = executable_in_path(name) {
            return Some(path);
        }
        for prefix in ["/bin", "/usr/bin"] {
            let path = Path::new(prefix).join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn default_shell() -> Option<PathBuf> {
    ["pwsh", "powershell"]
        .iter()
        .find_map(|name| executable_in_path(name))
}

fn supported_shell(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "zsh" | "bash" | "sh" | "pwsh" | "powershell"))
}

fn executable_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

pub(crate) struct RetainedMonitorPort {
    store: Arc<RuntimeStore>,
    session: Arc<dyn AccountSessionPort>,
    preparation: RwLock<
        Option<Arc<studio_runtime_services::account_worker::preparation::PreparationResources>>,
    >,
    input_replies: Mutex<BTreeMap<String, InputReply>>,
}

impl RetainedMonitorPort {
    pub(crate) fn new(store: Arc<RuntimeStore>, session: Arc<dyn AccountSessionPort>) -> Self {
        Self {
            store,
            session,
            preparation: RwLock::new(None),
            input_replies: Mutex::new(BTreeMap::new()),
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

    fn request(
        &self,
        operation: &Operation,
        request_operation: String,
        method: Method,
        params: Value,
        deadline: Instant,
        actor: bool,
    ) -> Result<SessionReceipt, AccountSessionError> {
        let scope = self
            .session
            .current_scope(&operation.account_key)
            .ok_or(AccountSessionError::Refused("No live provider session"))?;
        if scope.account != operation.account_key {
            return Err(AccountSessionError::Refused(
                "Provider account scope changed",
            ));
        }
        Ok(self.session.provider_request(ProviderRequest {
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
        })?)
    }

    fn dispatch_monitor_start(&self, id: &str, now: f64) -> Result<(), MonitorError> {
        use studio_runtime_store::{AgentId, AgentRepository, MonitorsRepository};

        let (monitor, actor) = self.store.transaction_with(|tx| {
            let (snapshot, actor) = studio_runtime_services::monitor_runtime::snapshot(tx, id)?;
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
            let saved = MonitorsRepository::get(tx, id)?
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
        })?;
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
            connection_id: scope.connection,
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
                &self.store,
                &resources,
                agent_id,
                epoch,
            )
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
        let _ = studio_runtime_services::monitor_driver::start(&self.store, self, id, &start, now)?;
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

    fn persist_rule_monitor(&self, check: &RuleCheck) -> Result<Value, String> {
        use studio_runtime_store::{AgentId, AgentRepository, RulesRepository};
        let request_id = format!("rule:{}:{}", check.rule_id, check.check_number);
        let request = json!({
            "command":check.command,
            "timeout_ms":check.timeout_ms,
            "stall_timeout_seconds":0,
            "liveness_command":"",
            "wake_on":"exit",
        });
        let log_root = self
            .store
            .root()
            .join("monitors")
            .to_string_lossy()
            .into_owned();
        self.store
            .transaction_with(|tx| {
                let rule = RulesRepository::get(tx, &check.rule_id)?.ok_or(
                    MonitorError::Refused("Rule was removed before monitor admission"),
                )?;
                let actor = AgentRepository::get(tx, &AgentId::from(check.agent_id.as_str()))?
                    .ok_or(MonitorError::Refused("Rule agent is unavailable"))?;
                let check_snapshot =
                    studio_runtime_services::rules_engine::RuleSnapshot::from_records(
                        rule.value().clone(),
                        actor.value(),
                    )
                    .map_err(|_| MonitorError::Refused("Rule check record is invalid"))?;
                if !check_snapshot.launch_allowed(check) {
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
            .map_err(|error: MonitorError| error.to_string())
    }

    fn launch_rule_check(&self, check: &RuleCheck) -> Result<CheckSubmission, String> {
        let actor = {
            let db = self.store.read().map_err(|error| error.to_string())?;
            studio_runtime_store::AgentRepository::get(
                &db,
                &studio_runtime_store::AgentId::from(check.agent_id.as_str()),
            )
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Rule agent is unavailable".to_owned())?
        };
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
                &self.store,
                &resources,
                &check.agent_id,
                check.epoch,
            )
            .map_err(|error| error.to_string())?;
        let monitor = self.persist_rule_monitor(check)?;
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
        ) {
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

    fn reconcile_rule_check(&self, check: &RuleCheck) -> CheckReconciliation {
        let loaded: Result<Option<(Value, MonitorSnapshot)>, MonitorError> =
            self.store.transaction_with(|tx| {
                let Some(record) = MonitorsRepository::get(tx, &check.monitor_id)? else {
                    return Ok(None);
                };
                let (snapshot, _) =
                    studio_runtime_services::monitor_runtime::snapshot(tx, &check.monitor_id)?;
                Ok(Some((record.value().clone(), snapshot)))
            });
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
        match self.request(
            operation,
            format!("monitor-status:{}", check.monitor_id),
            Method::OperationStatus,
            json!({"operationId":operation_id}),
            Instant::now() + Duration::from_secs(10),
            false,
        ) {
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
                            );
                            if queued.is_err() {
                                return CheckReconciliation::Unknown;
                            }
                        }
                    }
                    CheckReconciliation::Submitted
                } else if result["accepted"] == false {
                    let reason = result["reason"]
                        .as_str()
                        .unwrap_or("Native monitor command was not accepted");
                    let now = studio_runtime_services::event_write::unix_time().unwrap_or(0.0);
                    let finished = self.store.transaction_with(|tx| {
                        let (current, actor) = studio_runtime_services::monitor_runtime::snapshot(
                            tx,
                            &check.monitor_id,
                        )?;
                        if current.operation() != Some(operation) {
                            return Err(MonitorError::Refused(
                                "Monitor operation identity changed",
                            ));
                        }
                        let decision = studio_runtime_services::monitor_policy::finish(
                            &current,
                            Some(operation),
                            None,
                            Some(reason),
                            now,
                        )?;
                        studio_runtime_services::monitor_runtime::apply(
                            tx, &current, &actor, &decision, now,
                        )
                    });
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

    fn fail_unsubmitted_rule_check(&self, check: &RuleCheck, error: &str) -> Option<bool> {
        let now = studio_runtime_services::event_write::unix_time().ok()?;
        let outcome: Result<Option<bool>, MonitorError> = self.store.transaction_with(|tx| {
            let Some(record) = MonitorsRepository::get(tx, &check.monitor_id)? else {
                return Ok(None);
            };
            if record.get("ruleId").and_then(Value::as_str) != Some(&check.rule_id)
                || record.get("status").and_then(Value::as_str) != Some("starting")
                || record
                    .get("operation")
                    .is_some_and(|operation| !operation.is_null())
            {
                return Ok(Some(false));
            }
            studio_runtime_services::monitor_runtime::finish_in_transaction(
                tx,
                &check.monitor_id,
                None,
                None,
                Some(error),
                now,
            )?;
            Ok(Some(true))
        });
        outcome.ok().flatten()
    }
}

impl MonitorNative for RetainedMonitorPort {
    fn submit(&self, operation: &Operation, effect: &Effect) -> Submission {
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
        let result = self.request(
            operation,
            identity.clone(),
            method,
            params.clone(),
            Instant::now() + Duration::from_millis(timeout),
            true,
        );
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
            if let Err(error) = self.session.callback_barrier(
                &receipt.scope.account,
                &receipt.scope,
                Instant::now() + Duration::from_secs(10),
            ) {
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
            .is_err()
            {
                return Submission::Unknown;
            }
        } else if method == Method::CommandExecWrite || method == Method::CommandExecResize {
            let ticket = identity.rsplit(':').next().unwrap_or(identity).to_owned();
            let reply = match receipt.result {
                Some(ProviderResponse::Reply(value)) => InputReply::Acknowledged(value),
                Some(ProviderResponse::Rejected(value)) => InputReply::Rejected(value.to_string()),
                None => InputReply::Unknown("Input acknowledgement is pending".into()),
            };
            self.input_replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(ticket, reply);
        }
        Submission::Written
    }
}

impl RuleMonitor for RetainedMonitorPort {
    fn available(&self) -> bool {
        self.preparation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
    fn launch(&self, check: &RuleCheck) -> CheckSubmission {
        match self.launch_rule_check(check) {
            Ok(submission) => submission,
            Err(error) => match self.fail_unsubmitted_rule_check(check, &error) {
                Some(true) | None => CheckSubmission::NotSubmitted { error },
                Some(false) => CheckSubmission::Unknown,
            },
        }
    }
    fn reconcile(&self, check: &RuleCheck) -> CheckReconciliation {
        self.reconcile_rule_check(check)
    }
    fn cancel(&self, monitor_id: &str) -> Result<(), RulesTickError> {
        let record = self
            .store
            .read()
            .map_err(studio_runtime_store::StoreError::from)?;
        let Some(monitor) = MonitorsRepository::get(&record, monitor_id)? else {
            return Ok(());
        };
        let Some(agent) = monitor.get("agent").and_then(Value::as_str) else {
            return Err(RulesTickError::MonitorUnavailable);
        };
        studio_runtime_services::monitor_driver::cancel(
            &self.store,
            self,
            monitor_id,
            Some(agent),
            studio_runtime_services::event_write::unix_time()
                .map_err(|_| RulesTickError::MonitorUnavailable)?,
        )
        .map_err(|_| RulesTickError::MonitorUnavailable)?;
        Ok(())
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
    fn start_monitor(&self, id: &str, now: f64) -> Result<(), MonitorError> {
        self.dispatch_monitor_start(id, now)
    }

    fn next(&self) -> Result<Option<Delivery>, MonitorError> {
        monitor_event_queue::next(&self.store)
    }

    fn acknowledge(&self, delivery: &Delivery) -> Result<(), MonitorError> {
        monitor_event_queue::acknowledge(&self.store, delivery)
    }

    fn reattach(&self, snapshot: &MonitorSnapshot) -> Result<Option<ReattachProof>, MonitorError> {
        let Some(operation) = snapshot.operation() else {
            return Ok(None);
        };
        let operation_id = format!("monitor:{}", snapshot.id());
        let Some(scope) = self.session.current_scope(&operation.account_key) else {
            return Ok(None);
        };
        let receipt = match self.request(
            operation,
            format!("monitor-status:{}", snapshot.id()),
            Method::OperationStatus,
            json!({"operationId":operation_id}),
            Instant::now() + Duration::from_secs(10),
            false,
        ) {
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
        let Some(ProviderResponse::Reply(result)) = receipt.result else {
            return Ok(None);
        };
        if result["accepted"] != true {
            let reason = result["reason"]
                .as_str()
                .unwrap_or("operation_not_accepted");
            persist_not_reattachable(&self.store, snapshot, operation, reason)?;
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
            )?;
            return Ok(None);
        }
        Ok(Some(ReattachProof {
            operation: operation.clone(),
            operation_id,
            native_id,
            current_connection: scope.connection,
        }))
    }

    fn probe(&self, id: &str, generation: i64, command: &str) -> Result<(), MonitorError> {
        let (snapshot, actor) = self
            .store
            .transaction_with(|tx| studio_runtime_services::monitor_runtime::snapshot(tx, id))?;
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
        let id = id.to_owned();
        let command = command.to_owned();
        std::thread::Builder::new()
            .name(format!("monitor-probe-{id}-{generation}"))
            .spawn(move || {
                let result = run_probe(
                    &store,
                    session.as_ref(),
                    resources.as_ref(),
                    &id,
                    generation,
                    &command,
                    &operation,
                    actor.value(),
                )
                .unwrap_or_else(|error| json!({"error":error}));
                let _ = monitor_event_queue::enqueue_probe(
                    &store, &id, &operation, generation, &result,
                );
            })
            .map_err(|_| MonitorError::Refused("Could not schedule monitor liveness probe"))?;
        Ok(())
    }

    fn input_reply(&self, _operation: &Operation, ticket: &str) -> InputReply {
        self.input_replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ticket)
            .unwrap_or_else(|| InputReply::Unknown("Input acknowledgement is unknown".into()))
    }
}

fn persist_not_reattachable(
    store: &RuntimeStore,
    expected: &MonitorSnapshot,
    operation: &Operation,
    reason: &str,
) -> Result<(), MonitorError> {
    let now = studio_runtime_services::event_write::unix_time()
        .map_err(|_| MonitorError::Refused("System clock unavailable"))?;
    store.transaction_with(|tx| {
        let (current, actor) =
            studio_runtime_services::monitor_runtime::snapshot(tx, expected.id())?;
        if current.operation() != Some(operation)
            || current.record()["status"] != "lost"
            || current.record()["reattachRecovery"]["status"] != "running"
        {
            return Err(MonitorError::Refused("Monitor reattach state changed"));
        }
        let mut decision = studio_runtime_services::monitor_policy::Decision {
            record: current.record().clone(),
            effects: Vec::new(),
            response: Value::Null,
        };
        decision.record["reattachRecovery"]["status"] = json!("not_reattachable");
        decision.record["reattachRecovery"]["proof"] =
            json!(reason.chars().take(12000).collect::<String>());
        studio_runtime_services::monitor_runtime::apply(tx, &current, &actor, &decision, now)
    })
}

/// Monitor command preparation uses the shared permission builder without a
/// turn-start attempt gate: `prepare_monitor_command(store, resources,
/// agent_id, epoch)`. Its settings select sandboxPolicy/permissionProfile.
fn prepare_monitor_permissions(
    store: &RuntimeStore,
    resources: &studio_runtime_services::account_worker::preparation::PreparationResources,
    agent_id: &str,
    epoch: u64,
) -> Result<Value, String> {
    let prepared = studio_runtime_services::account_worker::preparation::prepare_monitor_command(
        store, resources, agent_id, epoch,
    )
    .map_err(|error| error.to_string())?;
    Ok(prepared.settings)
}

fn run_probe(
    store: &RuntimeStore,
    session: &dyn AccountSessionPort,
    resources: &studio_runtime_services::account_worker::preparation::PreparationResources,
    id: &str,
    generation: i64,
    command: &str,
    operation: &studio_runtime_services::monitor_policy::Operation,
    actor: Value,
) -> Result<Value, String> {
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
    let settings = prepare_monitor_permissions(
        store,
        resources,
        &operation.agent,
        u64::try_from(generation).map_err(|_| "Invalid liveness generation")?,
    )?;
    let config_operation = format!("monitor-config:{id}:{generation}");
    let config_receipt = provider_request(
        session,
        operation,
        config_operation,
        Method::ConfigRead,
        json!({"cwd":cwd,"includeLayers":false}),
        Instant::now() + Duration::from_secs(20),
    )?;
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
    )?;
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

fn provider_request(
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
    let sandbox = actor.get("sandbox").filter(|value| python_truthy(value));
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

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
