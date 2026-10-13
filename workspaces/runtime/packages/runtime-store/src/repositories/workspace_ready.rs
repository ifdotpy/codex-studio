//! Durable identity and compare-before-publication for host worktree provisioning.
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::{
    AgentRecord, AgentRepository, CheckpointRecord, CheckpointsRepository, EventWriteRepository,
    StoreError, StoreResult, Table, WorkspaceSnapshot,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvisionIdentity {
    pub id: String,
    pub agent: String,
    pub source: Value,
    pub repository: PathBuf,
    pub common_dir: PathBuf,
    pub destination: PathBuf,
    pub cwd: PathBuf,
    pub branch: String,
    pub base_commit: String,
}

/// Only workspace-affecting fields participate; unrelated progress updates may continue.
pub fn workspace_generation(actor: &Value) -> Value {
    const FIELDS: &[&str] = &[
        "id",
        "epoch",
        "cwd",
        "accountKey",
        "threadId",
        "rootId",
        "autoWake",
        "deletedAt",
        "worktree",
        "worktreeReady",
        "branch",
        "workerBaseCommit",
        "workspaceMode",
        "workspaceModeExplicit",
        "workspaceInGit",
        "executionMode",
        "environment",
        "imageWorkspace",
        "imageWorkspaceReady",
        "imageWorkspacePhase",
        "layrReady",
        "layrLine",
        "checkpointHistoryHead",
        "lastCompletedTurn",
    ];
    Value::Object(
        FIELDS
            .iter()
            .filter_map(|key| actor.get(*key).map(|v| ((*key).into(), v.clone())))
            .collect(),
    )
}

/// Submission must compare this opaque token inside its transaction before consuming input.
/// The only path accessor is successful validation; refused admission returns no cwd.
/// ```compile_fail
/// use studio_runtime_store::workspace_ready::ReadyToken;
/// fn bypass(token: ReadyToken) { let _ = token.cwd; }
/// ```
#[derive(Clone, PartialEq)]
pub struct ReadyToken {
    agent: String,
    epoch: u64,
    source: Value,
    cwd: PathBuf,
}
impl std::fmt::Debug for ReadyToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadyToken(..)")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyRefusal;

impl ReadyToken {
    pub fn new(record: &Value, cwd: PathBuf) -> Self {
        Self {
            agent: record["id"].as_str().unwrap_or_default().into(),
            epoch: record["epoch"].as_u64().unwrap_or_default(),
            source: workspace_generation(record),
            cwd,
        }
    }
    pub fn validate(&self, tx: &Transaction<'_>) -> StoreResult<Result<PathBuf, ReadyRefusal>> {
        let Some(current) = CheckpointsRepository::workspace_snapshot(tx, &self.agent)? else {
            return Ok(Err(ReadyRefusal));
        };
        let valid = current.agent.record["epoch"] == self.epoch
            && workspace_generation(&current.agent.record) == self.source
            && current.agent.record["autoWake"] == true
            && current
                .agent
                .record
                .get("deletedAt")
                .is_none_or(Value::is_null)
            && !reservations(&current)
                .iter()
                .any(|r| r.agent == self.agent || overlaps(&r.cwd, &self.cwd));
        Ok(if valid {
            Ok(self.cwd.clone())
        } else {
            Err(ReadyRefusal)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub agent: String,
    pub cwd: PathBuf,
    pub operation: String,
    pub id: Option<String>,
    pub phase: Option<String>,
}

pub fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Canonicalize existing paths; unresolved paths remain evidence rather than disappearing.
pub fn reservations(snapshot: &WorkspaceSnapshot) -> Vec<Reservation> {
    let path = |v: &Value| {
        let p = PathBuf::from(v.as_str().unwrap_or("/"));
        p.canonicalize().unwrap_or(p)
    };
    let mut result = Vec::new();
    for other in &snapshot.agents {
        if let Some(kind) = other.record["workspaceOperation"].as_str() {
            let id = other.record["workspaceReservationId"].as_str();
            let op = snapshot
                .operations
                .iter()
                .find(|op| Some(op.id.as_str()) == id)
                .or_else(|| {
                    snapshot
                        .operations
                        .iter()
                        .find(|op| op.record["agent"] == other.id)
                });
            result.push(Reservation {
                agent: other.id.clone(),
                cwd: path(&other.record["cwd"]),
                operation: kind.into(),
                id: id.map(str::to_owned).or_else(|| op.map(|o| o.id.clone())),
                phase: op
                    .and_then(|o| o.record["phase"].as_str())
                    .map(str::to_owned),
            });
        }
    }
    for op in &snapshot.operations {
        if result.iter().any(|r| r.id.as_deref() == Some(&op.id)) {
            continue;
        }
        result.push(Reservation {
            agent: op.record["agent"].as_str().unwrap_or_default().into(),
            cwd: path(&op.record["cwd"]),
            operation: op.record["kind"].as_str().unwrap_or_default().into(),
            id: Some(op.id.clone()),
            phase: op.record["phase"].as_str().map(str::to_owned),
        });
    }
    result
}

fn conflicts(snapshot: &WorkspaceSnapshot, plan: &ProvisionIdentity) -> Vec<Reservation> {
    let source = PathBuf::from(plan.source["cwd"].as_str().unwrap_or("/"));
    let source = source.canonicalize().unwrap_or(source);
    reservations(snapshot)
        .into_iter()
        .filter(|r| {
            !(r.agent == plan.agent
                && r.operation == "provision"
                && r.id.as_deref() == Some(&plan.id))
                && (r.agent == plan.agent
                    || overlaps(&r.cwd, &source)
                    || overlaps(&r.cwd, &plan.destination))
        })
        .collect()
}

pub struct WorkspaceReadyRepository;
impl WorkspaceReadyRepository {
    pub fn journal(connection: &rusqlite::Connection, id: &str) -> StoreResult<Option<Value>> {
        connection
            .query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|raw| serde_json::from_str(&raw).map_err(StoreError::from))
            .transpose()
    }

    pub fn reserve(
        tx: &Transaction<'_>,
        expected: &WorkspaceSnapshot,
        plan: &ProvisionIdentity,
        now: f64,
    ) -> StoreResult<bool> {
        let Some(current) = CheckpointsRepository::workspace_snapshot(tx, &plan.agent)? else {
            return Ok(false);
        };
        if workspace_generation(&current.agent.record) != plan.source
            || conflicts(&current, plan) != conflicts(expected, plan)
            || !conflicts(&current, plan).is_empty()
        {
            return Ok(false);
        }
        let mut operation = if let Some(operation) = Self::journal(tx, &plan.id)? {
            if operation.get("plan") != Some(&serde_json::to_value(plan)?)
                || operation["phase"] == "completed"
            {
                return Ok(false);
            }
            operation
        } else {
            json!({"id":plan.id,"kind":"provision","agent":plan.agent,"cwd":plan.destination,
                "epoch":plan.source.get("epoch"),"created":now,"plan":plan})
        };
        operation["phase"] = json!("local_mutation");
        operation["updated"] = json!(now);
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![plan.id, operation.to_string()])?;
        let mut actor = current.agent.record;
        actor["workspaceOperation"] = json!("provision");
        actor["workspaceReservationId"] = json!(plan.id);
        actor["worktreePreparation"] = json!("preparing");
        write_agent(
            tx,
            &actor,
            &[
                "workspaceOperation",
                "workspaceReservationId",
                "worktreePreparation",
            ],
        )?;
        Ok(true)
    }

    /// Leave evidence even when an actor was deleted, stopped, or replaced during Git.
    pub fn uncertain(
        tx: &Transaction<'_>,
        plan: &ProvisionIdentity,
        reason: &str,
        now: f64,
    ) -> StoreResult<()> {
        if let Some(mut operation) = Self::journal(tx, &plan.id)? {
            if operation.get("plan") != Some(&serde_json::to_value(plan)?)
                || operation["phase"] == "completed"
            {
                return Ok(());
            }
            operation["phase"] = json!("recovery_required");
            operation["error"] = json!(reason);
            operation["updated"] = json!(now);
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![plan.id, operation.to_string()],
            )?;
        }
        Ok(())
    }

    pub fn publish(
        tx: &Transaction<'_>,
        plan: &ProvisionIdentity,
        checkpoint: Result<&Value, &str>,
        now: f64,
    ) -> StoreResult<bool> {
        let Some(mut operation) = Self::journal(tx, &plan.id)? else {
            return Ok(false);
        };
        let current = CheckpointsRepository::agent(tx, &plan.agent)?;
        let relevant = CheckpointsRepository::workspace_snapshot(tx, &plan.agent)?;
        let valid = relevant
            .as_ref()
            .is_some_and(|s| conflicts(s, plan).is_empty())
            && current.as_ref().is_some_and(|a| {
                workspace_generation(&a.record) == plan.source
                    && a.record["autoWake"] == true
                    && a.record.get("deletedAt").is_none_or(Value::is_null)
                    && a.record["workspaceReservationId"] == plan.id
                    && a.record["workspaceOperation"] == "provision"
            })
            && operation.get("plan") == Some(&serde_json::to_value(plan)?)
            && matches!(
                operation["phase"].as_str(),
                Some("local_mutation" | "recovery_required")
            );
        if !valid {
            Self::uncertain(
                tx,
                plan,
                "Actor changed; created worktree remains unattached",
                now,
            )?;
            return Ok(false);
        }
        let Some(current) = current else {
            return Ok(false);
        };
        let mut actor = current.record;
        actor["cwd"] = json!(plan.cwd);
        actor["branch"] = json!(plan.branch);
        actor["worktreeReady"] = json!(true);
        actor["workspaceOperation"] = Value::Null;
        if let Some(object) = actor.as_object_mut() {
            object.remove("worktreePreparation");
            object.remove("workspaceReservationId");
        }
        match checkpoint {
            Ok(value) => {
                let mut value = value.clone();
                let parent = actor.get("checkpointHistoryHead").and_then(Value::as_str);
                let boundary = parent
                    .map(|id| CheckpointsRepository::checkpoint(tx, id))
                    .transpose()?
                    .flatten()
                    .and_then(|p| p.record["historyBoundary"].as_i64())
                    .filter(|b| *b != 0);
                let (delta, boundary) =
                    CheckpointsRepository::history_delta(tx, &plan.agent, boundary)?;
                value["historyParent"] = json!(parent);
                value["historyDelta"] = json!(delta);
                value["historyBoundary"] = json!(boundary);
                value["turnId"] = actor
                    .get("lastCompletedTurn")
                    .cloned()
                    .unwrap_or(Value::Null);
                let record = CheckpointRecord::from_value(value.clone())?;
                Table::<CheckpointRecord>::new(tx).put(&record)?;
                actor["checkpointHistoryHead"] = value["id"].clone();
                actor["checkpointError"] = Value::Null;
            }
            Err(error) => actor["checkpointError"] = json!(error),
        }
        write_agent(
            tx,
            &actor,
            &[
                "cwd",
                "branch",
                "worktreeReady",
                "workspaceOperation",
                "workspaceReservationId",
                "worktreePreparation",
                "checkpointHistoryHead",
                "lastCompletedTurn",
                "checkpointError",
            ],
        )?;
        operation["phase"] = json!("completed");
        operation["updated"] = json!(now);
        operation["error"] = Value::Null;
        tx.execute(
            "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
            params![plan.id, operation.to_string()],
        )?;
        Ok(true)
    }
}

fn write_agent(tx: &Transaction<'_>, value: &Value, fields: &[&str]) -> StoreResult<()> {
    let record = AgentRecord::from_value(value.clone())?;
    AgentRepository::put(tx, &record)?;
    let changes = fields
        .iter()
        .map(|field| (*field, value.get(*field).cloned().unwrap_or(Value::Null)))
        .collect::<Vec<_>>();
    EventWriteRepository::set_agent_sync_values(tx, record.id().0.as_str(), &changes)
}
