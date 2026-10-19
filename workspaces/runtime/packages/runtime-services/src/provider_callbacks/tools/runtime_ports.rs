//! Thin adapters from managed-tool ports to their owning runtime services.

use futures_util::future::BoxFuture;
use std::sync::Arc;

use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

use super::runtime_dispatch::{
    ChildCreationPort, ManagedToolRuntimePorts, MonitorCancelPort, MonitorCreatePort,
    MonitorInputPort, RulesActionResult, StatusWatchPort,
};
use crate::managed_host_tools::{
    Actor, DeliveryCertainty, DispatchedOperation, Error, OperationResult,
};

/// Runtime services available to retained managed-tool callbacks.
///
/// Host composition supplies account/catalog reads and P02 workspace admission
/// through a production `ManagedCreationEnvironment`.
pub struct ManagedToolRuntime {
    ports: ManagedToolRuntimePorts,
    creation_environment: Option<Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>>,
}

impl ManagedToolRuntime {
    #[cfg(test)]
    pub(crate) fn from_ports_for_test(ports: ManagedToolRuntimePorts) -> Arc<Self> {
        Arc::new(Self {
            ports,
            creation_environment: None,
        })
    }

    pub fn new(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
    ) -> Arc<Self> {
        Self::with_optional_creation(monitor, lifecycle, None, None)
    }

    pub fn with_creation(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
        environment: Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>,
    ) -> Arc<Self> {
        Self::with_optional_creation(monitor, lifecycle, Some(environment), None)
    }

    /// [`ManagedToolRuntime::with_creation`] plus the work-action owner for
    /// task claim, submit and accept (Python `model_work` -> `work_action`).
    pub fn with_creation_and_work(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
        environment: Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>,
        work_action: Arc<dyn super::runtime_dispatch::WorkActionPort>,
    ) -> Arc<Self> {
        Self::with_optional_creation(monitor, lifecycle, Some(environment), Some(work_action))
    }

    fn with_optional_creation(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
        environment: Option<Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>>,
        work_action: Option<Arc<dyn super::runtime_dispatch::WorkActionPort>>,
    ) -> Arc<Self> {
        let child_creation = environment.as_ref().map(|environment| {
            Arc::new(ManagedChildCreationAdapter(Arc::clone(environment)))
                as Arc<dyn ChildCreationPort>
        });
        let monitor_create = environment.as_ref().map(|environment| {
            Arc::new(ManagedMonitorCreateAdapter {
                environment: Arc::clone(environment),
                runtime: Arc::clone(&monitor),
            }) as Arc<dyn MonitorCreatePort>
        });
        Arc::new(Self {
            ports: ManagedToolRuntimePorts {
                status_watch: Some(Arc::new(ManagedStatusWatchAdapter)),
                child_creation,
                monitor_create,
                interrupt: Some(Arc::new(ManagedInterruptAdapter { lifecycle })),
                monitor_input: Some(Arc::new(ManagedMonitorInputAdapter {
                    runtime: Arc::clone(&monitor),
                })),
                monitor_cancel: Some(Arc::new(ManagedMonitorCancelAdapter { runtime: monitor })),
                work_action,
                late: Default::default(),
            },
            creation_environment: environment,
        })
    }

    pub(crate) async fn wait_for_catalog(
        &self,
        account_key: &str,
    ) -> Result<(), crate::managed_child_create::ManagedCreationEnvironmentError> {
        match &self.creation_environment {
            Some(environment) => environment.wait_for_catalog(account_key).await,
            None => Ok(()),
        }
    }

    pub(crate) fn ports(&self) -> ManagedToolRuntimePorts {
        self.ports.clone()
    }

    /// Attach the paired-server outbox driver for `orchestration_servers`
    /// once the HTTP state owns it. Ports cloned earlier share this slot.
    pub fn attach_multi_server(
        &self,
        store: Arc<RuntimeStore>,
        driver: Arc<crate::multi_server_orchestration::OrchestrationDriver>,
    ) {
        let tools = Arc::new(crate::multi_server_tools::ServerTools::new(
            store, driver, false,
        ));
        if self.ports.late.multi_server.set(tools).is_err() {
            // The first attached driver stays the owner of these requests.
        }
    }
}

struct ManagedChildCreationAdapter(
    Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>,
);

impl ChildCreationPort for ManagedChildCreationAdapter {
    fn spawn<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<OperationResult, Error>> {
        Box::pin(async move {
            crate::managed_child_create::spawn_agents(
                store,
                actor,
                args,
                request_id,
                now,
                self.0.as_ref(),
            )
            .await
            .map_err(child_creation_error)
        })
    }

    fn review<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<OperationResult, Error>> {
        Box::pin(async move {
            crate::managed_child_create::request_review(
                store,
                actor,
                args,
                request_id,
                now,
                self.0.as_ref(),
            )
            .await
            .map_err(child_creation_error)
        })
    }
}

struct ManagedMonitorCreateAdapter {
    environment: Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>,
    runtime: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
}

impl MonitorCreatePort for ManagedMonitorCreateAdapter {
    fn create<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            let wait_ms = match args.get("wait_timeout_ms") {
                None => 0,
                Some(value) => value
                    .as_u64()
                    .filter(|v| {
                        *v <= studio_runtime_schema::shared_values::MONITOR_WAIT_MAX_MILLISECONDS
                            as u64
                    })
                    .ok_or(Error::Invalid("wait_timeout_ms must be 0 to 10000"))?,
            };
            let deadline = (wait_ms > 0)
                .then(|| tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms));
            let has_command = args["command"].is_string();
            let monitor_id = args["monitor_id"].as_str();
            if has_command == monitor_id.is_some() {
                return Err(Error::Invalid(
                    "Supply exactly one of command or monitor_id",
                ));
            }
            let account = actor
                .record()
                .extra
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if let Some(id) = monitor_id {
                if id.is_empty()
                    || id.chars().count() > 200
                    || [
                        "command",
                        "output_patterns",
                        "timeout_ms",
                        "wake_on",
                        "success_exit_codes",
                        "interactive",
                        "stall_timeout_seconds",
                        "liveness_command",
                    ]
                    .iter()
                    .any(|key| args.get(*key).is_some())
                {
                    return Err(Error::Invalid(
                        "monitor_id observation accepts only monitor_id and wait_timeout_ms",
                    ));
                }
                let _slot = self
                    .runtime
                    .observation_slot(wait_ms)
                    .map_err(monitor_error)?;
                return self
                    .runtime
                    .observe(
                        actor.record().id.0.as_str(),
                        actor.record().epoch,
                        account,
                        id,
                        deadline,
                        None,
                    )
                    .await
                    .map_err(monitor_error);
            }
            let log_root = crate::monitor_output::log_root(store.root());
            let log_root = log_root
                .to_str()
                .ok_or(Error::Invalid("Monitor log directory is invalid"))?;
            let actor_id = actor.record().id.0.as_str().to_owned();
            let epoch = actor.record().epoch;
            let request_id = request_id.to_owned();
            let request = args.clone();
            let admission = crate::monitor_runtime::MonitorAdmission {
                actor_id: &actor_id,
                epoch,
                request_id: &request_id,
                request: &request,
                log_root,
            };
            if let Some(mut value) =
                crate::monitor_runtime::stored_create_result(store, &admission, now)
                    .await
                    .map_err(monitor_error)?
            {
                self.runtime.wake_tick();
                // Admission already committed: capacity exhaustion must retain
                // that command's identity rather than claim nothing was started.
                let _slot = match self.runtime.observation_slot(wait_ms) {
                    Ok(slot) => slot,
                    Err(_) => {
                        value["observationError"] = json!(
                            "Monitor wait capacity is full; the existing command remains recorded"
                        );
                        return Ok(value);
                    }
                };
                return self.observe_created(actor, value, deadline).await;
            }
            let _slot = self
                .runtime
                .observation_slot(wait_ms)
                .map_err(monitor_error)?;
            let workspace = self
                .environment
                .prepare_workspace(store, actor.record(), args, &request_id)
                .map_err(|error| match error {
                    crate::managed_child_create::ManagedCreationEnvironmentError::Invalid(
                        message,
                    ) => Error::Invalid(message),
                    crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(
                        name,
                    ) => Error::Unavailable(name),
                })?;
            let admission_actor = actor_id.clone();
            let admission_request_id = request_id.clone();
            let admission_request = request.clone();
            let log_root = admission.log_root.to_owned();
            let value = store
                .transaction_with(move |tx| {
                    let admission = crate::monitor_runtime::MonitorAdmission {
                        actor_id: &admission_actor,
                        epoch,
                        request_id: &admission_request_id,
                        request: &admission_request,
                        log_root: &log_root,
                    };
                    if deadline.is_some_and(|limit| tokio::time::Instant::now() >= limit) {
                        return Err(crate::monitor_policy::MonitorError::Refused(
                            "wait_timeout_ms expired before launch; command was not started",
                        ));
                    }
                    crate::monitor_runtime::create_in_transaction(
                        tx,
                        &admission,
                        workspace.as_ref(),
                        now,
                    )
                })
                .await
                .map_err(monitor_error)?;
            self.runtime.wake_tick();
            self.observe_created(actor, value, deadline).await
        })
    }
}

impl ManagedMonitorCreateAdapter {
    async fn observe_created(
        &self,
        actor: &Actor,
        record: Value,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Value, Error> {
        if deadline.is_none() {
            return Ok(record);
        }
        let Some(id) = record["id"].as_str() else {
            return Ok(record);
        };
        let account = actor
            .record()
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        match self
            .runtime
            .observe(
                actor.record().id.0.as_str(),
                actor.record().epoch,
                account,
                id,
                deadline,
                Some(record["outputEventSequence"].as_u64().unwrap_or(0)),
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => Ok(
                json!({"id":id,"status":"unknown","outcome":"unknown","observationError":error.to_string().chars().take(1000).collect::<String>()}),
            ),
        }
    }
}

fn child_creation_error(error: crate::managed_child_create::Error) -> Error {
    use crate::managed_child_create::Error as E;
    match error {
        E::Managed(error) => error,
        E::Store(error) => Error::Store(error),
        E::Unavailable(name) => Error::Unavailable(name),
        E::Refused => Error::Invalid("Managed child creation was refused"),
    }
}

struct ManagedStatusWatchAdapter;

impl StatusWatchPort for ManagedStatusWatchAdapter {
    fn status<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        now: f64,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            crate::managed_status_watch::model_directory(
                store,
                actor.record().id.0.as_str(),
                actor.record().epoch.try_into().map_err(|_| Error::Stale)?,
                args,
                now,
            )
            .await
            .map_err(status_error)
        })
    }

    fn rules<'a>(
        &'a self,
        store: &'a RuntimeStore,
        actor: &'a Actor,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<RulesActionResult, Error>> {
        Box::pin(async move {
            let result = crate::managed_status_watch::rules(
                store,
                actor.record().id.0.as_str(),
                actor.record().epoch.try_into().map_err(|_| Error::Stale)?,
                Some(args),
                request_id,
                now,
                &crate::managed_status_watch::LocalRuleWatchFiles,
            )
            .await
            .map_err(rules_error)?;
            Ok(RulesActionResult {
                value: result.value,
                cancel_monitor_ids: result.cancel_monitor_ids,
            })
        })
    }
}

struct ManagedMonitorInputAdapter {
    runtime: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
}

impl MonitorInputPort for ManagedMonitorInputAdapter {
    fn input<'a>(
        &'a self,
        request: super::runtime_dispatch::MonitorInputRequest<'a>,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            let input = serde_json::from_value(request.input.clone())
                .map_err(|_| Error::Invalid("Invalid monitor input"))?;
            self.runtime
                .input_for_owner(
                    request.caller_id,
                    request.caller_epoch,
                    request.monitor_id,
                    request.ticket,
                    &input,
                    request.now,
                )
                .await
                .map_err(monitor_error)
        })
    }
}

struct ManagedMonitorCancelAdapter {
    runtime: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
}

impl MonitorCancelPort for ManagedMonitorCancelAdapter {
    fn cancel_monitor<'a>(
        &'a self,
        monitor_id: &'a str,
        owner: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            self.runtime
                .cancel(monitor_id, Some(owner), now)
                .await
                .map_err(monitor_error)
        })
    }
}

struct ManagedInterruptAdapter {
    lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
}

impl super::runtime_dispatch::InterruptPort for ManagedInterruptAdapter {
    fn stop_by_agent<'a>(
        &'a self,
        store: Arc<RuntimeStore>,
        actor: &'a Actor,
        target_id: &'a str,
        _request_id: &'a str,
        now: f64,
    ) -> BoxFuture<'a, Result<DispatchedOperation, Error>> {
        Box::pin(async move {
            match crate::agent_lifecycle_runtime::stop_by_agent(
                store,
                actor.record().id.0.as_str(),
                actor.record().epoch,
                target_id,
                false,
                now,
                Arc::clone(&self.lifecycle),
            )
            .await
            {
                Ok(value) => Ok(DispatchedOperation {
                    result: OperationResult {
                        value,
                        signals: Vec::new(),
                    },
                    certainty: DeliveryCertainty::Written,
                    applied: Some(true),
                }),
                Err(crate::agent_management::ManagementError::Store(
                    studio_runtime_store::StoreError::RecordContract(
                        "Lifecycle committed; effect delivery remains pending",
                    ),
                )) => Ok(DispatchedOperation {
                    result: OperationResult {
                        value: json!("Lifecycle outcome unknown; do not repeat it"),
                        signals: Vec::new(),
                    },
                    certainty: DeliveryCertainty::Unknown,
                    applied: None,
                }),
                Err(error) => Err(management_error(error)),
            }
        })
    }
}

fn status_error(error: crate::managed_status_watch::StatusError) -> Error {
    use crate::managed_status_watch::StatusError as E;
    match error {
        E::Store(error) => Error::Store(error),
        E::Sqlite(error) => Error::Sqlite(error),
        E::Stale => Error::Stale,
        E::InvalidArguments | E::InvalidCursor | E::InvalidConcurrency => {
            Error::Invalid("Status request was refused")
        }
        E::Json(_) => Error::Invalid("Stored status data is invalid"),
    }
}

fn rules_error(error: crate::managed_status_watch::RulesServiceError) -> Error {
    use crate::managed_status_watch::RulesServiceError as E;
    match error {
        E::Store(error) => Error::Store(error),
        E::Sqlite(error) => Error::Sqlite(error),
        E::Stale => Error::Stale,
        E::UnknownAction | E::Invalid(_) | E::FilePath | E::Json(_) | E::Engine(_) => {
            Error::Invalid("Rule action was refused")
        }
    }
}

fn monitor_error(error: crate::monitor_policy::MonitorError) -> Error {
    use crate::monitor_policy::MonitorError as E;
    match error {
        E::Store(error) => Error::Store(error),
        E::Unavailable(name) => Error::Unavailable(name),
        E::InvalidStored { id, location } => Error::InvalidStored {
            table: "runtime_monitors",
            id,
            location,
        },
        _ => Error::Invalid("Monitor action was refused"),
    }
}

fn management_error(error: crate::agent_management::ManagementError) -> Error {
    use crate::agent_management::ManagementError as E;
    match error {
        E::Store(error) => Error::Store(error),
        E::Unavailable(name) => Error::Unavailable(name),
        E::Invalid(_) | E::Stored { .. } => Error::Invalid("Agent stop was refused"),
    }
}
