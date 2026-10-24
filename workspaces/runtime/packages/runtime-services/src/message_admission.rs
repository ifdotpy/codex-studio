//! Message admission runs before any enqueue write. Policy implementations are
//! injected by the host and must recheck durable preconditions in the caller's
//! transaction; they must not perform filesystem or provider work there.

use std::collections::BTreeMap;
use studio_runtime_store::path_identity::PathEvidence;

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentIndexedReads, AgentRecord, AgentRepository, Connection, StoreError, Transaction,
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
}

/// Default host adapter. Resolve workspace paths before entering the writer
/// transaction, including idle agents: they may start operations before admission.
/// A newly encountered cwd is refused when its canonical relation cannot be
/// determined from this snapshot.
#[derive(Clone, Default)]
pub struct PreparedAdmission {
    paths: BTreeMap<String, PathEvidence>,
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
        let mut paths = BTreeMap::new();
        for cwd in self.0 {
            paths
                .entry(cwd.clone())
                .or_insert_with(|| PathEvidence::component_fallback(std::path::Path::new(&cwd)));
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
            let mut reserved = false;
            if operation == "provision"
                && let Some(destination) = provision_destination(tx, &other)?
            {
                // A provision reserves only the destination of the new worktree,
                // as in `Reservation::blocks`. Its owner's cwd is still the
                // source checkout, so neither the parent, the siblings nor the
                // owner itself waits for it. The owner's next preparation adopts
                // or refuses the worktree.
                let Some(path) = path else {
                    return Err(CoordinationError::AdmissionDenied(
                        "Workspace admission changed while the message was being prepared".into(),
                    ));
                };
                if !path.as_path().starts_with(&destination) {
                    continue;
                }
                reserved = true;
            }
            if !reserved {
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
}

/// The admission of message ingress: the workspace snapshot of P03. Paths are
/// resolved before the caller's writer transaction.
pub struct MessageIngressAdmission {
    workspace: PreparedAdmission,
}

impl MessageIngressAdmission {
    /// Capture SQL inputs only. Call `prepare` after the read returns.
    pub fn read(
        connection: &Connection,
        recipient: &str,
    ) -> Result<MessageIngressSnapshot, CoordinationError> {
        Ok(MessageIngressSnapshot {
            workspace: PreparedAdmission::read(connection, recipient)?,
        })
    }
}

pub struct MessageIngressSnapshot {
    workspace: AdmissionPaths,
}

impl MessageIngressSnapshot {
    pub async fn prepare(self) -> Result<MessageIngressAdmission, CoordinationError> {
        Ok(MessageIngressAdmission {
            workspace: self.workspace.prepare().await?,
        })
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

/// The destination that the provisioning journal of `agent` reserves. The
/// provisioning owner builds it under the canonical repository root and
/// refuses a linked ancestor, so its recorded form is its identity.
fn provision_destination(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
) -> Result<Option<std::path::PathBuf>, CoordinationError> {
    let Some(id) = agent
        .extra
        .get("workspaceReservationId")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let Some(record) =
        studio_runtime_store::system_reads::get(tx, "runtime_workspace_operations", id)?
    else {
        return Ok(None);
    };
    let owned = record.get("kind").and_then(Value::as_str) == Some("provision")
        && record.get("agent").and_then(Value::as_str) == Some(agent.id.0.as_str());
    let destination = record.get("cwd").and_then(Value::as_str);
    Ok(destination.filter(|_| owned).map(std::path::PathBuf::from))
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
    use std::sync::Arc;
    use studio_runtime_store::RuntimeStore;

    #[tokio::test]
    async fn a_child_provision_reserves_only_its_destination() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let project_directory = tempfile::tempdir().expect("temporary project directory");
        let project =
            studio_runtime_store::path_identity::CanonicalPath::existing(project_directory.path())
                .expect("canonical project")
                .into_path_buf();
        let destination = project.join(".worktrees/codex-agents/child");
        std::fs::create_dir_all(destination.join("project")).expect("child worktree");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open runtime store"),
        );
        let operation = "provision:child:0";
        let agents = [
            json!({"id":"lead","rootId":"lead","epoch":0,"status":"idle","isLead":true,
                "autoWake":true,"cwd":project}),
            // A failed provisioning keeps its reservation; its cwd is the source.
            json!({"id":"child","rootId":"lead","parentId":"lead","epoch":0,"status":"failed",
                "autoWake":true,"cwd":project,"worktree":true,"worktreeReady":false,
                "workspaceOperation":"provision","workspaceReservationId":operation,
                "worktreePreparation":"preparing"}),
            json!({"id":"inside","rootId":"inside","epoch":0,"status":"idle","autoWake":true,
                "cwd":destination.join("project")}),
        ];
        let record = json!({"id":operation,"kind":"provision","agent":"child","epoch":0,
            "phase":"recovery_required","cwd":destination});
        store
            .transaction_with_error(move |tx| {
                for agent in agents {
                    AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
                }
                tx.execute(
                    "INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)",
                    rusqlite::params![operation, record.to_string()],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("seed agents");
        for (recipient, admitted) in [("lead", true), ("child", true), ("inside", false)] {
            let prepared = store
                .read_async("tests/message-admission", move |connection| {
                    PreparedAdmission::read(connection, recipient)
                })
                .await
                .expect("SQL inputs")
                .prepare()
                .await
                .expect("paths");
            let decision = store
                .read_async("tests/message-admission", move |tx| {
                    let agent =
                        AgentRepository::get(tx, &AgentId::from(recipient))?.expect("recipient");
                    Ok::<_, CoordinationError>(prepared.workspace(tx, &agent).is_ok())
                })
                .await
                .expect("admission");
            assert_eq!(decision, admitted, "{recipient}");
        }
    }
}
