//! Managed-tool dispatch policy after callback admission commits.
//!
//! Concrete sibling runtimes enter only through these ports. This module owns
//! operation routing, missing-capability refusals, monitor ownership checks,
//! and durable input receipt interpretation.

use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

use crate::managed_host_tools::{
    Actor, DeliveryCertainty, DispatchedOperation, Error, OperationResult, WakeSignal,
};

#[cfg(test)]
pub(crate) struct WorkerAccountSelection<'a> {
    pub(crate) accounts: &'a dyn crate::provider_callbacks::ManagedCreationAccountRead,
    pub(crate) parent_account_key: &'a str,
    pub(crate) requested_account_key: Option<&'a str>,
    pub(crate) default_account_key: Option<&'a str>,
    pub(crate) model: &'a str,
    pub(crate) excluded_account_keys: &'a [String],
    pub(crate) project_account_keys: Option<&'a [String]>,
    pub(crate) model_available: &'a mut dyn FnMut(
        &str,
        &crate::provider_callbacks::ManagedAccountIdentity,
        &str,
    ) -> Result<bool, Error>,
}

#[cfg(test)]
pub(crate) fn select_worker_account(
    request: WorkerAccountSelection<'_>,
) -> Result<(String, crate::provider_callbacks::ManagedAccountIdentity), Error> {
    let WorkerAccountSelection {
        accounts,
        parent_account_key,
        requested_account_key,
        default_account_key,
        model,
        excluded_account_keys,
        project_account_keys,
        model_available,
    } = request;
    let selected_default = default_account_key.filter(|key| !key.is_empty());
    if selected_default.is_some()
        && requested_account_key.is_some()
        && requested_account_key != selected_default
    {
        return Err(Error::Invalid(
            "Use the subagent account selected in chat settings",
        ));
    }
    if let Some(account_key) = requested_account_key.or(selected_default) {
        validate_selected_account(account_key, excluded_account_keys, project_account_keys)?;
        let identity = accounts
            .resolve_identity(account_key, parent_account_key)
            .map_err(account_read_error)?;
        if !model_available(account_key, &identity, model)? {
            return Err(Error::Invalid(
                "This model is not available for the selected worker accounts",
            ));
        }
        return Ok((account_key.to_owned(), identity));
    }

    let listing = accounts.worker_accounts().map_err(account_read_error)?;
    let candidates = ordered_worker_account_candidates(
        &listing,
        parent_account_key,
        excluded_account_keys,
        project_account_keys,
    );
    let provider = worker_model_provider(model);
    let mut saw_eligible = false;
    let mut saw_catalog = false;
    for candidate in candidates {
        if provider.is_some_and(|provider| {
            candidate.identity.provider.as_deref().unwrap_or("codex") != provider
        }) {
            continue;
        }
        saw_eligible = true;
        match model_available(&candidate.account_key, &candidate.identity, model) {
            Ok(true) => {
                return Ok((candidate.account_key.clone(), candidate.identity.clone()));
            }
            Ok(false) => saw_catalog = true,
            Err(Error::Unavailable("provider-session/model-list")) => {}
            Err(error) => return Err(error),
        }
    }
    if !saw_eligible || saw_catalog {
        return Err(Error::Invalid(
            "This model is not available for the selected worker accounts",
        ));
    }
    Err(Error::Unavailable("provider-session/model-list"))
}

#[cfg(test)]
fn account_read_error(error: crate::provider_callbacks::ManagedCreationAccountError) -> Error {
    use crate::provider_callbacks::ManagedCreationAccountError as AccountError;
    match error {
        AccountError::Unknown => Error::Invalid("Unknown worker account"),
        AccountError::Deleted => Error::Invalid("This worker account was deleted"),
        AccountError::Disconnected => {
            Error::Invalid("Sign in to the worker account before creating a worker")
        }
        AccountError::SignInRequired => {
            Error::Invalid("Sign in to the worker account before creating a worker")
        }
        AccountError::Unavailable => Error::Unavailable("account-driver/managed-child-account"),
    }
}

#[cfg(test)]
fn validate_selected_account(
    account_key: &str,
    excluded: &[String],
    project_keys: Option<&[String]>,
) -> Result<(), Error> {
    if account_key.is_empty() {
        return Err(Error::Invalid("Select an available worker account"));
    }
    if excluded.iter().any(|key| key == account_key) {
        return Err(Error::Invalid("This account is disabled for this chat"));
    }
    if project_keys
        .is_some_and(|keys| !keys.is_empty() && !keys.iter().any(|key| key == account_key))
    {
        return Err(Error::Invalid("Select a worker account of this project"));
    }
    Ok(())
}

#[cfg(test)]
fn ordered_worker_account_candidates<'a>(
    listing: &'a crate::provider_callbacks::ManagedCreationAccountListing,
    parent: &str,
    excluded: &[String],
    project_keys: Option<&[String]>,
) -> Vec<&'a crate::provider_callbacks::ManagedCreationAccountCandidate> {
    let mut seen = std::collections::HashSet::new();
    let mut ordered = Vec::new();
    for key in [Some(parent), listing.default_account_key.as_deref()]
        .into_iter()
        .flatten()
        .chain(listing.accounts.iter().map(|row| row.account_key.as_str()))
    {
        if !seen.insert(key.to_owned())
            || excluded.iter().any(|excluded| excluded == key)
            || project_keys.is_some_and(|allowed| {
                !allowed.is_empty() && !allowed.iter().any(|item| item == key)
            })
        {
            continue;
        }
        let Some(candidate) = listing.accounts.iter().find(|row| row.account_key == key) else {
            continue;
        };
        let is_parent = key == parent;
        if candidate.disconnected
            || candidate.deleted
            || candidate.duplicate
            || (!is_parent && !candidate.ready && !candidate.can_attempt_native_proof)
        {
            continue;
        }
        ordered.push(candidate);
    }
    ordered
}

#[cfg(test)]
fn worker_model_provider(model: &str) -> Option<&'static str> {
    if model.starts_with("gpt-") {
        Some("codex")
    } else if matches!(model, "default" | "sonnet" | "opus" | "haiku")
        || model.starts_with("claude-")
    {
        Some("claude")
    } else {
        None
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RulesActionResult {
    pub(crate) value: Value,
    pub(crate) cancel_monitor_ids: Vec<String>,
}

pub(crate) trait StatusWatchPort: Send + Sync {
    fn status(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        now: f64,
    ) -> Result<Value, Error>;

    fn rules(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<RulesActionResult, Error>;
}

pub(crate) trait ChildCreationPort: Send + Sync {
    fn spawn(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;

    fn review(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;
}

pub(crate) trait InterruptPort: Send + Sync {
    fn stop_by_agent(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        target_id: &str,
        request_id: &str,
        now: f64,
    ) -> Result<DispatchedOperation, Error>;
}

pub(crate) trait MonitorInputPort: Send + Sync {
    /// The authenticated caller is explicit even though the retained native
    /// runtime's input method takes the owner from the persisted monitor.
    fn input(&self, request: MonitorInputRequest<'_>) -> Result<Value, Error>;
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
    fn cancel_monitor(&self, monitor_id: &str, owner: &str, now: f64) -> Result<Value, Error>;
}

#[derive(Clone)]
pub(crate) struct ManagedToolRuntimePorts {
    pub(crate) status_watch: Option<std::sync::Arc<dyn StatusWatchPort>>,
    pub(crate) child_creation: Option<std::sync::Arc<dyn ChildCreationPort>>,
    pub(crate) interrupt: Option<std::sync::Arc<dyn InterruptPort>>,
    pub(crate) monitor_input: Option<std::sync::Arc<dyn MonitorInputPort>>,
    pub(crate) monitor_cancel: Option<std::sync::Arc<dyn MonitorCancelPort>>,
}

pub(super) struct DispatchRequest<'a> {
    pub(super) store: &'a RuntimeStore,
    pub(super) actor: &'a Actor,
    pub(super) context: &'a crate::provider_callbacks::CallbackContext,
    pub(super) tool: &'a str,
    pub(super) args: &'a Value,
    pub(super) request_id: &'a str,
    pub(super) now: f64,
    pub(super) ports: &'a ManagedToolRuntimePorts,
}

pub(super) fn dispatch(request: DispatchRequest<'_>) -> Result<Option<DispatchedOperation>, Error> {
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
    let actor_id = actor.record().id.0.as_str();
    let actor_epoch = actor.record().epoch;
    let dispatched = match tool {
        "orchestration_status" => {
            if studio_runtime_store::truthy(actor.record().extra.get("remoteOrigin")) {
                return Ok(Some(not_sent(Error::Unavailable(
                    "remote-transport/managed-status",
                ))));
            }
            let Some(port) = ports.status_watch.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "model-directory/status-and-dispatch-capacity",
                ))));
            };
            written(port.status(store, actor, args, now)?, Vec::new())
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
            let result = port.rules(store, actor, args, request_id, now)?;
            for monitor_id in result.cancel_monitor_ids {
                let Some(monitor_port) = ports.monitor_cancel.as_ref() else {
                    return Ok(Some(not_sent(Error::Unavailable(
                        "monitor-runtime/rule-cancel",
                    ))));
                };
                monitor_port.cancel_monitor(&monitor_id, actor_id, now)?;
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
                port.spawn(store, actor, args, request_id, now)?
            } else {
                port.review(store, actor, args, request_id, now)?
            };
            written(result.value, result.signals)
        }
        "orchestration_interrupt" => {
            let Some(port) = ports.interrupt.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "agent-lifecycle/production-ports",
                ))));
            };
            let target = args
                .get("agent_id")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid(
                    "Supply agent_id for orchestration_interrupt",
                ))?;
            let target = super::services::resolve_interrupt_target(store, context, target)?;
            port.stop_by_agent(store, actor, &target, request_id, now)?
        }
        "orchestration_monitor" if args["action"].as_str() != Some("cancel") => {
            return Ok(Some(not_sent(Error::Unavailable(
                "workspace-state/monitor-admission",
            ))));
        }
        "orchestration_monitor_input" => {
            let Some(port) = ports.monitor_input.as_ref() else {
                return Ok(Some(not_sent(Error::Unavailable(
                    "monitor-runtime/managed-input",
                ))));
            };
            let monitor_id = args
                .get("monitor_id")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid("Missing monitor_id"))?;
            let input: Value = serde_json::from_value(args.clone())
                .map_err(|_| Error::Invalid("Invalid monitor input"))?;
            authorize_monitor_input(store, actor, monitor_id)?;
            match port.input(MonitorInputRequest {
                caller_id: actor_id,
                caller_epoch: actor_epoch,
                monitor_id,
                ticket: request_id,
                input: &input,
                now,
            }) {
                Ok(value) => written(value, Vec::new()),
                Err(error) => match input_attempt(store, monitor_id, request_id) {
                    Ok(Some(record)) => managed_input_receipt(&record, request_id)?,
                    Ok(None) => not_sent(error),
                    Err(_) => unknown_input(),
                },
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(dispatched))
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

fn not_sent(error: Error) -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult {
            value: json!(error.to_string()),
            signals: Vec::new(),
        },
        certainty: DeliveryCertainty::NotSent,
        applied: Some(false),
    }
}

fn unknown_input() -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult {
            value: json!("Monitor input outcome unknown; do not repeat it"),
            signals: Vec::new(),
        },
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

#[cfg(test)]
mod tests {
    use super::managed_input_receipt;
    use crate::managed_host_tools::DeliveryCertainty;
    use serde_json::json;

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
}

#[cfg(test)]
mod account_identity_tests {
    use super::{WorkerAccountSelection, select_worker_account};
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

    fn candidate(key: &str, ready: bool, native_proof: bool) -> Candidate {
        Candidate {
            account_key: key.to_owned(),
            identity: ManagedAccountIdentity {
                provider: Some("codex".to_owned()),
                home: None,
                account_id: None,
                email: None,
            },
            ready,
            can_attempt_native_proof: native_proof,
            disconnected: false,
            deleted: false,
            duplicate: false,
        }
    }

    fn accounts() -> Accounts {
        Accounts(Listing {
            default_account_key: Some("configured".to_owned()),
            accounts: vec![
                candidate("parent", false, false),
                candidate("configured", true, false),
                candidate("ready", true, false),
                candidate("proof", false, true),
                candidate("offline", false, false),
            ],
        })
    }

    #[test]
    fn implicit_worker_accounts_follow_python_order_and_filters() {
        let accounts = accounts();
        let mut available_parent = |key: &str,
                                    _: &crate::provider_callbacks::ManagedAccountIdentity,
                                    _: &str| Ok(key == "parent");
        let selected = select_worker_account(WorkerAccountSelection {
            accounts: &accounts,
            parent_account_key: "parent",
            requested_account_key: None,
            default_account_key: None,
            model: "gpt-5",
            excluded_account_keys: &["configured".to_owned()],
            project_account_keys: None,
            model_available: &mut available_parent,
        })
        .expect("parent is the first eligible model account");
        assert_eq!(selected.0, "parent");

        let mut available_ready = |key: &str,
                                   _: &crate::provider_callbacks::ManagedAccountIdentity,
                                   _: &str| Ok(key == "ready");
        let selected = select_worker_account(WorkerAccountSelection {
            accounts: &accounts,
            parent_account_key: "parent",
            requested_account_key: None,
            default_account_key: None,
            model: "gpt-5",
            excluded_account_keys: &[],
            project_account_keys: None,
            model_available: &mut available_ready,
        })
        .expect("configured and ready candidates are probed in order");
        assert_eq!(selected.0, "ready");
    }

    #[test]
    fn explicit_worker_account_never_falls_back() {
        let accounts = accounts();
        let mut unavailable =
            |_: &str, _: &crate::provider_callbacks::ManagedAccountIdentity, _: &str| Ok(false);
        let result = select_worker_account(WorkerAccountSelection {
            accounts: &accounts,
            parent_account_key: "parent",
            requested_account_key: Some("offline"),
            default_account_key: None,
            model: "gpt-5",
            excluded_account_keys: &[],
            project_account_keys: None,
            model_available: &mut unavailable,
        });
        assert!(result.is_err());
    }
}
