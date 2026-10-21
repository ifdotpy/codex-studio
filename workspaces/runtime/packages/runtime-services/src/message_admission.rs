//! Message admission runs before any enqueue write. Policy implementations are
//! injected by the host and must recheck durable preconditions in the caller's
//! transaction; they must not perform filesystem or provider work there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentIndexedReads, AgentRecord, AgentRepository, Connection, RuntimeStore, StoreError,
    Transaction,
};

use crate::coordination::CoordinationError;

/// Synchronous policy port. Successful workspace admission returns the blockers
/// that permit queueing (checkpoint/capture), for the renderer's waitingFor view.
pub trait MessageAdmission: Send + Sync {
    fn workspace(
        &self,
        tx: &Transaction<'_>,
        agent: &AgentRecord,
    ) -> Result<Vec<Value>, CoordinationError>;

    fn budget(
        &self,
        tx: &Transaction<'_>,
        agent: &AgentRecord,
        root: &AgentRecord,
    ) -> Result<(), CoordinationError>;
}

/// Default host adapter. Resolve workspace paths before entering the writer
/// transaction, including idle agents: they may start operations before admission.
/// A newly encountered cwd is refused when its canonical relation cannot be
/// determined from this snapshot. Budget history needs the host's P09 port.
#[derive(Clone, Default)]
pub struct PreparedAdmission {
    paths: BTreeMap<String, PathBuf>,
}

impl PreparedAdmission {
    /// Capture only SQL inputs; resolve paths after the read returns.
    pub fn read(
        connection: &Connection,
        recipient: &str,
    ) -> Result<AdmissionPaths, CoordinationError> {
        let Some(recipient) = AgentRepository::get(connection, &AgentId::from(recipient))? else {
            return Ok(AdmissionPaths(Vec::new()));
        };
        let paths = std::iter::once(Some(cwd(&recipient)?.to_owned()))
            .chain(AgentIndexedReads::admission_cwds(connection)?)
            .flatten()
            .collect();
        Ok(AdmissionPaths(paths))
    }
}

pub struct AdmissionPaths(Vec<String>);

impl AdmissionPaths {
    fn resolve(self) -> Result<PreparedAdmission, CoordinationError> {
        studio_runtime_store::debug_assert_outside_sql();
        let mut paths = BTreeMap::new();
        for cwd in self.0 {
            paths
                .entry(cwd.clone())
                .or_insert_with(|| resolve_path(&cwd));
        }
        Ok(PreparedAdmission { paths })
    }

    pub async fn prepare(self) -> Result<PreparedAdmission, CoordinationError> {
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/message-admission/paths",
            move || self.resolve(),
        )
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
    }
}

impl MessageAdmission for PreparedAdmission {
    fn workspace(
        &self,
        tx: &Transaction<'_>,
        agent: &AgentRecord,
    ) -> Result<Vec<Value>, CoordinationError> {
        let mut query = tx.prepare_cached(AgentIndexedReads::WORKSPACE_OPERATION_IDS_SQL)?;
        let ids = query.query_map([], |row| row.get::<_, String>(0))?;
        let mut blockers = Vec::new();
        let mut unsupported_operation = false;
        for id in ids {
            let id = id?;
            let other =
                AgentRepository::get(tx, &AgentId::from(id.as_str()))?.ok_or_else(|| {
                    CoordinationError::InvalidStored {
                        object: "runtime_agents",
                        id: id.clone(),
                    }
                })?;
            let Some(operation) = other
                .extra
                .get("workspaceOperation")
                .and_then(Value::as_str)
                .filter(|operation| !operation.is_empty())
            else {
                continue;
            };
            let path = self.paths.get(cwd(agent)?);
            let other_path = self.paths.get(cwd(&other)?);
            if path.is_none() || other_path.is_none() {
                // Python resolves both cwd values in its locked snapshot
                // (codex_workspace.py:2219); Rust cannot canonicalize a newly
                // observed cwd inside the writer transaction.
                return Err(CoordinationError::AdmissionDenied(
                    "Workspace admission changed while the message was being prepared".into(),
                ));
            }
            if path != other_path {
                continue;
            }
            unsupported_operation |= !matches!(operation, "checkpoint" | "capture");
            let mut blocker = json!({"agentId":id,"operation":operation});
            if let Some(id) = other
                .extra
                .get("workspaceReservationId")
                .and_then(Value::as_str)
                && let Some(record) =
                    studio_runtime_store::system_reads::get(tx, "runtime_workspace_operations", id)?
            {
                for key in ["phase", "created", "turnId"] {
                    blocker[key] = record.get(key).cloned().unwrap_or(Value::Null);
                }
                blocker["operationId"] = record["id"].clone();
            }
            if !matches!(operation, "checkpoint" | "capture")
                && blocker.get("operationId").is_none()
                && let Some(record) = other_operation(tx, &id)?
            {
                for key in ["phase", "created", "turnId"] {
                    blocker[key] = record.get(key).cloned().unwrap_or(Value::Null);
                }
                blocker["operationId"] = record["id"].clone();
            }
            blockers.push(blocker);
        }
        if unsupported_operation {
            return Err(CoordinationError::AdmissionDenied(format!(
                "A workspace operation is active in this directory: {}",
                studio_runtime_store::python_json(&Value::Array(blockers)),
            )));
        }
        Ok(blockers)
    }

    fn budget(
        &self,
        _tx: &Transaction<'_>,
        _agent: &AgentRecord,
        root: &AgentRecord,
    ) -> Result<(), CoordinationError> {
        if root
            .extra
            .get("tokenBudget")
            .is_some_and(|budget| !budget.is_null())
        {
            return Err(CoordinationError::RequiresDependency(
                "budget-policy/message-admission",
            ));
        }
        Ok(())
    }
}

/// Message ingress adapter that combines P03's workspace snapshot with P09's
/// history-backed budget policy. Filesystem history is read before the caller's
/// writer transaction; durable bindings are rechecked inside that transaction.
pub struct MessageIngressAdmission {
    workspace: PreparedAdmission,
    store: Arc<RuntimeStore>,
    histories: BTreeMap<String, crate::budget::HistoryObservation>,
    now: f64,
}

impl MessageIngressAdmission {
    /// Capture SQL inputs only. Call `prepare` after the read returns.
    pub fn read(
        store: Arc<RuntimeStore>,
        connection: &Connection,
        recipient: &str,
    ) -> Result<MessageIngressSnapshot, CoordinationError> {
        let workspace = PreparedAdmission::read(connection, recipient)?;
        let members = match AgentRepository::get(connection, &AgentId::from(recipient))? {
            Some(agent) => {
                let root = AgentRepository::get(connection, &agent.root_id)?;
                if root.as_ref().is_some_and(|root| {
                    root.extra
                        .get("tokenBudget")
                        .is_some_and(|budget| !budget.is_null())
                        && root.extra.get("tokenBudget").and_then(Value::as_u64) != Some(0)
                }) {
                    Some(crate::budget_runtime::team_members(
                        connection,
                        &agent.root_id.0,
                    )?)
                } else {
                    None
                }
            }
            None => None,
        };
        Ok(MessageIngressSnapshot {
            workspace,
            store,
            members,
        })
    }
}

pub struct MessageIngressSnapshot {
    workspace: AdmissionPaths,
    store: Arc<RuntimeStore>,
    members: Option<Vec<crate::budget_runtime::TeamMember>>,
}

impl MessageIngressSnapshot {
    pub async fn prepare(self) -> Result<MessageIngressAdmission, CoordinationError> {
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/message-admission/history",
            move || {
                let workspace = self.workspace.resolve()?;
                let histories = match self.members {
                    Some(members) => {
                        crate::budget_runtime::observe_histories(self.store.root(), members)?
                    }
                    None => BTreeMap::new(),
                };
                Ok(MessageIngressAdmission {
                    workspace,
                    store: self.store,
                    histories,
                    now: unix_time(),
                })
            },
        )
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?
    }
}

/// SQL-only preparation for callers selecting an admission policy dynamically.
pub enum AdmissionSnapshot {
    Workspace(AdmissionPaths),
    Ingress(MessageIngressSnapshot),
}

impl AdmissionSnapshot {
    pub async fn prepare(self) -> Result<Box<dyn MessageAdmission>, CoordinationError> {
        match self {
            Self::Workspace(snapshot) => Ok(Box::new(snapshot.prepare().await?)),
            Self::Ingress(snapshot) => Ok(Box::new(snapshot.prepare().await?)),
        }
    }
}

impl MessageAdmission for MessageIngressAdmission {
    fn workspace(
        &self,
        tx: &Transaction<'_>,
        agent: &AgentRecord,
    ) -> Result<Vec<Value>, CoordinationError> {
        self.workspace.workspace(tx, agent)
    }

    fn budget(
        &self,
        tx: &Transaction<'_>,
        agent: &AgentRecord,
        root: &AgentRecord,
    ) -> Result<(), CoordinationError> {
        let decision = crate::budget_runtime::admission(
            &self.store,
            tx,
            &agent.value(),
            &root.value(),
            &self.histories,
            self.now,
        )
        .map_err(|error| CoordinationError::BudgetRuntime(error.into()))?;
        if let Some(denial) = decision.denial {
            return Err(CoordinationError::AdmissionDenied(denial.message));
        }
        Ok(())
    }
}

fn unix_time() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn cwd(agent: &AgentRecord) -> Result<&str, CoordinationError> {
    agent
        .extra
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| CoordinationError::InvalidStored {
            object: "runtime_agents",
            id: agent.id.0.clone(),
        })
}

fn resolve_path(path: &str) -> PathBuf {
    studio_runtime_store::debug_assert_outside_sql();
    use std::path::Component;

    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
            }
        }
    }
    resolved
}

/// Runtime.send stores accepted attachment ID strings verbatim for exact
/// replay. The shared payload-store owner validates their shape here; turn
/// preparation loads the referenced asset and applies ownership checks.
pub(crate) fn attachments(assets: &[Value]) -> Result<(), CoordinationError> {
    for asset in assets {
        crate::payload_store::asset_reference_id(asset)
            .map_err(|_| CoordinationError::InvalidRequest("Invalid attachment reference"))?;
    }
    Ok(())
}

fn other_operation(
    tx: &Transaction<'_>,
    agent_id: &str,
) -> Result<Option<Value>, CoordinationError> {
    const ACTIVE_PHASES: [&str; 6] = [
        "provider_pending",
        "provider_ready",
        "local_mutation",
        "recovery_required",
        "capture_pending",
        "capture_running",
    ];
    let mut query = tx.prepare(
        "SELECT id,record FROM runtime_workspace_operations \
         WHERE json_extract(record,'$.agent')=?1 ORDER BY rowid",
    )?;
    let rows = query.query_map([agent_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, raw) = row?;
        let value: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
        if value
            .get("phase")
            .and_then(Value::as_str)
            .is_some_and(|phase| ACTIVE_PHASES.contains(&phase))
        {
            return studio_runtime_store::workspace_operations::WorkspaceOperationsRepository::operation(tx, &id)
                .map_err(CoordinationError::from);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn positive_budget_admission_releases_sql_snapshot_before_history_read() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let project_directory = tempfile::tempdir().expect("temporary project directory");
        let project = project_directory.path().to_path_buf();
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let agent = AgentRecord::from_value(json!({
            "id":"lead", "rootId":"lead", "epoch":1, "status":"idle",
            "isLead":true, "autoWake":true, "accountKey":"default",
            "provider":"codex", "model":"gpt-6-astra", "cwd":project,
            "threadId":"thread-1", "tokenBudget":100
        }))
        .expect("valid lead record");
        store
            .transaction_with_error(move |tx| {
                AgentRepository::put(tx, &agent)?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("persist lead record");

        let read_store = Arc::clone(&store);
        let snapshot = store
            .read_async("tests/message-admission", move |connection| {
                MessageIngressAdmission::read(read_store, connection, "lead")
            })
            .await
            .expect("SQL inputs");
        let admission = snapshot
            .prepare()
            .await
            .expect("positive token budget history loads after the SQL callback");
        assert!(admission.histories.contains_key("lead"));
    }
}
