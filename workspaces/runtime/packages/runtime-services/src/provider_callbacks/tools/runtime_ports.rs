//! Thin adapters from managed-tool ports to their owning runtime services.

use std::sync::Arc;

use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

use super::runtime_dispatch::{
    ManagedToolRuntimePorts, MonitorCancelPort, MonitorInputPort, RulesActionResult,
    StatusWatchPort,
};
use crate::managed_host_tools::{
    Actor, DeliveryCertainty, DispatchedOperation, Error, OperationResult,
};

/// Runtime services available to retained managed-tool callbacks.
///
/// Child creation is absent until the combined host composition can supply P02
/// workspace admission and a production `ManagedCreationEnvironment`.
pub struct ManagedToolRuntime {
    ports: ManagedToolRuntimePorts,
}

impl ManagedToolRuntime {
    pub fn new(
        monitor: Arc<crate::monitor_runtime_wiring::MonitorRuntime>,
        lifecycle: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ports: ManagedToolRuntimePorts {
                status_watch: Some(Arc::new(ManagedStatusWatchAdapter)),
                child_creation: None,
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
