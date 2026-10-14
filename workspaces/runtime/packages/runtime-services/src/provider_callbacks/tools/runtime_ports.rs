//! Thin adapters from managed-tool ports to their owning runtime services.

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
}

impl ManagedToolRuntime {
    pub fn new(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
    ) -> Arc<Self> {
        Self::with_optional_creation(monitor, lifecycle, None)
    }

    pub fn with_creation(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
        environment: Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>,
    ) -> Arc<Self> {
        Self::with_optional_creation(monitor, lifecycle, Some(environment))
    }

    fn with_optional_creation(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
        environment: Option<Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>>,
    ) -> Arc<Self> {
        let child_creation = environment.as_ref().map(|environment| {
            Arc::new(ManagedChildCreationAdapter(Arc::clone(environment)))
                as Arc<dyn ChildCreationPort>
        });
        let monitor_create = environment.map(|environment| {
            Arc::new(ManagedMonitorCreateAdapter {
                environment,
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
            },
        })
    }

    pub(crate) fn ports(&self) -> ManagedToolRuntimePorts {
        self.ports.clone()
    }
}

struct ManagedChildCreationAdapter(
    Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>,
);

impl ChildCreationPort for ManagedChildCreationAdapter {
    fn spawn(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error> {
        crate::managed_child_create::spawn_agents(
            store,
            actor,
            args,
            request_id,
            now,
            self.0.as_ref(),
        )
        .map_err(child_creation_error)
    }

    fn review(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error> {
        crate::managed_child_create::request_review(
            store,
            actor,
            args,
            request_id,
            now,
            self.0.as_ref(),
        )
        .map_err(child_creation_error)
    }
}

struct ManagedMonitorCreateAdapter {
    environment: Arc<dyn crate::managed_child_create::ManagedCreationEnvironment>,
    runtime: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
}

impl MonitorCreatePort for ManagedMonitorCreateAdapter {
    fn create(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<Value, Error> {
        let log_root = store.root().join("monitor-logs");
        let log_root = log_root
            .to_str()
            .ok_or(Error::Invalid("Monitor log directory is invalid"))?;
        let admission = crate::monitor_runtime::MonitorAdmission {
            actor_id: actor.record().id.0.as_str(),
            epoch: actor.record().epoch,
            request_id,
            request: args,
            log_root,
        };
        if let Some(value) = crate::monitor_runtime::stored_create_result(store, &admission, now)
            .map_err(monitor_error)?
        {
            self.runtime.wake_tick();
            return Ok(value);
        }
        let workspace = self
            .environment
            .prepare_workspace(store, actor.record(), args, request_id)
            .map_err(|error| match error {
                crate::managed_child_create::ManagedCreationEnvironmentError::Invalid(message) => {
                    Error::Invalid(message)
                }
                crate::managed_child_create::ManagedCreationEnvironmentError::Unavailable(name) => {
                    Error::Unavailable(name)
                }
            })?;
        let value = store
            .transaction_with(|tx| {
                crate::monitor_runtime::create_in_transaction(
                    tx,
                    &admission,
                    workspace.as_ref(),
                    now,
                )
            })
            .map_err(monitor_error)?;
        self.runtime.wake_tick();
        Ok(value)
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
    fn status(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        now: f64,
    ) -> Result<Value, Error> {
        crate::managed_status_watch::model_directory(
            store,
            actor.record().id.0.as_str(),
            actor.record().epoch.try_into().map_err(|_| Error::Stale)?,
            args,
            now,
        )
        .map_err(status_error)
    }

    fn rules(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<RulesActionResult, Error> {
        let result = crate::managed_status_watch::rules(
            store,
            actor.record().id.0.as_str(),
            actor.record().epoch.try_into().map_err(|_| Error::Stale)?,
            Some(args),
            request_id,
            now,
            &crate::managed_status_watch::LocalRuleWatchFiles,
        )
        .map_err(rules_error)?;
        Ok(RulesActionResult {
            value: result.value,
            cancel_monitor_ids: result.cancel_monitor_ids,
        })
    }
}

struct ManagedMonitorInputAdapter {
    runtime: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
}

impl MonitorInputPort for ManagedMonitorInputAdapter {
    fn input(
        &self,
        request: super::runtime_dispatch::MonitorInputRequest<'_>,
    ) -> Result<Value, Error> {
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
            .map_err(monitor_error)
    }
}

struct ManagedMonitorCancelAdapter {
    runtime: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
}

impl MonitorCancelPort for ManagedMonitorCancelAdapter {
    fn cancel_monitor(&self, monitor_id: &str, owner: &str, now: f64) -> Result<Value, Error> {
        self.runtime
            .cancel(monitor_id, Some(owner), now)
            .map_err(monitor_error)
    }
}

struct ManagedInterruptAdapter {
    lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
}

impl super::runtime_dispatch::InterruptPort for ManagedInterruptAdapter {
    fn stop_by_agent(
        &self,
        store: &RuntimeStore,
        actor: &Actor,
        target_id: &str,
        _request_id: &str,
        now: f64,
    ) -> Result<DispatchedOperation, Error> {
        match crate::agent_lifecycle_runtime::stop_by_agent(
            store,
            actor.record().id.0.as_str(),
            actor.record().epoch,
            target_id,
            false,
            now,
            self.lifecycle.as_ref(),
        ) {
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
