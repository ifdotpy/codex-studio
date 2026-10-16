//! Lifecycle adapter for durable monitor cancellation intents.
//! The account worker still owns native monitor transport and remains unavailable.

use std::sync::Arc;

use serde_json::Value;
use studio_runtime_store::Transaction;
use tokio::sync::Notify;

use crate::{
    agent_lifecycle_runtime::{LifecycleEffect, LifecycleEffectKind, LifecyclePorts},
    agent_management::ManagementError,
    monitor_driver,
    monitor_policy::MonitorError,
    sync_policy,
};

pub struct MonitorLifecyclePorts {
    wake: Arc<Notify>,
}

impl MonitorLifecyclePorts {
    pub fn new(wake: Arc<Notify>) -> Self {
        Self { wake }
    }
}

impl LifecyclePorts for MonitorLifecyclePorts {
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        if effect.kind() != LifecycleEffectKind::CancelMonitors {
            return Err(ManagementError::Unavailable(effect.kind().dependency()));
        }
        let agent =
            effect
                .before()
                .get("id")
                .and_then(Value::as_str)
                .ok_or(ManagementError::Invalid(
                    "Invalid monitor cancellation owner",
                ))?;
        for (id, raw) in sync_policy::stored_records(tx, "runtime_monitors")? {
            let record: Value =
                serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
            if record.get("agent").and_then(Value::as_str) != Some(agent)
                || !record
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "starting" | "running" | "approval"))
            {
                continue;
            }
            // reserve_cancel persists the durable intent in this lifecycle
            // transaction. Dispatch belongs exclusively to MonitorRuntime.
            let (_, operation) = monitor_driver::reserve_cancel(tx, &id, Some(agent), effect.at())
                .map_err(lifecycle_monitor_error)?;
            drop(operation);
        }
        Ok(())
    }

    fn committed(&self) -> Result<(), ManagementError> {
        self.wake.notify_one();
        Ok(())
    }
}

fn lifecycle_monitor_error(error: MonitorError) -> ManagementError {
    match error {
        MonitorError::Unavailable(dependency) => ManagementError::Unavailable(dependency),
        MonitorError::Store(error) => ManagementError::Store(error),
        _ => ManagementError::Invalid("Invalid monitor cancellation"),
    }
}
