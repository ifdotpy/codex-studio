//! Message admission runs before any enqueue write. Policy implementations are
//! injected by the host and must recheck durable preconditions in the caller's
//! transaction; they must not perform filesystem or provider work there.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value, json};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, Connection, Transaction};

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
/// transaction. A path changed since preparation is refused, never admitted on
/// the basis of an obsolete snapshot. Budget history needs the host's P09 port.
#[derive(Default)]
pub struct PreparedAdmission {
    paths: BTreeMap<String, PathBuf>,
}

impl PreparedAdmission {
    pub fn load(connection: &Connection, recipient: &str) -> Result<Self, CoordinationError> {
        if AgentRepository::get(connection, &AgentId::from(recipient))?.is_none() {
            return Ok(Self::default());
        }
        let mut query = connection.prepare(
            "SELECT id FROM runtime_agents WHERE id=?1 OR \
             json_extract(record,'$.workspaceOperation') IS NOT NULL",
        )?;
        let ids = query.query_map([recipient], |row| row.get::<_, String>(0))?;
        let mut paths = BTreeMap::new();
        for id in ids {
            let id = id?;
            let agent = AgentRepository::get(connection, &AgentId::from(id.as_str()))?.ok_or_else(
                || CoordinationError::InvalidStored {
                    object: "runtime_agents",
                    id,
                },
            )?;
            let cwd = cwd(&agent)?;
            paths.entry(cwd.to_owned()).or_insert_with(|| {
                std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd))
            });
        }
        Ok(Self { paths })
    }
}

impl MessageAdmission for PreparedAdmission {
    fn workspace(
        &self,
        tx: &Transaction<'_>,
        agent: &AgentRecord,
    ) -> Result<Vec<Value>, CoordinationError> {
        let mut query = tx.prepare(
            "SELECT id FROM runtime_agents WHERE \
             json_extract(record,'$.workspaceOperation') IS NOT NULL",
        )?;
        let ids = query.query_map([], |row| row.get::<_, String>(0))?;
        let mut blockers = Vec::new();
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
                return Err(CoordinationError::RequiresDependency(
                    "workspace-policy/message-admission",
                ));
            }
            if path != other_path {
                continue;
            }
            if !matches!(operation, "checkpoint" | "capture") {
                return Err(CoordinationError::RequiresDependency(
                    "workspace-policy/message-admission",
                ));
            }
            let mut blocker = json!({"agentId":id,"operation":operation});
            if let Some(id) = other.extra.get("workspaceReservationId").and_then(Value::as_str)
                && let Some(record) = studio_runtime_store::workspace_operations::WorkspaceOperationsRepository::operation(tx, id)?
            {
                if record.get("id").and_then(Value::as_str) != Some(id)
                    || studio_runtime_schema::first_error("PersistedWorkspaceOperationRecord", &record)
                        .map_err(|_| CoordinationError::InvalidStored { object: "runtime_workspace_operations", id: id.to_owned() })?.is_some()
                {
                    return Err(CoordinationError::InvalidStored { object: "runtime_workspace_operations", id: id.to_owned() });
                }
                for key in ["phase", "created", "turnId"] {
                    blocker[key] = record.get(key).cloned().unwrap_or(Value::Null);
                }
                blocker["operationId"] = record["id"].clone();
            }
            blockers.push(blocker);
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

/// Runtime.send stores attachment IDs verbatim. Existence, count and ownership
/// belong to turn_prepare::compile, matching Python message_inputs timing.
/// Non-ID upload requests still require the separately owned upload adapter.
pub(crate) fn attachments(assets: &[Value]) -> Result<(), CoordinationError> {
    if assets.iter().any(|asset| !asset.is_string()) {
        return Err(CoordinationError::RequiresDependency(
            "payload-store/uploads",
        ));
    }
    Ok(())
}
