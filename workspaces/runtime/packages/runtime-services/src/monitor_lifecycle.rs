//! Lifecycle adapter for durable monitor cancellation intents.
//! The account worker still owns native monitor transport and remains unavailable.

use std::sync::Arc;

use serde_json::Value;
use studio_runtime_store::Transaction;
use tokio::sync::Notify;

use crate::{
    agent_lifecycle_runtime::{
        LifecycleCommitFuture, LifecycleEffect, LifecycleEffectKind, LifecyclePorts,
    },
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

/// Ids of the agent's monitors that may still be live, in rowid order. The
/// agent/status index serves the lookup, so monitor history is not scanned.
fn active_monitor_ids(tx: &Transaction<'_>, agent: &str) -> Result<Vec<String>, ManagementError> {
    if !sync_policy::table_exists(tx, "runtime_monitors")? {
        return Ok(Vec::new());
    }
    let statuses = studio_runtime_store::startup_recovery::ACTIVE_MONITOR_STATUSES
        .iter()
        .map(|status| format!("'{status}'"))
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = tx
        .prepare_cached(&format!(
            "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.agent')=?1 \
             AND json_extract(record,'$.status') IN ({statuses}) ORDER BY rowid"
        ))
        .map_err(studio_runtime_store::StoreError::from)?;
    let ids = statement
        .query_map([agent], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(studio_runtime_store::StoreError::from)?;
    Ok(ids)
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
        for id in active_monitor_ids(tx, agent)? {
            // reserve_cancel persists the durable intent in this lifecycle
            // transaction. Dispatch belongs exclusively to MonitorRuntime.
            let (_, operation) = monitor_driver::reserve_cancel(tx, &id, Some(agent), effect.at())
                .map_err(lifecycle_monitor_error)?;
            drop(operation);
        }
        Ok(())
    }

    fn committed(&self) -> LifecycleCommitFuture<'_> {
        Box::pin(async move {
            self.wake.notify_one();
            Ok(())
        })
    }
}

fn lifecycle_monitor_error(error: MonitorError) -> ManagementError {
    match error {
        MonitorError::Unavailable(dependency) => ManagementError::Unavailable(dependency),
        MonitorError::Store(error) => ManagementError::Store(error),
        _ => ManagementError::Invalid("Invalid monitor cancellation"),
    }
}
